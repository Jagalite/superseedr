// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use crate::torrent_manager::{PeerCandidate, PeerSource};

const DEFAULT_CANDIDATE_DEDUPE_WINDOW: Duration = Duration::from_secs(10);
const DEFAULT_SESSION_LIMIT: usize = 512;
const DEFAULT_MAX_CONCURRENT_PER_SUBNET: usize = 4;
const MAX_ENDPOINT_FAILURE_COUNT: u32 = 10;
const MAX_ENDPOINT_COOLDOWN: Duration = Duration::from_secs(1800);
const GREYLIST_FAILURE_THRESHOLD: u32 = 6;
const BLACKLIST_FAILURE_THRESHOLD: u32 = 10;
const GREYLIST_DURATION: Duration = Duration::from_secs(15 * 60);
const BLACKLIST_DURATION: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Copy)]
struct CandidateRecord {
    last_seen_at: Instant,
    source: PeerSource,
}

#[derive(Debug, Clone)]
struct QueuedCandidate {
    info_hash: Vec<u8>,
    candidate: PeerCandidate,
    queued_at: Instant,
}

#[derive(Debug, Default, Clone, Copy)]
struct EndpointState {
    consecutive_failures: u32,
    retry_after: Option<Instant>,
    greylisted_until: Option<Instant>,
    blacklisted_until: Option<Instant>,
}

