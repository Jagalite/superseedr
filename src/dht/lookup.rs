// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::bep42::{classify_node, same_public_identity_group};
use super::krpc::KrpcResponseBody;
use super::routing::{xor_distance, RoutingSnapshot};
use super::types::{
    AddressFamily, Bep42State, CompactNode, CompactPeer, InfoHash, LookupId, NodeId, NodeRecord,
    NodeTrust, TransactionId,
};
use std::cmp::{Ordering, Reverse};
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupKind {
    FindNode,
    GetPeers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupTarget {
    Node(NodeId),
    InfoHash(InfoHash),
}

impl LookupTarget {
    pub fn as_node_id(self) -> NodeId {
        match self {
            Self::Node(node_id) => node_id,
            Self::InfoHash(info_hash) => info_hash.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LookupConfig {
    pub initial_concurrency: usize,
    pub concurrency: usize,
    pub max_visits: usize,
    pub max_referrals_per_response: usize,
    pub per_prefix_limit: usize,
    pub termination_k: usize,
}

impl Default for LookupConfig {
    fn default() -> Self {
        Self {
            initial_concurrency: 5,
            concurrency: 5,
            max_visits: 256,
            max_referrals_per_response: 16,
            per_prefix_limit: 2,
            termination_k: 8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LookupRequest {
    pub lookup_id: LookupId,
    pub kind: LookupKind,
    pub target: LookupTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupCandidate {
    pub addr: SocketAddr,
    pub node_id: Option<NodeId>,
    pub trust: NodeTrust,
    pub bep42: Bep42State,
    pub seed_priority: u8,
    pub live_referral_count: u16,
    pub dead_referral_count: u16,
    pub insertion_order: u64,
    pub last_response_at: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupQuery {
    pub transaction_id: TransactionId,
    pub candidate: LookupCandidate,
    pub started_at: Instant,
    pub soft_timed_out: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupUpdate {
    pub completed_query: Option<LookupQuery>,
    pub emitted_peers: Vec<CompactPeer>,
    pub discovered_nodes: Vec<CompactNode>,
    pub finished: bool,
}

impl LookupUpdate {
    fn new(completed_query: Option<LookupQuery>, finished: bool) -> Self {
        Self {
            completed_query,
            emitted_peers: Vec::new(),
            discovered_nodes: Vec::new(),
            finished,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LookupResponder {
    addr: SocketAddr,
    node_id: Option<NodeId>,
    trust: NodeTrust,
    bep42: Bep42State,
}

#[derive(Debug, Clone)]
pub struct LookupState {
    request: LookupRequest,
    family: AddressFamily,
    started_at: Instant,
    frontier: Vec<LookupCandidate>,
    visited: HashSet<SocketAddr>,
    inflight: HashMap<TransactionId, LookupQuery>,
    received_peers: HashSet<SocketAddr>,
    closest_valid_responders: Vec<LookupResponder>,
    next_insertion_order: u64,
    config: LookupConfig,
}

#[derive(Debug, Clone)]
pub struct LookupManager {
    config: LookupConfig,
}

impl LookupManager {
    pub fn new(config: LookupConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &LookupConfig {
        &self.config
    }

    pub fn start(
        &self,
        request: LookupRequest,
        family: AddressFamily,
        routing: &RoutingSnapshot,
        bootstrap_nodes: &[SocketAddr],
        cached_responders: &[NodeRecord],
        now: Instant,
    ) -> LookupState {
        let mut state = LookupState {
            request,
            family,
            started_at: now,
            frontier: Vec::new(),
            visited: HashSet::new(),
            inflight: HashMap::new(),
            received_peers: HashSet::new(),
            closest_valid_responders: Vec::new(),
            next_insertion_order: 0,
            config: self.config.clone(),
        };

        let bootstrap_count = bootstrap_nodes
            .iter()
            .filter(|addr| AddressFamily::for_addr(**addr) == family)
            .count();
        let secure_routing = routing
            .nodes
            .iter()
            .filter(|record| record.family() == family)
            .filter(|record| matches!(record.bep42_state, Bep42State::Compliant | Bep42State::ExemptLocal))
            .count();
        let prefer_bootstrap = secure_routing == 0 || secure_routing < bootstrap_count;

        state.seed_cached_responders(cached_responders);
        state.seed_bootstrap(bootstrap_nodes, prefer_bootstrap);
        state.seed_from_routing(routing, prefer_bootstrap);
        state.resort_frontier();
        state
    }
}

impl LookupState {
    pub fn request(&self) -> LookupRequest {
        self.request
    }

    pub fn target_id(&self) -> NodeId {
        self.request.target.as_node_id()
    }

    pub fn started_at(&self) -> Instant {
        self.started_at
    }

    pub fn inflight_transaction_ids(&self) -> Vec<TransactionId> {
        self.inflight.keys().copied().collect()
    }

    pub fn next_candidates(&self) -> Vec<LookupCandidate> {
        let base_concurrency = if self.visited.is_empty() {
            self.config.initial_concurrency
        } else {
            self.config.concurrency
        };
        let soft_timed_out = self
            .inflight
            .values()
            .filter(|query| query.soft_timed_out)
            .count();
        let target_concurrency = (base_concurrency + soft_timed_out).min(16);
        let active_inflight = self
            .inflight
            .values()
            .filter(|query| !query.soft_timed_out)
            .count();
        let available_slots = target_concurrency.saturating_sub(active_inflight);
        self.frontier
            .iter()
            .filter(|candidate| !self.visited.contains(&candidate.addr))
            .take(available_slots)
            .cloned()
            .collect()
    }

    pub fn mark_inflight(
        &mut self,
        transaction_id: TransactionId,
        addr: SocketAddr,
        now: Instant,
    ) -> Option<LookupQuery> {
        if self.visited.len() >= self.config.max_visits {
            return None;
        }

        let index = self.frontier.iter().position(|candidate| candidate.addr == addr)?;
        let candidate = self.frontier.remove(index);
        self.visited.insert(addr);
        let query = LookupQuery {
            transaction_id,
            candidate,
            started_at: now,
            soft_timed_out: false,
        };
        self.inflight.insert(transaction_id, query.clone());
        Some(query)
    }

    pub fn mark_soft_timeout(&mut self, transaction_id: TransactionId) -> Option<LookupQuery> {
        let query = self.inflight.get_mut(&transaction_id)?;
        if query.soft_timed_out {
            return None;
        }
        query.soft_timed_out = true;
        Some(query.clone())
    }

    pub fn handle_response(
        &mut self,
        transaction_id: TransactionId,
        response: &KrpcResponseBody,
        now: Instant,
    ) -> LookupUpdate {
        let Some(mut query) = self.inflight.remove(&transaction_id) else {
            return LookupUpdate::new(None, self.is_finished());
        };

        if let Some(node_id) = response.node_id() {
            query.candidate.node_id = Some(node_id);
        }
        query.candidate.last_response_at = Some(now);
        self.record_responder(&query.candidate);

        let mut update = LookupUpdate::new(Some(query.clone()), false);
        if matches!(self.request.kind, LookupKind::GetPeers) {
            for peer in response.peers(self.family) {
                if self.received_peers.insert(peer.addr) {
                    update.emitted_peers.push(peer);
                }
            }
        }

        let mut discovered = response.closest_nodes(self.family);
        if discovered.len() > self.config.max_referrals_per_response {
            discovered.truncate(self.config.max_referrals_per_response);
        }
        let inserted = self.absorb_discovered_nodes(discovered);
        update.discovered_nodes = inserted;
        update.finished = self.is_finished();
        update
    }

    pub fn handle_error(&mut self, transaction_id: TransactionId) -> LookupUpdate {
        let completed_query = self.inflight.remove(&transaction_id);
        LookupUpdate::new(completed_query, self.is_finished())
    }

    pub fn handle_timeout(&mut self, transaction_id: TransactionId) -> LookupUpdate {
        let completed_query = self.inflight.remove(&transaction_id);
        LookupUpdate::new(completed_query, self.is_finished())
    }

    pub fn discard_candidate(&mut self, addr: SocketAddr) -> bool {
        if let Some(index) = self.frontier.iter().position(|candidate| candidate.addr == addr) {
            self.frontier.remove(index);
            self.visited.insert(addr);
            return true;
        }
        false
    }

    pub fn is_finished(&self) -> bool {
        if self.inflight.is_empty() && self.frontier.is_empty() {
            return true;
        }

        let eligible = self.eligible_responders();
        if eligible.len() < self.config.termination_k {
            return self.inflight.is_empty()
                && self.frontier.iter().all(|candidate| self.visited.contains(&candidate.addr));
        }

        let target = self.target_id();
        let worst = eligible[self.config.termination_k - 1].node_id;
        let Some(worst) = worst else {
            return false;
        };

        let has_pending_closer = self
            .frontier
            .iter()
            .chain(self.inflight.values().map(|query| &query.candidate))
            .filter(|candidate| termination_eligible(candidate))
            .filter_map(|candidate| candidate.node_id.map(|node_id| (candidate.addr, node_id)))
            .any(|(_, candidate_id)| {
                xor_distance(&candidate_id, &target) < xor_distance(&worst, &target)
            });

        !has_pending_closer
    }

    pub fn cacheable_responders(&self, limit: usize) -> Vec<NodeRecord> {
        self.closest_valid_responders
            .iter()
            .take(limit)
            .map(|responder| NodeRecord {
                addr: responder.addr,
                node_id: responder.node_id,
                last_query_sent_at: None,
                last_query_response_at: None,
                last_inbound_query_at: None,
                consecutive_failures: 0,
                last_changed_at: self.started_at,
                trust: responder.trust,
                bep42_state: responder.bep42,
                dead_referral_count: 0,
                live_referral_count: 0,
                id_churn_count: 0,
            })
            .collect()
    }

    fn seed_from_routing(&mut self, routing: &RoutingSnapshot, prefer_bootstrap: bool) {
        let mut records = routing
            .nodes
            .iter()
            .filter(|record| record.family() == self.family)
            .cloned()
            .collect::<Vec<_>>();
        let target = self.target_id();
        records.sort_by(|left, right| compare_seed_records(left, right, &target));
        records.truncate(16);

        for record in &records {
            let insertion_order = self.next_order();
            self.insert_candidate(candidate_from_record(
                record,
                if prefer_bootstrap { 1 } else { 0 },
                insertion_order,
            ));
        }
    }

    fn seed_cached_responders(&mut self, cached_responders: &[NodeRecord]) {
        for record in cached_responders {
            if record.family() != self.family {
                continue;
            }
            let insertion_order = self.next_order();
            self.insert_candidate(candidate_from_record(record, 0, insertion_order));
        }
    }

    fn seed_bootstrap(&mut self, bootstrap_nodes: &[SocketAddr], prefer_bootstrap: bool) {
        let family = self.family;
        for addr in bootstrap_nodes.iter().copied().filter(|addr| {
            matches!(
                (family, addr),
                (AddressFamily::Ipv4, SocketAddr::V4(_))
                    | (AddressFamily::Ipv6, SocketAddr::V6(_))
            )
        }) {
            let insertion_order = self.next_order();
            self.insert_candidate(LookupCandidate {
                addr,
                node_id: None,
                trust: NodeTrust::Neutral,
                bep42: Bep42State::Unknown,
                seed_priority: if prefer_bootstrap { 0 } else { 1 },
                live_referral_count: 0,
                dead_referral_count: 0,
                insertion_order,
                last_response_at: None,
            });
        }
    }

    fn absorb_discovered_nodes(&mut self, nodes: Vec<CompactNode>) -> Vec<CompactNode> {
        let mut accepted = Vec::new();
        let mut skipped_visited_or_inflight = 0usize;
        let mut skipped_prefix = 0usize;
        let mut skipped_conflict = 0usize;
        for node in nodes {
            if self.visited.contains(&node.addr)
                || self.inflight.values().any(|query| query.candidate.addr == node.addr)
            {
                skipped_visited_or_inflight += 1;
                continue;
            }

            if self.prefix_count(node.addr) >= self.config.per_prefix_limit {
                skipped_prefix += 1;
                continue;
            }

            let candidate = LookupCandidate {
                addr: node.addr,
                node_id: Some(node.id),
                trust: NodeTrust::Neutral,
                bep42: classify_node(node.addr, Some(node.id)),
                seed_priority: 1,
                live_referral_count: 0,
                dead_referral_count: 0,
                insertion_order: self.next_order(),
                last_response_at: None,
            };

            if self.conflicts_with_existing_public_identity(&candidate) {
                skipped_conflict += 1;
                continue;
            }

            if self.insert_candidate(candidate) {
                accepted.push(node);
            }
        }
        trace_lookup_admission(
            self.request.target.as_node_id(),
            format!(
                "admission family={:?} offered={} accepted={} skipped_visited_or_inflight={} skipped_prefix={} skipped_conflict={} frontier={} inflight={} visited={}",
                self.family,
                accepted.len() + skipped_visited_or_inflight + skipped_prefix + skipped_conflict,
                accepted.len(),
                skipped_visited_or_inflight,
                skipped_prefix,
                skipped_conflict,
                self.frontier.len(),
                self.inflight.len(),
                self.visited.len(),
            ),
        );
        accepted
    }

    fn insert_candidate(&mut self, candidate: LookupCandidate) -> bool {
        if self.frontier.iter().any(|existing| existing.addr == candidate.addr) {
            return false;
        }
        self.frontier.push(candidate);
        self.resort_frontier();
        true
    }

    fn record_responder(&mut self, candidate: &LookupCandidate) {
        self.closest_valid_responders.retain(|existing| existing.addr != candidate.addr);
        if let Some(index) = self
            .closest_valid_responders
            .iter()
            .position(|existing| responder_conflicts(existing, candidate))
        {
            if compare_responder_candidate(
                candidate,
                &self.closest_valid_responders[index],
                &self.target_id(),
            ) == Ordering::Less
            {
                self.closest_valid_responders.remove(index);
            } else {
                return;
            }
        }
        self.closest_valid_responders.push(LookupResponder {
            addr: candidate.addr,
            node_id: candidate.node_id,
            trust: candidate.trust,
            bep42: candidate.bep42,
        });
        let target = self.target_id();
        self.closest_valid_responders.sort_by(|left, right| {
            compare_responders(left, right, &target)
        });
        self.closest_valid_responders.truncate(self.config.max_visits.min(64));
    }

    fn eligible_responders(&self) -> Vec<LookupResponder> {
        self.closest_valid_responders
            .iter()
            .filter(|candidate| termination_eligible_responder(candidate))
            .cloned()
            .collect()
    }

    fn prefix_count(&self, addr: SocketAddr) -> usize {
        let prefix = prefix_key(addr);
        self.frontier
            .iter()
            .filter(|candidate| prefix_key(candidate.addr) == prefix)
            .count()
            + self
                .inflight
                .values()
                .filter(|query| prefix_key(query.candidate.addr) == prefix)
                .count()
    }

    fn resort_frontier(&mut self) {
        let target = self.target_id();
        self.frontier.sort_by(|left, right| compare_candidates(left, right, &target));
    }

    fn next_order(&mut self) -> u64 {
        let next = self.next_insertion_order;
        self.next_insertion_order = self.next_insertion_order.saturating_add(1);
        next
    }

    fn conflicts_with_existing_public_identity(&self, candidate: &LookupCandidate) -> bool {
        self.frontier.iter().any(|existing| {
            same_public_identity_group(
                candidate.addr,
                candidate.node_id,
                candidate.bep42,
                existing.addr,
                existing.node_id,
                existing.bep42,
            )
        }) || self.inflight.values().any(|query| {
            same_public_identity_group(
                candidate.addr,
                candidate.node_id,
                candidate.bep42,
                query.candidate.addr,
                query.candidate.node_id,
                query.candidate.bep42,
            )
        }) || self.closest_valid_responders.iter().any(|existing| {
            same_public_identity_group(
                candidate.addr,
                candidate.node_id,
                candidate.bep42,
                existing.addr,
                existing.node_id,
                existing.bep42,
            )
        })
    }
}

fn candidate_from_record(record: &NodeRecord, seed_priority: u8, insertion_order: u64) -> LookupCandidate {
    LookupCandidate {
        addr: record.addr,
        node_id: record.node_id,
        trust: record.trust,
        bep42: record.bep42_state,
        seed_priority,
        live_referral_count: record.live_referral_count,
        dead_referral_count: record.dead_referral_count,
        insertion_order,
        last_response_at: record.last_query_response_at,
    }
}

fn compare_candidates(left: &LookupCandidate, right: &LookupCandidate, target: &NodeId) -> Ordering {
    left.seed_priority
        .cmp(&right.seed_priority)
        .then_with(|| bep42_rank(left.bep42)
        .cmp(&bep42_rank(right.bep42))
        )
        .then_with(|| trust_rank(left.trust).cmp(&trust_rank(right.trust)))
        .then_with(|| compare_candidate_distance(left.node_id, right.node_id, target))
        .then_with(|| referral_quality_rank(left).cmp(&referral_quality_rank(right)))
        .then_with(|| {
            response_recency_rank(left.last_response_at).cmp(&response_recency_rank(right.last_response_at))
        })
        .then_with(|| left.insertion_order.cmp(&right.insertion_order))
}

fn compare_responders(left: &LookupResponder, right: &LookupResponder, target: &NodeId) -> Ordering {
    bep42_rank(left.bep42)
        .cmp(&bep42_rank(right.bep42))
        .then_with(|| trust_rank(left.trust).cmp(&trust_rank(right.trust)))
        .then_with(|| compare_candidate_distance(left.node_id, right.node_id, target))
        .then_with(|| left.addr.cmp(&right.addr))
}

fn compare_responder_candidate(
    candidate: &LookupCandidate,
    existing: &LookupResponder,
    target: &NodeId,
) -> Ordering {
    bep42_rank(candidate.bep42)
        .cmp(&bep42_rank(existing.bep42))
        .then_with(|| trust_rank(candidate.trust).cmp(&trust_rank(existing.trust)))
        .then_with(|| compare_candidate_distance(candidate.node_id, existing.node_id, target))
        .then_with(|| candidate.addr.cmp(&existing.addr))
}

fn compare_seed_records(left: &NodeRecord, right: &NodeRecord, target: &NodeId) -> Ordering {
    bep42_rank(left.bep42_state)
        .cmp(&bep42_rank(right.bep42_state))
        .then_with(|| trust_rank(left.trust).cmp(&trust_rank(right.trust)))
        .then_with(|| compare_candidate_distance(left.node_id, right.node_id, target))
        .then_with(|| left.addr.cmp(&right.addr))
}

fn compare_candidate_distance(
    left: Option<NodeId>,
    right: Option<NodeId>,
    target: &NodeId,
) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => xor_distance(&left, target).cmp(&xor_distance(&right, target)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn termination_eligible(candidate: &LookupCandidate) -> bool {
    candidate.node_id.is_some()
        && candidate.bep42 != Bep42State::NonCompliant
        && candidate.trust != NodeTrust::Suspicious
}

fn termination_eligible_responder(candidate: &LookupResponder) -> bool {
    candidate.node_id.is_some()
        && candidate.bep42 != Bep42State::NonCompliant
        && candidate.trust != NodeTrust::Suspicious
}

fn trust_rank(trust: NodeTrust) -> u8 {
    match trust {
        NodeTrust::Trusted => 0,
        NodeTrust::Neutral => 1,
        NodeTrust::Suspicious => 2,
    }
}

fn bep42_rank(state: Bep42State) -> u8 {
    match state {
        Bep42State::Compliant => 0,
        Bep42State::ExemptLocal => 1,
        Bep42State::Unknown => 2,
        Bep42State::NonCompliant => 3,
    }
}

fn referral_quality_rank(candidate: &LookupCandidate) -> (u16, u16) {
    (candidate.dead_referral_count, u16::MAX - candidate.live_referral_count)
}

fn response_recency_rank(last_response_at: Option<Instant>) -> (u8, Option<Reverse<Instant>>) {
    match last_response_at {
        Some(at) => (0, Some(Reverse(at))),
        None => (1, None),
    }
}

fn responder_conflicts(existing: &LookupResponder, candidate: &LookupCandidate) -> bool {
    same_public_identity_group(
        existing.addr,
        existing.node_id,
        existing.bep42,
        candidate.addr,
        candidate.node_id,
        candidate.bep42,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PrefixKey {
    V4([u8; 3]),
    V6([u8; 8]),
}

fn prefix_key(addr: SocketAddr) -> PrefixKey {
    match addr {
        SocketAddr::V4(addr) => {
            let octets = addr.ip().octets();
            PrefixKey::V4([octets[0], octets[1], octets[2]])
        }
        SocketAddr::V6(addr) => {
            let octets = addr.ip().octets();
            PrefixKey::V6([
                octets[0], octets[1], octets[2], octets[3], octets[4], octets[5], octets[6],
                octets[7],
            ])
        }
    }
}

fn trace_lookup_admission(target: NodeId, message: String) {
    static TRACE_ENABLED: OnceLock<bool> = OnceLock::new();
    static TRACE_TARGET: OnceLock<Option<String>> = OnceLock::new();

    let enabled =
        *TRACE_ENABLED.get_or_init(|| std::env::var_os("SUPERSEEDR_INTERNAL_TRACE").is_some());
    if !enabled {
        return;
    }

    let target_filter = TRACE_TARGET.get_or_init(|| {
        std::env::var("SUPERSEEDR_INTERNAL_TRACE_TARGET")
            .ok()
            .map(|value| value.to_ascii_lowercase())
    });

    if let Some(target_filter) = target_filter {
        if hex::encode(target.as_ref()).to_ascii_lowercase() != *target_filter {
            return;
        }
    }

    eprintln!(
        "[internal-trace target={}] {}",
        hex::encode(target.as_ref()),
        message
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dht::routing::RoutingSnapshot;
    use crate::dht::test_support::{seeded_info_hash, seeded_node_id};
    use std::collections::HashMap;
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Duration;

    #[derive(Clone)]
    enum ScriptedReply {
        Timeout,
        Nodes {
            responder_id: NodeId,
            nodes: Vec<CompactNode>,
        },
        Peers {
            responder_id: NodeId,
            peers: Vec<CompactPeer>,
        },
    }

    #[test]
    fn scripted_replay_walk_reaches_peers() {
        let info_hash = seeded_info_hash(0x40);
        let target = NodeId::from(info_hash);
        let bootstrap_nodes = vec![
            socket(127, 0, 10, 1, 30101),
            socket(127, 0, 10, 2, 30102),
            socket(127, 0, 10, 3, 30103),
        ];
        let layer_one = vec![
            compact_node(0x50, 127, 0, 21, 1, 31101),
            compact_node(0x51, 127, 0, 22, 1, 31102),
            compact_node(0x52, 127, 0, 23, 1, 31103),
            compact_node(0x53, 127, 0, 24, 1, 31104),
        ];
        let layer_two = vec![
            compact_node(0x60, 127, 0, 31, 1, 32101),
            compact_node(0x61, 127, 0, 32, 1, 32102),
            compact_node(0x62, 127, 0, 33, 1, 32103),
        ];
        let expected_peers = vec![
            compact_peer(127, 1, 1, 10, 40101),
            compact_peer(127, 1, 1, 11, 40102),
        ];

        let manager = LookupManager::new(LookupConfig::default());
        let mut now = Instant::now();
        let mut state = manager.start(
            LookupRequest {
                lookup_id: LookupId(1),
                kind: LookupKind::GetPeers,
                target: LookupTarget::InfoHash(info_hash),
            },
            AddressFamily::Ipv4,
            &empty_routing_snapshot(AddressFamily::Ipv4),
            &bootstrap_nodes,
            &[],
            now,
        );

        let mut script = HashMap::from([
            (
                bootstrap_nodes[0],
                ScriptedReply::Nodes {
                    responder_id: seeded_node_id(0x10),
                    nodes: layer_one.clone(),
                },
            ),
            (bootstrap_nodes[1], ScriptedReply::Timeout),
            (bootstrap_nodes[2], ScriptedReply::Timeout),
            (
                layer_one[0].addr,
                ScriptedReply::Nodes {
                    responder_id: layer_one[0].id,
                    nodes: layer_two.clone(),
                },
            ),
            (layer_one[1].addr, ScriptedReply::Timeout),
            (layer_one[2].addr, ScriptedReply::Timeout),
            (layer_one[3].addr, ScriptedReply::Timeout),
            (
                layer_two[0].addr,
                ScriptedReply::Peers {
                    responder_id: layer_two[0].id,
                    peers: expected_peers.clone(),
                },
            ),
            (layer_two[1].addr, ScriptedReply::Timeout),
            (layer_two[2].addr, ScriptedReply::Timeout),
        ]);

        let mut next_tid = 1u32;
        let mut emitted_peers = Vec::new();
        let mut safety = 0usize;

        while !state.is_finished() && safety < 32 {
            let candidates = state.next_candidates();
            if candidates.is_empty() {
                break;
            }

            for candidate in candidates {
                let transaction_id = TransactionId::from(next_tid.to_be_bytes());
                next_tid = next_tid.saturating_add(1);
                assert!(
                    state.mark_inflight(transaction_id, candidate.addr, now).is_some(),
                    "candidate should mark inflight"
                );

                let reply = script
                    .remove(&candidate.addr)
                    .unwrap_or(ScriptedReply::Timeout);
                let update = match reply {
                    ScriptedReply::Timeout => state.handle_timeout(transaction_id),
                    ScriptedReply::Nodes {
                        responder_id,
                        nodes,
                    } => state.handle_response(
                        transaction_id,
                        &KrpcResponseBody::with_closest_nodes(
                            responder_id,
                            &nodes,
                            AddressFamily::Ipv4,
                            b"tk",
                        ),
                        now,
                    ),
                    ScriptedReply::Peers {
                        responder_id,
                        peers,
                    } => state.handle_response(
                        transaction_id,
                        &KrpcResponseBody::with_peers(responder_id, &peers, b"tk"),
                        now,
                    ),
                };
                emitted_peers.extend(update.emitted_peers.into_iter().map(|peer| peer.addr));
            }

            now += Duration::from_millis(10);
            safety += 1;
        }

        assert!(state.is_finished(), "scripted walk should terminate");
        assert_eq!(
            emitted_peers,
            expected_peers.into_iter().map(|peer| peer.addr).collect::<Vec<_>>()
        );
        assert!(
            state.visited.len() >= 6,
            "expected bootstrap and deeper nodes to be visited"
        );
        assert!(
            state
                .cacheable_responders(8)
                .iter()
                .any(|record| record.addr == layer_two[0].addr),
            "peer-bearing responder should be retained for reuse"
        );
        assert_eq!(target, state.target_id());
    }

    #[test]
    fn repeated_same_node_referrals_only_admit_one_candidate() {
        let info_hash = seeded_info_hash(0x22);
        let bootstrap = socket(127, 0, 10, 9, 30999);
        let repeated = compact_node(0x70, 127, 0, 41, 1, 33101);

        let manager = LookupManager::new(LookupConfig::default());
        let now = Instant::now();
        let mut state = manager.start(
            LookupRequest {
                lookup_id: LookupId(2),
                kind: LookupKind::GetPeers,
                target: LookupTarget::InfoHash(info_hash),
            },
            AddressFamily::Ipv4,
            &empty_routing_snapshot(AddressFamily::Ipv4),
            &[bootstrap],
            &[],
            now,
        );

        let bootstrap_tx = TransactionId::from(1u32.to_be_bytes());
        assert!(state.mark_inflight(bootstrap_tx, bootstrap, now).is_some());
        let repeated_nodes = vec![repeated; 8];
        let update = state.handle_response(
            bootstrap_tx,
            &KrpcResponseBody::with_closest_nodes(
                seeded_node_id(0x71),
                &repeated_nodes,
                AddressFamily::Ipv4,
                b"tk",
            ),
            now,
        );

        assert_eq!(update.discovered_nodes.len(), 1);
        assert_eq!(state.frontier.len(), 1);
        assert_eq!(state.frontier[0].addr, repeated.addr);
    }

    fn empty_routing_snapshot(family: AddressFamily) -> RoutingSnapshot {
        RoutingSnapshot {
            family,
            buckets: Vec::new(),
            nodes: Vec::new(),
            replacement_count: 0,
            refresh_due_count: 0,
        }
    }

    fn compact_node(seed: u8, a: u8, b: u8, c: u8, d: u8, port: u16) -> CompactNode {
        CompactNode {
            id: seeded_node_id(seed),
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, d)), port),
        }
    }

    fn compact_peer(a: u8, b: u8, c: u8, d: u8, port: u16) -> CompactPeer {
        CompactPeer {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, d)), port),
        }
    }

    fn socket(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, d)), port)
    }
}
