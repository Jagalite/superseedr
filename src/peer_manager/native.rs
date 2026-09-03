// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::data::*;
use crate::app::TorrentMetrics;
#[cfg(not(test))]
use crate::config::runtime_persistence_dir;
use crate::persistence::atomic::{
    deserialize_versioned_toml, serialize_versioned_toml, write_string_atomically,
};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use web_time::SystemTime;

const MIN_TRANSFER_ABUSE_BYTES: u64 = 256 * 1024 * 1024;
const TRANSFER_ABUSE_MULTIPLIER: u64 = 2;
const RECONNECT_LIMIT: usize = 10;
pub(crate) const RECONNECT_WINDOW: Duration = Duration::from_secs(10);
const EXCESSIVE_TRANSFER_BLOCK_DURATION: Duration = Duration::from_secs(24 * 60 * 60);
const RECONNECT_BLOCK_DURATION: Duration = Duration::from_secs(2 * 60 * 60);
const HISTORY_RETENTION: Duration = Duration::from_secs(60 * 60);
const VIEW_PUBLISH_INTERVAL: Duration = Duration::from_millis(250);
#[cfg(not(test))]
const PEER_POLICY_FILE_NAME: &str = "peer_policy.toml";
const MAX_POLICY_RESTRICTIONS: usize = 1_000_000;
const MAX_PEER_HISTORIES: usize = 1_000_000;
const MAX_POLICY_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const PERSISTENCE_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(20 * 60);

type InfoHash = Vec<u8>;

// Test-only counters keep the manual performance probe out of production builds.
#[cfg(test)]
static PERF_NOTIFICATION_WAKES: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static PERF_NOTIFICATIONS_HANDLED: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static PERF_METRICS_REDUCTIONS: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static PERF_METRICS_REDUCTION_NANOS: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static PERF_VIEW_PUBLICATIONS: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static PERF_VIEW_BUILD_NANOS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ipv6) => ipv6.to_ipv4_mapped().map_or(IpAddr::V6(ipv6), IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

fn encode_superseedr_peer_version(major: u64, minor: u64, patch: u64) -> Option<String> {
    (major <= 9 && minor <= 9 && patch <= 99).then(|| format!("{major}{minor}{patch:02}"))
}

fn superseedr_peer_version_code() -> String {
    let major = env!("CARGO_PKG_VERSION_MAJOR")
        .parse()
        .expect("Cargo package major version must be numeric");
    let minor = env!("CARGO_PKG_VERSION_MINOR")
        .parse()
        .expect("Cargo package minor version must be numeric");
    let patch = env!("CARGO_PKG_VERSION_PATCH")
        .parse()
        .expect("Cargo package patch version must be numeric");
    encode_superseedr_peer_version(major, minor, patch)
        .expect("Superseedr peer IDs support major/minor <= 9 and patch <= 99")
}

pub(crate) fn superseedr_peer_id_prefix() -> String {
    format!(
        "-{}{}-",
        String::from_utf8_lossy(SUPERSEEDR_CLIENT_CODE),
        superseedr_peer_version_code()
    )
}

pub(crate) fn refresh_superseedr_peer_id_version(client_id: &str) -> Option<String> {
    let bytes = client_id.as_bytes();
    let is_superseedr_peer_id = bytes.len() == 20
        && bytes.first() == Some(&b'-')
        && bytes.get(1..3) == Some(SUPERSEEDR_CLIENT_CODE.as_slice())
        && bytes.get(7) == Some(&b'-');
    if !is_superseedr_peer_id {
        return None;
    }

    let prefix = superseedr_peer_id_prefix();
    (!client_id.starts_with(&prefix)).then(|| format!("{prefix}{}", &client_id[8..]))
}

impl PeerPolicy {
    pub(crate) fn blocks_ip(&self, ip: IpAddr, now: SystemTime) -> bool {
        self.restrictions
            .get(&normalize_ip(ip))
            .is_some_and(|restriction| restriction.blocked_until > now)
    }

    pub(crate) fn blocks_peer_address(&self, address: &str, now: SystemTime) -> bool {
        parse_peer_ip(address).is_some_and(|ip| self.blocks_ip(ip, now))
    }

    fn retain_live_and_bounded(&mut self, now: SystemTime) {
        self.retain_live_and_bounded_to(now, MAX_POLICY_RESTRICTIONS);
    }

    fn normalize_restrictions(&mut self) -> bool {
        let original = std::mem::take(&mut self.restrictions);
        let original_len = original.len();
        let mut changed = false;
        for (ip, restriction) in original {
            let normalized_ip = normalize_ip(ip);
            changed |= normalized_ip != ip;
            match self.restrictions.entry(normalized_ip) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(restriction);
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    changed = true;
                    if restriction.blocked_until > entry.get().blocked_until {
                        entry.insert(restriction);
                    }
                }
            }
        }
        changed || self.restrictions.len() != original_len
    }

    fn retain_live_and_bounded_to(&mut self, now: SystemTime, max_restrictions: usize) {
        self.restrictions
            .retain(|_, restriction| restriction.blocked_until > now);
        if self.restrictions.len() <= max_restrictions {
            return;
        }

        let mut by_expiry = self
            .restrictions
            .iter()
            .map(|(ip, restriction)| (*ip, restriction.blocked_until))
            .collect::<Vec<_>>();
        by_expiry.sort_unstable_by_key(|(ip, blocked_until)| (*blocked_until, *ip));
        let remove_count = by_expiry.len() - max_restrictions;
        for (ip, _) in by_expiry.into_iter().take(remove_count) {
            self.restrictions.remove(&ip);
        }
    }

    #[cfg(test)]
    pub(crate) fn from_blocked_until(blocked_until: HashMap<IpAddr, SystemTime>) -> Self {
        Self {
            restrictions: blocked_until
                .into_iter()
                .map(|(ip, blocked_until)| {
                    (
                        normalize_ip(ip),
                        PeerRestriction {
                            detected_at: SystemTime::UNIX_EPOCH,
                            blocked_until,
                            torrent_info_hash: None,
                            reason: PeerRestrictionReason::Manual,
                        },
                    )
                })
                .collect(),
        }
    }
}

fn parse_peer_ip(address: &str) -> Option<IpAddr> {
    let address = address
        .split_once("://")
        .map_or(address, |(_, socket_address)| socket_address);
    address
        .parse::<SocketAddr>()
        .map(|address| normalize_ip(address.ip()))
        .or_else(|_| address.parse::<IpAddr>().map(normalize_ip))
        .ok()
}

fn transfer_abuse_limit(total_size: u64) -> u64 {
    total_size
        .saturating_mul(TRANSFER_ABUSE_MULTIPLIER)
        .max(MIN_TRANSFER_ABUSE_BYTES)
}

#[cfg(not(test))]
fn peer_policy_file_path() -> io::Result<PathBuf> {
    runtime_persistence_dir()
        .map(|path| path.join(PEER_POLICY_FILE_NAME))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "peer policy path unavailable"))
}

fn save_peer_policy_to_path(path: &Path, policy: &PeerPolicy) -> io::Result<()> {
    let content = serialize_versioned_toml(policy)?;
    write_string_atomically(path, &content)
}

#[cfg(test)]
fn load_peer_policy_from_path(path: &Path, now: SystemTime) -> io::Result<PeerPolicy> {
    load_peer_policy_state_from_path(path, now).map(|(policy, _)| policy)
}

fn load_peer_policy_state_from_path(
    path: &Path,
    now: SystemTime,
) -> io::Result<(PeerPolicy, bool)> {
    if !path.exists() {
        return Ok((PeerPolicy::default(), false));
    }
    load_peer_policy_from_path_at_limit(path, now, MAX_POLICY_RESTRICTIONS)
}

fn load_peer_policy_from_path_at_limit(
    path: &Path,
    now: SystemTime,
    max_restrictions: usize,
) -> io::Result<(PeerPolicy, bool)> {
    let file_size = fs::metadata(path)?.len();
    if file_size > MAX_POLICY_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "peer policy file is {file_size} bytes; maximum is {MAX_POLICY_FILE_BYTES} bytes"
            ),
        ));
    }
    let content = fs::read_to_string(path)?;
    let mut policy: PeerPolicy = deserialize_versioned_toml(&content)?;
    let requires_reconciliation = policy.normalize_restrictions()
        || policy.restrictions.len() > max_restrictions
        || policy
            .restrictions
            .values()
            .any(|restriction| restriction.blocked_until <= now);
    policy.retain_live_and_bounded_to(now, max_restrictions);
    Ok((policy, requires_reconciliation))
}

#[derive(Debug, Clone, Copy, Default)]
struct EndpointTransferTotals {
    downloaded: u64,
    uploaded: u64,
    connection_count: u64,
    disconnect_count: u64,
}

#[derive(Debug, Default)]
struct PeerTorrentHistory {
    seen: bool,
    present: bool,
    reconnect_events_seen: u64,
    endpoint_transfers: HashMap<String, EndpointTransferTotals>,
    downloaded_bytes: u64,
    uploaded_bytes: u64,
    total_downloaded_bytes: u64,
    total_uploaded_bytes: u64,
    connection_count: u64,
    disconnect_count: u64,
    reconnects: VecDeque<SystemTime>,
    last_seen: Option<SystemTime>,
    clients: BTreeSet<String>,
}

impl PeerTorrentHistory {
    fn observe_snapshot(
        &mut self,
        endpoint_transfers: HashMap<String, EndpointTransferTotals>,
        clients: BTreeSet<String>,
        now: SystemTime,
        infer_reconnect: bool,
        is_present: bool,
    ) {
        let endpoints_replaced = self.present
            && !self.endpoint_transfers.is_empty()
            && !endpoint_transfers.is_empty()
            && self
                .endpoint_transfers
                .keys()
                .all(|endpoint| !endpoint_transfers.contains_key(endpoint));
        let counter_reset = self.present
            && endpoint_transfers.iter().any(|(endpoint, totals)| {
                self.endpoint_transfers
                    .get(endpoint)
                    .is_some_and(|previous| {
                        totals.downloaded < previous.downloaded
                            || totals.uploaded < previous.uploaded
                            || totals.connection_count < previous.connection_count
                            || totals.disconnect_count < previous.disconnect_count
                    })
            });
        if is_present
            && infer_reconnect
            && self.seen
            && (!self.present || endpoints_replaced || counter_reset)
        {
            self.reconnects.push_back(now);
        }
        self.seen = true;
        self.present = is_present;
        self.last_seen = Some(now);
        self.prune_reconnects(now);

        for (endpoint, totals) in &endpoint_transfers {
            let previous = self
                .endpoint_transfers
                .get(endpoint)
                .copied()
                .unwrap_or_default();
            let downloaded_delta = if totals.downloaded >= previous.downloaded {
                totals.downloaded - previous.downloaded
            } else {
                totals.downloaded
            };
            let uploaded_delta = if totals.uploaded >= previous.uploaded {
                totals.uploaded - previous.uploaded
            } else {
                totals.uploaded
            };
            let connection_delta = if totals.connection_count >= previous.connection_count {
                totals.connection_count - previous.connection_count
            } else {
                totals.connection_count
            };
            let disconnect_delta = if totals.disconnect_count >= previous.disconnect_count {
                totals.disconnect_count - previous.disconnect_count
            } else {
                totals.disconnect_count
            };
            self.downloaded_bytes = self.downloaded_bytes.saturating_add(downloaded_delta);
            self.uploaded_bytes = self.uploaded_bytes.saturating_add(uploaded_delta);
            self.total_downloaded_bytes =
                self.total_downloaded_bytes.saturating_add(downloaded_delta);
            self.total_uploaded_bytes = self.total_uploaded_bytes.saturating_add(uploaded_delta);
            self.connection_count = self.connection_count.saturating_add(connection_delta);
            self.disconnect_count = self.disconnect_count.saturating_add(disconnect_delta);
        }
        self.endpoint_transfers = endpoint_transfers;
        if !clients.is_empty() {
            self.clients = clients;
        }
    }

    fn observe_reconnect_count(&mut self, cumulative_reconnects: u64, now: SystemTime) {
        self.prune_reconnects(now);
        let new_reconnects = if cumulative_reconnects >= self.reconnect_events_seen {
            cumulative_reconnects - self.reconnect_events_seen
        } else {
            // A torrent manager restarted and its cumulative counter reset. Every
            // reconnect in the new generation is new evidence for this retained history.
            cumulative_reconnects
        };
        let available = RECONNECT_LIMIT.saturating_sub(self.reconnects.len()) as u64;
        self.reconnects.extend(std::iter::repeat_n(
            now,
            new_reconnects.min(available) as usize,
        ));
        self.reconnect_events_seen = cumulative_reconnects;
        if new_reconnects > 0 {
            self.seen = true;
            self.last_seen = Some(now);
        }
    }

    fn reset_reconnect_count_baseline(&mut self) {
        self.reconnect_events_seen = 0;
    }

    fn observe_absence(&mut self, now: SystemTime) {
        self.present = false;
        self.endpoint_transfers.clear();
        self.prune_reconnects(now);
    }

    fn prune_reconnects(&mut self, now: SystemTime) {
        self.reconnects.retain(|seen_at| {
            now.duration_since(*seen_at)
                .map_or(true, |age| age < RECONNECT_WINDOW)
        });
    }

    fn consume_triggering_evidence(&mut self) {
        self.downloaded_bytes = 0;
        self.uploaded_bytes = 0;
        self.reconnects.clear();
    }

    fn is_stale(&self, now: SystemTime) -> bool {
        !self.present
            && self.last_seen.is_some_and(|last_seen| {
                now.duration_since(last_seen)
                    .is_ok_and(|age| age >= HISTORY_RETENTION)
            })
    }
}

#[derive(Debug, Default)]
struct PeerPolicyReducer {
    histories: HashMap<InfoHash, HashMap<IpAddr, PeerTorrentHistory>>,
    policy: PeerPolicy,
    next_reconnect_expiry: Option<SystemTime>,
}

impl PeerPolicyReducer {
    fn policy(&self) -> &PeerPolicy {
        &self.policy
    }