impl EndpointState {
    fn is_blocked(&self, now: Instant) -> bool {
        self.retry_after
            .is_some_and(|retry_after| now < retry_after)
            || self.greylisted_until.is_some_and(|until| now < until)
            || self.blacklisted_until.is_some_and(|until| now < until)
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SubnetKey {
    V4([u8; 3]),
    V6([u8; 8]),
}

impl SubnetKey {
    fn from_addr(addr: SocketAddr) -> Self {
        match addr.ip() {
            IpAddr::V4(ipv4) => {
                let octets = ipv4.octets();
                Self::V4([octets[0], octets[1], octets[2]])
            }
            IpAddr::V6(ipv6) => {
                let octets = ipv6.octets();
                Self::V6([
                    octets[0], octets[1], octets[2], octets[3], octets[4], octets[5], octets[6],
                    octets[7],
                ])
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchedPeerCandidate {
    pub info_hash: Vec<u8>,
    pub candidate: PeerCandidate,
}

#[derive(Debug)]
pub struct GlobalPeerManager {
    candidate_dedupe_window: Duration,
    session_limit: usize,
    max_concurrent_per_subnet: usize,
    recent_candidates: HashMap<(Vec<u8>, SocketAddr), CandidateRecord>,
    endpoint_states: HashMap<SocketAddr, EndpointState>,
    queued_candidates: VecDeque<QueuedCandidate>,
    queued_candidate_keys: HashSet<(Vec<u8>, SocketAddr)>,
    pending_sessions: HashSet<(Vec<u8>, SocketAddr)>,
    active_peers: HashSet<(Vec<u8>, SocketAddr)>,
    known_peers: HashMap<Vec<u8>, HashSet<SocketAddr>>,
    paused_torrents: HashSet<Vec<u8>>,
}

impl Default for GlobalPeerManager {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalPeerManager {
    pub fn new() -> Self {
        Self {
            candidate_dedupe_window: DEFAULT_CANDIDATE_DEDUPE_WINDOW,
            session_limit: DEFAULT_SESSION_LIMIT,
            max_concurrent_per_subnet: DEFAULT_MAX_CONCURRENT_PER_SUBNET,
            recent_candidates: HashMap::new(),
            endpoint_states: HashMap::new(),
            queued_candidates: VecDeque::new(),
            queued_candidate_keys: HashSet::new(),
            pending_sessions: HashSet::new(),
            active_peers: HashSet::new(),
            known_peers: HashMap::new(),
            paused_torrents: HashSet::new(),
        }
    }

    pub fn with_session_limit(session_limit: usize) -> Self {
        let mut manager = Self::new();
        manager.session_limit = session_limit.max(1);
        manager
    }

    pub fn update_session_limit(
        &mut self,
        now: Instant,
        session_limit: usize,
    ) -> Vec<DispatchedPeerCandidate> {
        self.session_limit = session_limit.max(1);
        self.dispatch_ready_candidates(now)
    }

    pub fn submit_outgoing_candidate(
        &mut self,
        now: Instant,
        info_hash: &[u8],
        candidate: PeerCandidate,
    ) -> Vec<DispatchedPeerCandidate> {
        self.prune_expired(now);

        if !self.should_track_candidate(now, info_hash, candidate) {
            return Vec::new();
        }

        let key = Self::candidate_key(info_hash, candidate.addr);
        self.remember_recent_candidate(now, info_hash, candidate);
        self.queued_candidate_keys.insert(key);
        self.queued_candidates.push_back(QueuedCandidate {
            info_hash: info_hash.to_vec(),
            candidate,
            queued_at: now,
        });

        self.dispatch_ready_candidates(now)
    }

    pub fn reserve_incoming_candidate(
        &mut self,
        now: Instant,
        info_hash: &[u8],
        candidate: PeerCandidate,
    ) -> bool {
        self.prune_expired(now);

        if !self.should_track_candidate(now, info_hash, candidate) {
            return false;
        }
        if !self.can_dispatch_now(info_hash, candidate, now) {
            return false;
        }

        self.remember_recent_candidate(now, info_hash, candidate);
        self.pending_sessions
            .insert(Self::candidate_key(info_hash, candidate.addr));
        true
    }

    pub fn release_pending_candidate(
        &mut self,
        now: Instant,
        info_hash: &[u8],
        peer_addr: SocketAddr,
    ) -> Vec<DispatchedPeerCandidate> {
        self.pending_sessions
            .remove(&Self::candidate_key(info_hash, peer_addr));
        self.dispatch_ready_candidates(now)
    }

    pub fn record_connection_failure(
        &mut self,
        now: Instant,
        info_hash: &[u8],
        peer_addr: SocketAddr,
    ) -> Vec<DispatchedPeerCandidate> {
        self.pending_sessions
            .remove(&Self::candidate_key(info_hash, peer_addr));
        self.active_peers
            .remove(&Self::candidate_key(info_hash, peer_addr));

        let endpoint_state = self.endpoint_states.entry(peer_addr).or_default();
        endpoint_state.consecutive_failures =
            (endpoint_state.consecutive_failures + 1).min(MAX_ENDPOINT_FAILURE_COUNT);

        let backoff_secs = (15 * 2u64.pow(endpoint_state.consecutive_failures - 1))
            .min(MAX_ENDPOINT_COOLDOWN.as_secs());
        endpoint_state.retry_after = Some(now + Duration::from_secs(backoff_secs));

        if endpoint_state.consecutive_failures >= BLACKLIST_FAILURE_THRESHOLD {
            endpoint_state.blacklisted_until = Some(now + BLACKLIST_DURATION);
            endpoint_state.greylisted_until = Some(now + GREYLIST_DURATION);
        } else if endpoint_state.consecutive_failures >= GREYLIST_FAILURE_THRESHOLD {
            endpoint_state.greylisted_until = Some(now + GREYLIST_DURATION);
        }

        self.dispatch_ready_candidates(now)
    }

    pub fn record_peer_connected(&mut self, info_hash: &[u8], peer_addr: SocketAddr) {
        let key = Self::candidate_key(info_hash, peer_addr);
        self.pending_sessions.remove(&key);
        self.active_peers.insert(key);
        self.known_peers
            .entry(info_hash.to_vec())
            .or_default()
            .insert(peer_addr);
        if let Some(endpoint_state) = self.endpoint_states.get_mut(&peer_addr) {
            endpoint_state.clear();
        }
    }

    pub fn record_peer_disconnected(
        &mut self,
        now: Instant,
        info_hash: &[u8],
        peer_addr: SocketAddr,
    ) -> Vec<DispatchedPeerCandidate> {
        let key = Self::candidate_key(info_hash, peer_addr);
        self.pending_sessions.remove(&key);
        self.active_peers.remove(&key);
        self.known_peers
            .entry(info_hash.to_vec())
            .or_default()
            .insert(peer_addr);
        self.dispatch_ready_candidates(now)
    }

    pub fn record_torrent_paused(
        &mut self,
        now: Instant,
        info_hash: &[u8],
    ) -> Vec<DispatchedPeerCandidate> {
        self.paused_torrents.insert(info_hash.to_vec());
        self.recent_candidates
            .retain(|(candidate_info_hash, _), _| candidate_info_hash.as_slice() != info_hash);
        self.clear_torrent_runtime_state(info_hash);
        self.dispatch_ready_candidates(now)
    }

    pub fn record_torrent_resumed(
        &mut self,
        now: Instant,
        info_hash: &[u8],
    ) -> Vec<DispatchedPeerCandidate> {
        self.paused_torrents.remove(info_hash);

        if let Some(known_peers) = self.known_peers.get(info_hash).cloned() {
            for peer_addr in known_peers {
                let _ = self.enqueue_candidate_if_allowed(
                    now,
                    info_hash,
                    PeerCandidate::from_resume(peer_addr),
                );
            }
        }

        self.dispatch_ready_candidates(now)
    }

    pub fn remove_torrent(
        &mut self,
        now: Instant,
        info_hash: &[u8],
    ) -> Vec<DispatchedPeerCandidate> {
        self.recent_candidates
            .retain(|(candidate_info_hash, _), _| candidate_info_hash.as_slice() != info_hash);
        self.known_peers.remove(info_hash);
        self.paused_torrents.remove(info_hash);
        self.clear_torrent_runtime_state(info_hash);
        self.dispatch_ready_candidates(now)
    }

    fn enqueue_candidate_if_allowed(
        &mut self,
        now: Instant,
        info_hash: &[u8],
        candidate: PeerCandidate,
    ) -> bool {
        if !self.should_track_candidate(now, info_hash, candidate) {
            return false;
        }

        let key = Self::candidate_key(info_hash, candidate.addr);
        self.remember_recent_candidate(now, info_hash, candidate);
        self.queued_candidate_keys.insert(key);
        self.queued_candidates.push_back(QueuedCandidate {
            info_hash: info_hash.to_vec(),
            candidate,
            queued_at: now,
        });
        true
    }

    fn should_track_candidate(
        &mut self,
        now: Instant,
        info_hash: &[u8],
        candidate: PeerCandidate,
    ) -> bool {
        if self.paused_torrents.contains(info_hash) {
            return false;
        }

        let key = Self::candidate_key(info_hash, candidate.addr);
        if self.active_peers.contains(&key)
            || self.pending_sessions.contains(&key)
            || self.queued_candidate_keys.contains(&key)
            || self.endpoint_is_blocked(candidate.addr, now)
        {
            return false;
        }

        if let Some(record) = self.recent_candidates.get_mut(&key) {
            if now.saturating_duration_since(record.last_seen_at) < self.candidate_dedupe_window {
                record.last_seen_at = now;
                record.source = candidate.source;
                return false;
            }
        }

        true
    }

    fn remember_recent_candidate(
        &mut self,
        now: Instant,
        info_hash: &[u8],
        candidate: PeerCandidate,
    ) {
        self.recent_candidates.insert(
            Self::candidate_key(info_hash, candidate.addr),
            CandidateRecord {
                last_seen_at: now,
                source: candidate.source,
            },
        );
    }

    fn dispatch_ready_candidates(&mut self, now: Instant) -> Vec<DispatchedPeerCandidate> {
        let mut dispatched = Vec::new();

        while self.session_occupancy() < self.session_limit {
            let Some(index) = self.select_next_candidate_index(now) else {
                break;
            };

            let queued = self
                .queued_candidates
                .remove(index)
                .expect("selected queued candidate index should exist");
            self.queued_candidate_keys.remove(&Self::candidate_key(
                &queued.info_hash,
                queued.candidate.addr,
            ));
            self.pending_sessions.insert(Self::candidate_key(
                &queued.info_hash,
                queued.candidate.addr,
            ));
            dispatched.push(DispatchedPeerCandidate {
                info_hash: queued.info_hash,
                candidate: queued.candidate,
            });
        }

        dispatched
    }

    fn select_next_candidate_index(&self, now: Instant) -> Option<usize> {
        if self.session_occupancy() >= self.session_limit {
            return None;
        }

        let mut best_index = None;
        let mut best_load = usize::MAX;
        let mut best_queued_at = now;

        for (index, queued) in self.queued_candidates.iter().enumerate() {
            if !self.can_dispatch_now(&queued.info_hash, queued.candidate, now) {
                continue;
            }

            let torrent_load = self.torrent_load(&queued.info_hash);
            if torrent_load < best_load
                || (torrent_load == best_load && queued.queued_at <= best_queued_at)
            {
                best_index = Some(index);
                best_load = torrent_load;
                best_queued_at = queued.queued_at;
            }
        }

        best_index
    }

    fn can_dispatch_now(&self, info_hash: &[u8], candidate: PeerCandidate, now: Instant) -> bool {
        if self.paused_torrents.contains(info_hash) {
            return false;
        }
        if self.endpoint_is_blocked(candidate.addr, now) {
            return false;
        }
        if self.session_occupancy() >= self.session_limit {
            return false;
        }

        let subnet_key = SubnetKey::from_addr(candidate.addr);
        self.subnet_occupancy(subnet_key) < self.max_concurrent_per_subnet
    }

    fn endpoint_is_blocked(&self, peer_addr: SocketAddr, now: Instant) -> bool {
        self.endpoint_states
            .get(&peer_addr)
            .is_some_and(|endpoint_state| endpoint_state.is_blocked(now))
    }

    fn session_occupancy(&self) -> usize {
        self.pending_sessions.len() + self.active_peers.len()
    }

    fn torrent_load(&self, info_hash: &[u8]) -> usize {
        self.pending_sessions
            .iter()
            .filter(|(candidate_info_hash, _)| candidate_info_hash.as_slice() == info_hash)
            .count()
            + self
                .active_peers
                .iter()
                .filter(|(candidate_info_hash, _)| candidate_info_hash.as_slice() == info_hash)
                .count()
    }

    fn subnet_occupancy(&self, subnet_key: SubnetKey) -> usize {
        self.pending_sessions
            .iter()
            .filter(|(_, peer_addr)| SubnetKey::from_addr(*peer_addr) == subnet_key)
            .count()
            + self
                .active_peers
                .iter()
                .filter(|(_, peer_addr)| SubnetKey::from_addr(*peer_addr) == subnet_key)
                .count()
    }

    fn clear_torrent_runtime_state(&mut self, info_hash: &[u8]) {
        self.pending_sessions
            .retain(|(candidate_info_hash, _)| candidate_info_hash.as_slice() != info_hash);
        self.active_peers
            .retain(|(candidate_info_hash, _)| candidate_info_hash.as_slice() != info_hash);

        self.queued_candidates
            .retain(|queued| queued.info_hash.as_slice() != info_hash);
        self.queued_candidate_keys
            .retain(|(candidate_info_hash, _)| candidate_info_hash.as_slice() != info_hash);
    }

    fn prune_expired(&mut self, now: Instant) {
        let dedupe_window = self.candidate_dedupe_window;
        self.recent_candidates
            .retain(|_, record| now.saturating_duration_since(record.last_seen_at) < dedupe_window);

        self.endpoint_states.retain(|_, endpoint_state| {
            endpoint_state.consecutive_failures > 0
                || endpoint_state
                    .retry_after
                    .is_some_and(|retry_after| now < retry_after)
                || endpoint_state
                    .greylisted_until
                    .is_some_and(|until| now < until)
                || endpoint_state
                    .blacklisted_until
                    .is_some_and(|until| now < until)
        });
    }

    fn candidate_key(info_hash: &[u8], peer_addr: SocketAddr) -> (Vec<u8>, SocketAddr) {
        (info_hash.to_vec(), peer_addr)
    }
}

#[cfg(test)]
mod tests {
    use super::{DispatchedPeerCandidate, GlobalPeerManager, BLACKLIST_FAILURE_THRESHOLD};
    use crate::torrent_manager::{PeerCandidate, PeerSource};
    use proptest::prelude::*;
    use std::collections::HashSet;
    use std::net::SocketAddr;
    use std::time::{Duration, Instant};

    fn dispatched_contains(
        dispatched: &[DispatchedPeerCandidate],
        info_hash: &[u8],
        candidate: PeerCandidate,
    ) -> bool {
        dispatched
            .iter()
            .any(|item| item.info_hash.as_slice() == info_hash && item.candidate == candidate)
    }

    fn info_hash_from_index(index: u8) -> Vec<u8> {
        vec![index; 20]
    }

    fn candidate_from_index(index: u8) -> PeerCandidate {
        let source = match index % 4 {
            0 => PeerSource::Dht,
            1 => PeerSource::TrackerHttp,
            2 => PeerSource::TrackerUdp,
            _ => PeerSource::Resume,
        };
        let addr = match index % 6 {
            0 => SocketAddr::from(([10, 0, 0, 1], 6000 + index as u16)),
            1 => SocketAddr::from(([10, 0, 0, 2], 6000 + index as u16)),
            2 => SocketAddr::from(([10, 0, 1, 3], 6000 + index as u16)),
            3 => SocketAddr::from(([172, 16, 0, 4], 6000 + index as u16)),
            4 => format!("[2001:db8::{:x}]:{}", index as u16 + 1, 6000 + index as u16)
                .parse()
                .expect("ipv6 peer"),
            _ => format!(
                "[2001:db8:1::{:x}]:{}",
                index as u16 + 1,
                6000 + index as u16
            )
            .parse()
            .expect("ipv6 peer"),
        };

        PeerCandidate::new(addr, source)
    }

    fn assert_manager_invariants(manager: &GlobalPeerManager) {
        assert!(
            manager.session_occupancy() <= manager.session_limit,
            "session occupancy must never exceed limit"
        );

        let pending: HashSet<_> = manager.pending_sessions.iter().cloned().collect();
        let active: HashSet<_> = manager.active_peers.iter().cloned().collect();
        let queued_from_vec: HashSet<_> = manager
            .queued_candidates
            .iter()
            .map(|queued| (queued.info_hash.clone(), queued.candidate.addr))
            .collect();

        assert_eq!(
            queued_from_vec.len(),
            manager.queued_candidates.len(),
            "queued candidates should not contain duplicate keys"
        );
        assert_eq!(
            queued_from_vec, manager.queued_candidate_keys,
            "queued candidate key index should mirror queued candidates"
        );
        assert!(
            pending.is_disjoint(&active),
            "a peer cannot be both pending and active"
        );
        assert!(
            pending.is_disjoint(&queued_from_vec),
            "a peer cannot be pending and queued"
        );
        assert!(
            active.is_disjoint(&queued_from_vec),
            "a peer cannot be active and queued"
        );
    }

    #[test]
    fn admits_first_candidate_and_suppresses_duplicate_within_window() {
        let mut manager = GlobalPeerManager::new();
        let now = Instant::now();
        let info_hash = vec![1; 20];
        let candidate =
            PeerCandidate::new(SocketAddr::from(([127, 0, 0, 1], 6881)), PeerSource::Dht);

        let dispatched = manager.submit_outgoing_candidate(now, &info_hash, candidate);
        assert!(dispatched_contains(&dispatched, &info_hash, candidate));
        assert!(manager
            .submit_outgoing_candidate(now + Duration::from_secs(1), &info_hash, candidate)
            .is_empty());
    }

    #[test]
    fn allows_same_peer_for_different_torrents() {
        let mut manager = GlobalPeerManager::new();
        let now = Instant::now();
        let candidate = PeerCandidate::new(
            SocketAddr::from(([127, 0, 0, 1], 6881)),
            PeerSource::TrackerHttp,
        );

        let first = manager.submit_outgoing_candidate(now, &[1; 20], candidate);
        let second = manager.submit_outgoing_candidate(now, &[2; 20], candidate);

        assert!(dispatched_contains(&first, &[1; 20], candidate));
        assert!(dispatched_contains(&second, &[2; 20], candidate));
    }

    #[test]
    fn allows_candidate_again_after_dedupe_window_expires() {
        let mut manager = GlobalPeerManager::new();
        let now = Instant::now();
        let info_hash = vec![3; 20];
        let candidate =
            PeerCandidate::new(SocketAddr::from(([127, 0, 0, 1], 7000)), PeerSource::Resume);

        assert!(dispatched_contains(
            &manager.submit_outgoing_candidate(now, &info_hash, candidate),
            &info_hash,
            candidate
        ));
        manager.record_connection_failure(now, &info_hash, candidate.addr);
        assert!(dispatched_contains(
            &manager.submit_outgoing_candidate(
                now + Duration::from_secs(16),
                &info_hash,
                candidate
            ),
            &info_hash,
            candidate
        ));
    }

    #[test]
    fn suppresses_candidate_while_endpoint_is_on_cooldown() {
        let mut manager = GlobalPeerManager::new();
        let now = Instant::now();
        let info_hash = vec![4; 20];
        let candidate =
            PeerCandidate::new(SocketAddr::from(([127, 0, 0, 1], 7100)), PeerSource::Dht);

        manager.record_connection_failure(now, &info_hash, candidate.addr);

        assert!(manager
            .submit_outgoing_candidate(now + Duration::from_secs(1), &info_hash, candidate)
            .is_empty());
        assert!(dispatched_contains(
            &manager.submit_outgoing_candidate(
                now + Duration::from_secs(16),
                &info_hash,
                candidate
            ),
            &info_hash,
            candidate
        ));
    }

    #[test]
    fn peer_lifecycle_clears_endpoint_cooldown_once_connection_ends() {
        let mut manager = GlobalPeerManager::new();
        let now = Instant::now();
        let info_hash = vec![5; 20];
        let candidate = PeerCandidate::new(
            SocketAddr::from(([127, 0, 0, 1], 7200)),
            PeerSource::TrackerUdp,
        );

        manager.record_connection_failure(now, &info_hash, candidate.addr);
        manager.reserve_incoming_candidate(now + Duration::from_secs(16), &info_hash, candidate);
        manager.record_peer_connected(&info_hash, candidate.addr);
        assert!(manager
            .submit_outgoing_candidate(now + Duration::from_secs(17), &info_hash, candidate)
            .is_empty());

        let dispatched = manager.record_peer_disconnected(
            now + Duration::from_secs(18),
            &info_hash,
            candidate.addr,
        );
        assert!(dispatched.is_empty());
        assert!(dispatched_contains(
            &manager.submit_outgoing_candidate(
                now + Duration::from_secs(29),
                &info_hash,
                candidate
            ),
            &info_hash,
            candidate
        ));
    }

    #[test]
    fn suppresses_candidate_while_peer_is_already_active_for_torrent() {
        let mut manager = GlobalPeerManager::new();
        let now = Instant::now();
        let info_hash = vec![6; 20];
        let candidate =
            PeerCandidate::new(SocketAddr::from(([127, 0, 0, 1], 7300)), PeerSource::Pex);

        assert!(manager.reserve_incoming_candidate(now, &info_hash, candidate));
        manager.record_peer_connected(&info_hash, candidate.addr);

        assert!(manager
            .submit_outgoing_candidate(now + Duration::from_secs(1), &info_hash, candidate)
            .is_empty());
    }

    #[test]
    fn endpoint_blacklist_escalates_and_clears_on_successful_connection() {
        let mut manager = GlobalPeerManager::new();
        let now = Instant::now();
        let info_hash = vec![9; 20];
        let candidate =
            PeerCandidate::new(SocketAddr::from(([10, 10, 0, 9], 6881)), PeerSource::Dht);

        for step in 0..BLACKLIST_FAILURE_THRESHOLD {
            manager.record_connection_failure(
                now + Duration::from_secs(step as u64),
                &info_hash,
                candidate.addr,
            );
        }

        let endpoint_state = manager
            .endpoint_states
            .get(&candidate.addr)
            .expect("endpoint state");
        assert!(endpoint_state.greylisted_until.is_some());
        assert!(endpoint_state.blacklisted_until.is_some());

        manager.record_peer_connected(&info_hash, candidate.addr);
        let cleared = manager
            .endpoint_states
            .get(&candidate.addr)
            .expect("endpoint state after connect");
        assert_eq!(cleared.consecutive_failures, 0);
        assert!(cleared.retry_after.is_none());
        assert!(cleared.greylisted_until.is_none());
        assert!(cleared.blacklisted_until.is_none());
    }

    #[test]
    fn reducing_session_limit_does_not_dispatch_new_candidates_until_capacity_returns() {
        let mut manager = GlobalPeerManager::with_session_limit(2);
        let now = Instant::now();
        let info_hash = vec![8; 20];
        let first = candidate_from_index(0);
        let second = candidate_from_index(1);
        let third = candidate_from_index(2);

        assert_eq!(
            manager
                .submit_outgoing_candidate(now, &info_hash, first)
                .len(),
            1
        );
        assert_eq!(
            manager
                .submit_outgoing_candidate(now + Duration::from_secs(1), &info_hash, second)
                .len(),
            1
        );
        let queued =
            manager.submit_outgoing_candidate(now + Duration::from_secs(2), &info_hash, third);
        assert!(queued.is_empty());

        let dispatched = manager.update_session_limit(now + Duration::from_secs(3), 1);
        assert!(dispatched.is_empty());

        let after_disconnect =
            manager.record_peer_disconnected(now + Duration::from_secs(4), &info_hash, first.addr);
        assert!(after_disconnect.is_empty());

        let after_second_disconnect =
            manager.record_peer_disconnected(now + Duration::from_secs(5), &info_hash, second.addr);
        assert!(dispatched_contains(
            &after_second_disconnect,
            &info_hash,
            third
        ));
    }

    proptest! {
        #[test]
        fn invariants_hold_across_mixed_lifecycle_operations(
            steps in proptest::collection::vec((0u8..=6u8, 0u8..=3u8, 0u8..=11u8, 0u8..=8u8), 1..128)
        ) {
            let mut manager = GlobalPeerManager::with_session_limit(3);
            let mut now = Instant::now();

            for (op, torrent_ix, peer_ix, dt_secs) in steps {
                now += Duration::from_secs(dt_secs as u64);
                let info_hash = info_hash_from_index(torrent_ix);
                let candidate = candidate_from_index(peer_ix);

                match op {
                    0 => {
                        let _ = manager.submit_outgoing_candidate(now, &info_hash, candidate);
                    }
                    1 => {
                        let _ = manager.record_connection_failure(now, &info_hash, candidate.addr);
                    }
                    2 => {
                        let dispatched = manager.submit_outgoing_candidate(now, &info_hash, candidate);
                        if dispatched_contains(&dispatched, &info_hash, candidate) {
                            manager.record_peer_connected(&info_hash, candidate.addr);
                        }
                    }
                    3 => {
                        let _ = manager.record_peer_disconnected(now, &info_hash, candidate.addr);
                    }
                    4 => {
                        let _ = manager.record_torrent_paused(now, &info_hash);
                    }
                    5 => {
                        let _ = manager.record_torrent_resumed(now, &info_hash);
                    }
                    6 => {
                        let _ = manager.remove_torrent(now, &info_hash);
                    }
                    _ => unreachable!("operation generator only emits 0..=6"),
                }

                assert_manager_invariants(&manager);
            }
        }
    }

    #[test]
    fn allows_candidate_again_after_peer_disconnects() {
        let mut manager = GlobalPeerManager::new();
        let now = Instant::now();
        let info_hash = vec![7; 20];
        let candidate = PeerCandidate::new(
            SocketAddr::from(([127, 0, 0, 1], 7400)),
            PeerSource::TrackerHttp,
        );

        assert!(manager.reserve_incoming_candidate(now, &info_hash, candidate));
        manager.record_peer_connected(&info_hash, candidate.addr);
        manager.record_peer_disconnected(now + Duration::from_secs(1), &info_hash, candidate.addr);

        assert!(dispatched_contains(
            &manager.submit_outgoing_candidate(
                now + Duration::from_secs(12),
                &info_hash,
                candidate
            ),
            &info_hash,
            candidate
        ));
    }

    #[test]
    fn queued_candidates_dispatch_after_capacity_frees() {
        let mut manager = GlobalPeerManager::new();
        manager.update_session_limit(Instant::now(), 1);

        let now = Instant::now();
        let info_hash = vec![8; 20];
        let first = PeerCandidate::new(
            SocketAddr::from(([127, 0, 0, 1], 7500)),
            PeerSource::TrackerHttp,
        );
        let second = PeerCandidate::new(
            SocketAddr::from(([127, 0, 0, 2], 7501)),
            PeerSource::TrackerUdp,
        );

        assert!(dispatched_contains(
            &manager.submit_outgoing_candidate(now, &info_hash, first),
            &info_hash,
            first
        ));
        assert!(manager
            .submit_outgoing_candidate(now + Duration::from_secs(1), &info_hash, second)
            .is_empty());

        let dispatched =
            manager.record_connection_failure(now + Duration::from_secs(2), &info_hash, first.addr);
        assert!(dispatched_contains(&dispatched, &info_hash, second));
    }

    #[test]
    fn subnet_limit_prevents_storming_single_subnet() {
        let mut manager = GlobalPeerManager::new();
        manager.update_session_limit(Instant::now(), 8);

        let now = Instant::now();
        let info_hash = vec![9; 20];
        let mut dispatched_total = 0;

        for port in 7600..7606 {
            let candidate = PeerCandidate::new(
                SocketAddr::from(([192, 168, 10, port as u8], port)),
                PeerSource::Dht,
            );
            dispatched_total += manager
                .submit_outgoing_candidate(now, &info_hash, candidate)
                .len();
        }

        assert_eq!(dispatched_total, 4);
    }

    #[test]
    fn resume_requeues_known_peers_after_pause() {
        let mut manager = GlobalPeerManager::new();
        manager.update_session_limit(Instant::now(), 2);

        let now = Instant::now();
        let info_hash = vec![10; 20];
        let candidate =
            PeerCandidate::new(SocketAddr::from(([127, 0, 0, 1], 7700)), PeerSource::Dht);

        assert!(manager.reserve_incoming_candidate(now, &info_hash, candidate));
        manager.record_peer_connected(&info_hash, candidate.addr);
        manager.record_torrent_paused(now + Duration::from_secs(1), &info_hash);

        let dispatched = manager.record_torrent_resumed(now + Duration::from_secs(12), &info_hash);
        assert!(dispatched_contains(
            &dispatched,
            &info_hash,
            PeerCandidate::from_resume(candidate.addr)
        ));
    }
}