    fn reduce_metrics(
        &mut self,
        info_hash: &[u8],
        metrics: &TorrentMetrics,
        now: SystemTime,
    ) -> bool {
        let mut changed = false;
        let mut observed = HashMap::<IpAddr, HashMap<String, EndpointTransferTotals>>::new();
        let mut observed_clients = HashMap::<IpAddr, BTreeSet<String>>::new();
        let mut reconnect_counts = HashMap::<IpAddr, u64>::new();
        let mut active_ips = HashSet::<IpAddr>::new();

        for (ip, count) in &metrics.peer_reconnect_counts {
            let total = reconnect_counts.entry(normalize_ip(*ip)).or_default();
            *total = total.saturating_add(*count);
        }
        for (peer, is_active) in metrics
            .peers
            .iter()
            .map(|peer| (peer, true))
            .chain(metrics.departed_peers.iter().map(|peer| (peer, false)))
        {
            let Some(ip) = parse_peer_ip(&peer.address) else {
                continue;
            };
            if is_active {
                active_ips.insert(ip);
            }
            if !peer.peer_id.is_empty() {
                observed_clients
                    .entry(ip)
                    .or_default()
                    .insert(parse_peer_client(&peer.peer_id));
            }
            let endpoint_transfers = observed.entry(ip).or_default();
            let totals = endpoint_transfers.entry(peer.address.clone()).or_default();
            totals.downloaded = totals.downloaded.max(peer.total_downloaded);
            totals.uploaded = totals.uploaded.max(peer.total_uploaded);
            totals.connection_count = totals.connection_count.max(peer.connection_count);
            totals.disconnect_count = totals.disconnect_count.max(peer.disconnect_count);
        }

        let transfer_limit = transfer_abuse_limit(metrics.total_size);
        let mut restrictions = Vec::new();
        let mut next_reconnect_expiry = None;

        {
            let torrent_histories = self.histories.entry(info_hash.to_vec()).or_default();
            for (ip, history) in torrent_histories.iter_mut() {
                if !reconnect_counts.contains_key(ip) {
                    history.reset_reconnect_count_baseline();
                }
                if !active_ips.contains(ip) && !observed.contains_key(ip) {
                    history.observe_absence(now);
                }
            }

            for (ip, cumulative_reconnects) in &reconnect_counts {
                torrent_histories
                    .entry(*ip)
                    .or_default()
                    .observe_reconnect_count(*cumulative_reconnects, now);
            }

            for (ip, endpoint_transfers) in observed {
                let history = torrent_histories.entry(ip).or_default();
                history.observe_snapshot(
                    endpoint_transfers,
                    observed_clients.remove(&ip).unwrap_or_default(),
                    now,
                    !reconnect_counts.contains_key(&ip),
                    active_ips.contains(&ip),
                );
            }

            for (ip, history) in torrent_histories.iter_mut() {
                let reason = if history.uploaded_bytes > transfer_limit {
                    Some(PeerRestrictionReason::ExcessiveUpload {
                        uploaded_bytes: history.uploaded_bytes,
                        threshold_bytes: transfer_limit,
                    })
                } else if history.downloaded_bytes > transfer_limit {
                    Some(PeerRestrictionReason::ExcessiveDownload {
                        downloaded_bytes: history.downloaded_bytes,
                        threshold_bytes: transfer_limit,
                    })
                } else if history.reconnects.len() >= RECONNECT_LIMIT {
                    Some(PeerRestrictionReason::ReconnectChurn {
                        reconnects: history.reconnects.len() as u32,
                        threshold: RECONNECT_LIMIT as u32,
                        window_secs: RECONNECT_WINDOW.as_secs(),
                    })
                } else {
                    None
                };
                if let Some(reason) = reason {
                    history.consume_triggering_evidence();
                    restrictions.push((*ip, reason));
                }
                if let Some(expires_at) = history
                    .reconnects
                    .front()
                    .and_then(|seen_at| seen_at.checked_add(RECONNECT_WINDOW))
                {
                    next_reconnect_expiry = Some(
                        next_reconnect_expiry.map_or(expires_at, |scheduled: SystemTime| {
                            scheduled.min(expires_at)
                        }),
                    );
                }
            }
        }

        for (ip, reason) in restrictions {
            let block_duration = match &reason {
                PeerRestrictionReason::ExcessiveUpload { .. }
                | PeerRestrictionReason::ExcessiveDownload { .. } => {
                    EXCESSIVE_TRANSFER_BLOCK_DURATION
                }
                PeerRestrictionReason::ReconnectChurn { .. } => RECONNECT_BLOCK_DURATION,
                PeerRestrictionReason::Manual => EXCESSIVE_TRANSFER_BLOCK_DURATION,
            };
            let blocked_until = now.checked_add(block_duration).unwrap_or(now);
            changed |= self.restrict_ip(
                ip,
                PeerRestriction {
                    detected_at: now,
                    blocked_until,
                    torrent_info_hash: Some(info_hash.to_vec()),
                    reason,
                },
            );
        }
        if let Some(expires_at) = next_reconnect_expiry {
            self.next_reconnect_expiry = Some(
                self.next_reconnect_expiry
                    .map_or(expires_at, |scheduled| scheduled.min(expires_at)),
            );
        }
        changed
    }

    fn build_view(
        &self,
        latest_metrics: &HashMap<InfoHash, TorrentMetrics>,
        registered_torrents: usize,
        metrics_updates: u64,
    ) -> PeerManagerView {
        let mut tracked_peers = Vec::with_capacity(self.history_count());
        for (info_hash, histories) in &self.histories {
            let metrics = latest_metrics.get(info_hash);
            let torrent_name = metrics
                .map(|metrics| metrics.torrent_name.clone())
                .unwrap_or_default();
            let transfer_threshold_bytes = metrics
                .map(|metrics| transfer_abuse_limit(metrics.total_size))
                .unwrap_or(MIN_TRANSFER_ABUSE_BYTES);

            for (ip, history) in histories {
                let mut endpoints = if history.present {
                    history
                        .endpoint_transfers
                        .iter()
                        .map(|(address, totals)| PeerManagerEndpointView {
                            address: address.clone(),
                            total_downloaded: totals.downloaded,
                            total_uploaded: totals.uploaded,
                        })
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                endpoints.sort_unstable_by(|left, right| left.address.cmp(&right.address));

                tracked_peers.push(PeerManagerTrackedPeer {
                    torrent_info_hash: info_hash.clone(),
                    torrent_name: torrent_name.clone(),
                    ip: *ip,
                    is_active: history.present,
                    endpoints,
                    downloaded_evidence_bytes: history.downloaded_bytes,
                    uploaded_evidence_bytes: history.uploaded_bytes,
                    total_downloaded_bytes: history.total_downloaded_bytes,
                    total_uploaded_bytes: history.total_uploaded_bytes,
                    connection_count: history.connection_count,
                    disconnect_count: history.disconnect_count,
                    transfer_threshold_bytes,
                    reconnect_count: history.reconnects.len() as u32,
                    reconnect_limit: RECONNECT_LIMIT as u32,
                    reconnect_window_secs: RECONNECT_WINDOW.as_secs(),
                    last_seen: history.last_seen,
                    clients: history.clients.iter().cloned().collect(),
                });
            }
        }
        tracked_peers.sort_unstable_by(|left, right| {
            left.torrent_info_hash
                .cmp(&right.torrent_info_hash)
                .then_with(|| left.ip.cmp(&right.ip))
        });

        PeerManagerView {
            registered_torrents,
            metrics_updates,
            tracked_peers,
        }
    }

    #[cfg(test)]
    fn expire(&mut self, now: SystemTime) -> bool {
        self.maintain_to(now, MAX_PEER_HISTORIES)
    }

    fn maintain_to(&mut self, now: SystemTime, max_histories: usize) -> bool {
        for histories in self.histories.values_mut() {
            histories.retain(|_, history| !history.is_stale(now));
        }
        self.histories.retain(|_, histories| !histories.is_empty());

        let history_count = self.history_count();
        if history_count > max_histories {
            let mut by_last_seen = self
                .histories
                .iter()
                .flat_map(|(info_hash, histories)| {
                    histories.iter().map(|(ip, history)| {
                        (
                            history.last_seen.unwrap_or(SystemTime::UNIX_EPOCH),
                            info_hash.clone(),
                            *ip,
                        )
                    })
                })
                .collect::<Vec<_>>();
            by_last_seen.sort_unstable();
            for (_, info_hash, ip) in by_last_seen.into_iter().take(history_count - max_histories) {
                if let Some(histories) = self.histories.get_mut(&info_hash) {
                    histories.remove(&ip);
                }
            }
            self.histories.retain(|_, histories| !histories.is_empty());
        }

        let before = self.policy.restrictions.len();
        self.policy.retain_live_and_bounded(now);
        self.policy.restrictions.len() != before
    }

    fn prune_reconnect_evidence(&mut self, now: SystemTime) -> bool {
        let mut changed = false;
        for histories in self.histories.values_mut() {
            for history in histories.values_mut() {
                let reconnect_count = history.reconnects.len();
                history.prune_reconnects(now);
                changed |= history.reconnects.len() != reconnect_count;
            }
        }
        self.refresh_next_reconnect_expiry();
        changed
    }

    fn refresh_next_reconnect_expiry(&mut self) {
        self.next_reconnect_expiry = self
            .histories
            .values()
            .flat_map(HashMap::values)
            .flat_map(|history| history.reconnects.iter())
            .filter_map(|seen_at| seen_at.checked_add(RECONNECT_WINDOW))
            .min();
    }

    fn next_reconnect_expiry_delay(&self, now: SystemTime) -> Option<Duration> {
        self.next_reconnect_expiry
            .map(|expires_at| expires_at.duration_since(now).unwrap_or(Duration::ZERO))
    }

    fn history_count(&self) -> usize {
        self.histories.values().map(HashMap::len).sum()
    }

    #[cfg(test)]
    fn has_history(&self, info_hash: &[u8], ip: IpAddr) -> bool {
        self.histories
            .get(info_hash)
            .is_some_and(|histories| histories.contains_key(&normalize_ip(ip)))
    }

    fn restrict_ip(&mut self, ip: IpAddr, restriction: PeerRestriction) -> bool {
        let ip = normalize_ip(ip);
        if self
            .policy
            .restrictions
            .get(&ip)
            .is_some_and(|current| current.blocked_until >= restriction.blocked_until)
        {
            return false;
        }

        if !self.policy.restrictions.contains_key(&ip)
            && self.policy.restrictions.len() >= MAX_POLICY_RESTRICTIONS
        {
            let Some((eviction_ip, eviction_deadline)) = self
                .policy
                .restrictions
                .iter()
                .map(|(candidate_ip, candidate)| (*candidate_ip, candidate.blocked_until))
                .min_by_key(|(candidate_ip, blocked_until)| (*blocked_until, *candidate_ip))
            else {
                return false;
            };
            if eviction_deadline >= restriction.blocked_until {
                return false;
            }
            self.policy.restrictions.remove(&eviction_ip);
        }

        self.policy.restrictions.insert(ip, restriction);
        true
    }

    #[cfg(test)]
    fn block_ip_until(&mut self, ip: IpAddr, until: SystemTime) -> bool {
        self.restrict_ip(
            ip,
            PeerRestriction {
                detected_at: until
                    .checked_sub(EXCESSIVE_TRANSFER_BLOCK_DURATION)
                    .unwrap_or(until),
                blocked_until: until,
                torrent_info_hash: None,
                reason: PeerRestrictionReason::Manual,
            },
        )
    }

    fn remove_torrent(&mut self, info_hash: &[u8]) {
        self.histories.remove(info_hash);
        self.refresh_next_reconnect_expiry();
    }
}

#[cfg(test)]
pub(crate) fn default_policy_receiver() -> watch::Receiver<Arc<PeerPolicy>> {
    let (_policy_tx, policy_rx) = watch::channel(Arc::new(PeerPolicy::default()));
    policy_rx
}

#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct PeerManagerSnapshot {
    pub registered_torrents: usize,
    pub metrics_updates: u64,
    pub latest_metrics: HashMap<InfoHash, TorrentMetrics>,
}

#[derive(Clone)]
pub struct PeerManagerHandle {
    command_tx: mpsc::UnboundedSender<PeerManagerCommand>,
    policy_rx: watch::Receiver<Arc<PeerPolicy>>,
    view_rx: watch::Receiver<Arc<PeerManagerView>>,
}

impl PeerManagerHandle {
    pub fn register_torrent(
        &self,
        info_hash: InfoHash,
        metrics_rx: watch::Receiver<TorrentMetrics>,
    ) -> bool {
        self.command_tx
            .send(PeerManagerCommand::RegisterTorrent {
                info_hash,
                metrics_rx,
            })
            .is_ok()
    }

    pub fn subscribe_policy(&self) -> watch::Receiver<Arc<PeerPolicy>> {
        self.policy_rx.clone()
    }

    pub fn subscribe_view(&self) -> watch::Receiver<Arc<PeerManagerView>> {
        self.view_rx.clone()
    }

    pub fn unregister_torrent(&self, info_hash: InfoHash) -> bool {
        self.command_tx
            .send(PeerManagerCommand::UnregisterTorrent { info_hash })
            .is_ok()
    }

    pub async fn flush(&self) -> bool {
        let (response_tx, response_rx) = oneshot::channel();
        if self
            .command_tx
            .send(PeerManagerCommand::Flush { response_tx })
            .is_err()
        {
            return false;
        }
        response_rx.await.is_ok()
    }

    #[cfg(test)]
    pub fn block_ip_until(&self, ip: IpAddr, until: SystemTime) -> bool {
        self.command_tx
            .send(PeerManagerCommand::BlockIpUntil { ip, until })
            .is_ok()
    }

    #[cfg(test)]
    pub async fn snapshot(&self) -> Option<PeerManagerSnapshot> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(PeerManagerCommand::Snapshot { response_tx })
            .ok()?;
        response_rx.await.ok()
    }
}

pub struct PeerManagerService {
    handle: PeerManagerHandle,
    task: Option<JoinHandle<()>>,
    persistence_task: Option<JoinHandle<()>>,
}

impl PeerManagerService {
    pub fn new(shutdown_rx: broadcast::Receiver<()>) -> Self {
        #[cfg(not(test))]
        let persistence_path = match peer_policy_file_path() {
            Ok(path) => Some(path),
            Err(error) => {
                tracing::error!(%error, "Peer policy persistence is unavailable");
                None
            }
        };
        #[cfg(test)]
        let persistence_path = None;

        Self::new_with_persistence_path(shutdown_rx, persistence_path)
    }

    fn new_with_persistence_path(
        shutdown_rx: broadcast::Receiver<()>,
        persistence_path: Option<PathBuf>,
    ) -> Self {
        Self::new_with_persistence_options(
            shutdown_rx,
            persistence_path,
            PERSISTENCE_CHECKPOINT_INTERVAL,
        )
    }

    fn new_with_persistence_options(
        shutdown_rx: broadcast::Receiver<()>,
        persistence_path: Option<PathBuf>,
        checkpoint_interval: Duration,
    ) -> Self {
        Self::new_with_persistence_options_and_optional_writer_delay(
            shutdown_rx,
            persistence_path,
            checkpoint_interval,
            None,
        )
    }

    #[cfg(test)]
    fn new_with_persistence_options_and_writer_delay(
        shutdown_rx: broadcast::Receiver<()>,
        persistence_path: Option<PathBuf>,
        checkpoint_interval: Duration,
        writer_delay: Duration,
    ) -> Self {
        Self::new_with_persistence_options_and_optional_writer_delay(
            shutdown_rx,
            persistence_path,
            checkpoint_interval,
            Some(writer_delay),
        )
    }

    fn new_with_persistence_options_and_optional_writer_delay(
        shutdown_rx: broadcast::Receiver<()>,
        persistence_path: Option<PathBuf>,
        checkpoint_interval: Duration,
        writer_delay: Option<Duration>,
    ) -> Self {
        let (initial_policy, persistence_dirty) = persistence_path
            .as_deref()
            .map(
                |path| match load_peer_policy_state_from_path(path, SystemTime::now()) {
                    Ok(state) => state,
                    Err(error) => {
                        tracing::error!(%error, path = %path.display(), "Failed to load peer policy");
                        (PeerPolicy::default(), false)
                    }
                },
            )
            .unwrap_or_else(|| (PeerPolicy::default(), false));
        let reducer = PeerPolicyReducer {
            policy: initial_policy.clone(),
            histories: HashMap::new(),
            next_reconnect_expiry: None,
        };
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (metrics_notification_tx, metrics_notification_rx) = mpsc::unbounded_channel();
        let (policy_tx, policy_rx) = watch::channel(Arc::new(initial_policy));
        let (view_tx, view_rx) = watch::channel(Arc::new(PeerManagerView::default()));
        let handle = PeerManagerHandle {
            command_tx,
            policy_rx,
            view_rx,
        };
        let (persistence_tx, persistence_result_rx, persistence_task) = persistence_path
            .map(|path| spawn_policy_writer(path, writer_delay))
            .map_or((None, None, None), |(tx, result_rx, task)| {
                (Some(tx), Some(result_rx), Some(task))
            });
        let task = tokio::spawn(run_service(
            command_rx,
            MetricsNotificationRuntime {
                tx: metrics_notification_tx,
                rx: metrics_notification_rx,
            },
            policy_tx,
            view_tx,
            shutdown_rx,
            reducer,
            PolicyPersistenceRuntime {
                state: PolicyPersistenceState {
                    tx: persistence_tx,
                    dirty: persistence_dirty,
                    revision: u64::from(persistence_dirty),
                    queued_revision: None,
                },
                result_rx: persistence_result_rx,
                checkpoint_interval,
            },
        ));
        Self {
            handle,
            task: Some(task),
            persistence_task,
        }
    }

    pub fn handle(&self) -> PeerManagerHandle {
        self.handle.clone()
    }

    pub async fn wait_for_shutdown(&mut self) {
        if let Some(task) = self.task.take() {
            if let Err(error) = task.await {
                tracing::error!(%error, "Error joining peer manager task");
            }
        }
        if let Some(task) = self.persistence_task.take() {
            if let Err(error) = task.await {
                tracing::error!(%error, "Error joining peer policy persistence task");
            }
        }
    }

    #[cfg(test)]
    pub async fn join(mut self) {
        self.wait_for_shutdown().await;
    }
}

enum PeerManagerCommand {
    RegisterTorrent {
        info_hash: InfoHash,
        metrics_rx: watch::Receiver<TorrentMetrics>,
    },
    UnregisterTorrent {
        info_hash: InfoHash,
    },
    Flush {
        response_tx: oneshot::Sender<()>,
    },
    #[cfg(test)]
    Snapshot {
        response_tx: oneshot::Sender<PeerManagerSnapshot>,
    },
    #[cfg(test)]
    BlockIpUntil {
        ip: IpAddr,
        until: SystemTime,
    },
}

fn publish_policy(reducer: &PeerPolicyReducer, policy_tx: &watch::Sender<Arc<PeerPolicy>>) {
    policy_tx.send_replace(Arc::new(reducer.policy().clone()));
}

fn publish_view(
    reducer: &PeerPolicyReducer,
    latest_metrics: &HashMap<InfoHash, TorrentMetrics>,
    registered_torrents: usize,
    metrics_updates: u64,
    view_tx: &watch::Sender<Arc<PeerManagerView>>,
) {
    #[cfg(test)]
    let build_started = std::time::Instant::now();
    let view = reducer.build_view(latest_metrics, registered_torrents, metrics_updates);
    #[cfg(test)]
    {
        PERF_VIEW_PUBLICATIONS.fetch_add(1, Ordering::Relaxed);
        PERF_VIEW_BUILD_NANOS.fetch_add(
            u64::try_from(build_started.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
    view_tx.send_replace(Arc::new(view));
}

#[derive(Default)]
struct MetricsDrainResult {
    policy_changed: bool,
    view_changed: bool,
    receiver_closed: bool,
}

fn drain_metrics_update(
    reducer: &mut PeerPolicyReducer,
    info_hash: &InfoHash,
    metrics_rx: &mut watch::Receiver<TorrentMetrics>,
    latest_metrics: &mut HashMap<InfoHash, TorrentMetrics>,
    metrics_updates: &mut u64,
    now: SystemTime,
) -> MetricsDrainResult {
    #[cfg(test)]
    let reduction_started = std::time::Instant::now();
    let result = match metrics_rx.has_changed() {
        Ok(true) => {
            #[cfg(test)]
            PERF_METRICS_REDUCTIONS.fetch_add(1, Ordering::Relaxed);
            let metrics = metrics_rx.borrow_and_update().clone();
            let policy_changed = reducer.reduce_metrics(info_hash, &metrics, now);
            latest_metrics.insert(info_hash.clone(), metrics);
            *metrics_updates = metrics_updates.saturating_add(1);
            MetricsDrainResult {
                policy_changed,
                view_changed: true,
                receiver_closed: false,
            }
        }
        Ok(false) => MetricsDrainResult::default(),
        Err(_) => {
            #[cfg(test)]
            PERF_METRICS_REDUCTIONS.fetch_add(1, Ordering::Relaxed);
            // A sender may publish its final cumulative counters and drop before the
            // notification is handled. Reduce the terminal value before removing the
            // registration; replaying an already-seen cumulative snapshot is harmless
            // because the reducer records only deltas.
            let metrics = metrics_rx.borrow_and_update().clone();
            MetricsDrainResult {
                policy_changed: reducer.reduce_metrics(info_hash, &metrics, now),
                view_changed: true,
                receiver_closed: true,
            }
        }
    };
    #[cfg(test)]
    if result.view_changed {
        PERF_METRICS_REDUCTION_NANOS.fetch_add(
            u64::try_from(reduction_started.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
    result
}

fn drain_metrics_updates(
    reducer: &mut PeerPolicyReducer,
    metrics_rxs: &mut HashMap<InfoHash, watch::Receiver<TorrentMetrics>>,
    metrics_notification_cancellations: &mut HashMap<InfoHash, oneshot::Sender<()>>,
    latest_metrics: &mut HashMap<InfoHash, TorrentMetrics>,
    metrics_updates: &mut u64,
) -> (bool, bool) {
    let now = SystemTime::now();
    let mut policy_changed = false;
    let mut view_changed = false;
    let mut closed = Vec::new();

    for (info_hash, metrics_rx) in metrics_rxs.iter_mut() {
        let result = drain_metrics_update(
            reducer,
            info_hash,
            metrics_rx,
            latest_metrics,
            metrics_updates,
            now,
        );
        policy_changed |= result.policy_changed;
        view_changed |= result.view_changed;
        if result.receiver_closed {
            closed.push(info_hash.clone());
        }
    }

    for info_hash in closed {
        metrics_rxs.remove(&info_hash);
        if let Some(cancel_tx) = metrics_notification_cancellations.remove(&info_hash) {
            let _ = cancel_tx.send(());
        }
        latest_metrics.remove(&info_hash);
        reducer.remove_torrent(&info_hash);
    }

    (policy_changed, view_changed)
}

struct MetricsNotification {
    info_hash: InfoHash,
    processed_tx: oneshot::Sender<()>,
}

fn spawn_metrics_notification_task(
    info_hash: InfoHash,
    mut metrics_rx: watch::Receiver<TorrentMetrics>,
    metrics_notification_tx: mpsc::UnboundedSender<MetricsNotification>,
) -> oneshot::Sender<()> {
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    tokio::spawn(async move {
        loop {
            let receiver_closed = tokio::select! {
                _ = &mut cancel_rx => break,
                result = metrics_rx.changed() => result.is_err(),
            };
            #[cfg(test)]
            PERF_NOTIFICATION_WAKES.fetch_add(1, Ordering::Relaxed);
            let (processed_tx, processed_rx) = oneshot::channel();
            if metrics_notification_tx
                .send(MetricsNotification {
                    info_hash: info_hash.clone(),
                    processed_tx,
                })
                .is_err()
            {
                break;
            }
            tokio::select! {
                _ = &mut cancel_rx => break,
                _ = processed_rx => {}
            }
            if receiver_closed {
                break;
            }
        }
    });
    cancel_tx
}

#[derive(Clone)]
struct PolicyPersistenceRequest {
    revision: u64,
    policy: Arc<PeerPolicy>,
}

#[derive(Clone, Copy)]
struct PolicyPersistenceResult {
    revision: u64,
    succeeded: bool,
}

struct PolicyPersistenceState {
    tx: Option<watch::Sender<Option<PolicyPersistenceRequest>>>,
    dirty: bool,
    revision: u64,
    queued_revision: Option<u64>,
}

impl PolicyPersistenceState {
    fn mark_dirty(&mut self) {
        self.revision = self.revision.saturating_add(1);
        self.dirty = true;
    }

    fn apply_result(&mut self, result: PolicyPersistenceResult) {
        if result.revision != self.revision {
            return;
        }
        self.queued_revision = None;
        self.dirty = !result.succeeded;
    }

    fn queue_if_dirty(&mut self, reducer: &PeerPolicyReducer) {
        if !self.dirty || self.queued_revision == Some(self.revision) {
            return;
        }
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        tx.send_replace(Some(PolicyPersistenceRequest {
            revision: self.revision,
            policy: Arc::new(reducer.policy().clone()),
        }));
        if !tx.is_closed() {
            self.queued_revision = Some(self.revision);
        }
    }
}

fn spawn_policy_writer(
    path: PathBuf,
    writer_delay: Option<Duration>,
) -> (
    watch::Sender<Option<PolicyPersistenceRequest>>,
    mpsc::UnboundedReceiver<PolicyPersistenceResult>,
    JoinHandle<()>,
) {
    let (persistence_tx, persistence_rx) = watch::channel(None);
    let (result_tx, result_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(run_policy_writer(
        path,
        persistence_rx,
        result_tx,
        writer_delay,
    ));
    (persistence_tx, result_rx, task)
}

async fn run_policy_writer(
    path: PathBuf,
    mut persistence_rx: watch::Receiver<Option<PolicyPersistenceRequest>>,
    result_tx: mpsc::UnboundedSender<PolicyPersistenceResult>,
    writer_delay: Option<Duration>,
) {
    while persistence_rx.changed().await.is_ok() {
        let Some(request) = persistence_rx.borrow_and_update().clone() else {
            continue;
        };
        let revision = request.revision;
        let write_path = path.clone();
        let result = tokio::task::spawn_blocking(move || {
            if let Some(delay) = writer_delay {
                std::thread::sleep(delay);
            }
            save_peer_policy_to_path(&write_path, &request.policy)
        })
        .await;
        let succeeded = match result {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                tracing::error!(%error, path = %path.display(), "Failed to checkpoint peer policy");
                false
            }
            Err(error) => {
                tracing::error!(%error, path = %path.display(), "Peer policy persistence task failed");
                false
            }
        };
        let _ = result_tx.send(PolicyPersistenceResult {
            revision,
            succeeded,
        });
    }
}

struct PolicyPersistenceRuntime {
    state: PolicyPersistenceState,
    result_rx: Option<mpsc::UnboundedReceiver<PolicyPersistenceResult>>,
    checkpoint_interval: Duration,
}

struct MetricsNotificationRuntime {
    tx: mpsc::UnboundedSender<MetricsNotification>,
    rx: mpsc::UnboundedReceiver<MetricsNotification>,
}

async fn run_service(
    mut command_rx: mpsc::UnboundedReceiver<PeerManagerCommand>,
    metrics_notifications: MetricsNotificationRuntime,
    policy_tx: watch::Sender<Arc<PeerPolicy>>,
    view_tx: watch::Sender<Arc<PeerManagerView>>,
    mut shutdown_rx: broadcast::Receiver<()>,
    mut reducer: PeerPolicyReducer,
    persistence: PolicyPersistenceRuntime,
) {
    let metrics_notification_tx = metrics_notifications.tx;
    let mut metrics_notification_rx = metrics_notifications.rx;
    let mut persistence_state = persistence.state;
    let mut persistence_result_rx = persistence.result_rx;
    let checkpoint_interval = persistence.checkpoint_interval;
    let mut metrics_rxs = HashMap::<InfoHash, watch::Receiver<TorrentMetrics>>::new();
    let mut metrics_notification_cancellations = HashMap::<InfoHash, oneshot::Sender<()>>::new();
    let mut latest_metrics = HashMap::<InfoHash, TorrentMetrics>::new();
    let mut metrics_updates = 0_u64;
    let first_view_publish = tokio::time::Instant::now() + VIEW_PUBLISH_INTERVAL;
    let mut view_publish = tokio::time::interval_at(first_view_publish, VIEW_PUBLISH_INTERVAL);
    view_publish.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut view_dirty = false;
    let first_checkpoint = tokio::time::Instant::now() + checkpoint_interval;
    let mut policy_checkpoint = tokio::time::interval_at(first_checkpoint, checkpoint_interval);
    policy_checkpoint.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let reconnect_expiry_delay = reducer.next_reconnect_expiry_delay(SystemTime::now());
        tokio::select! {
            _ = shutdown_rx.recv() => break,
            persistence_result = async {
                match persistence_result_rx.as_mut() {
                    Some(result_rx) => result_rx.recv().await,
                    None => std::future::pending::<Option<PolicyPersistenceResult>>().await,
                }
            } => {
                if let Some(result) = persistence_result {
                    persistence_state.apply_result(result);
                } else {
                    persistence_result_rx = None;
                    persistence_state.tx = None;
                }
            }
            maybe_command = command_rx.recv() => {
                let Some(command) = maybe_command else {
                    break;
                };
                match command {
                    PeerManagerCommand::RegisterTorrent { info_hash, mut metrics_rx } => {
                        let notification_rx = metrics_rx.clone();
                        let had_pending_update = metrics_rx.has_changed().unwrap_or(false);
                        let metrics = metrics_rx.borrow_and_update().clone();
                        if reducer.reduce_metrics(&info_hash, &metrics, SystemTime::now()) {
                            persistence_state.mark_dirty();
                            publish_policy(&reducer, &policy_tx);
                        }
                        if had_pending_update {
                            metrics_updates = metrics_updates.saturating_add(1);
                        }
                        latest_metrics.insert(info_hash.clone(), metrics);
                        metrics_rxs.insert(info_hash.clone(), metrics_rx);
                        if let Some(cancel_tx) = metrics_notification_cancellations.remove(&info_hash) {
                            let _ = cancel_tx.send(());
                        }
                        let cancel_tx = spawn_metrics_notification_task(
                            info_hash.clone(),
                            notification_rx,
                            metrics_notification_tx.clone(),
                        );
                        metrics_notification_cancellations.insert(info_hash, cancel_tx);
                        publish_view(
                            &reducer,
                            &latest_metrics,
                            metrics_rxs.len(),
                            metrics_updates,
                            &view_tx,
                        );
                        view_dirty = false;
                    }
                    PeerManagerCommand::UnregisterTorrent { info_hash } => {
                        if let Some(cancel_tx) = metrics_notification_cancellations.remove(&info_hash) {
                            let _ = cancel_tx.send(());
                        }
                        if let Some(mut metrics_rx) = metrics_rxs.remove(&info_hash) {
                            let had_pending_update = metrics_rx.has_changed().unwrap_or(false);
                            let terminal_metrics = metrics_rx.borrow_and_update().clone();
                            if reducer.reduce_metrics(
                                &info_hash,
                                &terminal_metrics,
                                SystemTime::now(),
                            ) {
                                persistence_state.mark_dirty();
                                publish_policy(&reducer, &policy_tx);
                                persistence_state.queue_if_dirty(&reducer);
                            }
                            if had_pending_update {
                                metrics_updates = metrics_updates.saturating_add(1);
                            }
                        }
                        latest_metrics.remove(&info_hash);
                        reducer.remove_torrent(&info_hash);
                        publish_view(
                            &reducer,
                            &latest_metrics,
                            metrics_rxs.len(),
                            metrics_updates,
                            &view_tx,
                        );
                        view_dirty = false;
                    }
                    PeerManagerCommand::Flush { response_tx } => {
                        let (policy_changed, view_changed) = drain_metrics_updates(
                            &mut reducer,
                            &mut metrics_rxs,
                            &mut metrics_notification_cancellations,
                            &mut latest_metrics,
                            &mut metrics_updates,
                        );
                        if policy_changed {
                            persistence_state.mark_dirty();
                            publish_policy(&reducer, &policy_tx);
                        }
                        if view_changed {
                            publish_view(
                                &reducer,
                                &latest_metrics,
                                metrics_rxs.len(),
                                metrics_updates,
                                &view_tx,
                            );
                            view_dirty = false;
                        }
                        persistence_state.queue_if_dirty(&reducer);
                        let _ = response_tx.send(());
                    }
                    #[cfg(test)]
                    PeerManagerCommand::Snapshot { response_tx } => {
                        let _ = response_tx.send(PeerManagerSnapshot {
                            registered_torrents: metrics_rxs.len(),
                            metrics_updates,
                            latest_metrics: latest_metrics.clone(),
                        });
                    }
                    #[cfg(test)]
                    PeerManagerCommand::BlockIpUntil { ip, until } => {
                        if reducer.block_ip_until(ip, until) {
                            persistence_state.mark_dirty();
                            publish_policy(&reducer, &policy_tx);
                        }
                    }
                }
            }
            Some(first_notification) = metrics_notification_rx.recv() => {
                // Give other torrent notification tasks from the same telemetry burst
                // one scheduler turn to enqueue, then reduce the ready batch and publish
                // one final view. This stays event-driven without rebuilding the entire
                // peer view once per torrent in a synchronized manager tick.
                tokio::task::yield_now().await;
                let mut notifications = vec![first_notification];
                while let Ok(notification) = metrics_notification_rx.try_recv() {
                    notifications.push(notification);
                }
                let now = SystemTime::now();
                let mut policy_changed = false;
                let mut view_changed = false;
                let mut processed_txs = Vec::with_capacity(notifications.len());
                for notification in notifications {
                    #[cfg(test)]
                    PERF_NOTIFICATIONS_HANDLED.fetch_add(1, Ordering::Relaxed);
                    let info_hash = notification.info_hash;
                    processed_txs.push(notification.processed_tx);
                    if let Some(metrics_rx) = metrics_rxs.get_mut(&info_hash) {
                        let result = drain_metrics_update(
                            &mut reducer,
                            &info_hash,
                            metrics_rx,
                            &mut latest_metrics,
                            &mut metrics_updates,
                            now,
                        );
                        if result.receiver_closed {
                            metrics_rxs.remove(&info_hash);
                            if let Some(cancel_tx) = metrics_notification_cancellations.remove(&info_hash) {
                                let _ = cancel_tx.send(());
                            }
                            latest_metrics.remove(&info_hash);
                            reducer.remove_torrent(&info_hash);
                        }
                        policy_changed |= result.policy_changed;
                        view_changed |= result.view_changed;
                    }
                }
                if policy_changed {
                    persistence_state.mark_dirty();
                    publish_policy(&reducer, &policy_tx);
                }
                if view_changed {
                    view_dirty = true;
                }
                for processed_tx in processed_txs {
                    let _ = processed_tx.send(());
                }
            }
            _ = view_publish.tick(), if view_dirty => {
                publish_view(
                    &reducer,
                    &latest_metrics,
                    metrics_rxs.len(),
                    metrics_updates,
                    &view_tx,
                );
                view_dirty = false;
            }
            _ = async {
                match reconnect_expiry_delay {
                    Some(delay) => tokio::time::sleep(delay).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                if reducer.prune_reconnect_evidence(SystemTime::now()) {
                    view_dirty = true;
                }
            }
            _ = policy_checkpoint.tick() => {
                let reconnects_changed = reducer.prune_reconnect_evidence(SystemTime::now());
                let history_count = reducer.history_count();
                if reducer.maintain_to(SystemTime::now(), MAX_PEER_HISTORIES) {
                    persistence_state.mark_dirty();
                    publish_policy(&reducer, &policy_tx);
                }
                if reconnects_changed || reducer.history_count() != history_count {
                    view_dirty = true;
                }
                persistence_state.queue_if_dirty(&reducer);
            }
        }
    }

    persistence_state.queue_if_dirty(&reducer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::PeerInfo;
    use std::net::{IpAddr, Ipv4Addr};
    use tokio::time::timeout;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn peer_client_parser_distinguishes_peer_id_families() {
        let m_style = parse_peer_client(b"M4-3-6--abcdefghijkl");
        let mut classic = b"exbc".to_vec();
        classic.extend_from_slice(&[1, 2]);
        classic.extend_from_slice(b"abcdefghijklmn");
        let classic_style = parse_peer_client(&classic);
        let dashed_style = parse_peer_client(b"-BC0100-abcdefghijkl");

        assert_ne!(m_style, "Unknown");
        assert_ne!(classic_style, "Unknown");
        assert!(m_style.ends_with("4.3.6"));
        assert!(classic_style.ends_with("1.02"));
        assert_ne!(
            m_style.split_whitespace().next(),
            classic_style.split_whitespace().next()
        );
        assert_eq!(
            classic_style.split_whitespace().next(),
            dashed_style.split_whitespace().next()
        );
    }

    #[test]
    fn peer_client_parser_recognizes_superseedr_versions() {
        let legacy = parse_peer_client(b"-SS1000-abcdefghijkl");
        let current_peer_id = format!("{}abcdefghijkl", superseedr_peer_id_prefix());
        let current = parse_peer_client(current_peer_id.as_bytes());

        assert_eq!(legacy, format!("{SUPERSEEDR_CLIENT_NAME} 1.0.0"));
        assert_eq!(
            current,
            format!(
                "{SUPERSEEDR_CLIENT_NAME} {}.{}.{}",
                env!("CARGO_PKG_VERSION_MAJOR"),
                env!("CARGO_PKG_VERSION_MINOR"),
                env!("CARGO_PKG_VERSION_PATCH")
            )
        );
    }

    #[test]
    fn package_version_drives_superseedr_peer_id_prefix() {
        assert_eq!(
            encode_superseedr_peer_version(1, 0, 13).as_deref(),
            Some("1013")
        );
        assert_eq!(
            superseedr_peer_id_prefix(),
            format!("-SS{}-", superseedr_peer_version_code())
        );
        assert!(encode_superseedr_peer_version(10, 0, 0).is_none());
        assert!(encode_superseedr_peer_version(1, 10, 0).is_none());
        assert!(encode_superseedr_peer_version(1, 0, 100).is_none());
    }

    #[test]
    fn refreshing_superseedr_peer_id_updates_only_the_version_prefix() {
        let old = "-SS1000-abcdefghijkl";
        let refreshed = refresh_superseedr_peer_id_version(old).unwrap_or_else(|| old.to_string());

        assert_eq!(refreshed.len(), 20);
        assert!(refreshed.starts_with(&superseedr_peer_id_prefix()));
        assert!(refreshed.ends_with("abcdefghijkl"));
        assert_eq!(refresh_superseedr_peer_id_version(&refreshed), None);
        assert_eq!(refresh_superseedr_peer_id_version("custom-client-id"), None);
    }

    fn metrics_with_peer(
        info_hash: &[u8],
        total_size: u64,
        address: &str,
        total_uploaded: u64,
    ) -> TorrentMetrics {
        metrics_with_peers(info_hash, total_size, &[(address, total_uploaded)])
    }

    fn metrics_with_reconnect_count(
        info_hash: &[u8],
        total_size: u64,
        address: &str,
        cumulative_reconnects: u64,
    ) -> TorrentMetrics {
        let mut metrics = metrics_with_peer(info_hash, total_size, address, 0);
        let ip = parse_peer_ip(address).expect("peer address should contain a valid IP");
        metrics
            .peer_reconnect_counts
            .insert(ip, cumulative_reconnects);
        metrics
    }

    fn metrics_with_peer_transfer(
        info_hash: &[u8],
        total_size: u64,
        address: &str,
        total_downloaded: u64,
        total_uploaded: u64,
    ) -> TorrentMetrics {
        TorrentMetrics {
            info_hash: info_hash.to_vec(),
            total_size,
            peers: vec![PeerInfo {
                address: address.to_string(),
                total_downloaded,
                total_uploaded,
                ..PeerInfo::default()
            }],
            ..TorrentMetrics::default()
        }
    }

    fn metrics_with_peers(
        info_hash: &[u8],
        total_size: u64,
        peers: &[(&str, u64)],
    ) -> TorrentMetrics {
        TorrentMetrics {
            info_hash: info_hash.to_vec(),
            total_size,
            peers: peers
                .iter()
                .map(|(address, total_uploaded)| PeerInfo {
                    address: (*address).to_string(),
                    total_uploaded: *total_uploaded,
                    ..PeerInfo::default()
                })
                .collect(),
            ..TorrentMetrics::default()
        }
    }

    fn metrics_without_peers(info_hash: &[u8], total_size: u64) -> TorrentMetrics {
        TorrentMetrics {
            info_hash: info_hash.to_vec(),
            total_size,
            ..TorrentMetrics::default()
        }
    }

    async fn wait_for_metrics_update(
        handle: &PeerManagerHandle,
        expected_updates: u64,
    ) -> PeerManagerSnapshot {
        timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = handle.snapshot().await.expect("peer manager snapshot");
                if snapshot.metrics_updates >= expected_updates {
                    break snapshot;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("peer manager should observe metrics")
    }

    async fn wait_for_view_metrics_update(
        view_rx: &mut watch::Receiver<Arc<PeerManagerView>>,
        expected_updates: u64,
    ) {
        timeout(Duration::from_secs(30), async {
            loop {
                if view_rx.borrow_and_update().metrics_updates >= expected_updates {
                    break;
                }
                view_rx
                    .changed()
                    .await
                    .expect("peer manager view publisher should remain open");
            }
        })
        .await
        .expect("process performance-probe metrics");
    }

    #[tokio::test]
    async fn registered_torrent_metrics_are_observed() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = PeerManagerService::new(shutdown_tx.subscribe());
        let handle = service.handle();
        let info_hash = vec![7; 20];
        let (metrics_tx, metrics_rx) = watch::channel(TorrentMetrics::default());

        assert!(handle.register_torrent(info_hash.clone(), metrics_rx));
        metrics_tx
            .send(TorrentMetrics {
                info_hash: info_hash.clone(),
                torrent_name: "Sample Swarm".to_string(),
                session_total_downloaded: 4096,
                ..TorrentMetrics::default()
            })
            .expect("send torrent metrics");

        let snapshot = wait_for_metrics_update(&handle, 1).await;
        assert_eq!(snapshot.registered_torrents, 1);
        let observed = snapshot
            .latest_metrics
            .get(&info_hash)
            .expect("registered torrent metrics");
        assert_eq!(observed.torrent_name, "Sample Swarm");
        assert_eq!(observed.session_total_downloaded, 4096);

        let _ = shutdown_tx.send(());
        service.join().await;
    }

    #[tokio::test]
    async fn registration_can_be_removed_without_closing_metrics_sender() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = PeerManagerService::new(shutdown_tx.subscribe());
        let handle = service.handle();
        let info_hash = vec![8; 20];
        let (_metrics_tx, metrics_rx) = watch::channel(TorrentMetrics::default());

        assert!(handle.register_torrent(info_hash.clone(), metrics_rx));
        timeout(Duration::from_secs(1), async {
            loop {
                let snapshot = handle.snapshot().await.expect("peer manager snapshot");
                if snapshot.registered_torrents == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("torrent should register");

        assert!(handle.unregister_torrent(info_hash.clone()));
        let snapshot = timeout(Duration::from_secs(1), async {
            loop {
                let snapshot = handle.snapshot().await.expect("peer manager snapshot");
                if snapshot.registered_torrents == 0 {
                    break snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("torrent should unregister");
        assert!(!snapshot.latest_metrics.contains_key(&info_hash));

        let _ = shutdown_tx.send(());
        service.join().await;
    }

    #[tokio::test(start_paused = true)]
    async fn unregister_reduces_pending_terminal_metrics_before_dropping_history() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = PeerManagerService::new(shutdown_tx.subscribe());
        let handle = service.handle();
        let policy_rx = handle.subscribe_policy();
        let info_hash = vec![0x18; 20];
        let peer_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 118));
        let peer_address = format!("{peer_ip}:6881");
        let (metrics_tx, metrics_rx) = watch::channel(TorrentMetrics::default());

        assert!(handle.register_torrent(info_hash.clone(), metrics_rx));
        timeout(Duration::from_secs(1), async {
            loop {
                let snapshot = handle.snapshot().await.expect("peer manager snapshot");
                if snapshot.registered_torrents == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("torrent should register");

        metrics_tx
            .send(metrics_with_peer(
                &info_hash,
                1,
                &peer_address,
                MIN_TRANSFER_ABUSE_BYTES + 1,
            ))
            .expect("publish terminal peer metrics");
        assert!(handle.unregister_torrent(info_hash.clone()));

        let snapshot = handle.snapshot().await.expect("peer manager snapshot");
        assert_eq!(snapshot.registered_torrents, 0);
        assert!(policy_rx.borrow().blocks_ip(peer_ip, SystemTime::now()));

        let _ = shutdown_tx.send(());
        service.join().await;
    }

    #[tokio::test]
    async fn flush_reduces_terminal_metrics_after_sender_closes() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = PeerManagerService::new(shutdown_tx.subscribe());
        let handle = service.handle();
        let policy_rx = handle.subscribe_policy();
        let info_hash = vec![0x19; 20];
        let peer_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 119));
        let peer_address = format!("{peer_ip}:6881");
        let (metrics_tx, metrics_rx) = watch::channel(TorrentMetrics::default());

        assert!(handle.register_torrent(info_hash.clone(), metrics_rx));
        timeout(Duration::from_secs(1), async {
            loop {
                let snapshot = handle.snapshot().await.expect("peer manager snapshot");
                if snapshot.registered_torrents == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("torrent should register");

        metrics_tx
            .send(metrics_with_peer(
                &info_hash,
                1,
                &peer_address,
                MIN_TRANSFER_ABUSE_BYTES + 1,
            ))
            .expect("publish terminal peer metrics");
        drop(metrics_tx);

        assert!(handle.flush().await);
        assert!(policy_rx.borrow().blocks_ip(peer_ip, SystemTime::now()));

        let _ = shutdown_tx.send(());
        service.join().await;
    }

    #[tokio::test]
    async fn service_restores_live_policy_before_subscribers_start() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let path = temp.path().join("peer_policy.toml");
        let now = SystemTime::now();
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 77));
        let restriction = PeerRestriction {
            detected_at: now,
            blocked_until: now + Duration::from_secs(600),
            torrent_info_hash: Some(vec![0x77; 20]),
            reason: PeerRestrictionReason::ReconnectChurn {
                reconnects: RECONNECT_LIMIT as u32,
                threshold: RECONNECT_LIMIT as u32,
                window_secs: RECONNECT_WINDOW.as_secs(),
            },
        };
        save_peer_policy_to_path(
            &path,
            &PeerPolicy {
                restrictions: HashMap::from([(ip, restriction.clone())]),
            },
        )
        .expect("seed persisted policy");

        let (shutdown_tx, _) = broadcast::channel(1);
        let service =
            PeerManagerService::new_with_persistence_path(shutdown_tx.subscribe(), Some(path));
        let policy_rx = service.handle().subscribe_policy();

        assert_eq!(policy_rx.borrow().restrictions.get(&ip), Some(&restriction));

        let _ = shutdown_tx.send(());
        service.join().await;
    }

    #[tokio::test]
    async fn policy_changes_publish_immediately_and_checkpoint_only_when_dirty_interval_elapses() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let path = temp.path().join("peer_policy.toml");
        let (shutdown_tx, _) = broadcast::channel(1);
        let checkpoint_interval = Duration::from_millis(100);
        let service = PeerManagerService::new_with_persistence_options(
            shutdown_tx.subscribe(),
            Some(path.clone()),
            checkpoint_interval,
        );
        let handle = service.handle();
        let mut policy_rx = handle.subscribe_policy();
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 78));
        let blocked_until = SystemTime::now() + Duration::from_secs(600);

        assert!(handle.block_ip_until(ip, blocked_until));
        timeout(Duration::from_secs(1), policy_rx.changed())
            .await
            .expect("policy update timeout")
            .expect("policy publisher should remain open");
        assert!(!path.exists(), "dirty policy must not write immediately");

        timeout(Duration::from_secs(1), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("dirty policy checkpoint timeout");
        let restored =
            load_peer_policy_from_path(&path, SystemTime::now()).expect("load persisted policy");
        assert!(restored.blocks_ip(ip, SystemTime::now()));
        assert_eq!(
            restored.restrictions.get(&ip).map(|entry| &entry.reason),
            Some(&PeerRestrictionReason::Manual)
        );

        let _ = shutdown_tx.send(());
        service.join().await;
    }

    #[tokio::test]
    async fn clean_checkpoint_intervals_do_not_create_policy_file() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let path = temp.path().join("peer_policy.toml");
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = PeerManagerService::new_with_persistence_options(
            shutdown_tx.subscribe(),
            Some(path.clone()),
            Duration::from_millis(20),
        );

        tokio::time::sleep(Duration::from_millis(75)).await;
        assert!(!path.exists());

        let _ = shutdown_tx.send(());
        service.join().await;
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn dirty_policy_is_flushed_during_orderly_shutdown() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let path = temp.path().join("peer_policy.toml");
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = PeerManagerService::new_with_persistence_options(
            shutdown_tx.subscribe(),
            Some(path.clone()),
            Duration::from_secs(60),
        );
        let handle = service.handle();
        let mut policy_rx = handle.subscribe_policy();
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 79));

        assert!(handle.block_ip_until(ip, SystemTime::now() + Duration::from_secs(600)));
        timeout(Duration::from_secs(1), policy_rx.changed())
            .await
            .expect("policy update timeout")
            .expect("policy publisher should remain open");
        assert!(!path.exists());

        let _ = shutdown_tx.send(());
        service.join().await;

        let restored =
            load_peer_policy_from_path(&path, SystemTime::now()).expect("load shutdown checkpoint");
        assert!(restored.blocks_ip(ip, SystemTime::now()));
    }

    #[tokio::test]
    async fn slow_policy_write_does_not_block_commands_and_shutdown_flushes_latest() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let path = temp.path().join("peer_policy.toml");
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = PeerManagerService::new_with_persistence_options_and_writer_delay(
            shutdown_tx.subscribe(),
            Some(path.clone()),
            Duration::from_millis(10),
            Duration::from_millis(250),
        );
        let handle = service.handle();
        let first_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 80));
        let latest_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 81));
        let blocked_until = SystemTime::now() + Duration::from_secs(600);

        assert!(handle.block_ip_until(first_ip, blocked_until));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(handle.block_ip_until(latest_ip, blocked_until));
        timeout(Duration::from_millis(100), handle.snapshot())
            .await
            .expect("slow disk write must not block peer-manager commands")
            .expect("peer manager should answer snapshot");

        let _ = shutdown_tx.send(());
        service.join().await;

        let restored =
            load_peer_policy_from_path(&path, SystemTime::now()).expect("load final checkpoint");
        assert!(restored.blocks_ip(first_ip, SystemTime::now()));
        assert!(restored.blocks_ip(latest_ip, SystemTime::now()));
    }

    #[tokio::test]
    async fn policy_is_published_to_existing_and_new_subscribers() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = PeerManagerService::new(shutdown_tx.subscribe());
        let handle = service.handle();
        let mut app_rx = handle.subscribe_policy();
        let mut manager_rx = handle.subscribe_policy();
        let blocked_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));
        let blocked_until = SystemTime::now() + Duration::from_secs(86_400);

        assert!(handle.block_ip_until(blocked_ip, blocked_until));
        for receiver in [&mut app_rx, &mut manager_rx] {
            timeout(Duration::from_secs(1), receiver.changed())
                .await
                .expect("policy update should arrive")
                .expect("policy publisher should remain open");
            let policy = receiver.borrow_and_update().clone();
            assert_eq!(
                policy
                    .restrictions
                    .get(&blocked_ip)
                    .map(|restriction| restriction.blocked_until),
                Some(blocked_until)
            );
        }

        let new_rx = handle.subscribe_policy();
        let current_policy = new_rx.borrow().clone();
        assert_eq!(
            current_policy
                .restrictions
                .get(&blocked_ip)
                .map(|restriction| restriction.blocked_until),
            Some(blocked_until)
        );

        let _ = shutdown_tx.send(());
        service.join().await;
    }

    #[tokio::test]
    async fn metrics_reducer_publishes_upload_abuse_policy() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = PeerManagerService::new(shutdown_tx.subscribe());
        let handle = service.handle();
        let info_hash = vec![6; 20];
        let blocked_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 60));
        let (metrics_tx, metrics_rx) = watch::channel(TorrentMetrics::default());
        let mut policy_rx = handle.subscribe_policy();

        assert!(handle.register_torrent(info_hash.clone(), metrics_rx));
        metrics_tx
            .send(metrics_with_peer(
                &info_hash,
                100 * 1024 * 1024,
                "203.0.113.60:6000",
                MIN_TRANSFER_ABUSE_BYTES + 1,
            ))
            .expect("metrics receiver should remain open");

        timeout(Duration::from_secs(1), policy_rx.changed())
            .await
            .expect("reducer should publish policy")
            .expect("policy publisher should remain open");
        let policy = policy_rx.borrow_and_update();
        assert!(policy.restrictions.contains_key(&blocked_ip));

        let _ = shutdown_tx.send(());
        service.join().await;
    }

    #[test]
    fn tracked_peer_view_exposes_active_and_recently_absent_history() {
        let info_hash = vec![0x31; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 70));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(20_000);
        let mut metrics = metrics_with_peer_transfer(
            &info_hash,
            100 * MIB,
            "tcp://[::ffff:203.0.113.70]:6881",
            64 * MIB,
            32 * MIB,
        );
        metrics.torrent_name = "Nebula Archive".to_string();
        metrics.peers[0].peer_id = b"-ZZ1234-abcdefghijkl".to_vec();
        metrics.peers[0].connection_count = 2;
        metrics.peers[0].disconnect_count = 1;
        let mut reducer = PeerPolicyReducer::default();

        assert!(!reducer.reduce_metrics(&info_hash, &metrics, now));
        let mut latest_metrics = HashMap::from([(info_hash.clone(), metrics)]);
        let view = reducer.build_view(&latest_metrics, 1, 4);

        assert_eq!(view.registered_torrents, 1);
        assert_eq!(view.metrics_updates, 4);
        assert_eq!(view.tracked_peers.len(), 1);
        let tracked = &view.tracked_peers[0];
        assert_eq!(tracked.torrent_info_hash, info_hash);
        assert_eq!(tracked.torrent_name, "Nebula Archive");
        assert_eq!(tracked.ip, ip);
        assert!(tracked.is_active);
        assert_eq!(tracked.downloaded_evidence_bytes, 64 * MIB);
        assert_eq!(tracked.uploaded_evidence_bytes, 32 * MIB);
        assert_eq!(tracked.total_downloaded_bytes, 64 * MIB);
        assert_eq!(tracked.total_uploaded_bytes, 32 * MIB);
        assert_eq!(tracked.connection_count, 2);
        assert_eq!(tracked.disconnect_count, 1);
        assert_eq!(tracked.transfer_threshold_bytes, 256 * MIB);
        assert_eq!(tracked.reconnect_count, 0);
        assert_eq!(tracked.reconnect_limit, RECONNECT_LIMIT as u32);
        assert_eq!(tracked.reconnect_window_secs, RECONNECT_WINDOW.as_secs());
        assert_eq!(tracked.last_seen, Some(now));
        assert_eq!(tracked.clients, vec!["Unknown (ZZ1234)".to_string()]);
        assert_eq!(
            tracked.endpoints,
            vec![PeerManagerEndpointView {
                address: "tcp://[::ffff:203.0.113.70]:6881".to_string(),
                total_downloaded: 64 * MIB,
                total_uploaded: 32 * MIB,
            }]
        );

        let mut absent_metrics = metrics_without_peers(&info_hash, 100 * MIB);
        absent_metrics.torrent_name = "Nebula Archive".to_string();
        assert!(!reducer.reduce_metrics(&info_hash, &absent_metrics, now + Duration::from_secs(1),));
        latest_metrics.insert(info_hash, absent_metrics);
        let view = reducer.build_view(&latest_metrics, 1, 5);
        let tracked = &view.tracked_peers[0];
        assert!(!tracked.is_active);
        assert!(tracked.endpoints.is_empty());
        assert_eq!(tracked.last_seen, Some(now));
        assert_eq!(tracked.downloaded_evidence_bytes, 64 * MIB);
        assert_eq!(tracked.uploaded_evidence_bytes, 32 * MIB);
        assert_eq!(tracked.total_downloaded_bytes, 64 * MIB);
        assert_eq!(tracked.total_uploaded_bytes, 32 * MIB);
        assert_eq!(tracked.connection_count, 2);
        assert_eq!(tracked.disconnect_count, 1);
        assert_eq!(tracked.clients, vec!["Unknown (ZZ1234)".to_string()]);
    }

    #[tokio::test]
    async fn service_publishes_tracked_peer_view_updates() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = PeerManagerService::new(shutdown_tx.subscribe());
        let handle = service.handle();
        let info_hash = vec![0x32; 20];
        let (metrics_tx, metrics_rx) = watch::channel(TorrentMetrics::default());
        let mut view_rx = handle.subscribe_view();

        assert!(handle.register_torrent(info_hash.clone(), metrics_rx));
        let mut metrics = metrics_with_peer_transfer(
            &info_hash,
            100 * MIB,
            "utp://198.51.100.80:6881",
            8 * MIB,
            4 * MIB,
        );
        metrics.torrent_name = "Quiet Comet".to_string();
        metrics_tx
            .send(metrics)
            .expect("view subscriber should keep metrics receiver open");

        let active_view = timeout(Duration::from_secs(2), async {
            loop {
                view_rx
                    .changed()
                    .await
                    .expect("view publisher remains open");
                let view = view_rx.borrow_and_update().clone();
                if view
                    .tracked_peers
                    .first()
                    .is_some_and(|peer| peer.is_active)
                {
                    break view;
                }
            }
        })
        .await
        .expect("active tracked peer view");
        assert_eq!(active_view.registered_torrents, 1);
        assert_eq!(active_view.tracked_peers[0].torrent_name, "Quiet Comet");

        let mut absent_metrics = metrics_without_peers(&info_hash, 100 * MIB);
        absent_metrics.torrent_name = "Quiet Comet".to_string();
        metrics_tx
            .send(absent_metrics)
            .expect("view subscriber should keep metrics receiver open");
        let absent_view = timeout(Duration::from_secs(2), async {
            loop {
                view_rx
                    .changed()
                    .await
                    .expect("view publisher remains open");
                let view = view_rx.borrow_and_update().clone();
                if view
                    .tracked_peers
                    .first()
                    .is_some_and(|peer| !peer.is_active)
                {
                    break view;
                }
            }
        })
        .await
        .expect("recently absent tracked peer view");
        assert_eq!(absent_view.tracked_peers.len(), 1);
        assert!(absent_view.tracked_peers[0].endpoints.is_empty());

        let _ = shutdown_tx.send(());
        service.join().await;
    }

    #[tokio::test(start_paused = true)]
    async fn metrics_policy_and_view_updates_publish_when_metrics_arrive() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = PeerManagerService::new(shutdown_tx.subscribe());
        let handle = service.handle();
        let info_hash = vec![0x33; 20];
        let peer_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 83));
        let peer_address = format!("{peer_ip}:6881");
        let (metrics_tx, metrics_rx) = watch::channel(TorrentMetrics::default());
        let mut policy_rx = handle.subscribe_policy();
        let mut view_rx = handle.subscribe_view();

        assert!(handle.register_torrent(info_hash.clone(), metrics_rx));
        loop {
            let snapshot = handle.snapshot().await.expect("peer manager snapshot");
            if snapshot.registered_torrents == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        view_rx.borrow_and_update();

        metrics_tx
            .send(metrics_with_peer(
                &info_hash,
                1,
                &peer_address,
                MIN_TRANSFER_ABUSE_BYTES + 1,
            ))
            .expect("publish abusive metrics");
        timeout(Duration::from_secs(1), policy_rx.changed())
            .await
            .expect("policy should publish without a metrics timer")
            .expect("policy should remain published");
        assert!(policy_rx.borrow().blocks_ip(peer_ip, SystemTime::now()));
        assert!(matches!(view_rx.has_changed(), Ok(false)));

        tokio::time::advance(VIEW_PUBLISH_INTERVAL).await;
        view_rx
            .changed()
            .await
            .expect("view should publish on its coalescing cadence");

        let _ = shutdown_tx.send(());
        service.join().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "manual performance probe"]
    async fn peer_manager_event_path_performance_probe() {
        let torrents = std::env::var("SUPERSEEDR_PM_PROBE_TORRENTS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(128);
        let rounds = std::env::var("SUPERSEEDR_PM_PROBE_ROUNDS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(8);
        assert!(torrents > 0);
        assert!(rounds > 0);

        let (shutdown_tx, _) = broadcast::channel(1);
        let service = PeerManagerService::new(shutdown_tx.subscribe());
        let handle = service.handle();
        let mut view_rx = handle.subscribe_view();
        let mut senders = Vec::with_capacity(torrents);
        let mut info_hashes = Vec::with_capacity(torrents);

        for torrent_index in 0..torrents {
            let mut info_hash = vec![0_u8; 20];
            info_hash[..8].copy_from_slice(&(torrent_index as u64).to_be_bytes());
            let (metrics_tx, metrics_rx) = watch::channel(TorrentMetrics::default());
            assert!(handle.register_torrent(info_hash.clone(), metrics_rx));
            senders.push(metrics_tx);
            info_hashes.push(info_hash);
        }

        timeout(Duration::from_secs(30), async {
            loop {
                let snapshot = handle.snapshot().await.expect("peer manager snapshot");
                if snapshot.registered_torrents == torrents {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("register performance-probe torrents");
        view_rx.borrow_and_update();

        let publish_round = |round: usize| {
            for (torrent_index, (metrics_tx, info_hash)) in
                senders.iter().zip(&info_hashes).enumerate()
            {
                let address = format!(
                    "192.0.2.{}:{}",
                    torrent_index % 250 + 1,
                    10_000 + torrent_index % 50_000
                );
                metrics_tx.send_replace(metrics_with_peer_transfer(
                    info_hash,
                    1024 * MIB,
                    &address,
                    round as u64,
                    round as u64,
                ));
            }
        };
        publish_round(1);
        wait_for_view_metrics_update(&mut view_rx, torrents as u64).await;
        PERF_NOTIFICATION_WAKES.store(0, Ordering::Relaxed);
        PERF_NOTIFICATIONS_HANDLED.store(0, Ordering::Relaxed);
        PERF_METRICS_REDUCTIONS.store(0, Ordering::Relaxed);
        PERF_METRICS_REDUCTION_NANOS.store(0, Ordering::Relaxed);
        PERF_VIEW_PUBLICATIONS.store(0, Ordering::Relaxed);
        PERF_VIEW_BUILD_NANOS.store(0, Ordering::Relaxed);

        let measured_started = std::time::Instant::now();
        let mut round_latencies = Vec::with_capacity(rounds);
        for measured_round in 0..rounds {
            let round_started = std::time::Instant::now();
            publish_round(measured_round + 2);
            let expected_updates = torrents.saturating_mul(measured_round + 2) as u64;
            wait_for_view_metrics_update(&mut view_rx, expected_updates).await;
            round_latencies.push(round_started.elapsed());
        }
        let measured_elapsed = measured_started.elapsed();
        round_latencies.sort_unstable();

        let updates = torrents.saturating_mul(rounds) as u64;
        let notification_wakes = PERF_NOTIFICATION_WAKES.load(Ordering::Relaxed);
        let notifications_handled = PERF_NOTIFICATIONS_HANDLED.load(Ordering::Relaxed);
        let reductions = PERF_METRICS_REDUCTIONS.load(Ordering::Relaxed);
        let reduction_nanos = PERF_METRICS_REDUCTION_NANOS.load(Ordering::Relaxed);
        let view_publications = PERF_VIEW_PUBLICATIONS.load(Ordering::Relaxed);
        let view_build_nanos = PERF_VIEW_BUILD_NANOS.load(Ordering::Relaxed);
        let median_round = round_latencies[round_latencies.len() / 2];
        let max_round = *round_latencies.last().expect("at least one measured round");
        println!(
            "PM_PERF torrents={torrents} rounds={rounds} updates={updates} elapsed_ms={:.3} updates_per_sec={:.1} median_round_ms={:.3} max_round_ms={:.3} notification_wakes={notification_wakes} notifications_handled={notifications_handled} reductions={reductions} reduction_ms={:.3} avg_reduction_us={:.3} view_publications={view_publications} view_build_ms={:.3} avg_view_build_us={:.3}",
            measured_elapsed.as_secs_f64() * 1000.0,
            updates as f64 / measured_elapsed.as_secs_f64(),
            median_round.as_secs_f64() * 1000.0,
            max_round.as_secs_f64() * 1000.0,
            reduction_nanos as f64 / 1_000_000.0,
            reduction_nanos as f64 / reductions.max(1) as f64 / 1000.0,
            view_build_nanos as f64 / 1_000_000.0,
            view_build_nanos as f64 / view_publications.max(1) as f64 / 1000.0,
        );

        let _ = shutdown_tx.send(());
        service.join().await;
    }

    #[test]
    fn upload_abuse_policy_contains_tui_ready_metadata() {
        let info_hash = vec![0x2a; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 42));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        let threshold = MIN_TRANSFER_ABUSE_BYTES;
        let metrics = metrics_with_peer(&info_hash, 1, "tcp://203.0.113.42:6881", threshold + 1);
        let mut reducer = PeerPolicyReducer::default();

        assert!(reducer.reduce_metrics(&info_hash, &metrics, now));
        let restriction = reducer
            .policy()
            .restrictions
            .get(&ip)
            .expect("IP should be restricted");
        assert_eq!(restriction.detected_at, now);
        assert_eq!(
            restriction.blocked_until,
            now + Duration::from_secs(24 * 60 * 60)
        );
        assert_eq!(
            restriction.torrent_info_hash.as_deref(),
            Some(info_hash.as_slice())
        );
        assert_eq!(
            restriction.reason,
            PeerRestrictionReason::ExcessiveUpload {
                uploaded_bytes: threshold + 1,
                threshold_bytes: threshold,
            }
        );
    }

    #[test]
    fn departed_peer_metrics_are_counted_without_remaining_active() {
        let info_hash = vec![0x36; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 86));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(36_000);
        let mut metrics = metrics_without_peers(&info_hash, 1);
        metrics.departed_peers.push(PeerInfo {
            address: format!("{ip}:6881"),
            total_uploaded: MIN_TRANSFER_ABUSE_BYTES + 1,
            last_action: "Disconnected".to_string(),
            ..PeerInfo::default()
        });
        let mut reducer = PeerPolicyReducer::default();

        assert!(reducer.reduce_metrics(&info_hash, &metrics, now));
        assert!(!reducer.reduce_metrics(&info_hash, &metrics, now + Duration::from_secs(1)));
        assert!(reducer.policy().restrictions.contains_key(&ip));
        let view = reducer.build_view(&HashMap::from([(info_hash.clone(), metrics)]), 1, 1);
        let departed = view.tracked_peers.first().expect("tracked departed peer");
        assert_eq!(departed.ip, ip);
        assert!(!departed.is_active);
        assert!(departed.endpoints.is_empty());
    }

    #[test]
    fn download_abuse_blocks_for_twenty_four_hours() {
        let info_hash = vec![0x2b; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 43));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(11_000);
        let threshold = MIN_TRANSFER_ABUSE_BYTES;
        let metrics =
            metrics_with_peer_transfer(&info_hash, 1, "tcp://203.0.113.43:6881", threshold + 1, 0);
        let mut reducer = PeerPolicyReducer::default();

        assert!(reducer.reduce_metrics(&info_hash, &metrics, now));
        let restriction = reducer
            .policy()
            .restrictions
            .get(&ip)
            .expect("IP should be restricted");
        assert_eq!(
            restriction.blocked_until,
            now + Duration::from_secs(24 * 60 * 60)
        );
        assert_eq!(
            restriction.reason,
            PeerRestrictionReason::ExcessiveDownload {
                downloaded_bytes: threshold + 1,
                threshold_bytes: threshold,
            }
        );
    }

    #[test]
    fn oversized_persisted_policy_is_rejected_before_parsing() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let path = temp.path().join("peer_policy.toml");
        let file = fs::File::create(&path).expect("create sparse policy file");
        file.set_len(MAX_POLICY_FILE_BYTES + 1)
            .expect("extend sparse policy file");

        let error = load_peer_policy_from_path(&path, SystemTime::now())
            .expect_err("oversized policy must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn persisted_policy_round_trip_preserves_metadata_and_discards_expired_entries() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let path = temp.path().join("peer_policy.toml");
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(20_000);
        let active_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 50));
        let expired_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 51));
        let active = PeerRestriction {
            detected_at: now - Duration::from_secs(60),
            blocked_until: now + Duration::from_secs(600),
            torrent_info_hash: Some(vec![0x50; 20]),
            reason: PeerRestrictionReason::ReconnectChurn {
                reconnects: RECONNECT_LIMIT as u32,
                threshold: RECONNECT_LIMIT as u32,
                window_secs: RECONNECT_WINDOW.as_secs(),
            },
        };
        let expired = PeerRestriction {
            blocked_until: now,
            ..active.clone()
        };
        let policy = PeerPolicy {
            restrictions: HashMap::from([(active_ip, active.clone()), (expired_ip, expired)]),
        };

        save_peer_policy_to_path(&path, &policy).expect("persist policy");
        let (restored, reconciliation_dirty) =
            load_peer_policy_state_from_path(&path, now).expect("restore policy");

        assert_eq!(restored.restrictions.get(&active_ip), Some(&active));
        assert!(!restored.restrictions.contains_key(&expired_ip));
        assert!(reconciliation_dirty);
    }

    #[test]
    fn stale_persistence_ack_does_not_clear_newer_dirty_revision() {
        let (tx, _rx) = watch::channel(None);
        let mut persistence = PolicyPersistenceState {
            tx: Some(tx),
            dirty: false,
            revision: 0,
            queued_revision: None,
        };
        let reducer = PeerPolicyReducer::default();

        persistence.mark_dirty();
        persistence.queue_if_dirty(&reducer);
        let stale_revision = persistence.revision;
        persistence.mark_dirty();
        let current_revision = persistence.revision;

        persistence.apply_result(PolicyPersistenceResult {
            revision: stale_revision,
            succeeded: true,
        });
        assert!(persistence.dirty);
        assert_eq!(persistence.revision, current_revision);

        persistence.apply_result(PolicyPersistenceResult {
            revision: current_revision,
            succeeded: true,
        });
        assert!(!persistence.dirty);
    }

    #[tokio::test]
    async fn failed_checkpoint_remains_dirty_and_retries_successfully() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let blocked_parent = temp.path().join("not-a-directory");
        fs::write(&blocked_parent, "block directory creation").expect("seed blocking file");
        let path = blocked_parent.join("peer_policy.toml");
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = PeerManagerService::new_with_persistence_options(
            shutdown_tx.subscribe(),
            Some(path.clone()),
            Duration::from_millis(20),
        );
        let handle = service.handle();
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 52));
        let now = SystemTime::now();

        assert!(handle.block_ip_until(ip, now + Duration::from_secs(600)));
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!path.exists());

        fs::remove_file(&blocked_parent).expect("remove blocking file");
        fs::create_dir(&blocked_parent).expect("create checkpoint parent");
        timeout(Duration::from_secs(1), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("dirty checkpoint should retry after a failed write");

        let _ = shutdown_tx.send(());
        service.join().await;
        let restored =
            load_peer_policy_from_path(&path, SystemTime::now()).expect("load retried checkpoint");
        assert!(restored.blocks_ip(ip, SystemTime::now()));
    }

    #[test]
    fn policy_cap_is_one_million_and_bounding_keeps_longest_lived_restrictions() {
        assert_eq!(MAX_POLICY_RESTRICTIONS, 1_000_000);
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut policy = PeerPolicy {
            restrictions: (0..4_u128)
                .map(|index| {
                    let ip = IpAddr::V6(index.into());
                    let blocked_until = now + Duration::from_secs(60 + index as u64);
                    (
                        ip,
                        PeerRestriction {
                            detected_at: now,
                            blocked_until,
                            torrent_info_hash: Some(vec![0x33; 20]),
                            reason: PeerRestrictionReason::Manual,
                        },
                    )
                })
                .collect(),
        };

        policy.retain_live_and_bounded_to(now, 3);

        assert_eq!(policy.restrictions.len(), 3);
        assert!(!policy.restrictions.contains_key(&IpAddr::V6(0_u128.into())));
        assert!(policy.restrictions.contains_key(&IpAddr::V6(3_u128.into())));
    }

    #[test]
    fn restored_policy_merges_mapped_ipv6_collision_by_later_deadline() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let path = temp.path().join("peer_policy.toml");
        let ipv4 = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 72));
        let mapped = IpAddr::V6(Ipv4Addr::new(203, 0, 113, 72).to_ipv6_mapped());
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(72_000);
        let earlier = PeerRestriction {
            detected_at: now,
            blocked_until: now + Duration::from_secs(300),
            torrent_info_hash: None,
            reason: PeerRestrictionReason::Manual,
        };
        let later = PeerRestriction {
            detected_at: now + Duration::from_secs(1),
            blocked_until: now + Duration::from_secs(600),
            torrent_info_hash: Some(b"mapped".to_vec()),
            reason: PeerRestrictionReason::ReconnectChurn {
                reconnects: RECONNECT_LIMIT as u32,
                threshold: RECONNECT_LIMIT as u32,
                window_secs: RECONNECT_WINDOW.as_secs(),
            },
        };
        let policy = PeerPolicy {
            restrictions: HashMap::from([(ipv4, earlier), (mapped, later.clone())]),
        };
        save_peer_policy_to_path(&path, &policy).expect("persist collision policy");

        let (restored, reconciliation_dirty) =
            load_peer_policy_state_from_path(&path, now).expect("restore normalized policy");

        assert!(reconciliation_dirty);
        assert_eq!(restored.restrictions.len(), 1);
        assert_eq!(restored.restrictions.get(&ipv4), Some(&later));
    }

    #[test]
    fn parses_transport_qualified_ipv4_and_ipv6_peer_addresses() {
        assert_eq!(
            parse_peer_ip("tcp://203.0.113.70:6881"),
            Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 70)))
        );
        assert_eq!(
            parse_peer_ip("utp://[2001:db8::70]:6881"),
            Some("2001:db8::70".parse().expect("valid IPv6 address"))
        );
    }

    #[test]
    fn ipv4_mapped_ipv6_uses_the_ipv4_policy_identity() {
        let ipv4 = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 71));
        let mapped = IpAddr::V6(Ipv4Addr::new(203, 0, 113, 71).to_ipv6_mapped());
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(71_000);
        let policy =
            PeerPolicy::from_blocked_until(HashMap::from([(ipv4, now + Duration::from_secs(600))]));

        assert_eq!(
            parse_peer_ip("tcp://[::ffff:203.0.113.71]:6881"),
            Some(ipv4)
        );
        assert!(policy.blocks_ip(mapped, now));
    }

    #[test]
    fn same_endpoint_counter_resets_count_as_reconnects() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![7; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 70));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(7_000_000);
        let torrent_size = 4 * 1024 * 1024 * 1024;

        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(&info_hash, torrent_size, "tcp://192.0.2.70:6000", 100),
            now,
        ));
        for reconnect in 1..RECONNECT_LIMIT {
            let base = now + Duration::from_secs(reconnect as u64);
            assert!(!reducer.reduce_metrics(
                &info_hash,
                &metrics_with_peer(&info_hash, torrent_size, "tcp://192.0.2.70:6000", 0),
                base,
            ));
            assert!(!reducer.reduce_metrics(
                &info_hash,
                &metrics_with_peer(&info_hash, torrent_size, "tcp://192.0.2.70:6000", 100),
                base + Duration::from_secs(1),
            ));
        }
        assert!(reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(&info_hash, torrent_size, "tcp://192.0.2.70:6000", 0),
            now + Duration::from_secs(RECONNECT_LIMIT as u64),
        ));
        assert!(reducer.policy().restrictions.contains_key(&ip));
    }

    #[test]
    fn cumulative_reconnect_count_captures_lifecycle_events_between_snapshots() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![0x71; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 71));
        let address = "tcp://192.0.2.71:6000";
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(7_100_000);
        let torrent_size = 4 * 1024 * 1024 * 1024;

        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(&info_hash, torrent_size, address, 0),
            now,
        ));
        assert!(reducer.reduce_metrics(
            &info_hash,
            &metrics_with_reconnect_count(
                &info_hash,
                torrent_size,
                address,
                RECONNECT_LIMIT as u64,
            ),
            now + Duration::from_secs(1),
        ));

        assert!(matches!(
            reducer.policy().restrictions.get(&ip).map(|restriction| &restriction.reason),
            Some(PeerRestrictionReason::ReconnectChurn {
                reconnects,
                threshold,
                ..
            }) if *reconnects == RECONNECT_LIMIT as u32 && *threshold == RECONNECT_LIMIT as u32
        ));
    }

    #[tokio::test]
    async fn closed_metrics_receiver_removes_latest_snapshot() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = PeerManagerService::new(shutdown_tx.subscribe());
        let handle = service.handle();
        let info_hash = vec![8; 20];
        let (metrics_tx, metrics_rx) = watch::channel(TorrentMetrics::default());

        assert!(handle.register_torrent(info_hash.clone(), metrics_rx));
        metrics_tx
            .send(metrics_with_peer(
                &info_hash,
                1024 * 1024 * 1024,
                "tcp://198.51.100.80:6000",
                100,
            ))
            .expect("metrics receiver should remain open");
        let snapshot = wait_for_metrics_update(&handle, 1).await;
        assert!(snapshot.latest_metrics.contains_key(&info_hash));
        drop(metrics_tx);

        let snapshot = timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = handle.snapshot().await.expect("peer manager snapshot");
                if snapshot.registered_torrents == 0 {
                    break snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("closed receiver should be removed");
        assert!(!snapshot.latest_metrics.contains_key(&info_hash));

        let _ = shutdown_tx.send(());
        service.join().await;
    }

    #[test]
    fn upload_abuse_uses_cumulative_deltas_and_a_minimum_floor() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![1; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let small_torrent = 100 * 1024 * 1024;

        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(
                &info_hash,
                small_torrent,
                "203.0.113.10:6000",
                128 * 1024 * 1024
            ),
            now,
        ));
        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(
                &info_hash,
                small_torrent,
                "203.0.113.10:6000",
                MIN_TRANSFER_ABUSE_BYTES,
            ),
            now + Duration::from_secs(1),
        ));
        assert!(reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(
                &info_hash,
                small_torrent,
                "203.0.113.10:6000",
                MIN_TRANSFER_ABUSE_BYTES + 1,
            ),
            now + Duration::from_secs(2),
        ));
        assert_eq!(
            reducer
                .policy()
                .restrictions
                .get(&ip)
                .map(|restriction| restriction.blocked_until),
            Some(now + Duration::from_secs(2) + EXCESSIVE_TRANSFER_BLOCK_DURATION)
        );
        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(
                &info_hash,
                small_torrent,
                "203.0.113.10:6000",
                MIN_TRANSFER_ABUSE_BYTES + 1,
            ),
            now + Duration::from_secs(3),
        ));
    }

    #[test]
    fn upload_abuse_aggregates_reconnected_source_ports_per_torrent_and_ip() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![2; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 20));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000);
        let torrent_size = 512 * 1024 * 1024;

        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(
                &info_hash,
                torrent_size,
                "198.51.100.20:6000",
                600 * 1024 * 1024,
            ),
            now,
        ));
        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_without_peers(&info_hash, torrent_size),
            now + Duration::from_secs(1),
        ));
        assert!(reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(
                &info_hash,
                torrent_size,
                "198.51.100.20:7000",
                500 * 1024 * 1024,
            ),
            now + Duration::from_secs(2),
        ));
        assert!(reducer.policy().restrictions.contains_key(&ip));
    }

    #[test]
    fn upload_threshold_matrix_covers_floor_multiplier_and_overflow_boundaries() {
        let cases = [
            ("zero_size_at_floor", 0, 256 * MIB, false),
            ("zero_size_over_floor", 0, 256 * MIB + 1, true),
            ("small_at_floor", 100 * MIB, 256 * MIB, false),
            ("small_over_floor", 100 * MIB, 256 * MIB + 1, true),
            ("factor_at_exact_limit", 512 * MIB, 1024 * MIB, false),
            ("factor_over_limit", 512 * MIB, 1024 * MIB + 1, true),
            ("saturated_limit", u64::MAX, u64::MAX, false),
        ];
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000_000);

        for (index, (name, torrent_size, uploaded, expected_block)) in cases.into_iter().enumerate()
        {
            let mut reducer = PeerPolicyReducer::default();
            let info_hash = vec![index as u8; 20];
            let address = format!("203.0.113.{}:6000", index + 1);
            let ip = parse_peer_ip(&address).expect("scenario IP");
            let changed = reducer.reduce_metrics(
                &info_hash,
                &metrics_with_peer(&info_hash, torrent_size, &address, uploaded),
                now,
            );

            assert_eq!(changed, expected_block, "scenario: {name}");
            assert_eq!(
                reducer.policy().restrictions.contains_key(&ip),
                expected_block,
                "scenario: {name}"
            );
        }
    }

    #[test]
    fn repeated_cumulative_snapshots_do_not_double_count_uploads() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![11; 20];
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(11_000_000);
        let address = "203.0.113.111:6000";

        for offset in 0..20 {
            assert!(!reducer.reduce_metrics(
                &info_hash,
                &metrics_with_peer(&info_hash, 100 * MIB, address, 200 * MIB),
                now + Duration::from_secs(offset),
            ));
        }
        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(&info_hash, 100 * MIB, address, 256 * MIB),
            now + Duration::from_secs(20),
        ));
        assert!(reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(&info_hash, 100 * MIB, address, 256 * MIB + 1),
            now + Duration::from_secs(21),
        ));
    }

    #[test]
    fn upload_evidence_is_not_aggregated_across_torrents() {
        let mut reducer = PeerPolicyReducer::default();
        let first_hash = vec![12; 20];
        let second_hash = vec![13; 20];
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(12_000_000);
        let address = "198.51.100.120:6000";
        let ip = parse_peer_ip(address).expect("scenario IP");

        for info_hash in [&first_hash, &second_hash] {
            assert!(!reducer.reduce_metrics(
                info_hash,
                &metrics_with_peer(info_hash, 100 * MIB, address, 200 * MIB),
                now,
            ));
        }
        assert!(!reducer.policy().restrictions.contains_key(&ip));

        assert!(reducer.reduce_metrics(
            &first_hash,
            &metrics_with_peer(&first_hash, 100 * MIB, address, 256 * MIB + 1),
            now + Duration::from_secs(1),
        ));
        assert!(reducer.policy().restrictions.contains_key(&ip));
    }

    #[test]
    fn simultaneous_endpoints_for_one_ip_aggregate_uploads() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![14; 20];
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(14_000_000);
        let ip = "2001:db8::14".parse().expect("scenario IPv6");
        let metrics = metrics_with_peers(
            &info_hash,
            100 * MIB,
            &[
                ("tcp://[2001:db8::14]:6000", 140 * MIB),
                ("utp://[2001:db8::14]:7000", 140 * MIB),
            ],
        );

        assert!(reducer.reduce_metrics(&info_hash, &metrics, now));
        assert!(reducer.policy().restrictions.contains_key(&ip));
    }

    #[test]
    fn duplicate_rows_for_one_endpoint_use_the_largest_counter() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![15; 20];
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(15_000_000);
        let address = "tcp://203.0.113.150:6000";

        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peers(
                &info_hash,
                100 * MIB,
                &[(address, 180 * MIB), (address, 200 * MIB)],
            ),
            now,
        ));
        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peers(
                &info_hash,
                100 * MIB,
                &[(address, 220 * MIB), (address, 250 * MIB)],
            ),
            now + Duration::from_secs(1),
        ));
        assert!(reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peers(
                &info_hash,
                100 * MIB,
                &[(address, 257 * MIB), (address, 251 * MIB)],
            ),
            now + Duration::from_secs(2),
        ));
    }

    #[test]
    fn abusive_peer_is_blocked_without_blocking_benign_peer() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![16; 20];
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(16_000_000);
        let abusive_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 161));
        let benign_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 162));
        let metrics = metrics_with_peers(
            &info_hash,
            100 * MIB,
            &[
                ("203.0.113.161:6000", 256 * MIB + 1),
                ("203.0.113.162:6000", 16 * MIB),
            ],
        );

        assert!(reducer.reduce_metrics(&info_hash, &metrics, now));
        assert!(reducer.policy().restrictions.contains_key(&abusive_ip));
        assert!(!reducer.policy().restrictions.contains_key(&benign_ip));
    }

    #[test]
    fn malformed_peer_addresses_are_ignored_without_affecting_valid_peers() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![17; 20];
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(17_000_000);
        let valid_ip = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 170));
        let metrics = metrics_with_peers(
            &info_hash,
            100 * MIB,
            &[
                ("not-an-address", u64::MAX),
                ("tcp://missing-port", u64::MAX),
                ("198.51.100.170:6000", 1),
            ],
        );

        assert!(!reducer.reduce_metrics(&info_hash, &metrics, now));
        assert_eq!(reducer.histories.len(), 1);
        assert!(reducer.has_history(&info_hash, valid_ip));
    }

    #[test]
    fn maintenance_hard_bounds_histories_by_oldest_last_seen() {
        let mut reducer = PeerPolicyReducer::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(300_000);
        let info_hash = vec![42; 20];
        let oldest = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        let middle = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2));
        let newest = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 3));

        for (offset, address) in [
            (0, "192.0.2.1:6000"),
            (1, "192.0.2.2:6000"),
            (2, "192.0.2.3:6000"),
        ] {
            assert!(!reducer.reduce_metrics(
                &info_hash,
                &metrics_with_peer(&info_hash, 1024 * MIB, address, 0),
                now + Duration::from_secs(offset),
            ));
        }

        assert_eq!(reducer.history_count(), 3);
        assert!(!reducer.maintain_to(now + Duration::from_secs(3), 2));
        assert_eq!(reducer.history_count(), 2);
        assert!(!reducer.has_history(&info_hash, oldest));
        assert!(reducer.has_history(&info_hash, middle));
        assert!(reducer.has_history(&info_hash, newest));
    }

    #[test]
    fn counter_reset_counts_only_new_session_bytes() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![18; 20];
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(18_000_000);
        let address = "utp://198.51.100.180:6000";

        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(&info_hash, 100 * MIB, address, 200 * MIB),
            now,
        ));
        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(&info_hash, 100 * MIB, address, 10 * MIB),
            now + Duration::from_secs(1),
        ));
        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(&info_hash, 100 * MIB, address, 50 * MIB),
            now + Duration::from_secs(2),
        ));
        assert!(reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(&info_hash, 100 * MIB, address, 57 * MIB),
            now + Duration::from_secs(3),
        ));
    }

    #[test]
    fn eleventh_connection_within_ten_seconds_blocks_for_two_hours() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![3; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 30));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(3_000_000);
        let torrent_size = 1024 * 1024 * 1024;

        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(&info_hash, torrent_size, "192.0.2.30:6000", 0),
            now,
        ));
        for reconnect in 1..RECONNECT_LIMIT {
            let offset = Duration::from_secs(reconnect as u64);
            assert!(!reducer.reduce_metrics(
                &info_hash,
                &metrics_with_peer(
                    &info_hash,
                    torrent_size,
                    &format!("192.0.2.30:{}", 6000 + reconnect),
                    0,
                ),
                now + offset,
            ));
        }

        let final_offset = Duration::from_secs(RECONNECT_LIMIT as u64);
        assert!(reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(&info_hash, torrent_size, "192.0.2.30:7000", 0),
            now + final_offset,
        ));
        let restriction = reducer
            .policy()
            .restrictions
            .get(&ip)
            .expect("reconnect restriction");
        assert_eq!(restriction.detected_at, now + final_offset);
        assert_eq!(
            restriction.blocked_until,
            now + final_offset + Duration::from_secs(2 * 60 * 60)
        );
        assert_eq!(
            restriction.torrent_info_hash.as_deref(),
            Some(info_hash.as_slice())
        );
        assert_eq!(
            restriction.reason,
            PeerRestrictionReason::ReconnectChurn {
                reconnects: RECONNECT_LIMIT as u32,
                threshold: RECONNECT_LIMIT as u32,
                window_secs: RECONNECT_WINDOW.as_secs(),
            }
        );
    }

    #[test]
    fn reconnects_outside_the_window_do_not_accumulate() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![4; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 40));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000);
        let torrent_size = 1024 * 1024 * 1024;

        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(&info_hash, torrent_size, "192.0.2.40:6000", 0),
            now,
        ));
        for reconnect in 1..=RECONNECT_LIMIT {
            let present_at = now + RECONNECT_WINDOW * (reconnect as u32);
            assert!(!reducer.reduce_metrics(
                &info_hash,
                &metrics_with_peer(
                    &info_hash,
                    torrent_size,
                    &format!("192.0.2.40:{}", 6000 + reconnect),
                    0,
                ),
                present_at,
            ));
        }
        assert!(!reducer.policy().restrictions.contains_key(&ip));
    }

    #[test]
    fn reconnect_evidence_expires_without_another_metrics_snapshot() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![0x3a; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 58));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(58_000_000);
        let mut metrics = metrics_without_peers(&info_hash, 1024 * MIB);
        metrics.peer_reconnect_counts.insert(ip, 1);

        assert!(!reducer.reduce_metrics(&info_hash, &metrics, now));
        assert_eq!(
            reducer.next_reconnect_expiry_delay(now),
            Some(RECONNECT_WINDOW)
        );
        assert!(
            !reducer.prune_reconnect_evidence(now + RECONNECT_WINDOW - Duration::from_millis(1))
        );
        assert!(reducer.prune_reconnect_evidence(now + RECONNECT_WINDOW));
        assert_eq!(reducer.next_reconnect_expiry_delay(now), None);

        let view = reducer.build_view(&HashMap::from([(info_hash, metrics)]), 1, 1);
        assert_eq!(view.tracked_peers[0].reconnect_count, 0);
    }

    #[test]
    fn partial_endpoint_replacement_does_not_count_as_ip_reconnect() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![19; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 190));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(19_000_000);
        let snapshots = [
            [("tcp://192.0.2.190:6000", 0), ("utp://192.0.2.190:7000", 0)],
            [("utp://192.0.2.190:7000", 0), ("tcp://192.0.2.190:8000", 0)],
            [("tcp://192.0.2.190:8000", 0), ("utp://192.0.2.190:9000", 0)],
        ];

        for (offset, peers) in snapshots.iter().enumerate() {
            assert!(!reducer.reduce_metrics(
                &info_hash,
                &metrics_with_peers(&info_hash, 1024 * MIB, peers),
                now + Duration::from_secs(offset as u64),
            ));
        }
        let history = reducer
            .histories
            .get(&info_hash)
            .and_then(|histories| histories.get(&ip))
            .expect("IP history");
        assert!(history.reconnects.is_empty());
    }

    #[test]
    fn repeated_absent_snapshots_count_only_one_reconnect_on_return() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![20; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 200));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(20_000_000);

        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(&info_hash, 1024 * MIB, "192.0.2.200:6000", 0),
            now,
        ));
        for offset in 1..10 {
            assert!(!reducer.reduce_metrics(
                &info_hash,
                &metrics_without_peers(&info_hash, 1024 * MIB),
                now + Duration::from_secs(offset),
            ));
        }
        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(&info_hash, 1024 * MIB, "192.0.2.200:6000", 0),
            now + Duration::from_secs(10),
        ));
        let history = reducer
            .histories
            .get(&info_hash)
            .and_then(|histories| histories.get(&ip))
            .expect("IP history");
        assert_eq!(history.reconnects.len(), 1);
    }

    #[test]
    fn reconnect_exactly_at_window_boundary_does_not_accumulate() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![21; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 210));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(21_000_000);

        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(&info_hash, 1024 * MIB, "192.0.2.210:6000", 0),
            now,
        ));
        for reconnect in 1..=RECONNECT_LIMIT {
            let offset = 1 + ((reconnect - 1) as u64 * RECONNECT_WINDOW.as_secs());
            assert!(!reducer.reduce_metrics(
                &info_hash,
                &metrics_with_peer(
                    &info_hash,
                    1024 * MIB,
                    &format!("192.0.2.210:{}", 6000 + reconnect),
                    0,
                ),
                now + Duration::from_secs(offset),
            ));
        }
        assert!(!reducer.policy().restrictions.contains_key(&ip));
    }

    #[test]
    fn transport_switches_share_ip_reputation_and_count_as_reconnects() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![22; 20];
        let ip = "2001:db8::22".parse().expect("scenario IPv6");
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(22_000_000);

        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(&info_hash, 1024 * MIB, "tcp://[2001:db8::22]:6000", 0),
            now,
        ));
        for reconnect in 1..=RECONNECT_LIMIT {
            let transport = if reconnect % 2 == 0 { "tcp" } else { "utp" };
            let address = format!("{transport}://[2001:db8::22]:6000");
            let changed = reducer.reduce_metrics(
                &info_hash,
                &metrics_with_peer(&info_hash, 1024 * MIB, &address, 0),
                now + Duration::from_secs(reconnect as u64),
            );
            assert_eq!(changed, reconnect == RECONNECT_LIMIT);
        }
        assert!(reducer.policy().restrictions.contains_key(&ip));
        assert_eq!(reducer.histories.len(), 1);
    }

    #[test]
    fn reconnect_evidence_is_not_aggregated_across_torrents() {
        let mut reducer = PeerPolicyReducer::default();
        let hashes = [vec![23; 20], vec![24; 20]];
        let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 230));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(23_000_000);

        for info_hash in &hashes {
            assert!(!reducer.reduce_metrics(
                info_hash,
                &metrics_with_peer(info_hash, 1024 * MIB, "192.0.2.230:6000", 0),
                now,
            ));
            for reconnect in 1..=3 {
                assert!(!reducer.reduce_metrics(
                    info_hash,
                    &metrics_with_peer(
                        info_hash,
                        1024 * MIB,
                        &format!("192.0.2.230:{}", 6000 + reconnect),
                        0,
                    ),
                    now + Duration::from_secs(reconnect as u64),
                ));
            }
        }
        assert!(!reducer.policy().restrictions.contains_key(&ip));
    }

    #[test]
    fn active_history_is_not_pruned_after_retention_period() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![25; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 250));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(25_000_000);

        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(&info_hash, 1024 * MIB, "192.0.2.250:6000", 1),
            now,
        ));
        assert!(!reducer.expire(now + HISTORY_RETENTION * 2));
        assert!(reducer.has_history(&info_hash, ip));
    }

    #[test]
    fn removing_one_torrent_preserves_other_torrent_history() {
        let mut reducer = PeerPolicyReducer::default();
        let removed_hash = vec![26; 20];
        let retained_hash = vec![27; 20];
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(26_000_000);

        for info_hash in [&removed_hash, &retained_hash] {
            assert!(!reducer.reduce_metrics(
                info_hash,
                &metrics_with_peer(info_hash, 1024 * MIB, "198.51.100.26:6000", 1),
                now,
            ));
        }
        reducer.remove_torrent(&removed_hash);

        assert_eq!(reducer.histories.len(), 1);
        assert!(reducer
            .histories
            .keys()
            .all(|info_hash| info_hash == &retained_hash));
    }

    #[test]
    fn policy_deadlines_expire_independently() {
        let mut reducer = PeerPolicyReducer::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(27_000_000);
        let first_ip = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 27));
        let second_ip = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 28));

        assert!(reducer.block_ip_until(first_ip, now + Duration::from_secs(10)));
        assert!(reducer.block_ip_until(second_ip, now + Duration::from_secs(20)));
        assert!(reducer.expire(now + Duration::from_secs(10)));
        assert!(!reducer.policy().restrictions.contains_key(&first_ip));
        assert!(reducer.policy().restrictions.contains_key(&second_ip));
        assert!(!reducer.expire(now + Duration::from_secs(15)));
        assert!(reducer.expire(now + Duration::from_secs(20)));
        assert!(reducer.policy().restrictions.is_empty());
    }

    #[test]
    fn shorter_block_deadline_does_not_replace_longer_deadline() {
        let mut reducer = PeerPolicyReducer::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(28_000_000);
        let ip = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 29));
        let long_deadline = now + Duration::from_secs(100);

        assert!(reducer.block_ip_until(ip, long_deadline));
        assert!(!reducer.block_ip_until(ip, now + Duration::from_secs(50)));
        assert_eq!(
            reducer
                .policy()
                .restrictions
                .get(&ip)
                .map(|restriction| restriction.blocked_until),
            Some(long_deadline)
        );
    }

    #[test]
    fn inactive_history_is_pruned_after_retention_period() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![9; 20];
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(9_000_000);
        let torrent_size = 1024 * 1024 * 1024;

        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_with_peer(&info_hash, torrent_size, "tcp://192.0.2.90:6000", 100),
            now,
        ));
        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_without_peers(&info_hash, torrent_size),
            now + Duration::from_secs(1),
        ));
        assert_eq!(reducer.histories.len(), 1);

        assert!(!reducer.expire(now + HISTORY_RETENTION));
        assert!(reducer.histories.is_empty());
    }

    #[test]
    fn repeated_cumulative_reconnect_count_does_not_refresh_last_seen() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![0x34; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 84));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(34_000_000);
        let mut metrics = metrics_without_peers(&info_hash, 1024 * MIB);
        metrics.peer_reconnect_counts.insert(ip, 1);

        assert!(!reducer.reduce_metrics(&info_hash, &metrics, now));
        assert!(!reducer.reduce_metrics(&info_hash, &metrics, now + HISTORY_RETENTION / 2,));
        assert_eq!(reducer.histories[&info_hash][&ip].last_seen, Some(now),);

        assert!(!reducer.expire(now + HISTORY_RETENTION));
        assert!(!reducer.has_history(&info_hash, ip));
    }

    #[test]
    fn reconnect_counter_restarts_after_source_baseline_disappears() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![0x35; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 85));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(35_000_000);
        let mut metrics = metrics_without_peers(&info_hash, 1024 * MIB);
        metrics.peer_reconnect_counts.insert(ip, 1);

        assert!(!reducer.reduce_metrics(&info_hash, &metrics, now));
        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics_without_peers(&info_hash, 1024 * MIB),
            now + RECONNECT_WINDOW,
        ));

        for reconnect_count in 1..=RECONNECT_LIMIT as u64 {
            metrics.peer_reconnect_counts.insert(ip, reconnect_count);
            let changed = reducer.reduce_metrics(
                &info_hash,
                &metrics,
                now + RECONNECT_WINDOW + Duration::from_secs(reconnect_count),
            );
            assert_eq!(changed, reconnect_count == RECONNECT_LIMIT as u64);
        }

        assert!(reducer.policy().restrictions.contains_key(&ip));
    }

    #[tokio::test]
    async fn registration_evaluates_current_abusive_metrics_immediately() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = PeerManagerService::new(shutdown_tx.subscribe());
        let handle = service.handle();
        let info_hash = vec![29; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 29));
        let (_metrics_tx, metrics_rx) = watch::channel(metrics_with_peer(
            &info_hash,
            100 * MIB,
            "tcp://203.0.113.29:6000",
            256 * MIB + 1,
        ));
        let mut policy_rx = handle.subscribe_policy();

        assert!(handle.register_torrent(info_hash, metrics_rx));
        timeout(Duration::from_secs(2), policy_rx.changed())
            .await
            .expect("registration should publish policy")
            .expect("policy publisher should remain open");
        assert!(policy_rx.borrow_and_update().restrictions.contains_key(&ip));

        let _ = shutdown_tx.send(());
        service.join().await;
    }

    #[tokio::test]
    async fn registration_processes_preexisting_watch_update_once() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = PeerManagerService::new(shutdown_tx.subscribe());
        let handle = service.handle();
        let info_hash = vec![30; 20];
        let (metrics_tx, metrics_rx) = watch::channel(TorrentMetrics::default());
        metrics_tx
            .send(metrics_with_peer(
                &info_hash,
                1024 * MIB,
                "198.51.100.30:6000",
                1,
            ))
            .expect("metrics receiver should remain open");

        assert!(handle.register_torrent(info_hash, metrics_rx));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let snapshot = handle.snapshot().await.expect("peer manager snapshot");
        assert_eq!(snapshot.metrics_updates, 1);

        let _ = shutdown_tx.send(());
        service.join().await;
    }

    #[tokio::test]
    async fn coalesced_metrics_still_apply_latest_cumulative_upload_total() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = PeerManagerService::new(shutdown_tx.subscribe());
        let handle = service.handle();
        let info_hash = vec![31; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 31));
        let (metrics_tx, metrics_rx) = watch::channel(TorrentMetrics::default());
        let mut policy_rx = handle.subscribe_policy();

        assert!(handle.register_torrent(info_hash.clone(), metrics_rx));
        metrics_tx
            .send(metrics_with_peer(
                &info_hash,
                100 * MIB,
                "203.0.113.31:6000",
                100 * MIB,
            ))
            .expect("metrics receiver should remain open");
        metrics_tx
            .send(metrics_with_peer(
                &info_hash,
                100 * MIB,
                "203.0.113.31:6000",
                256 * MIB + 1,
            ))
            .expect("metrics receiver should remain open");

        timeout(Duration::from_secs(2), policy_rx.changed())
            .await
            .expect("latest cumulative metrics should publish policy")
            .expect("policy publisher should remain open");
        assert!(policy_rx.borrow_and_update().restrictions.contains_key(&ip));

        let _ = shutdown_tx.send(());
        service.join().await;
    }

    #[tokio::test]
    async fn policy_expiry_is_published_without_new_metrics() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = PeerManagerService::new_with_persistence_options(
            shutdown_tx.subscribe(),
            None,
            Duration::from_millis(20),
        );
        let handle = service.handle();
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 32));
        let mut policy_rx = handle.subscribe_policy();

        assert!(handle.block_ip_until(ip, SystemTime::now() + Duration::from_millis(100)));
        timeout(Duration::from_secs(1), policy_rx.changed())
            .await
            .expect("block should be published")
            .expect("policy publisher should remain open");
        assert!(policy_rx.borrow_and_update().restrictions.contains_key(&ip));

        timeout(Duration::from_secs(1), policy_rx.changed())
            .await
            .expect("expiry should be published")
            .expect("policy publisher should remain open");
        assert!(!policy_rx.borrow_and_update().restrictions.contains_key(&ip));

        let _ = shutdown_tx.send(());
        service.join().await;
    }

    #[tokio::test]
    async fn benign_metrics_do_not_publish_unchanged_policy() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = PeerManagerService::new(shutdown_tx.subscribe());
        let handle = service.handle();
        let info_hash = vec![10; 20];
        let (metrics_tx, metrics_rx) = watch::channel(TorrentMetrics::default());
        let mut policy_rx = handle.subscribe_policy();

        assert!(handle.register_torrent(info_hash.clone(), metrics_rx));
        metrics_tx
            .send(metrics_with_peer(
                &info_hash,
                1024 * 1024 * 1024,
                "utp://198.51.100.100:6000",
                1024,
            ))
            .expect("metrics receiver should remain open");
        let _ = wait_for_metrics_update(&handle, 1).await;
        assert!(timeout(Duration::from_millis(100), policy_rx.changed())
            .await
            .is_err());

        let _ = shutdown_tx.send(());
        service.join().await;
    }

    #[test]
    fn expired_policy_is_removed_without_replaying_consumed_evidence() {
        let mut reducer = PeerPolicyReducer::default();
        let info_hash = vec![5; 20];
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 50));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000_000);
        let metrics = metrics_with_peer(
            &info_hash,
            100 * 1024 * 1024,
            "203.0.113.50:6000",
            MIN_TRANSFER_ABUSE_BYTES + 1,
        );

        assert!(reducer.reduce_metrics(&info_hash, &metrics, now));
        assert!(reducer.policy().restrictions.contains_key(&ip));
        assert!(reducer.expire(now + EXCESSIVE_TRANSFER_BLOCK_DURATION));
        assert!(!reducer.policy().restrictions.contains_key(&ip));
        assert!(!reducer.reduce_metrics(
            &info_hash,
            &metrics,
            now + EXCESSIVE_TRANSFER_BLOCK_DURATION + Duration::from_secs(1),
        ));
    }
}
