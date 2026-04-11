// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::config::Settings;
use crate::network_metrics;
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant as StdInstant};
use tokio::net::lookup_host;
use tokio::net::UdpSocket;
use tokio::sync::broadcast;
use tokio::sync::mpsc::{self, Sender};
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::sync::Mutex;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;
use tokio::time::MissedTickBehavior;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{empty, Stream, StreamExt};

#[cfg(feature = "dht")]
use mainline::{async_dht::AsyncDht, Dht, Id};
use rand::random;

type PeerBatchStream = Pin<Box<dyn Stream<Item = Vec<SocketAddr>> + Send>>;
type HealthFuture = Pin<Box<dyn Future<Output = DhtHealthSnapshot> + Send>>;
type AnnounceFuture = Pin<Box<dyn Future<Output = bool> + Send>>;
type MaintenanceFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type RecoveryStateFuture =
    Pin<Box<dyn Future<Output = Option<InternalPrototypeRecoveryState>> + Send>>;

const DHT_LOOKUP_REFRESH_INTERVAL: Duration = Duration::from_secs(300);
const DHT_RETRY_INTERVAL: Duration = Duration::from_secs(60);
const DHT_HEALTH_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const INTERNAL_DHT_QUERY_TIMEOUT: Duration = Duration::from_millis(500);
const INTERNAL_DHT_ROUTE_QUERY_TIMEOUT: Duration = Duration::from_millis(1500);
const INTERNAL_DHT_SOCKET_BUFFER: usize = 16 * 1024;
const INTERNAL_DHT_MAX_VISITS_PER_FAMILY: usize = 240;
const INTERNAL_DHT_INITIAL_QUERY_FANOUT: usize = 4;
const INTERNAL_DHT_MAX_CONCURRENT_FAMILY_QUERIES: usize = 4;
const INTERNAL_DHT_IPV4_FAST_LOOKUP_QUERY_FANOUT: usize = 8;
const INTERNAL_DHT_DISCOVERY_HEDGE_DELAY: Duration = Duration::from_millis(75);
const INTERNAL_DHT_MAX_RETURNED_PEERS: usize = 512;
const INTERNAL_DHT_HEALTH_PROBE_LIMIT: usize = 4;
const INTERNAL_DHT_DISCOVERED_NODE_LIMIT: usize = 256;
const INTERNAL_DHT_IPV4_ACTIVE_ROUTE_LIMIT: usize = 160;
const INTERNAL_DHT_IPV4_K_BUCKET_SIZE: usize = 20;
const INTERNAL_DHT_IPV4_UNBUCKETED_ROUTE_LIMIT: usize = 32;
const INTERNAL_DHT_IPV6_ACTIVE_ROUTE_LIMIT: usize = 128;
const INTERNAL_DHT_ACTIVE_ROUTE_REFILL_FLOOR: usize = 64;
const INTERNAL_DHT_FAST_ACTIVE_FRONTIER_LIMIT: usize = 24;
const INTERNAL_DHT_FAST_ACTIVE_FRONTIER_READY_FLOOR: usize = 12;
const INTERNAL_DHT_SEED_NODE_LIMIT: usize = 24;
const INTERNAL_DHT_SEED_BOOTSTRAP_RESERVE: usize = 2;
const INTERNAL_DHT_LOOKUP_CACHE_TTL: Duration = Duration::from_secs(10);
const INTERNAL_DHT_LOOKUP_STREAM_BUFFER: usize = 16;
const INTERNAL_DHT_ROUTE_WARM_LIMIT: usize = 8;
const INTERNAL_DHT_ROUTE_WARM_MAX_VISITS: usize = 48;
const INTERNAL_DHT_ROUTE_MAINTENANCE_LIMIT: usize = 8;
const INTERNAL_DHT_MAX_FAILURES_PER_NODE: u16 = 8;
const INTERNAL_DHT_TOKEN_CACHE_LIMIT: usize = 64;
const INTERNAL_DHT_TOKEN_TARGET_LIMIT: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DhtBackendKind {
    #[default]
    Disabled,
    Mainline,
    InternalPrototype,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhtServiceConfig {
    pub port: u16,
    pub bootstrap_nodes: Vec<String>,
    pub preferred_backend: DhtBackendKind,
    #[cfg(test)]
    pub force_internal_failure: bool,
}

impl DhtServiceConfig {
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            port: settings.client_port,
            bootstrap_nodes: settings.bootstrap_nodes.clone(),
            preferred_backend: std::env::var("SUPERSEEDR_DHT_BACKEND")
                .ok()
                .as_deref()
                .and_then(DhtBackendKind::from_override)
                .unwrap_or(DhtBackendKind::InternalPrototype),
            #[cfg(test)]
            force_internal_failure: false,
        }
    }
}

impl DhtBackendKind {
    fn from_override(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "disabled" | "off" => Some(Self::Disabled),
            "mainline" | "compat" => Some(Self::Mainline),
            "internal" | "internal-prototype" | "builtin" => Some(Self::InternalPrototype),
            _ => None,
        }
    }
}

fn forced_internal_backend_error(config: &DhtServiceConfig) -> Option<String> {
    #[cfg(test)]
    if config.force_internal_failure {
        return Some("forced internal backend failure".to_string());
    }

    let _ = config;
    None
}

fn internal_probe_enabled() -> bool {
    std::env::var("SUPERSEEDR_INTERNAL_PROBE")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn emit_internal_probe(message: String) {
    if !internal_probe_enabled() {
        return;
    }

    if let Ok(path) = std::env::var("SUPERSEEDR_INTERNAL_PROBE_PATH") {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = std::io::Write::write_all(&mut file, message.as_bytes());
            let _ = std::io::Write::write_all(&mut file, b"\n");
            return;
        }
    }

    eprintln!("internal_probe {message}");
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DhtHealthSnapshot {
    pub backend: DhtBackendKind,
    pub preferred_backend: Option<DhtBackendKind>,
    pub recovery_pending: bool,
    pub enabled: bool,
    pub local_addr: Option<SocketAddr>,
    pub ipv4_local_addr: Option<SocketAddr>,
    pub ipv6_local_addr: Option<SocketAddr>,
    pub bound_family_count: usize,
    pub cached_ipv4_routes: usize,
    pub cached_ipv6_routes: usize,
    pub active_ipv4_routes: usize,
    pub active_ipv6_routes: usize,
    pub cached_ipv4_announce_tokens: usize,
    pub cached_ipv6_announce_tokens: usize,
    pub cached_lookup_results: usize,
    pub inflight_lookups: usize,
    pub inflight_ipv4_queries: usize,
    pub inflight_ipv6_queries: usize,
    pub public_addr: Option<SocketAddr>,
    pub firewalled: Option<bool>,
    pub server_mode: Option<bool>,
    pub exported_bootstrap_nodes: usize,
    pub dht_size_estimate: Option<DhtSizeEstimate>,
    pub ipv4_bootstrap_nodes: usize,
    pub ipv6_bootstrap_nodes: usize,
    pub responsive_ipv4_bootstrap_nodes: usize,
    pub responsive_ipv6_bootstrap_nodes: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DhtSizeEstimate {
    pub node_count: usize,
    pub std_dev: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DhtStatus {
    pub generation: u64,
    pub warning: Option<String>,
    pub health: DhtHealthSnapshot,
}

#[derive(Debug, Clone, Default)]
pub struct DhtLookupRun {
    pub batch_count: usize,
    pub total_peers: usize,
    pub unique_peers: usize,
    pub unique_ipv4_peers: usize,
    pub unique_ipv6_peers: usize,
    pub first_batch_ms: Option<u64>,
}

#[derive(Clone)]
struct DhtRuntimeState {
    generation: u64,
    client: Arc<dyn DhtBackendClient>,
}

impl std::fmt::Debug for DhtRuntimeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DhtRuntimeState")
            .field("generation", &self.generation)
            .field("backend", &self.client.backend_kind())
            .finish()
    }
}

trait DhtBackendClient: Send + Sync + 'static {
    fn backend_kind(&self) -> DhtBackendKind;
    fn get_peers(&self, info_hash: [u8; 20]) -> PeerBatchStream;
    fn health_snapshot(&self) -> HealthFuture;
    fn announce_peer(&self, info_hash: [u8; 20], port: Option<u16>) -> AnnounceFuture;
    fn maintenance_tick(&self) -> MaintenanceFuture;
    fn export_recovery_state(&self) -> RecoveryStateFuture {
        Box::pin(async { None })
    }
}

#[derive(Debug, Clone, Default)]
struct DisabledDhtClient;

impl DhtBackendClient for DisabledDhtClient {
    fn backend_kind(&self) -> DhtBackendKind {
        DhtBackendKind::Disabled
    }

    fn get_peers(&self, _info_hash: [u8; 20]) -> PeerBatchStream {
        Box::pin(empty())
    }

    fn health_snapshot(&self) -> HealthFuture {
        Box::pin(async move {
            DhtHealthSnapshot {
                backend: DhtBackendKind::Disabled,
                enabled: false,
                ..Default::default()
            }
        })
    }

    fn announce_peer(&self, _info_hash: [u8; 20], _port: Option<u16>) -> AnnounceFuture {
        Box::pin(async move { false })
    }

    fn maintenance_tick(&self) -> MaintenanceFuture {
        Box::pin(async {})
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub(crate) struct TestDhtRecorder {
    announce_requests: Arc<std::sync::Mutex<Vec<(Vec<u8>, Option<u16>)>>>,
}

#[cfg(test)]
impl TestDhtRecorder {
    pub(crate) fn recorded_announces(&self) -> Vec<(Vec<u8>, Option<u16>)> {
        self.announce_requests
            .lock()
            .expect("test dht recorder lock")
            .clone()
    }
}

#[cfg(test)]
impl DhtBackendClient for TestDhtRecorder {
    fn backend_kind(&self) -> DhtBackendKind {
        DhtBackendKind::InternalPrototype
    }

    fn get_peers(&self, _info_hash: [u8; 20]) -> PeerBatchStream {
        Box::pin(empty())
    }

    fn health_snapshot(&self) -> HealthFuture {
        Box::pin(async move {
            DhtHealthSnapshot {
                backend: DhtBackendKind::InternalPrototype,
                preferred_backend: Some(DhtBackendKind::InternalPrototype),
                enabled: true,
                ..Default::default()
            }
        })
    }

    fn announce_peer(&self, info_hash: [u8; 20], port: Option<u16>) -> AnnounceFuture {
        let recorder = self.clone();
        Box::pin(async move {
            recorder
                .announce_requests
                .lock()
                .expect("test dht recorder lock")
                .push((info_hash.to_vec(), port));
            true
        })
    }

    fn maintenance_tick(&self) -> MaintenanceFuture {
        Box::pin(async {})
    }
}

#[derive(Debug, Clone)]
struct InternalPrototypeClient {
    state: InternalPrototypeState,
    sockets: InternalPrototypeSockets,
    node_id: [u8; 20],
    discovered_nodes: Arc<Mutex<InternalPrototypeDiscoveredNodes>>,
    active_routes: Arc<Mutex<InternalPrototypeActiveRoutes>>,
    announce_tokens: Arc<Mutex<InternalPrototypeAnnounceTokens>>,
    peer_lookup_cache: Arc<Mutex<InternalPrototypePeerLookupCache>>,
    bootstrap_probe: Arc<Mutex<InternalBootstrapProbeResult>>,
}

impl InternalPrototypeClient {
    #[cfg(test)]
    async fn bind(port: u16, bootstrap_nodes: &[String]) -> Result<(Self, Option<String>), String> {
        Self::bind_with_recovery(port, bootstrap_nodes, None).await
    }

    async fn bind_with_recovery(
        port: u16,
        bootstrap_nodes: &[String],
        recovery_state: Option<InternalPrototypeRecoveryState>,
    ) -> Result<(Self, Option<String>), String> {
        let mut state = resolve_bootstrap_nodes(bootstrap_nodes).await;
        let (sockets, warning) = InternalPrototypeSockets::bind(port).await?;
        state.ipv4_local_addr = sockets.ipv4_local_addr();
        state.ipv6_local_addr = sockets.ipv6_local_addr();
        let recovery_state = recovery_state.unwrap_or_default();

        let client = Self {
            state,
            sockets,
            node_id: recovery_state.node_id.unwrap_or_else(random),
            discovered_nodes: Arc::new(Mutex::new(recovery_state.discovered_nodes)),
            active_routes: Arc::new(Mutex::new(recovery_state.active_routes)),
            announce_tokens: Arc::new(Mutex::new(recovery_state.announce_tokens)),
            peer_lookup_cache: Arc::new(Mutex::new(InternalPrototypePeerLookupCache::default())),
            bootstrap_probe: Arc::new(Mutex::new(InternalBootstrapProbeResult::default())),
        };
        client
            .active_routes
            .lock()
            .await
            .set_ipv4_local_node_id(client.node_id);
        client.warm_routes().await;
        client.refresh_bootstrap_probe().await;

        Ok((client, warning))
    }

    #[cfg(test)]
    async fn query_get_peers(&self, info_hash: [u8; 20]) -> Vec<SocketAddr> {
        self.query_get_peers_with_batches(info_hash, false, "lookup")
            .await
    }

    async fn query_get_peers_with_batches(
        &self,
        info_hash: [u8; 20],
        shared_lookup: bool,
        purpose: &'static str,
    ) -> Vec<SocketAddr> {
        let (ipv4_peers, ipv6_peers) = tokio::join!(
            self.query_family_get_peers(
                self.sockets.ipv4.as_ref(),
                &self.state.ipv4_bootstrap_nodes,
                info_hash,
                false,
                shared_lookup,
                purpose,
            ),
            self.query_family_get_peers(
                self.sockets.ipv6.as_ref(),
                &self.state.ipv6_bootstrap_nodes,
                info_hash,
                true,
                shared_lookup,
                purpose,
            ),
        );

        let mut peers = ipv4_peers;
        peers.extend(ipv6_peers);

        let mut peers = peers.into_iter().collect::<Vec<_>>();
        peers.sort_unstable_by_key(|addr| addr.to_string());
        peers
    }

    async fn query_family_get_peers(
        &self,
        socket: Option<&InternalPrototypeFamilySocket>,
        bootstrap_nodes: &HashSet<SocketAddr>,
        info_hash: [u8; 20],
        is_ipv6: bool,
        shared_lookup: bool,
        purpose: &'static str,
    ) -> HashSet<SocketAddr> {
        let Some(socket) = socket else {
            return HashSet::new();
        };

        let (active_routes_available, fast_frontier_available) = {
            let active_routes = self.active_routes.lock().await;
            (
                active_routes.family_count(is_ipv6),
                active_routes
                    .snapshot_fast_frontier_for_family(is_ipv6, Some(info_hash))
                    .len(),
            )
        };
        let cached_routes_available = self.discovered_nodes.lock().await.family_count(is_ipv6);
        let family_initial_query_fanout =
            initial_family_query_fanout(is_ipv6, purpose, fast_frontier_available);
        let mut pending = self
            .seed_family_nodes(bootstrap_nodes, is_ipv6, Some(info_hash))
            .await;
        let initial_cached_seed_addrs = pending
            .iter()
            .filter(|addr| !bootstrap_nodes.contains(addr))
            .copied()
            .collect::<HashSet<_>>();
        let bootstrap_seed_count = pending
            .iter()
            .filter(|addr| bootstrap_nodes.contains(addr))
            .count();
        let cached_seed_count = pending.len().saturating_sub(bootstrap_seed_count);
        let thin_cached_frontier = cached_seed_count < INTERNAL_DHT_SEED_BOOTSTRAP_RESERVE.max(1);
        let initial_wave_limit = if bootstrap_seed_count > 0
            && pending.len() <= family_initial_query_fanout
            && cached_seed_count > 0
            && !thin_cached_frontier
        {
            pending.len().saturating_sub(
                INTERNAL_DHT_SEED_BOOTSTRAP_RESERVE
                    .min(bootstrap_seed_count)
                    .max(1),
            )
        } else {
            pending.len().min(family_initial_query_fanout)
        };
        let family = if is_ipv6 { "ipv6" } else { "ipv4" };
        let mut metrics = InternalFamilyLookupMetrics::new(
            family,
            purpose,
            shared_lookup,
            bootstrap_nodes.len(),
            active_routes_available,
            fast_frontier_available,
            cached_routes_available,
            pending.len(),
            bootstrap_seed_count,
            cached_seed_count,
            initial_wave_limit,
        );
        let mut visited = HashSet::new();
        let mut peers = HashSet::new();
        let mut join_set = JoinSet::new();
        let lookup_started_at = StdInstant::now();

        if shared_lookup && !self.lookup_has_live_subscribers(info_hash).await {
            self.record_internal_family_lookup_summary(
                info_hash,
                &metrics,
                visited.len(),
                peers.len(),
                "subscriber_gone_before_start",
                lookup_started_at,
            );
            return peers;
        }
        let _ = self.spawn_family_get_peers_round(
            socket,
            info_hash,
            bootstrap_nodes,
            &initial_cached_seed_addrs,
            &mut pending,
            &mut visited,
            &mut join_set,
            &mut metrics,
            initial_wave_limit.min(INTERNAL_DHT_MAX_VISITS_PER_FAMILY),
            family_max_concurrent_queries(
                is_ipv6,
                purpose,
                fast_frontier_available,
                peers.is_empty(),
            ),
            peers.is_empty(),
        );
        if join_set.is_empty() {
            self.record_internal_family_lookup_summary(
                info_hash,
                &metrics,
                visited.len(),
                peers.len(),
                "frontier_exhausted",
                lookup_started_at,
            );
            return peers;
        }

        while !join_set.is_empty() || !pending.is_empty() {
            if shared_lookup && !self.lookup_has_live_subscribers(info_hash).await {
                join_set.abort_all();
                self.record_internal_family_lookup_summary(
                    info_hash,
                    &metrics,
                    visited.len(),
                    peers.len(),
                    "subscriber_gone",
                    lookup_started_at,
                );
                break;
            }

            if join_set.is_empty() {
                if self
                    .spawn_family_get_peers_round(
                        socket,
                        info_hash,
                        bootstrap_nodes,
                        &initial_cached_seed_addrs,
                        &mut pending,
                        &mut visited,
                        &mut join_set,
                        &mut metrics,
                        1,
                        family_max_concurrent_queries(
                            is_ipv6,
                            purpose,
                            fast_frontier_available,
                            peers.is_empty(),
                        ),
                        peers.is_empty(),
                    )
                    == 0
                {
                    break;
                }
                continue;
            }

            let pending_has_non_bootstrap =
                pending.iter().any(|addr| !bootstrap_nodes.contains(addr));
            let max_concurrent_queries = family_max_concurrent_queries(
                is_ipv6,
                purpose,
                fast_frontier_available,
                peers.is_empty(),
            );
            let can_hedge = peers.is_empty()
                && !pending.is_empty()
                && pending_has_non_bootstrap
                && join_set.len() < max_concurrent_queries
                && visited.len() < INTERNAL_DHT_MAX_VISITS_PER_FAMILY;

            let maybe_result = if can_hedge {
                tokio::select! {
                    biased;
                    join_result = join_set.join_next() => join_result,
                    _ = tokio::time::sleep(INTERNAL_DHT_DISCOVERY_HEDGE_DELAY) => {
                        let _ = self.spawn_family_get_peers_round(
                            socket,
                            info_hash,
                            bootstrap_nodes,
                            &initial_cached_seed_addrs,
                            &mut pending,
                            &mut visited,
                            &mut join_set,
                            &mut metrics,
                            1,
                            max_concurrent_queries,
                            peers.is_empty(),
                        );
                        continue;
                    }
                }
            } else {
                join_set.join_next().await
            };

            let Some(join_result) = maybe_result else {
                continue;
            };
            let Ok((node_addr, source, response)) = join_result else {
                continue;
            };
            if self
                .handle_family_get_peers_response(
                    node_addr,
                    source,
                    response,
                    info_hash,
                    is_ipv6,
                    shared_lookup,
                    &mut pending,
                    &visited,
                    &mut peers,
                    &mut metrics,
                )
                .await
            {
                join_set.abort_all();
                self.record_internal_family_lookup_summary(
                    info_hash,
                    &metrics,
                    visited.len(),
                    peers.len(),
                    "peer_limit_reached",
                    lookup_started_at,
                );
                return peers;
            }

            if peers.is_empty() {
                let _ = self.spawn_family_get_peers_round(
                    socket,
                    info_hash,
                    bootstrap_nodes,
                    &initial_cached_seed_addrs,
                    &mut pending,
                    &mut visited,
                    &mut join_set,
                    &mut metrics,
                    1,
                    family_max_concurrent_queries(
                        is_ipv6,
                        purpose,
                        fast_frontier_available,
                        peers.is_empty(),
                    ),
                    peers.is_empty(),
                );
            } else {
                let _ = self.spawn_family_get_peers_round(
                    socket,
                    info_hash,
                    bootstrap_nodes,
                    &initial_cached_seed_addrs,
                    &mut pending,
                    &mut visited,
                    &mut join_set,
                    &mut metrics,
                    family_max_concurrent_queries(
                        is_ipv6,
                        purpose,
                        fast_frontier_available,
                        peers.is_empty(),
                    ),
                    family_max_concurrent_queries(
                        is_ipv6,
                        purpose,
                        fast_frontier_available,
                        peers.is_empty(),
                    ),
                    peers.is_empty(),
                );
            }
        }

        let ended_reason = if visited.len() >= INTERNAL_DHT_MAX_VISITS_PER_FAMILY {
            "visit_cap_reached"
        } else if pending.is_empty() && join_set.is_empty() {
            "frontier_exhausted"
        } else {
            "stopped"
        };
        self.record_internal_family_lookup_summary(
            info_hash,
            &metrics,
            visited.len(),
            peers.len(),
            ended_reason,
            lookup_started_at,
        );
        peers
    }

    fn spawn_family_get_peers_query(
        &self,
        socket: &InternalPrototypeFamilySocket,
        info_hash: [u8; 20],
        bootstrap_nodes: &HashSet<SocketAddr>,
        initial_cached_seed_addrs: &HashSet<SocketAddr>,
        pending: &mut VecDeque<SocketAddr>,
        visited: &mut HashSet<SocketAddr>,
        join_set: &mut JoinSet<(SocketAddr, InternalQuerySource, Option<KrpcResponseBody>)>,
        metrics: &mut InternalFamilyLookupMetrics,
        max_concurrent_queries: usize,
    ) -> Option<InternalQuerySource> {
        if visited.len() >= INTERNAL_DHT_MAX_VISITS_PER_FAMILY
            || join_set.len() >= max_concurrent_queries
        {
            return None;
        }

        while let Some(node_addr) = pending.pop_front() {
            if !visited.insert(node_addr) {
                continue;
            }
            let source = if bootstrap_nodes.contains(&node_addr) {
                InternalQuerySource::Bootstrap
            } else if initial_cached_seed_addrs.contains(&node_addr) {
                InternalQuerySource::Seed
            } else {
                InternalQuerySource::Discovered
            };
            metrics.queries_spawned += 1;
            match source {
                InternalQuerySource::Bootstrap => metrics.bootstrap_queries_spawned += 1,
                InternalQuerySource::Seed => metrics.cached_queries_spawned += 1,
                InternalQuerySource::Discovered => metrics.discovered_queries_spawned += 1,
            }
            metrics.max_pending = metrics.max_pending.max(pending.len());
            metrics.max_inflight = metrics.max_inflight.max(join_set.len() + 1);
            let family_socket = socket.clone();
            let node_id = self.node_id;
            join_set.spawn(async move {
                let response = family_socket
                    .get_peers(node_addr, &node_id, &info_hash)
                    .await;
                (node_addr, source, response)
            });
            return Some(source);
        }

        None
    }

    fn spawn_family_get_peers_round(
        &self,
        socket: &InternalPrototypeFamilySocket,
        info_hash: [u8; 20],
        bootstrap_nodes: &HashSet<SocketAddr>,
        initial_cached_seed_addrs: &HashSet<SocketAddr>,
        pending: &mut VecDeque<SocketAddr>,
        visited: &mut HashSet<SocketAddr>,
        join_set: &mut JoinSet<(SocketAddr, InternalQuerySource, Option<KrpcResponseBody>)>,
        metrics: &mut InternalFamilyLookupMetrics,
        spawn_limit: usize,
        max_concurrent_queries: usize,
        before_first_batch: bool,
    ) -> usize {
        if spawn_limit == 0 {
            return 0;
        }

        let pending_before = pending.len();
        let visited_before = visited.len();
        let inflight_before = join_set.len();
        let mut round_bootstrap = 0;
        let mut round_seed = 0;
        let mut round_discovered = 0;
        let mut spawned = 0;

        while spawned < spawn_limit {
            let Some(source) = self.spawn_family_get_peers_query(
                socket,
                info_hash,
                bootstrap_nodes,
                initial_cached_seed_addrs,
                pending,
                visited,
                join_set,
                metrics,
                max_concurrent_queries,
            ) else {
                break;
            };

            spawned += 1;
            match source {
                InternalQuerySource::Bootstrap => round_bootstrap += 1,
                InternalQuerySource::Seed => round_seed += 1,
                InternalQuerySource::Discovered => round_discovered += 1,
            }
        }

        if spawned > 0 {
            metrics.visit_rounds += 1;
            metrics.max_round_batch = metrics.max_round_batch.max(spawned);
            emit_internal_probe(format!(
                "event=query_visit_round family={} purpose={} target={:?} round={} batch_size={} bootstrap={} seed={} discovered={} pending_before={} visited_before={} inflight_before={} before_first_batch={}",
                metrics.family,
                metrics.purpose,
                info_hash,
                metrics.visit_rounds,
                spawned,
                round_bootstrap,
                round_seed,
                round_discovered,
                pending_before,
                visited_before,
                inflight_before,
                before_first_batch,
            ));
        }

        spawned
    }

    async fn handle_family_get_peers_response(
        &self,
        node_addr: SocketAddr,
        source: InternalQuerySource,
        response: Option<KrpcResponseBody>,
        info_hash: [u8; 20],
        is_ipv6: bool,
        shared_lookup: bool,
        pending: &mut VecDeque<SocketAddr>,
        visited: &HashSet<SocketAddr>,
        peers: &mut HashSet<SocketAddr>,
        metrics: &mut InternalFamilyLookupMetrics,
    ) -> bool {
        let Some(response) = response else {
            metrics.query_failures += 1;
            self.record_lookup_failure(node_addr).await;
            return false;
        };
        metrics.query_successes += 1;
        self.record_lookup_success(node_addr, response.node_id())
            .await;
        self.record_announce_token(node_addr, info_hash, response.token.as_ref())
            .await;

        if shared_lookup && !self.lookup_has_live_subscribers(info_hash).await {
            return true;
        }

        let before_first_batch = peers.is_empty();
        let mut new_batch = Vec::new();
        for compact_peer in response.values {
            let decoded_peers = decode_compact_peers(compact_peer.as_ref(), is_ipv6);
            metrics.peer_values_seen += decoded_peers.len();
            if before_first_batch {
                metrics.peer_values_before_first_batch += decoded_peers.len();
            } else {
                metrics.peer_values_after_first_batch += decoded_peers.len();
            }
            for peer_addr in decoded_peers {
                if peers.insert(peer_addr) {
                    new_batch.push(peer_addr);
                } else {
                    metrics.duplicate_peers_filtered += 1;
                    if before_first_batch {
                        metrics.duplicate_peers_before_first_batch += 1;
                    } else {
                        metrics.duplicate_peers_after_first_batch += 1;
                    }
                }
                if peers.len() >= INTERNAL_DHT_MAX_RETURNED_PEERS {
                    if !new_batch.is_empty() {
                        metrics.responses_with_peers += 1;
                        if before_first_batch {
                            metrics.responses_with_peers_before_first_batch += 1;
                            metrics.unique_peers_before_first_batch += new_batch.len();
                        } else {
                            metrics.responses_with_peers_after_first_batch += 1;
                            metrics.unique_peers_after_first_batch += new_batch.len();
                        }
                    }
                    new_batch.sort_unstable_by_key(|addr| addr.to_string());
                    metrics.batches_published += usize::from(!new_batch.is_empty());
                    metrics.peers_published += new_batch.len();
                    self.publish_peer_lookup_batch(info_hash, new_batch).await;
                    return true;
                }
            }
        }
        if !new_batch.is_empty() {
            if metrics.first_value_source == "none" {
                metrics.first_value_source = source.label();
                emit_internal_probe(format!(
                    "event=query_first_value family={} purpose={} target={:?} source={} from={} visited={} peers_before={} pending={} responses_before={}",
                    metrics.family,
                    metrics.purpose,
                    info_hash,
                    source.label(),
                    node_addr,
                    visited.len(),
                    peers.len().saturating_sub(new_batch.len()),
                    pending.len(),
                    metrics.responses_with_peers,
                ));
            }
            metrics.responses_with_peers += 1;
            if before_first_batch {
                metrics.responses_with_peers_before_first_batch += 1;
                metrics.unique_peers_before_first_batch += new_batch.len();
            } else {
                metrics.responses_with_peers_after_first_batch += 1;
                metrics.unique_peers_after_first_batch += new_batch.len();
            }
        }
        new_batch.sort_unstable_by_key(|addr| addr.to_string());
        metrics.batches_published += usize::from(!new_batch.is_empty());
        metrics.peers_published += new_batch.len();
        self.publish_peer_lookup_batch(info_hash, new_batch).await;

        let mut next_nodes = if is_ipv6 {
            decode_compact_nodes(response.nodes6.as_ref(), true)
        } else {
            decode_compact_nodes(response.nodes.as_ref(), false)
        };
        let discovered_node_count = next_nodes.len();
        next_nodes.sort_by(|left, right| compare_compact_node_distance(left, right, &info_hash));
        metrics.nodes_discovered += discovered_node_count;
        self.record_discovered_nodes(&next_nodes).await;

        let mut accepted_nodes = Vec::new();
        for next_node in next_nodes {
            if visited.contains(&next_node.addr) || pending.contains(&next_node.addr) {
                continue;
            }
            accepted_nodes.push(next_node.addr);
            if pending.len() + visited.len() + accepted_nodes.len()
                >= INTERNAL_DHT_MAX_VISITS_PER_FAMILY
            {
                break;
            }
        }
        metrics.nodes_accepted += accepted_nodes.len();
        metrics.nodes_rejected += discovered_node_count.saturating_sub(accepted_nodes.len());
        for next_addr in accepted_nodes.into_iter().rev() {
            pending.push_front(next_addr);
        }
        metrics.max_pending = metrics.max_pending.max(pending.len());

        false
    }

    async fn seed_family_nodes(
        &self,
        bootstrap_nodes: &HashSet<SocketAddr>,
        is_ipv6: bool,
        target: Option<[u8; 20]>,
    ) -> VecDeque<SocketAddr> {
        let (frontier_nodes, active_nodes, active_probe_summary) = {
            let active_routes = self.active_routes.lock().await;
            (
                active_routes.snapshot_fast_frontier_for_family(is_ipv6, target),
                active_routes.snapshot_for_family(is_ipv6, target),
                active_routes.family_probe_summary(is_ipv6),
            )
        };
        let fast_frontier_count = frontier_nodes.len();
        let warm_mature_ipv4_lookup = !is_ipv6
            && target.is_some()
            && fast_frontier_count >= INTERNAL_DHT_FAST_ACTIVE_FRONTIER_READY_FLOOR;
        let frontier_nodes = prioritize_non_bootstrap_nodes(frontier_nodes, bootstrap_nodes);
        let active_nodes = prioritize_non_bootstrap_nodes(active_nodes, bootstrap_nodes);
        let cached_nodes = self
            .discovered_nodes
            .lock()
            .await
            .snapshot_for_family(is_ipv6, target);
        let cached_nodes = prioritize_non_bootstrap_nodes(cached_nodes, bootstrap_nodes);
        let active_seed_candidate_count = active_nodes.len();
        let cached_seed_candidate_count = cached_nodes.len();
        let frontier_node_addrs = frontier_nodes.iter().copied().collect::<HashSet<_>>();
        let active_node_addrs = active_nodes.iter().copied().collect::<HashSet<_>>();
        let mut route_nodes = frontier_nodes
            .into_iter()
            .chain(
                active_nodes
                    .into_iter()
                    .filter(|addr| !frontier_node_addrs.contains(addr)),
            )
            .collect::<Vec<_>>();
        if !warm_mature_ipv4_lookup {
            route_nodes.extend(cached_nodes.into_iter().filter(|addr| {
                !frontier_node_addrs.contains(addr) && !active_node_addrs.contains(addr)
            }));
        }
        let bootstrap_reserve = usize::from(!bootstrap_nodes.is_empty())
            * INTERNAL_DHT_SEED_BOOTSTRAP_RESERVE.min(INTERNAL_DHT_SEED_NODE_LIMIT);
        let cached_limit = if route_nodes.is_empty() {
            0
        } else {
            INTERNAL_DHT_SEED_NODE_LIMIT.saturating_sub(bootstrap_reserve)
        };
        let ordered_bootstrap_nodes = self.ordered_bootstrap_nodes(bootstrap_nodes, is_ipv6).await;
        if internal_probe_enabled() && target.is_some() {
            emit_internal_probe(format!(
                "event=query_seed_state family={} active_total={} active_with_node_id={} active_positive_routes={} active_fast_eligible={} active_lookup_proven={} active_lookup_fast_eligible={} fast_frontier_available={} warm_mature_ipv4_lookup={} active_seed_candidates={} cached_seed_candidates={} bootstrap_candidates={}",
                if is_ipv6 { "ipv6" } else { "ipv4" },
                active_probe_summary.total,
                active_probe_summary.with_node_id,
                active_probe_summary.positive_routes,
                active_probe_summary.fast_eligible,
                active_probe_summary.lookup_proven,
                active_probe_summary.lookup_fast_eligible,
                fast_frontier_count,
                warm_mature_ipv4_lookup,
                active_seed_candidate_count,
                cached_seed_candidate_count,
                ordered_bootstrap_nodes.len(),
            ));
        }
        let bootstrap_front_limit =
            if !route_nodes.is_empty() && fast_frontier_count < INTERNAL_DHT_INITIAL_QUERY_FANOUT {
                ordered_bootstrap_nodes
                    .len()
                    .min(bootstrap_reserve)
                    .min(INTERNAL_DHT_INITIAL_QUERY_FANOUT.saturating_sub(fast_frontier_count))
            } else {
                0
            };
        let front_cached_limit = cached_limit
            .min(INTERNAL_DHT_INITIAL_QUERY_FANOUT.saturating_sub(bootstrap_front_limit));
        let mut pending = VecDeque::new();
        let mut push_unique = |addr: SocketAddr| {
            if pending.len() < INTERNAL_DHT_SEED_NODE_LIMIT && !pending.contains(&addr) {
                pending.push_back(addr);
            }
        };

        let mut cached_iter = route_nodes.into_iter();
        for cached_node in cached_iter.by_ref().take(front_cached_limit) {
            push_unique(cached_node);
        }
        for bootstrap_node in ordered_bootstrap_nodes
            .iter()
            .copied()
            .take(bootstrap_front_limit)
        {
            push_unique(bootstrap_node);
        }
        for cached_node in cached_iter
            .by_ref()
            .take(cached_limit.saturating_sub(front_cached_limit))
        {
            push_unique(cached_node);
        }
        for bootstrap_node in ordered_bootstrap_nodes
            .iter()
            .copied()
            .skip(bootstrap_front_limit)
        {
            push_unique(bootstrap_node);
        }
        for cached_node in cached_iter {
            push_unique(cached_node);
        }
        if pending.is_empty() {
            for bootstrap_node in ordered_bootstrap_nodes {
                pending.push_back(bootstrap_node);
            }
        }
        pending
    }

    async fn ordered_bootstrap_nodes(
        &self,
        bootstrap_nodes: &HashSet<SocketAddr>,
        is_ipv6: bool,
    ) -> Vec<SocketAddr> {
        let responsive = self.cached_bootstrap_probe().await;
        let responsive_family = if is_ipv6 {
            responsive.ipv6
        } else {
            responsive.ipv4
        };

        let mut ordered = bootstrap_nodes.iter().copied().collect::<Vec<_>>();
        ordered.sort_unstable_by_key(|addr| (!responsive_family.contains(addr), addr.to_string()));
        ordered
    }

    fn record_internal_family_lookup_summary(
        &self,
        info_hash: [u8; 20],
        metrics: &InternalFamilyLookupMetrics,
        visited: usize,
        peers: usize,
        ended_reason: &'static str,
        lookup_started_at: StdInstant,
    ) {
        network_metrics::record(
            "dht_internal_family_summary",
            Some(&info_hash),
            None,
            None,
            serde_json::json!({
                "family": metrics.family,
                "purpose": metrics.purpose,
                "shared_lookup": metrics.shared_lookup,
                "bootstrap_nodes": metrics.bootstrap_nodes,
                "active_routes_available": metrics.active_routes_available,
                "fast_frontier_available": metrics.fast_frontier_available,
                "cached_routes_available": metrics.cached_routes_available,
                "seeded_total": metrics.seeded_total,
                "seeded_bootstrap": metrics.seeded_bootstrap,
                "seeded_cached": metrics.seeded_cached,
                "initial_wave_limit": metrics.initial_wave_limit,
                "queries_spawned": metrics.queries_spawned,
                "bootstrap_queries_spawned": metrics.bootstrap_queries_spawned,
                "cached_queries_spawned": metrics.cached_queries_spawned,
                "discovered_queries_spawned": metrics.discovered_queries_spawned,
                "query_successes": metrics.query_successes,
                "query_failures": metrics.query_failures,
                "responses_with_peers": metrics.responses_with_peers,
                "responses_with_peers_before_first_batch": metrics.responses_with_peers_before_first_batch,
                "responses_with_peers_after_first_batch": metrics.responses_with_peers_after_first_batch,
                "peer_values_seen": metrics.peer_values_seen,
                "peer_values_before_first_batch": metrics.peer_values_before_first_batch,
                "peer_values_after_first_batch": metrics.peer_values_after_first_batch,
                "duplicate_peers_filtered": metrics.duplicate_peers_filtered,
                "duplicate_peers_before_first_batch": metrics.duplicate_peers_before_first_batch,
                "duplicate_peers_after_first_batch": metrics.duplicate_peers_after_first_batch,
                "unique_peers_before_first_batch": metrics.unique_peers_before_first_batch,
                "unique_peers_after_first_batch": metrics.unique_peers_after_first_batch,
                "nodes_discovered": metrics.nodes_discovered,
                "nodes_accepted": metrics.nodes_accepted,
                "nodes_rejected": metrics.nodes_rejected,
                "batches_published": metrics.batches_published,
                "peers_published": metrics.peers_published,
                "max_pending": metrics.max_pending,
                "max_inflight": metrics.max_inflight,
                "visited": visited,
                "peers": peers,
                "visit_cap_reached": visited >= INTERNAL_DHT_MAX_VISITS_PER_FAMILY,
                "peer_cap_reached": peers >= INTERNAL_DHT_MAX_RETURNED_PEERS,
                "ended_reason": ended_reason,
                "elapsed_ms": network_metrics::elapsed_ms(lookup_started_at),
            }),
        );
    }

    async fn record_discovered_nodes(&self, nodes: &[InternalCompactNode]) {
        let mut discovered_nodes = self.discovered_nodes.lock().await;
        discovered_nodes.insert_all(nodes.iter().copied());
    }

    async fn record_lookup_success(&self, addr: SocketAddr, node_id: Option<[u8; 20]>) {
        let mut discovered_nodes = self.discovered_nodes.lock().await;
        let resolved_node_id = node_id.or_else(|| discovered_nodes.node_id_for(addr));
        discovered_nodes.record_success(addr, resolved_node_id);
        drop(discovered_nodes);
        let mut active_routes = self.active_routes.lock().await;
        active_routes.record_lookup_success(addr, resolved_node_id);
    }

    async fn record_route_success(&self, addr: SocketAddr, node_id: Option<[u8; 20]>) {
        let mut discovered_nodes = self.discovered_nodes.lock().await;
        let resolved_node_id = node_id.or_else(|| discovered_nodes.node_id_for(addr));
        discovered_nodes.record_success(addr, resolved_node_id);
        drop(discovered_nodes);
        let mut active_routes = self.active_routes.lock().await;
        active_routes.record_success(addr, resolved_node_id);
    }

    async fn record_route_refresh_success(&self, addr: SocketAddr, node_id: Option<[u8; 20]>) {
        let mut discovered_nodes = self.discovered_nodes.lock().await;
        let resolved_node_id = node_id.or_else(|| discovered_nodes.node_id_for(addr));
        discovered_nodes.record_success(addr, resolved_node_id);
        drop(discovered_nodes);
        let mut active_routes = self.active_routes.lock().await;
        if active_routes.contains(addr)
            || active_routes.family_count(addr.is_ipv6()) < INTERNAL_DHT_ACTIVE_ROUTE_REFILL_FLOOR
        {
            active_routes.record_success(addr, resolved_node_id);
        }
    }

    async fn record_lookup_failure(&self, addr: SocketAddr) {
        let mut discovered_nodes = self.discovered_nodes.lock().await;
        discovered_nodes.record_failure(addr);
        drop(discovered_nodes);
        let mut active_routes = self.active_routes.lock().await;
        active_routes.record_soft_failure(addr);
    }

    async fn record_route_failure(&self, addr: SocketAddr) {
        let mut discovered_nodes = self.discovered_nodes.lock().await;
        discovered_nodes.record_failure(addr);
        drop(discovered_nodes);
        let mut active_routes = self.active_routes.lock().await;
        active_routes.record_failure(addr);
    }

    async fn record_announce_token(&self, addr: SocketAddr, info_hash: [u8; 20], token: &[u8]) {
        if token.is_empty() {
            return;
        }
        let mut announce_tokens = self.announce_tokens.lock().await;
        announce_tokens.insert(addr, info_hash, token.to_vec());
    }

    async fn register_peer_lookup(
        &self,
        info_hash: [u8; 20],
    ) -> InternalPrototypePeerLookupRegistration {
        let mut peer_lookup_cache = self.peer_lookup_cache.lock().await;
        peer_lookup_cache.register(info_hash)
    }

    async fn complete_peer_lookup(&self, info_hash: [u8; 20], peers: Vec<SocketAddr>) {
        let mut peer_lookup_cache = self.peer_lookup_cache.lock().await;
        peer_lookup_cache.complete(info_hash, peers);
    }

    async fn unregister_peer_lookup_subscriber(&self, info_hash: [u8; 20], subscriber_id: u64) {
        let mut peer_lookup_cache = self.peer_lookup_cache.lock().await;
        peer_lookup_cache.unregister(info_hash, subscriber_id);
    }

    async fn lookup_has_live_subscribers(&self, info_hash: [u8; 20]) -> bool {
        let mut peer_lookup_cache = self.peer_lookup_cache.lock().await;
        peer_lookup_cache.has_live_subscribers(info_hash)
    }

    async fn publish_peer_lookup_batch(&self, info_hash: [u8; 20], peers: Vec<SocketAddr>) {
        if peers.is_empty() {
            return;
        }

        let subscribers = {
            let mut peer_lookup_cache = self.peer_lookup_cache.lock().await;
            peer_lookup_cache.publish(info_hash, peers.clone())
        };

        for subscriber in subscribers {
            let _ = subscriber.send(peers.clone()).await;
        }
    }

    async fn announce_peer(&self, info_hash: [u8; 20], port: Option<u16>) -> bool {
        let (ipv4, ipv6) = tokio::join!(
            self.announce_family_peer(
                self.sockets.ipv4.as_ref(),
                &self.state.ipv4_bootstrap_nodes,
                info_hash,
                port,
                false,
            ),
            self.announce_family_peer(
                self.sockets.ipv6.as_ref(),
                &self.state.ipv6_bootstrap_nodes,
                info_hash,
                port,
                true,
            ),
        );

        ipv4 || ipv6
    }

    async fn announce_family_peer(
        &self,
        socket: Option<&InternalPrototypeFamilySocket>,
        bootstrap_nodes: &HashSet<SocketAddr>,
        info_hash: [u8; 20],
        port: Option<u16>,
        is_ipv6: bool,
    ) -> bool {
        let Some(socket) = socket else {
            return false;
        };

        if !self
            .announce_tokens
            .lock()
            .await
            .has_family_token(info_hash, is_ipv6)
        {
            let _ = self
                .query_family_get_peers(
                    socket.into(),
                    bootstrap_nodes,
                    info_hash,
                    is_ipv6,
                    false,
                    "announce_token_refresh",
                )
                .await;
        }

        let tokens = self
            .announce_tokens
            .lock()
            .await
            .snapshot_for_family(info_hash, is_ipv6);
        let mut announced = false;

        for token in tokens.into_iter().take(INTERNAL_DHT_TOKEN_TARGET_LIMIT) {
            if socket
                .announce_peer(
                    token.addr,
                    &self.node_id,
                    &info_hash,
                    token.token.as_slice(),
                    port,
                )
                .await
            {
                announced = true;
                self.record_route_success(token.addr, None).await;
                self.announce_tokens
                    .lock()
                    .await
                    .record_success(token.addr, info_hash);
            } else {
                self.record_route_failure(token.addr).await;
                self.announce_tokens
                    .lock()
                    .await
                    .record_failure(token.addr, info_hash);
            }
        }

        announced
    }

    async fn probe_bootstrap_nodes(&self) -> InternalBootstrapProbeResult {
        let (ipv4, ipv6) = tokio::join!(
            self.probe_family_bootstrap_nodes(
                self.sockets.ipv4.as_ref(),
                &self.state.ipv4_bootstrap_nodes,
            ),
            self.probe_family_bootstrap_nodes(
                self.sockets.ipv6.as_ref(),
                &self.state.ipv6_bootstrap_nodes,
            ),
        );

        InternalBootstrapProbeResult { ipv4, ipv6 }
    }

    async fn refresh_bootstrap_probe(&self) {
        let probe = self.probe_bootstrap_nodes().await;
        *self.bootstrap_probe.lock().await = probe;
    }

    async fn cached_bootstrap_probe(&self) -> InternalBootstrapProbeResult {
        self.bootstrap_probe.lock().await.clone()
    }

    async fn probe_family_bootstrap_nodes(
        &self,
        socket: Option<&InternalPrototypeFamilySocket>,
        bootstrap_nodes: &HashSet<SocketAddr>,
    ) -> HashSet<SocketAddr> {
        let Some(socket) = socket else {
            return HashSet::new();
        };

        let mut responsive = HashSet::new();
        for bootstrap_node in bootstrap_nodes
            .iter()
            .copied()
            .take(INTERNAL_DHT_HEALTH_PROBE_LIMIT)
        {
            if socket.ping(bootstrap_node, &self.node_id).await {
                responsive.insert(bootstrap_node);
            }
        }
        responsive
    }
}

impl DhtBackendClient for InternalPrototypeClient {
    fn backend_kind(&self) -> DhtBackendKind {
        DhtBackendKind::InternalPrototype
    }

    fn get_peers(&self, info_hash: [u8; 20]) -> PeerBatchStream {
        let (tx, rx) = mpsc::channel(INTERNAL_DHT_LOOKUP_STREAM_BUFFER);
        let client = self.clone();
        tokio::spawn(async move {
            match client.register_peer_lookup(info_hash).await {
                InternalPrototypePeerLookupRegistration::Cached(peers) => {
                    if !peers.is_empty() {
                        let _ = tx.send(peers).await;
                    }
                }
                InternalPrototypePeerLookupRegistration::Follow {
                    subscriber_id,
                    rx: mut shared_rx,
                } => {
                    if tx.is_closed() {
                        client
                            .unregister_peer_lookup_subscriber(info_hash, subscriber_id)
                            .await;
                        return;
                    }
                    loop {
                        tokio::select! {
                            _ = tx.closed() => {
                                client.unregister_peer_lookup_subscriber(info_hash, subscriber_id).await;
                                break;
                            }
                            maybe_peers = shared_rx.recv() => {
                                let Some(peers) = maybe_peers else {
                                    break;
                                };
                                if tx.send(peers).await.is_err() {
                                    client.unregister_peer_lookup_subscriber(info_hash, subscriber_id).await;
                                    break;
                                }
                            }
                        }
                    }
                }
                InternalPrototypePeerLookupRegistration::Start {
                    subscriber_id,
                    rx: mut shared_rx,
                } => {
                    if tx.is_closed() {
                        client
                            .unregister_peer_lookup_subscriber(info_hash, subscriber_id)
                            .await;
                        return;
                    }
                    let query_client = client.clone();
                    let query_task = tokio::spawn(async move {
                        let peers = query_client
                            .query_get_peers_with_batches(info_hash, true, "lookup")
                            .await;
                        query_client.complete_peer_lookup(info_hash, peers).await;
                    });

                    loop {
                        tokio::select! {
                            _ = tx.closed() => {
                                client.unregister_peer_lookup_subscriber(info_hash, subscriber_id).await;
                                break;
                            }
                            maybe_peers = shared_rx.recv() => {
                                let Some(peers) = maybe_peers else {
                                    break;
                                };
                                if tx.send(peers).await.is_err() {
                                    client.unregister_peer_lookup_subscriber(info_hash, subscriber_id).await;
                                    break;
                                }
                            }
                        }
                    }

                    let _ = query_task.await;
                }
            }
        });

        Box::pin(ReceiverStream::new(rx))
    }

    fn health_snapshot(&self) -> HealthFuture {
        let client = self.clone();
        Box::pin(async move {
            let responsive = client.cached_bootstrap_probe().await;
            let discovered_nodes = client.discovered_nodes.lock().await;
            let active_routes = client.active_routes.lock().await;
            let announce_tokens = client.announce_tokens.lock().await;
            let peer_lookup_cache = client.peer_lookup_cache.lock().await;
            let exported_bootstrap_nodes = discovered_nodes.total_count();
            DhtHealthSnapshot {
                backend: DhtBackendKind::InternalPrototype,
                enabled: true,
                local_addr: client
                    .state
                    .ipv4_local_addr
                    .or(client.state.ipv6_local_addr),
                ipv4_local_addr: client.state.ipv4_local_addr,
                ipv6_local_addr: client.state.ipv6_local_addr,
                bound_family_count: usize::from(client.state.ipv4_local_addr.is_some())
                    + usize::from(client.state.ipv6_local_addr.is_some()),
                cached_ipv4_routes: discovered_nodes.family_count(false),
                cached_ipv6_routes: discovered_nodes.family_count(true),
                active_ipv4_routes: active_routes.family_count(false),
                active_ipv6_routes: active_routes.family_count(true),
                cached_ipv4_announce_tokens: announce_tokens.family_count(false),
                cached_ipv6_announce_tokens: announce_tokens.family_count(true),
                cached_lookup_results: peer_lookup_cache.ready_count(),
                inflight_lookups: peer_lookup_cache.inflight_count(),
                inflight_ipv4_queries: client
                    .sockets
                    .ipv4
                    .as_ref()
                    .map_or(0, InternalPrototypeFamilySocket::inflight_query_count),
                inflight_ipv6_queries: client
                    .sockets
                    .ipv6
                    .as_ref()
                    .map_or(0, InternalPrototypeFamilySocket::inflight_query_count),
                server_mode: Some(true),
                exported_bootstrap_nodes,
                dht_size_estimate: Some(DhtSizeEstimate {
                    node_count: exported_bootstrap_nodes,
                    std_dev: None,
                }),
                ipv4_bootstrap_nodes: client.state.ipv4_bootstrap_nodes.len(),
                ipv6_bootstrap_nodes: client.state.ipv6_bootstrap_nodes.len(),
                responsive_ipv4_bootstrap_nodes: responsive.ipv4.len(),
                responsive_ipv6_bootstrap_nodes: responsive.ipv6.len(),
                ..Default::default()
            }
        })
    }

    fn announce_peer(&self, info_hash: [u8; 20], port: Option<u16>) -> AnnounceFuture {
        let client = self.clone();
        Box::pin(async move { client.announce_peer(info_hash, port).await })
    }

    fn maintenance_tick(&self) -> MaintenanceFuture {
        let client = self.clone();
        Box::pin(async move {
            client.maintenance_tick().await;
        })
    }

    fn export_recovery_state(&self) -> RecoveryStateFuture {
        let client = self.clone();
        Box::pin(async move {
            Some(InternalPrototypeRecoveryState {
                node_id: Some(client.node_id),
                discovered_nodes: client.discovered_nodes.lock().await.clone(),
                active_routes: client.active_routes.lock().await.clone(),
                announce_tokens: client.announce_tokens.lock().await.clone(),
            })
        })
    }
}

#[derive(Debug, Clone, Default)]
struct InternalPrototypeState {
    ipv4_bootstrap_nodes: HashSet<SocketAddr>,
    ipv6_bootstrap_nodes: HashSet<SocketAddr>,
    ipv4_local_addr: Option<SocketAddr>,
    ipv6_local_addr: Option<SocketAddr>,
}

impl InternalPrototypeState {
    fn from_bootstrap_nodes(nodes: &[String]) -> Self {
        let mut state = Self::default();

        for node in nodes {
            let Ok(addr) = node.parse::<SocketAddr>() else {
                continue;
            };
            if addr.is_ipv4() {
                state.ipv4_bootstrap_nodes.insert(addr);
            } else {
                state.ipv6_bootstrap_nodes.insert(addr);
            }
        }

        state
    }
}

#[derive(Debug, Clone, Default)]
struct InternalBootstrapProbeResult {
    ipv4: HashSet<SocketAddr>,
    ipv6: HashSet<SocketAddr>,
}

#[derive(Debug, Clone)]
struct InternalFamilyLookupMetrics {
    family: &'static str,
    purpose: &'static str,
    shared_lookup: bool,
    bootstrap_nodes: usize,
    active_routes_available: usize,
    fast_frontier_available: usize,
    cached_routes_available: usize,
    seeded_total: usize,
    seeded_bootstrap: usize,
    seeded_cached: usize,
    initial_wave_limit: usize,
    queries_spawned: usize,
    bootstrap_queries_spawned: usize,
    cached_queries_spawned: usize,
    discovered_queries_spawned: usize,
    query_successes: usize,
    query_failures: usize,
    responses_with_peers: usize,
    responses_with_peers_before_first_batch: usize,
    responses_with_peers_after_first_batch: usize,
    peer_values_seen: usize,
    peer_values_before_first_batch: usize,
    peer_values_after_first_batch: usize,
    duplicate_peers_filtered: usize,
    duplicate_peers_before_first_batch: usize,
    duplicate_peers_after_first_batch: usize,
    unique_peers_before_first_batch: usize,
    unique_peers_after_first_batch: usize,
    nodes_discovered: usize,
    nodes_accepted: usize,
    nodes_rejected: usize,
    batches_published: usize,
    peers_published: usize,
    max_pending: usize,
    max_inflight: usize,
    visit_rounds: usize,
    max_round_batch: usize,
    first_value_source: &'static str,
}

impl InternalFamilyLookupMetrics {
    fn new(
        family: &'static str,
        purpose: &'static str,
        shared_lookup: bool,
        bootstrap_nodes: usize,
        active_routes_available: usize,
        fast_frontier_available: usize,
        cached_routes_available: usize,
        seeded_total: usize,
        seeded_bootstrap: usize,
        seeded_cached: usize,
        initial_wave_limit: usize,
    ) -> Self {
        Self {
            family,
            purpose,
            shared_lookup,
            bootstrap_nodes,
            active_routes_available,
            fast_frontier_available,
            cached_routes_available,
            seeded_total,
            seeded_bootstrap,
            seeded_cached,
            initial_wave_limit,
            queries_spawned: 0,
            bootstrap_queries_spawned: 0,
            cached_queries_spawned: 0,
            discovered_queries_spawned: 0,
            query_successes: 0,
            query_failures: 0,
            responses_with_peers: 0,
            responses_with_peers_before_first_batch: 0,
            responses_with_peers_after_first_batch: 0,
            peer_values_seen: 0,
            peer_values_before_first_batch: 0,
            peer_values_after_first_batch: 0,
            duplicate_peers_filtered: 0,
            duplicate_peers_before_first_batch: 0,
            duplicate_peers_after_first_batch: 0,
            unique_peers_before_first_batch: 0,
            unique_peers_after_first_batch: 0,
            nodes_discovered: 0,
            nodes_accepted: 0,
            nodes_rejected: 0,
            batches_published: 0,
            peers_published: 0,
            max_pending: seeded_total,
            max_inflight: 0,
            visit_rounds: 0,
            max_round_batch: 0,
            first_value_source: "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InternalQuerySource {
    Bootstrap,
    Seed,
    Discovered,
}

impl InternalQuerySource {
    fn label(self) -> &'static str {
        match self {
            Self::Bootstrap => "bootstrap",
            Self::Seed => "seed",
            Self::Discovered => "discovered",
        }
    }
}

#[derive(Debug, Clone, Default)]
struct InternalPrototypeRecoveryState {
    node_id: Option<[u8; 20]>,
    discovered_nodes: InternalPrototypeDiscoveredNodes,
    active_routes: InternalPrototypeActiveRoutes,
    announce_tokens: InternalPrototypeAnnounceTokens,
}

#[derive(Debug, Clone, Default)]
struct InternalPrototypeDiscoveredNodes {
    ipv4: VecDeque<InternalPrototypeNodeRecord>,
    ipv6: VecDeque<InternalPrototypeNodeRecord>,
}

#[derive(Debug, Clone, Default)]
struct InternalPrototypeActiveRoutes {
    ipv4: InternalPrototypeIpv4RouteTable,
    ipv6: VecDeque<InternalPrototypeNodeRecord>,
}

#[derive(Debug, Clone, Default)]
struct InternalPrototypeIpv4RouteTable {
    local_node_id: Option<[u8; 20]>,
    buckets: BTreeMap<u8, VecDeque<InternalPrototypeNodeRecord>>,
    overflow: VecDeque<InternalPrototypeNodeRecord>,
}


#[derive(Debug, Clone, Default)]
struct InternalPrototypeAnnounceTokens {
    ipv4: VecDeque<InternalAnnounceTokenRecord>,
    ipv6: VecDeque<InternalAnnounceTokenRecord>,
}

#[derive(Default)]
struct InternalPrototypePeerLookupCache {
    entries: HashMap<[u8; 20], InternalPrototypePeerLookupEntry>,
    next_subscriber_id: u64,
}

impl std::fmt::Debug for InternalPrototypePeerLookupCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InternalPrototypePeerLookupCache")
            .field("ready_count", &self.ready_count())
            .field("inflight_count", &self.inflight_count())
            .finish()
    }
}

enum InternalPrototypePeerLookupEntry {
    Ready {
        peers: Vec<SocketAddr>,
        refreshed_at: StdInstant,
    },
    InFlight {
        streamed_batches: Vec<Vec<SocketAddr>>,
        subscribers: Vec<InternalPrototypePeerLookupSubscriber>,
    },
}

enum InternalPrototypePeerLookupRegistration {
    Start {
        subscriber_id: u64,
        rx: mpsc::Receiver<Vec<SocketAddr>>,
    },
    Cached(Vec<SocketAddr>),
    Follow {
        subscriber_id: u64,
        rx: mpsc::Receiver<Vec<SocketAddr>>,
    },
}

#[derive(Clone)]
struct InternalPrototypePeerLookupSubscriber {
    id: u64,
    tx: Sender<Vec<SocketAddr>>,
}

impl InternalPrototypeAnnounceTokens {
    fn insert(&mut self, addr: SocketAddr, info_hash: [u8; 20], token: Vec<u8>) {
        let tokens = if addr.is_ipv6() {
            &mut self.ipv6
        } else {
            &mut self.ipv4
        };
        tokens.retain(|existing| !(existing.addr == addr && existing.info_hash == info_hash));
        tokens.push_back(InternalAnnounceTokenRecord {
            addr,
            info_hash,
            token,
            success_count: 0,
            failure_count: 0,
            recency_epoch: tokens
                .back()
                .map_or(0, |last| last.recency_epoch.saturating_add(1)),
        });
        while tokens.len() > INTERNAL_DHT_TOKEN_CACHE_LIMIT {
            tokens.pop_front();
        }
    }

    fn has_family_token(&self, info_hash: [u8; 20], is_ipv6: bool) -> bool {
        self.snapshot_for_family(info_hash, is_ipv6)
            .first()
            .is_some()
    }

    fn snapshot_for_family(
        &self,
        info_hash: [u8; 20],
        is_ipv6: bool,
    ) -> Vec<InternalAnnounceTokenRecord> {
        let mut tokens = if is_ipv6 {
            self.ipv6
                .iter()
                .filter(|existing| existing.info_hash == info_hash)
                .cloned()
                .collect::<Vec<_>>()
        } else {
            self.ipv4
                .iter()
                .filter(|existing| existing.info_hash == info_hash)
                .cloned()
                .collect::<Vec<_>>()
        };

        tokens.sort_by(|left, right| {
            left.failure_count
                .cmp(&right.failure_count)
                .then_with(|| right.success_count.cmp(&left.success_count))
                .then_with(|| right.recency_epoch.cmp(&left.recency_epoch))
        });
        tokens
    }

    fn record_success(&mut self, addr: SocketAddr, info_hash: [u8; 20]) {
        if let Some(token) = self.get_mut(addr, info_hash) {
            token.success_count = token.success_count.saturating_add(1);
            token.failure_count = token.failure_count.saturating_sub(1);
            token.recency_epoch = token.recency_epoch.saturating_add(1);
        }
    }

    fn record_failure(&mut self, addr: SocketAddr, info_hash: [u8; 20]) {
        let tokens = if addr.is_ipv6() {
            &mut self.ipv6
        } else {
            &mut self.ipv4
        };

        if let Some(token) = tokens
            .iter_mut()
            .find(|existing| existing.addr == addr && existing.info_hash == info_hash)
        {
            token.failure_count = token.failure_count.saturating_add(1);
            token.recency_epoch = token.recency_epoch.saturating_add(1);
        }

        tokens.retain(|existing| {
            !(existing.addr == addr
                && existing.info_hash == info_hash
                && existing.failure_count >= INTERNAL_DHT_MAX_FAILURES_PER_NODE)
        });
    }

    fn get_mut(
        &mut self,
        addr: SocketAddr,
        info_hash: [u8; 20],
    ) -> Option<&mut InternalAnnounceTokenRecord> {
        let tokens = if addr.is_ipv6() {
            &mut self.ipv6
        } else {
            &mut self.ipv4
        };
        tokens
            .iter_mut()
            .find(|existing| existing.addr == addr && existing.info_hash == info_hash)
    }

    fn family_count(&self, is_ipv6: bool) -> usize {
        if is_ipv6 {
            self.ipv6.len()
        } else {
            self.ipv4.len()
        }
    }
}

impl InternalPrototypePeerLookupCache {
    fn register(&mut self, info_hash: [u8; 20]) -> InternalPrototypePeerLookupRegistration {
        self.prune_expired();
        let next_subscriber_id = self.allocate_subscriber_id();

        if let Some(entry) = self.entries.get_mut(&info_hash) {
            match entry {
                InternalPrototypePeerLookupEntry::Ready { peers, .. } => {
                    return InternalPrototypePeerLookupRegistration::Cached(peers.clone());
                }
                InternalPrototypePeerLookupEntry::InFlight {
                    streamed_batches,
                    subscribers,
                } => {
                    let (tx, rx) = mpsc::channel(INTERNAL_DHT_LOOKUP_STREAM_BUFFER);
                    for batch in streamed_batches.iter() {
                        let _ = tx.try_send(batch.clone());
                    }
                    subscribers.push(InternalPrototypePeerLookupSubscriber {
                        id: next_subscriber_id,
                        tx,
                    });
                    return InternalPrototypePeerLookupRegistration::Follow {
                        subscriber_id: next_subscriber_id,
                        rx,
                    };
                }
            }
        }

        let (tx, rx) = mpsc::channel(INTERNAL_DHT_LOOKUP_STREAM_BUFFER);
        self.entries.insert(
            info_hash,
            InternalPrototypePeerLookupEntry::InFlight {
                streamed_batches: Vec::new(),
                subscribers: vec![InternalPrototypePeerLookupSubscriber {
                    id: next_subscriber_id,
                    tx,
                }],
            },
        );
        InternalPrototypePeerLookupRegistration::Start {
            subscriber_id: next_subscriber_id,
            rx,
        }
    }

    fn complete(&mut self, info_hash: [u8; 20], peers: Vec<SocketAddr>) {
        let _ = match self.entries.insert(
            info_hash,
            InternalPrototypePeerLookupEntry::Ready {
                peers: peers.clone(),
                refreshed_at: StdInstant::now(),
            },
        ) {
            Some(InternalPrototypePeerLookupEntry::InFlight { subscribers, .. }) => subscribers,
            _ => Vec::new(),
        };
    }

    fn publish(
        &mut self,
        info_hash: [u8; 20],
        peers: Vec<SocketAddr>,
    ) -> Vec<Sender<Vec<SocketAddr>>> {
        let Some(InternalPrototypePeerLookupEntry::InFlight {
            streamed_batches,
            subscribers,
        }) = self.entries.get_mut(&info_hash)
        else {
            return Vec::new();
        };

        streamed_batches.push(peers);
        subscribers
            .iter()
            .map(|subscriber| subscriber.tx.clone())
            .collect()
    }

    fn unregister(&mut self, info_hash: [u8; 20], subscriber_id: u64) {
        let Some(InternalPrototypePeerLookupEntry::InFlight { subscribers, .. }) =
            self.entries.get_mut(&info_hash)
        else {
            return;
        };

        subscribers.retain(|subscriber| subscriber.id != subscriber_id);
    }

    fn has_live_subscribers(&mut self, info_hash: [u8; 20]) -> bool {
        let Some(entry) = self.entries.get_mut(&info_hash) else {
            return true;
        };

        let InternalPrototypePeerLookupEntry::InFlight { subscribers, .. } = entry else {
            return true;
        };

        subscribers.retain(|subscriber| !subscriber.tx.is_closed());
        !subscribers.is_empty()
    }

    fn allocate_subscriber_id(&mut self) -> u64 {
        let id = self.next_subscriber_id;
        self.next_subscriber_id = self.next_subscriber_id.saturating_add(1);
        id
    }

    fn prune_expired(&mut self) {
        let now = StdInstant::now();
        self.entries.retain(|_, entry| match entry {
            InternalPrototypePeerLookupEntry::Ready { refreshed_at, .. } => {
                now.duration_since(*refreshed_at) <= INTERNAL_DHT_LOOKUP_CACHE_TTL
            }
            InternalPrototypePeerLookupEntry::InFlight { subscribers, .. } => subscribers
                .iter()
                .any(|subscriber| !subscriber.tx.is_closed()),
        });
    }

    fn ready_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| matches!(entry, InternalPrototypePeerLookupEntry::Ready { .. }))
            .count()
    }

    fn inflight_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| {
                matches!(
                    entry,
                    InternalPrototypePeerLookupEntry::InFlight { subscribers, .. }
                    if subscribers.iter().any(|subscriber| !subscriber.tx.is_closed())
                )
            })
            .count()
    }
}

impl InternalPrototypeDiscoveredNodes {
    fn node_id_for(&self, addr: SocketAddr) -> Option<[u8; 20]> {
        let family_nodes = if addr.is_ipv6() {
            &self.ipv6
        } else {
            &self.ipv4
        };
        family_nodes
            .iter()
            .find(|existing| existing.addr == addr)
            .and_then(|existing| existing.node_id)
    }

    fn snapshot_for_family(&self, is_ipv6: bool, target: Option<[u8; 20]>) -> Vec<SocketAddr> {
        let mut nodes = if is_ipv6 {
            self.ipv6.iter().cloned().collect::<Vec<_>>()
        } else {
            self.ipv4.iter().cloned().collect::<Vec<_>>()
        };
        nodes.sort_by(|left, right| compare_node_records(left, right, target.as_ref()));
        nodes.into_iter().map(|record| record.addr).collect()
    }

    fn insert_all<I>(&mut self, addrs: I)
    where
        I: IntoIterator<Item = InternalCompactNode>,
    {
        for addr in addrs {
            self.insert(addr);
        }
    }

    fn insert(&mut self, node: InternalCompactNode) {
        let family_nodes = if node.addr.is_ipv6() {
            &mut self.ipv6
        } else {
            &mut self.ipv4
        };

        let mut record = family_nodes
            .iter()
            .find(|existing| existing.addr == node.addr)
            .cloned()
            .unwrap_or_else(|| InternalPrototypeNodeRecord::new(node.addr));
        record.node_id = Some(node.id);
        record.bump_recency();

        family_nodes.retain(|existing| existing.addr != node.addr);
        family_nodes.push_back(record);
        while family_nodes.len() > INTERNAL_DHT_DISCOVERED_NODE_LIMIT {
            family_nodes.pop_front();
        }
    }

    fn record_success(&mut self, addr: SocketAddr, node_id: Option<[u8; 20]>) {
        let record = self
            .get_or_insert_record(addr)
            .unwrap_or_else(|| unreachable!("record inserted"));
        record.success_count = record.success_count.saturating_add(1);
        record.failure_count = record.failure_count.saturating_sub(1);
        if let Some(node_id) = node_id {
            record.node_id = Some(node_id);
        }
        record.bump_recency();
    }

    fn record_failure(&mut self, addr: SocketAddr) {
        let record = self
            .get_or_insert_record(addr)
            .unwrap_or_else(|| unreachable!("record inserted"));
        record.failure_count = record.failure_count.saturating_add(1);
        record.bump_recency();

        let family_nodes = if addr.is_ipv6() {
            &mut self.ipv6
        } else {
            &mut self.ipv4
        };
        family_nodes.retain(|existing| existing.failure_count < INTERNAL_DHT_MAX_FAILURES_PER_NODE);
    }

    fn get_or_insert_record(
        &mut self,
        addr: SocketAddr,
    ) -> Option<&mut InternalPrototypeNodeRecord> {
        let family_nodes = if addr.is_ipv6() {
            &mut self.ipv6
        } else {
            &mut self.ipv4
        };

        if !family_nodes.iter().any(|existing| existing.addr == addr) {
            family_nodes.push_back(InternalPrototypeNodeRecord::new(addr));
            while family_nodes.len() > INTERNAL_DHT_DISCOVERED_NODE_LIMIT {
                family_nodes.pop_front();
            }
        }

        family_nodes
            .iter_mut()
            .find(|existing| existing.addr == addr)
    }

    fn total_count(&self) -> usize {
        self.ipv4.len() + self.ipv6.len()
    }

    fn family_count(&self, is_ipv6: bool) -> usize {
        if is_ipv6 {
            self.ipv6.len()
        } else {
            self.ipv4.len()
        }
    }

}

impl InternalPrototypeIpv4RouteTable {
    fn set_local_node_id(&mut self, node_id: [u8; 20]) {
        self.local_node_id = Some(node_id);
        self.rebalance();
    }

    fn local_node_id(&self) -> Option<[u8; 20]> {
        self.local_node_id
    }

    fn snapshot_records(&self) -> Vec<InternalPrototypeNodeRecord> {
        let mut records = self.overflow.iter().cloned().collect::<Vec<_>>();
        for bucket in self.buckets.values() {
            records.extend(bucket.iter().cloned());
        }
        records
    }

    fn total_count(&self) -> usize {
        self.overflow.len()
            + self
                .buckets
                .values()
                .map(VecDeque::len)
                .sum::<usize>()
    }

    fn contains(&self, addr: SocketAddr) -> bool {
        self.overflow.iter().any(|record| record.addr == addr)
            || self
                .buckets
                .values()
                .any(|bucket| bucket.iter().any(|record| record.addr == addr))
    }

    fn rebalance(&mut self) {
        self.replace_records(self.snapshot_records());
    }

    fn replace_records(&mut self, records: Vec<InternalPrototypeNodeRecord>) {
        if self.local_node_id.is_none() {
            let mut ordered = records;
            ordered.sort_by(compare_active_route_retention_records);
            ordered.truncate(INTERNAL_DHT_IPV4_ACTIVE_ROUTE_LIMIT);
            self.buckets.clear();
            self.overflow = ordered.into();
            return;
        }

        let mut buckets = BTreeMap::<u8, Vec<InternalPrototypeNodeRecord>>::new();
        let mut overflow = Vec::new();

        for record in records {
            let bucket = self.local_node_id.and_then(|local_node_id| {
                (record.lookup_success_count > 0)
                    .then_some(())
                    .and(record
                        .node_id
                        .as_ref()
                        .and_then(|node_id| routing_bucket_key(&local_node_id, node_id)))
            });
            if let Some(bucket) = bucket {
                buckets.entry(bucket).or_default().push(record);
            } else {
                overflow.push(record);
            }
        }

        let mut bucketed = BTreeMap::<u8, VecDeque<InternalPrototypeNodeRecord>>::new();
        for (bucket, mut bucket_records) in buckets {
            bucket_records.sort_by(compare_active_route_retention_records);
            bucket_records.truncate(INTERNAL_DHT_IPV4_K_BUCKET_SIZE);
            if !bucket_records.is_empty() {
                bucketed.insert(bucket, bucket_records.into());
            }
        }

        overflow.sort_by(compare_active_route_retention_records);
        overflow.truncate(INTERNAL_DHT_IPV4_UNBUCKETED_ROUTE_LIMIT);
        let mut overflow = VecDeque::from(overflow);

        while bucketed.values().map(VecDeque::len).sum::<usize>() + overflow.len()
            > INTERNAL_DHT_IPV4_ACTIVE_ROUTE_LIMIT
        {
            if overflow.pop_back().is_some() {
                continue;
            }
            if !trim_ipv4_bucketed_routes(&mut bucketed) {
                break;
            }
        }

        self.buckets = bucketed;
        self.overflow = overflow;
    }
}

impl InternalPrototypeActiveRoutes {
    fn set_ipv4_local_node_id(&mut self, node_id: [u8; 20]) {
        self.ipv4.set_local_node_id(node_id);
    }

    fn family_probe_summary(&self, is_ipv6: bool) -> InternalActiveRouteProbeSummary {
        let family_nodes = if is_ipv6 {
            self.ipv6.iter().cloned().collect::<Vec<_>>()
        } else {
            self.ipv4.snapshot_records()
        };
        family_nodes.iter().fold(
            InternalActiveRouteProbeSummary::default(),
            |mut summary, record| {
                summary.total += 1;
                if record.node_id.is_some() {
                    summary.with_node_id += 1;
                }
                if record.success_count > record.failure_count {
                    summary.positive_routes += 1;
                }
                if record.node_id.is_some() && record.success_count > record.failure_count {
                    summary.fast_eligible += 1;
                }
                if record.lookup_success_count > 0 {
                    summary.lookup_proven += 1;
                }
                if record.lookup_success_count > 0
                    && record.node_id.is_some()
                    && record.success_count > record.failure_count
                {
                    summary.lookup_fast_eligible += 1;
                }
                summary.max_success_count = summary.max_success_count.max(record.success_count);
                summary.max_lookup_success_count = summary
                    .max_lookup_success_count
                    .max(record.lookup_success_count);
                summary
            },
        )
    }

    fn emit_probe_event(
        &self,
        kind: &'static str,
        addr: SocketAddr,
        inserted: bool,
        removed: usize,
        is_ipv6: bool,
        has_node_id: bool,
    ) {
        if !internal_probe_enabled() {
            return;
        }

        let summary = self.family_probe_summary(is_ipv6);
        emit_internal_probe(format!(
            "event=active_route_update family={} kind={} addr={} inserted={} removed={} has_node_id={} total={} with_node_id={} positive_routes={} fast_eligible={} lookup_proven={} lookup_fast_eligible={} max_success={} max_lookup_success={}",
            if is_ipv6 { "ipv6" } else { "ipv4" },
            kind,
            addr,
            inserted,
            removed,
            has_node_id,
            summary.total,
            summary.with_node_id,
            summary.positive_routes,
            summary.fast_eligible,
            summary.lookup_proven,
            summary.lookup_fast_eligible,
            summary.max_success_count,
            summary.max_lookup_success_count,
        ));
    }

    fn record_soft_failure(&mut self, addr: SocketAddr) {
        let is_ipv6 = addr.is_ipv6();
        let before_total = self.family_count(is_ipv6);
        let has_node_id = if is_ipv6 {
            let Some(record) = self.ipv6.iter_mut().find(|existing| existing.addr == addr) else {
                return;
            };
            record.failure_count = record.failure_count.saturating_add(1);
            record.bump_recency();
            let has_node_id = record.node_id.is_some();
            self.ipv6.retain(|existing| {
                existing.failure_count < INTERNAL_DHT_MAX_FAILURES_PER_NODE
                    && existing.failure_count <= existing.success_count.saturating_add(4)
            });
            has_node_id
        } else {
            let mut records = self.ipv4.snapshot_records();
            let Some(record) = records.iter_mut().find(|existing| existing.addr == addr) else {
                return;
            };
            record.failure_count = record.failure_count.saturating_add(1);
            record.bump_recency();
            let has_node_id = record.node_id.is_some();
            records.retain(|existing| {
                existing.failure_count < INTERNAL_DHT_MAX_FAILURES_PER_NODE
                    && existing.failure_count <= existing.success_count.saturating_add(4)
            });
            self.ipv4.replace_records(records);
            has_node_id
        };

        if is_ipv6 {
            self.trim_family(true);
        }
        let after_total = self.family_count(is_ipv6);
        let removed = before_total.saturating_sub(after_total);
        self.emit_probe_event("soft_failure", addr, false, removed, is_ipv6, has_node_id);
    }

    fn snapshot_fast_frontier_for_family(
        &self,
        is_ipv6: bool,
        target: Option<[u8; 20]>,
    ) -> Vec<SocketAddr> {
        let Some(target) = target else {
            return Vec::new();
        };

        let family_nodes = if is_ipv6 {
            self.ipv6.iter().cloned().collect::<Vec<_>>()
        } else {
            self.ipv4.snapshot_records()
        };
        let mut frontier = family_nodes
            .iter()
            .filter(|record| {
                record.node_id.is_some()
                    && record.lookup_success_count > 0
                    && record.success_count > record.failure_count
            })
            .cloned()
            .collect::<Vec<_>>();
        frontier.sort_by(|left, right| compare_frontier_route_records(left, right, &target));
        if !is_ipv6 {
            frontier = diversify_ipv4_route_records(frontier);
        }
        if frontier.len() < INTERNAL_DHT_FAST_ACTIVE_FRONTIER_READY_FLOOR {
            let frontier_addrs = frontier
                .iter()
                .map(|record| record.addr)
                .collect::<HashSet<_>>();
            let mut supplemental = family_nodes
                .iter()
                .filter(|record| {
                    record.node_id.is_some()
                        && record.success_count > record.failure_count
                        && !frontier_addrs.contains(&record.addr)
                })
                .cloned()
                .collect::<Vec<_>>();
            supplemental.sort_by(|left, right| compare_frontier_route_records(left, right, &target));
            frontier.extend(supplemental);
        }
        if frontier.len() < INTERNAL_DHT_INITIAL_QUERY_FANOUT {
            let frontier_addrs = frontier
                .iter()
                .map(|record| record.addr)
                .collect::<HashSet<_>>();
            let mut supplemental = family_nodes
                .iter()
                .filter(|record| !frontier_addrs.contains(&record.addr))
                .cloned()
                .collect::<Vec<_>>();
            supplemental
                .sort_by(|left, right| compare_active_route_records(left, right, Some(&target)));
            frontier.extend(supplemental);
        }
        if !is_ipv6 {
            frontier = diversify_ipv4_route_records(frontier);
        }
        frontier
            .into_iter()
            .take(INTERNAL_DHT_FAST_ACTIVE_FRONTIER_LIMIT)
            .map(|record| record.addr)
            .collect()
    }

    fn snapshot_for_family(
        &self,
        is_ipv6: bool,
        target: Option<[u8; 20]>,
    ) -> Vec<SocketAddr> {
        let mut nodes = if is_ipv6 {
            self.ipv6.iter().cloned().collect::<Vec<_>>()
        } else {
            self.ipv4.snapshot_records()
        };
        nodes.sort_by(|left, right| compare_active_route_records(left, right, target.as_ref()));
        if !is_ipv6 && target.is_some() {
            nodes = diversify_ipv4_route_records(nodes);
        }
        nodes.into_iter().map(|record| record.addr).collect()
    }

    fn record_lookup_success(&mut self, addr: SocketAddr, node_id: Option<[u8; 20]>) {
        let is_ipv6 = addr.is_ipv6();
        let before_total = self.family_count(is_ipv6);
        let mut inserted = false;
        let has_node_id;
        if is_ipv6 {
            let family_nodes = &mut self.ipv6;
            has_node_id =
                if let Some(record) = family_nodes.iter_mut().find(|existing| existing.addr == addr) {
                    record.success_count = record.success_count.saturating_add(1);
                    record.lookup_success_count = record.lookup_success_count.saturating_add(1);
                    record.failure_count = record.failure_count.saturating_sub(1);
                    if let Some(node_id) = node_id {
                        record.node_id = Some(node_id);
                    }
                    record.bump_recency();
                    record.node_id.is_some()
                } else {
                    let mut candidate = InternalPrototypeNodeRecord::new(addr);
                    candidate.success_count = 1;
                    candidate.lookup_success_count = 1;
                    candidate.node_id = node_id;
                    candidate.bump_recency();
                    let has_node_id = candidate.node_id.is_some();
                    let existing_snapshot = family_nodes.iter().cloned().collect::<Vec<_>>();
                    if !should_admit_active_route_record(
                        &existing_snapshot,
                        &candidate,
                        true,
                        None,
                    ) {
                        self.emit_probe_event(
                            "lookup_success_rejected",
                            addr,
                            false,
                            0,
                            true,
                            has_node_id,
                        );
                        return;
                    }
                    family_nodes.push_back(candidate);
                    inserted = true;
                    has_node_id
                };
            self.trim_family(true);
            inserted = inserted && self.ipv6.iter().any(|record| record.addr == addr);
        } else {
            let mut family_nodes = self.ipv4.snapshot_records();
            let ipv4_local_node_id = self.ipv4.local_node_id();
            has_node_id =
                if let Some(record) = family_nodes.iter_mut().find(|existing| existing.addr == addr) {
                    record.success_count = record.success_count.saturating_add(1);
                    record.lookup_success_count = record.lookup_success_count.saturating_add(1);
                    record.failure_count = record.failure_count.saturating_sub(1);
                    if let Some(node_id) = node_id {
                        record.node_id = Some(node_id);
                    }
                    record.bump_recency();
                    record.node_id.is_some()
                } else {
                    let mut candidate = InternalPrototypeNodeRecord::new(addr);
                    candidate.success_count = 1;
                    candidate.lookup_success_count = 1;
                    candidate.node_id = node_id;
                    candidate.bump_recency();
                    let has_node_id = candidate.node_id.is_some();
                    if !should_admit_active_route_record(
                        &family_nodes,
                        &candidate,
                        false,
                        ipv4_local_node_id,
                    ) {
                        self.emit_probe_event(
                            "lookup_success_rejected",
                            addr,
                            false,
                            0,
                            false,
                            has_node_id,
                        );
                        return;
                    }
                    family_nodes.push(candidate);
                    inserted = true;
                    has_node_id
                };
            self.ipv4.replace_records(family_nodes);
            inserted = inserted && self.ipv4.contains(addr);
        }

        let after_total = self.family_count(is_ipv6);
        let removed = before_total
            .saturating_add(usize::from(inserted))
            .saturating_sub(after_total);
        self.emit_probe_event("lookup_success", addr, inserted, removed, is_ipv6, has_node_id);
    }

    fn record_success(&mut self, addr: SocketAddr, node_id: Option<[u8; 20]>) {
        let is_ipv6 = addr.is_ipv6();
        let before_total = self.family_count(is_ipv6);
        let mut inserted = false;
        let has_node_id;
        if is_ipv6 {
            let family_nodes = &mut self.ipv6;
            has_node_id =
                if let Some(record) = family_nodes.iter_mut().find(|existing| existing.addr == addr) {
                    record.success_count = record.success_count.saturating_add(1);
                    record.failure_count = record.failure_count.saturating_sub(1);
                    if let Some(node_id) = node_id {
                        record.node_id = Some(node_id);
                    }
                    record.bump_recency();
                    record.node_id.is_some()
                } else {
                    let mut candidate = InternalPrototypeNodeRecord::new(addr);
                    candidate.success_count = 1;
                    candidate.node_id = node_id;
                    candidate.bump_recency();
                    let has_node_id = candidate.node_id.is_some();
                    let existing_snapshot = family_nodes.iter().cloned().collect::<Vec<_>>();
                    if !should_admit_active_route_record(
                        &existing_snapshot,
                        &candidate,
                        true,
                        None,
                    ) {
                        self.emit_probe_event(
                            "route_success_rejected",
                            addr,
                            false,
                            0,
                            true,
                            has_node_id,
                        );
                        return;
                    }
                    family_nodes.push_back(candidate);
                    inserted = true;
                    has_node_id
                };
            self.trim_family(true);
            inserted = inserted && self.ipv6.iter().any(|record| record.addr == addr);
        } else {
            let mut family_nodes = self.ipv4.snapshot_records();
            let ipv4_local_node_id = self.ipv4.local_node_id();
            has_node_id =
                if let Some(record) = family_nodes.iter_mut().find(|existing| existing.addr == addr) {
                    record.success_count = record.success_count.saturating_add(1);
                    record.failure_count = record.failure_count.saturating_sub(1);
                    if let Some(node_id) = node_id {
                        record.node_id = Some(node_id);
                    }
                    record.bump_recency();
                    record.node_id.is_some()
                } else {
                    let mut candidate = InternalPrototypeNodeRecord::new(addr);
                    candidate.success_count = 1;
                    candidate.node_id = node_id;
                    candidate.bump_recency();
                    let has_node_id = candidate.node_id.is_some();
                    if !should_admit_active_route_record(
                        &family_nodes,
                        &candidate,
                        false,
                        ipv4_local_node_id,
                    ) {
                        self.emit_probe_event(
                            "route_success_rejected",
                            addr,
                            false,
                            0,
                            false,
                            has_node_id,
                        );
                        return;
                    }
                    family_nodes.push(candidate);
                    inserted = true;
                    has_node_id
                };
            self.ipv4.replace_records(family_nodes);
            inserted = inserted && self.ipv4.contains(addr);
        }

        let after_total = self.family_count(is_ipv6);
        let removed = before_total
            .saturating_add(usize::from(inserted))
            .saturating_sub(after_total);
        self.emit_probe_event("route_success", addr, inserted, removed, is_ipv6, has_node_id);
    }

    fn record_failure(&mut self, addr: SocketAddr) {
        let is_ipv6 = addr.is_ipv6();
        let before_total = self.family_count(is_ipv6);
        let has_node_id = if is_ipv6 {
            let Some(record) = self.ipv6.iter_mut().find(|existing| existing.addr == addr) else {
                return;
            };
            record.failure_count = record.failure_count.saturating_add(1);
            record.bump_recency();
            let has_node_id = record.node_id.is_some();
            self.ipv6.retain(|existing| {
                existing.failure_count < INTERNAL_DHT_MAX_FAILURES_PER_NODE
                    && existing.failure_count <= existing.success_count.saturating_add(4)
            });
            self.trim_family(true);
            has_node_id
        } else {
            let mut family_nodes = self.ipv4.snapshot_records();
            let Some(record) = family_nodes.iter_mut().find(|existing| existing.addr == addr) else {
                return;
            };
            record.failure_count = record.failure_count.saturating_add(1);
            record.bump_recency();
            let has_node_id = record.node_id.is_some();
            family_nodes.retain(|existing| {
                existing.failure_count < INTERNAL_DHT_MAX_FAILURES_PER_NODE
                    && existing.failure_count <= existing.success_count.saturating_add(4)
            });
            self.ipv4.replace_records(family_nodes);
            has_node_id
        };

        let after_total = self.family_count(is_ipv6);
        let removed = before_total.saturating_sub(after_total);
        self.emit_probe_event("hard_failure", addr, false, removed, is_ipv6, has_node_id);
    }

    fn family_count(&self, is_ipv6: bool) -> usize {
        if is_ipv6 {
            self.ipv6.len()
        } else {
            self.ipv4.total_count()
        }
    }

    fn contains(&self, addr: SocketAddr) -> bool {
        if addr.is_ipv6() {
            self.ipv6.iter().any(|existing| existing.addr == addr)
        } else {
            self.ipv4.contains(addr)
        }
    }

    fn trim_family(&mut self, is_ipv6: bool) {
        if is_ipv6 {
            let mut ordered = self.ipv6.iter().cloned().collect::<Vec<_>>();
            ordered.sort_by(compare_active_route_retention_records);
            ordered.truncate(internal_active_route_limit(true));
            self.ipv6 = ordered.into();
        } else {
            self.ipv4.rebalance();
        }
    }

}

#[derive(Debug, Clone, Copy, Default)]
struct InternalActiveRouteProbeSummary {
    total: usize,
    with_node_id: usize,
    positive_routes: usize,
    fast_eligible: usize,
    lookup_proven: usize,
    lookup_fast_eligible: usize,
    max_success_count: u16,
    max_lookup_success_count: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InternalCompactNode {
    id: [u8; 20],
    addr: SocketAddr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InternalPrototypeNodeRecord {
    addr: SocketAddr,
    node_id: Option<[u8; 20]>,
    success_count: u16,
    lookup_success_count: u16,
    failure_count: u16,
    recency_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InternalAnnounceTokenRecord {
    addr: SocketAddr,
    info_hash: [u8; 20],
    token: Vec<u8>,
    success_count: u16,
    failure_count: u16,
    recency_epoch: u64,
}

impl InternalPrototypeNodeRecord {
    fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            node_id: None,
            success_count: 0,
            lookup_success_count: 0,
            failure_count: 0,
            recency_epoch: 0,
        }
    }

    fn bump_recency(&mut self) {
        self.recency_epoch = self.recency_epoch.saturating_add(1);
    }
}

fn compare_node_records(
    left: &InternalPrototypeNodeRecord,
    right: &InternalPrototypeNodeRecord,
    target: Option<&[u8; 20]>,
) -> Ordering {
    let failure_order = left.failure_count.cmp(&right.failure_count);
    if failure_order != Ordering::Equal {
        return failure_order;
    }

    if let Some(target) = target {
        let distance_order =
            compare_node_distance(left.node_id.as_ref(), right.node_id.as_ref(), target);
        if distance_order != Ordering::Equal {
            return distance_order;
        }
    }

    let success_order = right.success_count.cmp(&left.success_count);
    if success_order != Ordering::Equal {
        return success_order;
    }

    right.recency_epoch.cmp(&left.recency_epoch)
}

fn compare_active_route_records(
    left: &InternalPrototypeNodeRecord,
    right: &InternalPrototypeNodeRecord,
    target: Option<&[u8; 20]>,
) -> Ordering {
    let failure_order = left.failure_count.cmp(&right.failure_count);
    if failure_order != Ordering::Equal {
        return failure_order;
    }

    if let Some(target) = target {
        let distance_order =
            compare_node_distance(left.node_id.as_ref(), right.node_id.as_ref(), target);
        if distance_order != Ordering::Equal {
            return distance_order;
        }
    }

    let success_order = right.success_count.cmp(&left.success_count);
    if success_order != Ordering::Equal {
        return success_order;
    }

    right.recency_epoch.cmp(&left.recency_epoch)
}

fn compare_frontier_route_records(
    left: &InternalPrototypeNodeRecord,
    right: &InternalPrototypeNodeRecord,
    target: &[u8; 20],
) -> Ordering {
    left.failure_count
        .cmp(&right.failure_count)
        .then_with(|| compare_node_distance(left.node_id.as_ref(), right.node_id.as_ref(), target))
        .then_with(|| right.lookup_success_count.cmp(&left.lookup_success_count))
        .then_with(|| right.success_count.cmp(&left.success_count))
        .then_with(|| right.recency_epoch.cmp(&left.recency_epoch))
}

fn compare_active_route_retention_records(
    left: &InternalPrototypeNodeRecord,
    right: &InternalPrototypeNodeRecord,
) -> Ordering {
    left.failure_count
        .cmp(&right.failure_count)
        .then_with(|| right.lookup_success_count.cmp(&left.lookup_success_count))
        .then_with(|| right.success_count.cmp(&left.success_count))
        .then_with(|| right.recency_epoch.cmp(&left.recency_epoch))
}

fn internal_active_route_limit(is_ipv6: bool) -> usize {
    if is_ipv6 {
        INTERNAL_DHT_IPV6_ACTIVE_ROUTE_LIMIT
    } else {
        INTERNAL_DHT_IPV4_ACTIVE_ROUTE_LIMIT
    }
}

fn trim_ipv4_bucketed_routes(
    buckets: &mut BTreeMap<u8, VecDeque<InternalPrototypeNodeRecord>>,
) -> bool {
    let Some(bucket_to_trim) = buckets
        .iter()
        .filter(|(_, records)| records.len() > 1)
        .max_by(|(_, left_records), (_, right_records)| {
            left_records
                .len()
                .cmp(&right_records.len())
                .then_with(|| match (left_records.back(), right_records.back()) {
                    (Some(left_tail), Some(right_tail)) => {
                        compare_active_route_retention_records(left_tail, right_tail)
                    }
                    _ => Ordering::Equal,
                })
        })
        .map(|(bucket, _)| *bucket)
    else {
        return false;
    };

    let (removed, remove_bucket) = {
        let Some(records) = buckets.get_mut(&bucket_to_trim) else {
            return false;
        };
        let removed = records.pop_back().is_some();
        (removed, records.is_empty())
    };
    if remove_bucket {
        buckets.remove(&bucket_to_trim);
    }
    removed
}

fn should_admit_active_route_record(
    existing: &[InternalPrototypeNodeRecord],
    candidate: &InternalPrototypeNodeRecord,
    is_ipv6: bool,
    ipv4_local_node_id: Option<[u8; 20]>,
) -> bool {
    if existing.len() < internal_active_route_limit(is_ipv6) {
        return true;
    }

    let candidate_beats_worst = existing
        .iter()
        .max_by(|left, right| compare_active_route_retention_records(left, right))
        .is_some_and(|worst| {
            compare_active_route_retention_records(candidate, worst) == Ordering::Less
        });
    if candidate_beats_worst {
        return true;
    }

    if is_ipv6 || candidate.lookup_success_count == 0 || candidate.node_id.is_none() {
        return false;
    }

    let Some(local_node_id) = ipv4_local_node_id else {
        return false;
    };
    let Some(candidate_bucket) = candidate
        .node_id
        .as_ref()
        .and_then(|node_id| routing_bucket_key(&local_node_id, node_id))
    else {
        return false;
    };

    let mut bucket_counts = HashMap::new();
    for bucket in existing.iter().filter_map(|record| {
        record
            .node_id
            .as_ref()
            .and_then(|node_id| routing_bucket_key(&local_node_id, node_id))
    }) {
        *bucket_counts.entry(bucket).or_insert(0usize) += 1;
    }

    !bucket_counts.contains_key(&candidate_bucket)
        && bucket_counts.values().any(|count| *count > 1)
        && candidate.success_count > candidate.failure_count
}

fn routing_bucket_key(local_node_id: &[u8; 20], remote_node_id: &[u8; 20]) -> Option<u8> {
    let distance = xor_distance(local_node_id, remote_node_id);
    for (idx, byte) in distance.iter().enumerate() {
        if *byte == 0 {
            continue;
        }
        let remaining_bits = ((distance.len() - idx - 1) * 8) as u8;
        return Some(remaining_bits + (8 - byte.leading_zeros() as u8));
    }
    None
}

fn diversify_ipv4_route_records(
    records: Vec<InternalPrototypeNodeRecord>,
) -> Vec<InternalPrototypeNodeRecord> {
    let mut seen_subnets = HashSet::new();
    let mut preferred = Vec::with_capacity(records.len());
    let mut remainder = Vec::new();

    for record in records {
        match ipv4_subnet_key(record.addr) {
            Some(subnet) if seen_subnets.insert(subnet) => preferred.push(record),
            Some(_) => remainder.push(record),
            None => preferred.push(record),
        }
    }

    preferred.extend(remainder);
    preferred
}

fn ipv4_subnet_key(addr: SocketAddr) -> Option<[u8; 3]> {
    match addr {
        SocketAddr::V4(addr) => {
            let octets = addr.ip().octets();
            Some([octets[0], octets[1], octets[2]])
        }
        SocketAddr::V6(_) => None,
    }
}

fn compare_node_distance(
    left: Option<&[u8; 20]>,
    right: Option<&[u8; 20]>,
    target: &[u8; 20],
) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => xor_distance(left, target).cmp(&xor_distance(right, target)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_compact_node_distance(
    left: &InternalCompactNode,
    right: &InternalCompactNode,
    target: &[u8; 20],
) -> Ordering {
    xor_distance(&left.id, target)
        .cmp(&xor_distance(&right.id, target))
        .then_with(|| left.addr.to_string().cmp(&right.addr.to_string()))
}

fn prioritize_non_bootstrap_nodes(
    nodes: Vec<SocketAddr>,
    bootstrap_nodes: &HashSet<SocketAddr>,
) -> Vec<SocketAddr> {
    if bootstrap_nodes.is_empty() {
        return nodes;
    }

    let mut preferred = Vec::with_capacity(nodes.len());
    let mut bootstrap = Vec::new();
    for addr in nodes {
        if bootstrap_nodes.contains(&addr) {
            bootstrap.push(addr);
        } else {
            preferred.push(addr);
        }
    }
    preferred.extend(bootstrap);
    preferred
}

fn initial_family_query_fanout(
    is_ipv6: bool,
    purpose: &str,
    fast_frontier_available: usize,
) -> usize {
    if !is_ipv6
        && purpose == "lookup"
        && fast_frontier_available >= INTERNAL_DHT_FAST_ACTIVE_FRONTIER_READY_FLOOR
    {
        INTERNAL_DHT_IPV4_FAST_LOOKUP_QUERY_FANOUT
    } else {
        INTERNAL_DHT_INITIAL_QUERY_FANOUT
    }
}

fn family_max_concurrent_queries(
    is_ipv6: bool,
    purpose: &str,
    fast_frontier_available: usize,
    before_first_batch: bool,
) -> usize {
    if before_first_batch {
        initial_family_query_fanout(is_ipv6, purpose, fast_frontier_available)
            .max(INTERNAL_DHT_MAX_CONCURRENT_FAMILY_QUERIES)
    } else {
        INTERNAL_DHT_MAX_CONCURRENT_FAMILY_QUERIES
    }
}

fn warm_lookup_targets(node_id: [u8; 20]) -> [[u8; 20]; 4] {
    let mut far_1 = node_id;
    far_1[0] ^= 0x80;
    let mut far_2 = node_id;
    far_2[0] ^= 0x40;
    let mut far_3 = node_id;
    far_3[0] ^= 0x20;
    [node_id, far_1, far_2, far_3]
}

fn xor_distance(left: &[u8; 20], right: &[u8; 20]) -> [u8; 20] {
    let mut distance = [0u8; 20];
    for (idx, (left_byte, right_byte)) in left.iter().zip(right.iter()).enumerate() {
        distance[idx] = left_byte ^ right_byte;
    }
    distance
}

#[derive(Clone, Default)]
struct InternalPrototypeSockets {
    ipv4: Option<InternalPrototypeFamilySocket>,
    ipv6: Option<InternalPrototypeFamilySocket>,
}

#[derive(Clone)]
struct InternalPrototypeFamilySocket {
    inner: Arc<InternalPrototypeFamilySocketInner>,
}

struct InternalPrototypeFamilySocketInner {
    socket: Arc<UdpSocket>,
    inflight_queries: Arc<StdMutex<HashMap<[u8; 4], InternalPrototypeInflightQuery>>>,
    next_transaction_id: AtomicU32,
    shutdown_tx: watch::Sender<bool>,
}

struct InternalPrototypeInflightQuery {
    target: SocketAddr,
    response_tx: oneshot::Sender<Option<KrpcResponseBody>>,
}

struct InternalPrototypeInflightQueryGuard {
    inflight_queries: Arc<StdMutex<HashMap<[u8; 4], InternalPrototypeInflightQuery>>>,
    transaction_id: Option<[u8; 4]>,
}

impl InternalPrototypeInflightQueryGuard {
    fn new(
        inflight_queries: Arc<StdMutex<HashMap<[u8; 4], InternalPrototypeInflightQuery>>>,
        transaction_id: [u8; 4],
    ) -> Self {
        Self {
            inflight_queries,
            transaction_id: Some(transaction_id),
        }
    }

    fn disarm(&mut self) {
        self.transaction_id = None;
    }
}

impl Drop for InternalPrototypeInflightQueryGuard {
    fn drop(&mut self) {
        let Some(transaction_id) = self.transaction_id.take() else {
            return;
        };
        if let Ok(mut inflight_queries) = self.inflight_queries.lock() {
            inflight_queries.remove(&transaction_id);
        }
    }
}

impl Drop for InternalPrototypeFamilySocketInner {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
    }
}

impl std::fmt::Debug for InternalPrototypeFamilySocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InternalPrototypeFamilySocket")
            .field("local_addr", &self.local_addr())
            .finish()
    }
}

impl InternalPrototypeFamilySocket {
    fn new(socket: UdpSocket) -> Self {
        let socket = Arc::new(socket);
        let inflight_queries = Arc::new(StdMutex::new(HashMap::new()));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Self::spawn_receive_loop(socket.clone(), inflight_queries.clone(), shutdown_rx);
        Self {
            inner: Arc::new(InternalPrototypeFamilySocketInner {
                socket,
                inflight_queries,
                next_transaction_id: AtomicU32::new(random::<u32>()),
                shutdown_tx,
            }),
        }
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.inner.socket.local_addr().ok()
    }

    async fn ping(&self, target: SocketAddr, node_id: &[u8; 20]) -> bool {
        self.send_query_with_timeout(
            target,
            "ping",
            PingArgs {
                id: node_id.as_ref(),
            },
            INTERNAL_DHT_ROUTE_QUERY_TIMEOUT,
        )
        .await
        .is_some()
    }

    async fn find_node(
        &self,
        target: SocketAddr,
        node_id: &[u8; 20],
        lookup_target: &[u8; 20],
    ) -> Option<KrpcResponseBody> {
        self.send_query_with_timeout(
            target,
            "find_node",
            FindNodeArgs {
                id: node_id.as_ref(),
                target: lookup_target.as_ref(),
            },
            INTERNAL_DHT_ROUTE_QUERY_TIMEOUT,
        )
        .await
    }

    async fn get_peers(
        &self,
        target: SocketAddr,
        node_id: &[u8; 20],
        info_hash: &[u8; 20],
    ) -> Option<KrpcResponseBody> {
        self.send_query(
            target,
            "get_peers",
            GetPeersArgs {
                id: node_id.as_ref(),
                info_hash: info_hash.as_ref(),
            },
        )
        .await
    }

    async fn announce_peer(
        &self,
        target: SocketAddr,
        node_id: &[u8; 20],
        info_hash: &[u8; 20],
        token: &[u8],
        port: Option<u16>,
    ) -> bool {
        let (port, implied_port) = match port {
            Some(port) => (port, None),
            None => (0, Some(1)),
        };

        self.send_query(
            target,
            "announce_peer",
            AnnouncePeerArgs {
                id: node_id.as_ref(),
                info_hash,
                port,
                implied_port,
                token,
            },
        )
        .await
        .is_some()
    }

    async fn send_query_with_timeout<A>(
        &self,
        target: SocketAddr,
        query: &'static str,
        args: A,
        query_timeout: Duration,
    ) -> Option<KrpcResponseBody>
    where
        A: Serialize,
    {
        let (transaction_id, response_rx) = self.register_inflight_query(target);
        let mut inflight_guard = InternalPrototypeInflightQueryGuard::new(
            self.inner.inflight_queries.clone(),
            transaction_id,
        );
        let payload = serde_bencode::to_bytes(&KrpcQueryEnvelope {
            t: transaction_id.as_slice(),
            y: "q",
            q: query,
            a: args,
        })
        .ok()?;

        if self.inner.socket.send_to(&payload, target).await.is_err() {
            return None;
        }

        match timeout(query_timeout, response_rx).await {
            Ok(Ok(response)) => {
                inflight_guard.disarm();
                response
            }
            _ => None,
        }
    }

    async fn send_query<A>(
        &self,
        target: SocketAddr,
        query: &'static str,
        args: A,
    ) -> Option<KrpcResponseBody>
    where
        A: Serialize,
    {
        self.send_query_with_timeout(target, query, args, INTERNAL_DHT_QUERY_TIMEOUT)
            .await
    }

    fn register_inflight_query(
        &self,
        target: SocketAddr,
    ) -> ([u8; 4], oneshot::Receiver<Option<KrpcResponseBody>>) {
        loop {
            let transaction_id = self
                .inner
                .next_transaction_id
                .fetch_add(1, AtomicOrdering::Relaxed)
                .to_be_bytes();
            let (response_tx, response_rx) = oneshot::channel();
            let mut inflight_queries = self
                .inner
                .inflight_queries
                .lock()
                .expect("internal dht inflight query lock");
            if let std::collections::hash_map::Entry::Vacant(entry) =
                inflight_queries.entry(transaction_id)
            {
                entry.insert(InternalPrototypeInflightQuery {
                    target,
                    response_tx,
                });
                return (transaction_id, response_rx);
            }
        }
    }

    fn spawn_receive_loop(
        socket: Arc<UdpSocket>,
        inflight_queries: Arc<StdMutex<HashMap<[u8; 4], InternalPrototypeInflightQuery>>>,
        mut shutdown_rx: watch::Receiver<bool>,
    ) {
        tokio::spawn(async move {
            let mut buffer = [0u8; INTERNAL_DHT_SOCKET_BUFFER];
            loop {
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    result = socket.recv_from(&mut buffer) => {
                        let (len, source_addr) = match result {
                            Ok(result) => result,
                            Err(error) if is_transient_udp_recv_error(&error) => continue,
                            Err(_) => break,
                        };
                        let Ok(response) =
                            serde_bencode::from_bytes::<KrpcResponseEnvelope>(&buffer[..len])
                        else {
                            continue;
                        };
                        let Ok(transaction_id) = <[u8; 4]>::try_from(response.t.as_ref()) else {
                            continue;
                        };

                        let mut inflight_queries = inflight_queries
                            .lock()
                            .expect("internal dht inflight query lock");
                        let Some(inflight_query) = inflight_queries.remove(&transaction_id) else {
                            continue;
                        };
                        if inflight_query.target != source_addr {
                            inflight_queries.insert(transaction_id, inflight_query);
                            continue;
                        }

                        let response_body = if response.y.as_ref() == b"r" {
                            response.r
                        } else {
                            None
                        };
                        let _ = inflight_query.response_tx.send(response_body);
                    }
                }
            }

            let waiters = {
                let mut inflight_queries = inflight_queries
                    .lock()
                    .expect("internal dht inflight query lock");
                inflight_queries
                    .drain()
                    .map(|(_, inflight_query)| inflight_query.response_tx)
                    .collect::<Vec<_>>()
            };
            for waiter in waiters {
                let _ = waiter.send(None::<KrpcResponseBody>);
            }
        });
    }

    fn inflight_query_count(&self) -> usize {
        self.inner
            .inflight_queries
            .lock()
            .expect("internal dht inflight query lock")
            .len()
    }
}

fn is_transient_udp_recv_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::Interrupted
            | io::ErrorKind::TimedOut
    )
}

impl InternalPrototypeClient {
    async fn warm_routes(&self) {
        let (ipv4_nodes, ipv6_nodes) = tokio::join!(
            self.warm_family_routes(
                self.sockets.ipv4.as_ref(),
                &self.state.ipv4_bootstrap_nodes,
                false,
            ),
            self.warm_family_routes(
                self.sockets.ipv6.as_ref(),
                &self.state.ipv6_bootstrap_nodes,
                true,
            ),
        );

        if !ipv4_nodes.is_empty() {
            self.record_discovered_nodes(&ipv4_nodes).await;
        }
        if !ipv6_nodes.is_empty() {
            self.record_discovered_nodes(&ipv6_nodes).await;
        }
    }

    async fn maintenance_tick(&self) {
        self.peer_lookup_cache.lock().await.prune_expired();
        self.warm_routes().await;
        self.refresh_discovered_routes().await;
        self.refresh_bootstrap_probe().await;
    }

    async fn refresh_discovered_routes(&self) {
        tokio::join!(
            self.refresh_family_routes(
                self.sockets.ipv4.as_ref(),
                &self.state.ipv4_bootstrap_nodes,
                false,
            ),
            self.refresh_family_routes(
                self.sockets.ipv6.as_ref(),
                &self.state.ipv6_bootstrap_nodes,
                true,
            ),
        );
    }

    async fn refresh_family_routes(
        &self,
        socket: Option<&InternalPrototypeFamilySocket>,
        bootstrap_nodes: &HashSet<SocketAddr>,
        is_ipv6: bool,
    ) {
        let Some(socket) = socket else {
            return;
        };

        let candidates = self
            .discovered_nodes
            .lock()
            .await
            .snapshot_for_family(is_ipv6, Some(self.node_id))
            .into_iter()
            .filter(|addr| !bootstrap_nodes.contains(addr))
            .take(INTERNAL_DHT_ROUTE_MAINTENANCE_LIMIT)
            .collect::<Vec<_>>();

        for candidate in candidates {
            if socket.ping(candidate, &self.node_id).await {
                self.record_route_refresh_success(candidate, None).await;
            } else {
                self.record_route_failure(candidate).await;
            }
        }
    }

    async fn warm_family_routes(
        &self,
        socket: Option<&InternalPrototypeFamilySocket>,
        bootstrap_nodes: &HashSet<SocketAddr>,
        is_ipv6: bool,
    ) -> Vec<InternalCompactNode> {
        let Some(socket) = socket.cloned() else {
            return Vec::new();
        };

        let mut discovered = Vec::new();
        let mut join_set = JoinSet::new();
        let mut visited = HashSet::new();
        let mut started_visits = 0usize;
        let mut pending_discovered: VecDeque<InternalCompactNode> = VecDeque::new();
        let mut pending_discovered_addrs = HashSet::new();
        let warm_targets = warm_lookup_targets(self.node_id);
        let ordered_bootstrap_nodes = self.ordered_bootstrap_nodes(bootstrap_nodes, is_ipv6).await;
        let mut pending_bootstrap = ordered_bootstrap_nodes
            .into_iter()
            .take(INTERNAL_DHT_ROUTE_WARM_LIMIT)
            .collect::<VecDeque<_>>();

        loop {
            while join_set.len() < INTERNAL_DHT_ROUTE_WARM_LIMIT
                && started_visits < INTERNAL_DHT_ROUTE_WARM_MAX_VISITS
            {
                let next_candidate = if let Some(node) = pending_discovered.pop_front() {
                    pending_discovered_addrs.remove(&node.addr);
                    Some(node.addr)
                } else {
                    pending_bootstrap.pop_front()
                };
                let Some(candidate) = next_candidate else {
                    break;
                };
                if !visited.insert(candidate) {
                    continue;
                }
                started_visits += 1;
                let family_socket = socket.clone();
                let node_id = self.node_id;
                let lookup_target = warm_targets[(started_visits - 1) % warm_targets.len()];
                join_set.spawn(async move {
                    (
                        candidate,
                        family_socket
                            .find_node(candidate, &node_id, &lookup_target)
                            .await,
                    )
                });
            }

            let Some(Ok((candidate, response))) = join_set.join_next().await else {
                break;
            };
            let Some(response) = response else {
                self.record_route_failure(candidate).await;
                if join_set.is_empty()
                    && pending_discovered.is_empty()
                    && pending_bootstrap.is_empty()
                {
                    break;
                }
                continue;
            };
            self.record_route_success(candidate, response.node_id()).await;

            let mut nodes = if is_ipv6 {
                decode_compact_nodes(response.nodes6.as_ref(), true)
            } else {
                decode_compact_nodes(response.nodes.as_ref(), false)
            };
            nodes.sort_by(|left, right| compare_compact_node_distance(left, right, &self.node_id));
            discovered.extend(nodes.iter().copied());

            for node in nodes {
                if bootstrap_nodes.contains(&node.addr)
                    || visited.contains(&node.addr)
                    || pending_discovered_addrs.contains(&node.addr)
                {
                    continue;
                }
                pending_discovered_addrs.insert(node.addr);
                pending_discovered.push_back(node);
            }

            if pending_discovered.len() > 1 {
                let mut reordered = pending_discovered.drain(..).collect::<Vec<_>>();
                reordered
                    .sort_by(|left, right| compare_compact_node_distance(left, right, &self.node_id));
                pending_discovered = reordered.into();
            }

            if join_set.is_empty()
                && pending_discovered.is_empty()
                && pending_bootstrap.is_empty()
            {
                break;
            }
        }

        discovered.sort_by(|left, right| compare_compact_node_distance(left, right, &self.node_id));
        discovered.dedup_by_key(|node| node.addr);

        discovered
    }
}

impl std::fmt::Debug for InternalPrototypeSockets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InternalPrototypeSockets")
            .field("ipv4_local_addr", &self.ipv4_local_addr())
            .field("ipv6_local_addr", &self.ipv6_local_addr())
            .finish()
    }
}

impl InternalPrototypeSockets {
    async fn bind(port: u16) -> Result<(Self, Option<String>), String> {
        let mut warnings = Vec::new();

        let ipv6 =
            match UdpSocket::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port)).await {
                Ok(socket) => Some(InternalPrototypeFamilySocket::new(socket)),
                Err(error) => {
                    warnings.push(format!("IPv6 UDP bind failed: {}", error));
                    None
                }
            };

        let ipv4_port = match (port, ipv6.as_ref()) {
            (0, Some(socket)) => socket
                .local_addr()
                .ok_or_else(|| "Failed to read IPv6 UDP local addr.".to_string())?
                .port(),
            _ => port,
        };

        let ipv4 = match UdpSocket::bind(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            ipv4_port,
        ))
        .await
        {
            Ok(socket) => Some(InternalPrototypeFamilySocket::new(socket)),
            Err(error) if ipv6.is_some() && error.kind() == io::ErrorKind::AddrInUse => None,
            Err(error) => {
                warnings.push(format!("IPv4 UDP bind failed: {}", error));
                None
            }
        };

        if ipv4.is_none() && ipv6.is_none() {
            return Err(
                "Failed to bind IPv4 and IPv6 UDP sockets for internal DHT backend.".to_string(),
            );
        }

        let warning = if warnings.is_empty() {
            None
        } else {
            Some(format!(
                "Warning: internal DHT backend running with partial socket coverage ({}).",
                warnings.join(" | ")
            ))
        };

        Ok((Self { ipv4, ipv6 }, warning))
    }

    fn ipv4_local_addr(&self) -> Option<SocketAddr> {
        self.ipv4
            .as_ref()
            .and_then(InternalPrototypeFamilySocket::local_addr)
    }

    fn ipv6_local_addr(&self) -> Option<SocketAddr> {
        self.ipv6
            .as_ref()
            .and_then(InternalPrototypeFamilySocket::local_addr)
    }
}

#[derive(Debug, Serialize)]
struct KrpcQueryEnvelope<'a, A> {
    #[serde(with = "serde_bytes")]
    t: &'a [u8],
    y: &'static str,
    q: &'static str,
    a: A,
}

#[derive(Debug, Serialize)]
struct PingArgs<'a> {
    #[serde(with = "serde_bytes")]
    id: &'a [u8],
}

#[derive(Debug, Serialize)]
struct GetPeersArgs<'a> {
    #[serde(with = "serde_bytes")]
    id: &'a [u8],
    #[serde(with = "serde_bytes")]
    info_hash: &'a [u8],
}

#[derive(Debug, Serialize)]
struct AnnouncePeerArgs<'a> {
    #[serde(with = "serde_bytes")]
    id: &'a [u8],
    #[serde(with = "serde_bytes")]
    info_hash: &'a [u8],
    port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    implied_port: Option<u8>,
    #[serde(with = "serde_bytes")]
    token: &'a [u8],
}

#[derive(Debug, Serialize)]
struct FindNodeArgs<'a> {
    #[serde(with = "serde_bytes")]
    id: &'a [u8],
    #[serde(with = "serde_bytes")]
    target: &'a [u8],
}

#[derive(Debug, Serialize, Deserialize)]
struct KrpcResponseEnvelope {
    t: ByteBuf,
    y: ByteBuf,
    #[serde(default)]
    r: Option<KrpcResponseBody>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct KrpcResponseBody {
    #[serde(default)]
    id: ByteBuf,
    #[serde(default)]
    token: ByteBuf,
    #[serde(default)]
    values: Vec<ByteBuf>,
    #[serde(default)]
    nodes: ByteBuf,
    #[serde(default)]
    nodes6: ByteBuf,
}

async fn resolve_bootstrap_nodes(nodes: &[String]) -> InternalPrototypeState {
    let mut state = InternalPrototypeState::default();

    for node in nodes {
        if let Ok(addr) = node.parse::<SocketAddr>() {
            if addr.is_ipv4() {
                state.ipv4_bootstrap_nodes.insert(addr);
            } else {
                state.ipv6_bootstrap_nodes.insert(addr);
            }
            continue;
        }

        if let Ok(resolved) = lookup_host(node).await {
            for addr in resolved {
                if addr.is_ipv4() {
                    state.ipv4_bootstrap_nodes.insert(addr);
                } else {
                    state.ipv6_bootstrap_nodes.insert(addr);
                }
            }
        }
    }

    state
}

impl KrpcResponseBody {
    fn node_id(&self) -> Option<[u8; 20]> {
        (self.id.len() == 20).then(|| {
            let mut id = [0u8; 20];
            id.copy_from_slice(self.id.as_ref());
            id
        })
    }
}

fn decode_compact_peers(bytes: &[u8], is_ipv6: bool) -> Vec<SocketAddr> {
    if !is_ipv6 && bytes.len() % 6 == 0 && !bytes.is_empty() {
        return bytes
            .chunks_exact(6)
            .map(|chunk| {
                SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3])),
                    u16::from_be_bytes([chunk[4], chunk[5]]),
                )
            })
            .collect();
    }

    if is_ipv6 && bytes.len() % 18 == 0 && !bytes.is_empty() {
        return bytes
            .chunks_exact(18)
            .map(|chunk| {
                let mut ip = [0u8; 16];
                ip.copy_from_slice(&chunk[..16]);
                SocketAddr::new(
                    IpAddr::V6(Ipv6Addr::from(ip)),
                    u16::from_be_bytes([chunk[16], chunk[17]]),
                )
            })
            .collect();
    }

    Vec::new()
}

fn decode_compact_nodes(bytes: &[u8], is_ipv6: bool) -> Vec<InternalCompactNode> {
    if is_ipv6 {
        if bytes.len() % 38 != 0 {
            return Vec::new();
        }

        return bytes
            .chunks_exact(38)
            .map(|chunk| {
                let mut id = [0u8; 20];
                id.copy_from_slice(&chunk[..20]);
                let mut ip = [0u8; 16];
                ip.copy_from_slice(&chunk[20..36]);
                InternalCompactNode {
                    id,
                    addr: SocketAddr::new(
                        IpAddr::V6(Ipv6Addr::from(ip)),
                        u16::from_be_bytes([chunk[36], chunk[37]]),
                    ),
                }
            })
            .collect();
    }

    if bytes.len() % 26 != 0 {
        return Vec::new();
    }

    bytes
        .chunks_exact(26)
        .map(|chunk| {
            let mut id = [0u8; 20];
            id.copy_from_slice(&chunk[..20]);
            InternalCompactNode {
                id,
                addr: SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(chunk[20], chunk[21], chunk[22], chunk[23])),
                    u16::from_be_bytes([chunk[24], chunk[25]]),
                ),
            }
        })
        .collect()
}

#[cfg(feature = "dht")]
#[derive(Debug, Clone)]
struct MainlineDhtClient {
    inner: AsyncDht,
}

#[cfg(feature = "dht")]
impl MainlineDhtClient {
    fn new(inner: AsyncDht) -> Self {
        Self { inner }
    }
}

#[cfg(feature = "dht")]
impl DhtBackendClient for MainlineDhtClient {
    fn backend_kind(&self) -> DhtBackendKind {
        DhtBackendKind::Mainline
    }

    fn get_peers(&self, info_hash: [u8; 20]) -> PeerBatchStream {
        let Ok(info_hash_id) = Id::from_bytes(info_hash) else {
            return Box::pin(empty());
        };

        let stream = self.inner.get_peers(info_hash_id).map(|peers| {
            peers
                .into_iter()
                .map(SocketAddr::V4)
                .collect::<Vec<SocketAddr>>()
        });

        Box::pin(stream)
    }

    fn health_snapshot(&self) -> HealthFuture {
        let inner = self.inner.clone();
        Box::pin(async move {
            let info = inner.info().await;
            let exported_bootstrap_nodes = inner.to_bootstrap().await.len();

            DhtHealthSnapshot {
                backend: DhtBackendKind::Mainline,
                enabled: true,
                local_addr: Some(SocketAddr::V4(info.local_addr())),
                ipv4_local_addr: Some(SocketAddr::V4(info.local_addr())),
                bound_family_count: 1,
                public_addr: info.public_address().map(SocketAddr::V4),
                firewalled: Some(info.firewalled()),
                server_mode: Some(info.server_mode()),
                exported_bootstrap_nodes,
                dht_size_estimate: Some(sanitize_dht_size_estimate(info.dht_size_estimate())),
                ..Default::default()
            }
        })
    }

    fn announce_peer(&self, info_hash: [u8; 20], port: Option<u16>) -> AnnounceFuture {
        let inner = self.inner.clone();
        Box::pin(async move {
            let Ok(info_hash_id) = Id::from_bytes(info_hash) else {
                return false;
            };
            inner.announce_peer(info_hash_id, port).await.is_ok()
        })
    }

    fn maintenance_tick(&self) -> MaintenanceFuture {
        Box::pin(async {})
    }
}

#[derive(Debug)]
struct BuiltRuntime {
    runtime: DhtRuntimeState,
    warning: Option<String>,
}

enum DhtCommand {
    Reconfigure(DhtServiceConfig),
}

#[derive(Debug)]
pub struct DhtService {
    handle: DhtHandle,
    status_rx: watch::Receiver<DhtStatus>,
    command_tx: mpsc::UnboundedSender<DhtCommand>,
    #[allow(dead_code)]
    task: Option<JoinHandle<()>>,
}

impl DhtService {
    pub async fn new(
        config: DhtServiceConfig,
        shutdown_rx: broadcast::Receiver<()>,
    ) -> Result<Self, String> {
        network_metrics::record(
            "dht_startup_attempt",
            None,
            None,
            None,
            serde_json::json!({
                "preferred_backend": format!("{:?}", config.preferred_backend).to_ascii_lowercase(),
                "port": config.port,
                "bootstrap_nodes": config.bootstrap_nodes.len(),
            }),
        );
        let initial = match build_runtime(&config, 0, false, None).await {
            Ok(runtime) => runtime,
            Err(error) => {
                network_metrics::record(
                    "dht_startup_failed",
                    None,
                    None,
                    None,
                    serde_json::json!({
                        "preferred_backend": format!("{:?}", config.preferred_backend).to_ascii_lowercase(),
                        "port": config.port,
                        "bootstrap_nodes": config.bootstrap_nodes.len(),
                        "error": error,
                    }),
                );
                return Err(error);
            }
        };
        let initial_status = build_status(
            &initial.runtime,
            initial.warning.clone(),
            config.preferred_backend,
        )
        .await;
        network_metrics::record(
            "dht_startup_succeeded",
            None,
            initial_status.health.local_addr,
            None,
            serde_json::json!({
                "preferred_backend": format!("{:?}", config.preferred_backend).to_ascii_lowercase(),
                "active_backend": format!("{:?}", initial_status.health.backend).to_ascii_lowercase(),
                "warning": initial_status.warning,
            }),
        );
        let (runtime_tx, runtime_rx) = watch::channel(initial.runtime);
        let (status_tx, status_rx) = watch::channel(initial_status);
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let task = Some(tokio::spawn(run_service(
            config,
            runtime_tx,
            status_tx,
            command_rx,
            shutdown_rx,
        )));

        Ok(Self {
            handle: DhtHandle { runtime_rx },
            status_rx,
            command_tx,
            task,
        })
    }

    pub fn handle(&self) -> DhtHandle {
        self.handle.clone()
    }

    pub fn subscribe_status(&self) -> watch::Receiver<DhtStatus> {
        self.status_rx.clone()
    }

    #[allow(dead_code)]
    pub fn current_status(&self) -> DhtStatus {
        self.status_rx.borrow().clone()
    }

    pub fn current_warning(&self) -> Option<String> {
        self.status_rx.borrow().warning.clone()
    }

    pub fn reconfigure(&self, config: DhtServiceConfig) {
        let _ = self.command_tx.send(DhtCommand::Reconfigure(config));
    }
}

#[cfg(test)]
impl DhtService {
    pub(crate) fn from_test_recorder(recorder: TestDhtRecorder) -> Self {
        let client: Arc<dyn DhtBackendClient> = Arc::new(recorder);
        let handle = DhtHandle::from_client(client, 0);
        let (_status_tx, status_rx) = watch::channel(DhtStatus {
            generation: 0,
            warning: None,
            health: DhtHealthSnapshot {
                backend: DhtBackendKind::InternalPrototype,
                preferred_backend: Some(DhtBackendKind::InternalPrototype),
                enabled: true,
                ..Default::default()
            },
        });
        let (command_tx, _command_rx) = mpsc::unbounded_channel();

        Self {
            handle,
            status_rx,
            command_tx,
            task: None,
        }
    }
}

pub fn configured_status_from_settings(settings: &Settings) -> DhtStatus {
    configured_status_from_config(&DhtServiceConfig::from_settings(settings))
}

fn configured_status_from_config(config: &DhtServiceConfig) -> DhtStatus {
    let prototype = InternalPrototypeState::from_bootstrap_nodes(&config.bootstrap_nodes);
    DhtStatus {
        generation: 0,
        warning: None,
        health: DhtHealthSnapshot {
            backend: config.preferred_backend,
            preferred_backend: Some(config.preferred_backend),
            recovery_pending: false,
            enabled: !matches!(config.preferred_backend, DhtBackendKind::Disabled),
            ipv4_bootstrap_nodes: prototype.ipv4_bootstrap_nodes.len(),
            ipv6_bootstrap_nodes: prototype.ipv6_bootstrap_nodes.len(),
            ..Default::default()
        },
    }
}

fn sanitize_dht_size_estimate(raw: (usize, f64)) -> DhtSizeEstimate {
    DhtSizeEstimate {
        node_count: raw.0,
        std_dev: raw.1.is_finite().then_some(raw.1),
    }
}

#[derive(Clone)]
pub struct DhtHandle {
    runtime_rx: watch::Receiver<DhtRuntimeState>,
}

impl std::fmt::Debug for DhtHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let runtime = self.runtime_rx.borrow().clone();
        f.debug_struct("DhtHandle")
            .field("generation", &runtime.generation)
            .field("backend", &runtime.client.backend_kind())
            .finish()
    }
}

impl Default for DhtHandle {
    fn default() -> Self {
        Self::disabled()
    }
}

impl DhtHandle {
    #[cfg(feature = "dht")]
    #[allow(dead_code)]
    pub fn from_async(inner: AsyncDht) -> Self {
        let client: Arc<dyn DhtBackendClient> = Arc::new(MainlineDhtClient::new(inner));
        Self::from_client(client, 0)
    }

    fn from_client(client: Arc<dyn DhtBackendClient>, generation: u64) -> Self {
        let (_runtime_tx, runtime_rx) = watch::channel(DhtRuntimeState { generation, client });
        Self { runtime_rx }
    }

    pub fn disabled() -> Self {
        let client: Arc<dyn DhtBackendClient> = Arc::new(DisabledDhtClient);
        Self::from_client(client, 0)
    }

    pub fn spawn_lookup_task(
        &self,
        info_hash: Vec<u8>,
        dht_tx: Sender<Vec<SocketAddr>>,
        mut shutdown_rx: broadcast::Receiver<()>,
        mut dht_trigger_rx: watch::Receiver<()>,
    ) -> Option<JoinHandle<()>> {
        let info_hash: [u8; 20] = info_hash.try_into().ok()?;
        let mut runtime_rx = self.runtime_rx.clone();

        Some(tokio::spawn(async move {
            loop {
                let runtime = runtime_rx.borrow().clone();
                let mut peers_stream = runtime.client.get_peers(info_hash);
                let lookup_started_at = StdInstant::now();
                let lookup_id = format!("{:016x}", rand::random::<u64>());
                let mut batch_count = 0usize;
                let mut peer_count = 0usize;
                let info_hash_bytes = info_hash.to_vec();

                network_metrics::record(
                    "dht_lookup_started",
                    Some(&info_hash_bytes),
                    None,
                    None,
                    serde_json::json!({
                        "lookup_id": lookup_id,
                        "backend": format!("{:?}", runtime.client.backend_kind()).to_ascii_lowercase(),
                        "generation": runtime.generation,
                    }),
                );

                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        network_metrics::record(
                            "dht_lookup_aborted",
                            Some(&info_hash_bytes),
                            None,
                            None,
                            serde_json::json!({
                                "lookup_id": lookup_id,
                                "reason": "shutdown",
                                "elapsed_ms": network_metrics::elapsed_ms(lookup_started_at),
                                "batches": batch_count,
                                "peers": peer_count,
                            }),
                        );
                        break
                    },
                    _ = async {
                        while let Some(peers) = peers_stream.next().await {
                            batch_count += 1;
                            peer_count += peers.len();
                            let unique_batch_peers = peers.iter().copied().collect::<HashSet<_>>();
                            let ipv4_count = peers.iter().filter(|peer| peer.is_ipv4()).count();
                            let ipv6_count = peers.len().saturating_sub(ipv4_count);
                            network_metrics::record(
                                "dht_lookup_batch",
                                Some(&info_hash_bytes),
                                None,
                                None,
                                serde_json::json!({
                                    "lookup_id": lookup_id,
                                    "backend": format!("{:?}", runtime.client.backend_kind()).to_ascii_lowercase(),
                                    "generation": runtime.generation,
                                    "elapsed_ms": network_metrics::elapsed_ms(lookup_started_at),
                                    "batch_size": peers.len(),
                                    "unique_batch_peers": unique_batch_peers.len(),
                                    "ipv4_count": ipv4_count,
                                    "ipv6_count": ipv6_count,
                                }),
                            );
                            if dht_tx.send(peers).await.is_err() {
                                network_metrics::record(
                                    "dht_lookup_aborted",
                                    Some(&info_hash_bytes),
                                    None,
                                    None,
                                    serde_json::json!({
                                        "lookup_id": lookup_id,
                                        "reason": "channel_closed",
                                        "elapsed_ms": network_metrics::elapsed_ms(lookup_started_at),
                                        "batches": batch_count,
                                        "peers": peer_count,
                                    }),
                                );
                                return;
                            }
                        }
                        network_metrics::record(
                            "dht_lookup_completed",
                            Some(&info_hash_bytes),
                            None,
                            None,
                            serde_json::json!({
                                "lookup_id": lookup_id,
                                "backend": format!("{:?}", runtime.client.backend_kind()).to_ascii_lowercase(),
                                "generation": runtime.generation,
                                "elapsed_ms": network_metrics::elapsed_ms(lookup_started_at),
                                "batches": batch_count,
                                "peers": peer_count,
                            }),
                        );
                    } => {}
                }

                tokio::select! {
                    _ = shutdown_rx.recv() => break,
                    changed = runtime_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                    changed = dht_trigger_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                    _ = tokio::time::sleep(DHT_LOOKUP_REFRESH_INTERVAL) => {}
                }
            }
        }))
    }

    pub async fn lookup_once(
        &self,
        info_hash: Vec<u8>,
        idle_timeout: Duration,
        overall_timeout: Duration,
    ) -> Option<DhtLookupRun> {
        let info_hash: [u8; 20] = info_hash.try_into().ok()?;
        let runtime = self.runtime_rx.borrow().clone();
        let mut peers_stream = runtime.client.get_peers(info_hash);
        let started_at = StdInstant::now();
        let mut idle_sleep = Box::pin(tokio::time::sleep(idle_timeout));
        let overall_sleep = tokio::time::sleep(overall_timeout);
        tokio::pin!(overall_sleep);

        let mut unique_peers = HashSet::new();
        let mut batch_count = 0usize;
        let mut total_peers = 0usize;
        let mut first_batch_ms = None;

        loop {
            tokio::select! {
                _ = &mut overall_sleep => break,
                _ = &mut idle_sleep => break,
                maybe_batch = peers_stream.next() => {
                    let Some(peers) = maybe_batch else {
                        break;
                    };
                    batch_count += 1;
                    total_peers += peers.len();
                    for peer in peers {
                        unique_peers.insert(peer);
                    }
                    if first_batch_ms.is_none() {
                        first_batch_ms = Some(network_metrics::elapsed_ms(started_at));
                    }
                    idle_sleep
                        .as_mut()
                        .reset(tokio::time::Instant::now() + idle_timeout);
                }
            }
        }

        let unique_ipv4_peers = unique_peers.iter().filter(|peer| peer.is_ipv4()).count();
        let unique_ipv6_peers = unique_peers.len().saturating_sub(unique_ipv4_peers);

        Some(DhtLookupRun {
            batch_count,
            total_peers,
            unique_peers: unique_peers.len(),
            unique_ipv4_peers,
            unique_ipv6_peers,
            first_batch_ms,
        })
    }

    pub async fn announce_peer(&self, info_hash: Vec<u8>, port: Option<u16>) -> bool {
        let Ok(info_hash) = <[u8; 20]>::try_from(info_hash) else {
            return false;
        };
        let runtime = self.runtime_rx.borrow().clone();
        let announce_started_at = StdInstant::now();
        let success = runtime.client.announce_peer(info_hash, port).await;
        network_metrics::record(
            "dht_announce_completed",
            Some(&info_hash),
            None,
            None,
            serde_json::json!({
                "backend": format!("{:?}", runtime.client.backend_kind()).to_ascii_lowercase(),
                "generation": runtime.generation,
                "port": port,
                "success": success,
                "elapsed_ms": network_metrics::elapsed_ms(announce_started_at),
            }),
        );
        success
    }
}

async fn run_service(
    mut config: DhtServiceConfig,
    runtime_tx: watch::Sender<DhtRuntimeState>,
    status_tx: watch::Sender<DhtStatus>,
    mut command_rx: mpsc::UnboundedReceiver<DhtCommand>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let mut retry_interval = tokio::time::interval(DHT_RETRY_INTERVAL);
    let mut health_interval = tokio::time::interval(DHT_HEALTH_REFRESH_INTERVAL);
    retry_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    health_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut current_generation = status_tx.borrow().generation;

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => break,
            Some(command) = command_rx.recv() => {
                match command {
                    DhtCommand::Reconfigure(new_config) => {
                        network_metrics::record(
                            "dht_reconfigure_attempt",
                            None,
                            None,
                            None,
                            serde_json::json!({
                                "preferred_backend": format!("{:?}", new_config.preferred_backend).to_ascii_lowercase(),
                                "port": new_config.port,
                                "bootstrap_nodes": new_config.bootstrap_nodes.len(),
                            }),
                        );
                        config = new_config;
                        current_generation = current_generation.saturating_add(1);
                        let runtime = runtime_tx.borrow().clone();
                        let recovery_state = runtime.client.export_recovery_state().await;
                        match build_runtime(&config, current_generation, true, recovery_state).await {
                            Ok(next_runtime) => {
                                let status = build_status(
                                    &next_runtime.runtime,
                                    next_runtime.warning,
                                    config.preferred_backend,
                                )
                                .await;
                                network_metrics::record(
                                    "dht_reconfigure_succeeded",
                                    None,
                                    status.health.local_addr,
                                    None,
                                    serde_json::json!({
                                        "preferred_backend": format!("{:?}", config.preferred_backend).to_ascii_lowercase(),
                                        "active_backend": format!("{:?}", status.health.backend).to_ascii_lowercase(),
                                        "warning": status.warning,
                                        "generation": status.generation,
                                    }),
                                );
                                let _ = runtime_tx.send(next_runtime.runtime);
                                let _ = status_tx.send(status);
                            }
                            Err(error) => {
                                network_metrics::record(
                                    "dht_reconfigure_failed",
                                    None,
                                    None,
                                    None,
                                    serde_json::json!({
                                        "preferred_backend": format!("{:?}", config.preferred_backend).to_ascii_lowercase(),
                                        "generation": current_generation,
                                        "error": error,
                                    }),
                                );
                                let _ = status_tx.send(DhtStatus {
                                    generation: current_generation,
                                    warning: Some(error),
                                    health: status_tx.borrow().health.clone(),
                                });
                            }
                        }
                    }
                }
            }
            _ = retry_interval.tick(), if status_tx.borrow().warning.is_some() => {
                network_metrics::record(
                    "dht_recovery_attempt",
                    None,
                    None,
                    None,
                    serde_json::json!({
                        "preferred_backend": format!("{:?}", config.preferred_backend).to_ascii_lowercase(),
                        "generation": current_generation.saturating_add(1),
                    }),
                );
                let runtime = runtime_tx.borrow().clone();
                let recovery_state = runtime.client.export_recovery_state().await;
                if let Ok(Some(next_runtime)) = try_recover_preferred_runtime(
                    &config,
                    current_generation.saturating_add(1),
                    recovery_state,
                ).await {
                    current_generation = next_runtime.runtime.generation;
                    let status = build_status(
                        &next_runtime.runtime,
                        next_runtime.warning.clone(),
                        config.preferred_backend,
                    )
                    .await;
                    network_metrics::record(
                        "dht_recovery_succeeded",
                        None,
                        status.health.local_addr,
                        None,
                        serde_json::json!({
                            "preferred_backend": format!("{:?}", config.preferred_backend).to_ascii_lowercase(),
                            "active_backend": format!("{:?}", status.health.backend).to_ascii_lowercase(),
                            "warning": status.warning,
                            "generation": status.generation,
                        }),
                    );
                    let _ = runtime_tx.send(next_runtime.runtime);
                    let _ = status_tx.send(status);
                }
            }
            _ = health_interval.tick() => {
                let runtime = runtime_tx.borrow().clone();
                runtime.client.maintenance_tick().await;
                let warning = status_tx.borrow().warning.clone();
                let status = build_status(&runtime, warning, config.preferred_backend).await;
                network_metrics::record(
                    "dht_health_snapshot",
                    None,
                    status.health.local_addr,
                    None,
                    serde_json::json!({
                        "preferred_backend": status.health.preferred_backend.map(|backend| format!("{:?}", backend).to_ascii_lowercase()),
                        "active_backend": format!("{:?}", status.health.backend).to_ascii_lowercase(),
                        "recovery_pending": status.health.recovery_pending,
                        "enabled": status.health.enabled,
                        "bound_family_count": status.health.bound_family_count,
                        "cached_ipv4_routes": status.health.cached_ipv4_routes,
                        "cached_ipv6_routes": status.health.cached_ipv6_routes,
                        "cached_ipv4_announce_tokens": status.health.cached_ipv4_announce_tokens,
                        "cached_ipv6_announce_tokens": status.health.cached_ipv6_announce_tokens,
                        "cached_lookup_results": status.health.cached_lookup_results,
                        "inflight_lookups": status.health.inflight_lookups,
                        "inflight_ipv4_queries": status.health.inflight_ipv4_queries,
                        "inflight_ipv6_queries": status.health.inflight_ipv6_queries,
                        "public_addr": status.health.public_addr.map(|addr| addr.to_string()),
                        "firewalled": status.health.firewalled,
                        "server_mode": status.health.server_mode,
                        "exported_bootstrap_nodes": status.health.exported_bootstrap_nodes,
                        "ipv4_bootstrap_nodes": status.health.ipv4_bootstrap_nodes,
                        "ipv6_bootstrap_nodes": status.health.ipv6_bootstrap_nodes,
                        "responsive_ipv4_bootstrap_nodes": status.health.responsive_ipv4_bootstrap_nodes,
                        "responsive_ipv6_bootstrap_nodes": status.health.responsive_ipv6_bootstrap_nodes,
                        "dht_size_estimate": status.health.dht_size_estimate.as_ref().map(|estimate| serde_json::json!({
                            "node_count": estimate.node_count,
                            "std_dev": estimate.std_dev,
                        })),
                        "warning": status.warning,
                    }),
                );
                let _ = status_tx.send(status);
            }
        }
    }
}

async fn build_status(
    runtime: &DhtRuntimeState,
    warning: Option<String>,
    preferred_backend: DhtBackendKind,
) -> DhtStatus {
    let mut health = runtime.client.health_snapshot().await;
    health.preferred_backend = Some(preferred_backend);
    health.recovery_pending = warning.is_some()
        && preferred_backend != DhtBackendKind::Disabled
        && health.backend != preferred_backend;

    DhtStatus {
        generation: runtime.generation,
        warning,
        health,
    }
}

async fn build_runtime(
    config: &DhtServiceConfig,
    generation: u64,
    allow_disabled_fallback: bool,
    recovery_state: Option<InternalPrototypeRecoveryState>,
) -> Result<BuiltRuntime, String> {
    let (client, warning) = match config.preferred_backend {
        DhtBackendKind::Disabled => (
            Arc::new(DisabledDhtClient) as Arc<dyn DhtBackendClient>,
            None,
        ),
        DhtBackendKind::InternalPrototype => {
            build_internal_runtime(config, allow_disabled_fallback, recovery_state).await?
        }
        DhtBackendKind::Mainline => build_mainline_runtime(config, allow_disabled_fallback)?,
    };

    Ok(BuiltRuntime {
        runtime: DhtRuntimeState { generation, client },
        warning,
    })
}

async fn build_internal_runtime(
    config: &DhtServiceConfig,
    allow_disabled_fallback: bool,
    recovery_state: Option<InternalPrototypeRecoveryState>,
) -> Result<(Arc<dyn DhtBackendClient>, Option<String>), String> {
    if let Some(error) = forced_internal_backend_error(config) {
        return handle_internal_backend_failure(config, error, allow_disabled_fallback);
    }

    match InternalPrototypeClient::bind_with_recovery(
        config.port,
        &config.bootstrap_nodes,
        recovery_state,
    )
    .await
    {
        Ok((client, warning)) => Ok((Arc::new(client) as Arc<dyn DhtBackendClient>, warning)),
        Err(error) => handle_internal_backend_failure(config, error, allow_disabled_fallback),
    }
}

#[cfg(feature = "dht")]
fn handle_internal_backend_failure(
    config: &DhtServiceConfig,
    internal_error: String,
    allow_disabled_fallback: bool,
) -> Result<(Arc<dyn DhtBackendClient>, Option<String>), String> {
    match build_mainline_runtime(config, allow_disabled_fallback) {
        Ok((client, warning)) => {
            let prefix = if client.backend_kind() == DhtBackendKind::Mainline {
                format!(
                    "Warning: internal DHT backend unavailable ({}). Falling back to compat DHT backend until recovery succeeds.",
                    internal_error
                )
            } else {
                format!(
                    "Warning: internal DHT backend unavailable ({}). Compat fallback also unavailable.",
                    internal_error
                )
            };
            let warning = match warning {
                Some(existing) => Some(format!("{} {}", prefix, existing)),
                None => Some(prefix),
            };
            Ok((client, warning))
        }
        Err(mainline_error) => Err(format!(
            "Failed to initialize internal DHT backend ({}) and compat fallback ({}).",
            internal_error, mainline_error
        )),
    }
}

#[cfg(not(feature = "dht"))]
fn handle_internal_backend_failure(
    _config: &DhtServiceConfig,
    internal_error: String,
    allow_disabled_fallback: bool,
) -> Result<(Arc<dyn DhtBackendClient>, Option<String>), String> {
    if allow_disabled_fallback {
        Ok((
            Arc::new(DisabledDhtClient) as Arc<dyn DhtBackendClient>,
            Some(format!(
                "Warning: internal DHT backend unavailable ({}). Running with DHT disabled until reconfigured.",
                internal_error
            )),
        ))
    } else {
        Err(internal_error)
    }
}

#[cfg(feature = "dht")]
fn build_mainline_runtime(
    config: &DhtServiceConfig,
    allow_disabled_fallback: bool,
) -> Result<(Arc<dyn DhtBackendClient>, Option<String>), String> {
    match build_mainline_async(config, true) {
        Ok(inner) => Ok((
            Arc::new(MainlineDhtClient::new(inner)) as Arc<dyn DhtBackendClient>,
            None,
        )),
        Err(bootstrap_error) => {
            let warning = format!(
                "Warning: DHT bootstrap unavailable ({}). Running without bootstrap; retrying automatically.",
                bootstrap_error
            );

            match build_mainline_async(config, false) {
                Ok(inner) => Ok((
                    Arc::new(MainlineDhtClient::new(inner)) as Arc<dyn DhtBackendClient>,
                    Some(warning),
                )),
                Err(fallback_error) if allow_disabled_fallback => Ok((
                    Arc::new(DisabledDhtClient) as Arc<dyn DhtBackendClient>,
                    Some(format!(
                        "Warning: DHT unavailable (bootstrap error: {}; fallback error: {}). Running with DHT disabled until reconfigured.",
                        bootstrap_error, fallback_error
                    )),
                )),
                Err(fallback_error) => Err(format!(
                    "Failed to initialize DHT startup fallback. Bootstrap error: {}. Fallback error: {}",
                    bootstrap_error, fallback_error
                )),
            }
        }
    }
}

#[cfg(not(feature = "dht"))]
fn build_mainline_runtime(
    _config: &DhtServiceConfig,
    _allow_disabled_fallback: bool,
) -> Result<(Arc<dyn DhtBackendClient>, Option<String>), String> {
    Ok((
        Arc::new(DisabledDhtClient) as Arc<dyn DhtBackendClient>,
        None,
    ))
}

#[cfg(feature = "dht")]
fn build_mainline_async(
    config: &DhtServiceConfig,
    with_bootstrap: bool,
) -> Result<AsyncDht, String> {
    let mut builder = Dht::builder();
    let bootstrap_nodes: Vec<&str> = config.bootstrap_nodes.iter().map(String::as_str).collect();

    if with_bootstrap && !bootstrap_nodes.is_empty() {
        builder.bootstrap(&bootstrap_nodes);
    }

    builder
        .port(config.port)
        .server_mode()
        .build()
        .map(|dht| dht.as_async())
        .map_err(|error| error.to_string())
}

#[cfg(feature = "dht")]
async fn try_recover_preferred_runtime(
    config: &DhtServiceConfig,
    generation: u64,
    recovery_state: Option<InternalPrototypeRecoveryState>,
) -> Result<Option<BuiltRuntime>, String> {
    match config.preferred_backend {
        DhtBackendKind::InternalPrototype => {
            match build_internal_runtime(config, false, recovery_state).await {
                Ok((client, warning)) => Ok(Some(BuiltRuntime {
                    runtime: DhtRuntimeState { generation, client },
                    warning,
                })),
                Err(_) => Ok(None),
            }
        }
        DhtBackendKind::Mainline => match build_mainline_async(config, true) {
            Ok(inner) => Ok(Some(BuiltRuntime {
                runtime: DhtRuntimeState {
                    generation,
                    client: Arc::new(MainlineDhtClient::new(inner)),
                },
                warning: None,
            })),
            Err(_) => Ok(None),
        },
        DhtBackendKind::Disabled => Ok(None),
    }
}

#[cfg(not(feature = "dht"))]
async fn try_recover_preferred_runtime(
    config: &DhtServiceConfig,
    generation: u64,
    recovery_state: Option<InternalPrototypeRecoveryState>,
) -> Result<Option<BuiltRuntime>, String> {
    match config.preferred_backend {
        DhtBackendKind::InternalPrototype => {
            match build_internal_runtime(config, false, recovery_state).await {
                Ok((client, warning)) => Ok(Some(BuiltRuntime {
                    runtime: DhtRuntimeState { generation, client },
                    warning,
                })),
                Err(_) => Ok(None),
            }
        }
        DhtBackendKind::Disabled | DhtBackendKind::Mainline => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::VecDeque;
    use std::sync::{Mutex, OnceLock};
    use tokio::net::UdpSocket;
    use tokio::sync::mpsc;

    #[derive(Debug, Clone)]
    struct FakeBackend {
        backend: DhtBackendKind,
        batches: Arc<Mutex<VecDeque<Vec<SocketAddr>>>>,
    }

    impl FakeBackend {
        fn new(backend: DhtBackendKind, batches: Vec<Vec<SocketAddr>>) -> Self {
            Self {
                backend,
                batches: Arc::new(Mutex::new(batches.into())),
            }
        }
    }

    impl DhtBackendClient for FakeBackend {
        fn backend_kind(&self) -> DhtBackendKind {
            self.backend
        }

        fn get_peers(&self, _info_hash: [u8; 20]) -> PeerBatchStream {
            let batches = self
                .batches
                .lock()
                .expect("fake backend lock")
                .drain(..)
                .collect::<Vec<_>>();
            Box::pin(tokio_stream::iter(batches))
        }

        fn health_snapshot(&self) -> HealthFuture {
            let backend = self.backend;
            Box::pin(async move {
                DhtHealthSnapshot {
                    backend,
                    enabled: true,
                    ..Default::default()
                }
            })
        }

        fn announce_peer(&self, _info_hash: [u8; 20], _port: Option<u16>) -> AnnounceFuture {
            Box::pin(async move { true })
        }

        fn maintenance_tick(&self) -> MaintenanceFuture {
            Box::pin(async {})
        }
    }

    #[derive(Debug, Deserialize)]
    struct TestKrpcQuery {
        t: ByteBuf,
        y: String,
        q: String,
    }

    #[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
    struct TestAnnouncePeerArgs {
        info_hash: ByteBuf,
        port: u16,
        #[serde(default)]
        implied_port: Option<u8>,
        token: ByteBuf,
    }

    #[derive(Debug, Deserialize)]
    struct TestAnnouncePeerQuery {
        a: TestAnnouncePeerArgs,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum TestKrpcObservation {
        Ping,
        FindNode,
        GetPeers,
        AnnouncePeer(TestAnnouncePeerArgs),
    }

    #[derive(Debug, Clone, Default)]
    struct TestKrpcReply {
        values: Vec<SocketAddr>,
        nodes: Vec<SocketAddr>,
        nodes6: Vec<SocketAddr>,
        token: Vec<u8>,
        response_delay: Duration,
    }

    async fn spawn_test_krpc_server(
        bind_addr: SocketAddr,
        reply: TestKrpcReply,
    ) -> (SocketAddr, JoinHandle<()>) {
        spawn_observing_test_krpc_server(bind_addr, reply, None).await
    }

    async fn spawn_observing_test_krpc_server(
        bind_addr: SocketAddr,
        reply: TestKrpcReply,
        observation_tx: Option<mpsc::UnboundedSender<TestKrpcObservation>>,
    ) -> (SocketAddr, JoinHandle<()>) {
        let socket = UdpSocket::bind(bind_addr)
            .await
            .expect("bind test krpc socket");
        let local_addr = socket.local_addr().expect("test krpc local addr");

        let task = tokio::spawn(async move {
            let mut buffer = [0u8; INTERNAL_DHT_SOCKET_BUFFER];
            loop {
                let Ok((len, source_addr)) = socket.recv_from(&mut buffer).await else {
                    break;
                };
                let Ok(query) = serde_bencode::from_bytes::<TestKrpcQuery>(&buffer[..len]) else {
                    continue;
                };
                if query.y != "q" {
                    continue;
                }

                let response_body = match query.q.as_str() {
                    "ping" => {
                        if let Some(tx) = observation_tx.as_ref() {
                            let _ = tx.send(TestKrpcObservation::Ping);
                        }
                        KrpcResponseBody::default()
                    }
                    "find_node" => {
                        if let Some(tx) = observation_tx.as_ref() {
                            let _ = tx.send(TestKrpcObservation::FindNode);
                        }
                        KrpcResponseBody {
                            id: ByteBuf::from(test_node_id(98).to_vec()),
                            token: ByteBuf::from(reply.token.clone()),
                            values: Vec::new(),
                            nodes: encode_compact_nodes(&reply.nodes),
                            nodes6: encode_compact_nodes(&reply.nodes6),
                        }
                    }
                    "get_peers" => {
                        if let Some(tx) = observation_tx.as_ref() {
                            let _ = tx.send(TestKrpcObservation::GetPeers);
                        }
                        KrpcResponseBody {
                            id: ByteBuf::from(test_node_id(99).to_vec()),
                            token: ByteBuf::from(reply.token.clone()),
                            values: reply
                                .values
                                .iter()
                                .copied()
                                .map(encode_compact_peer)
                                .collect(),
                            nodes: encode_compact_nodes(&reply.nodes),
                            nodes6: encode_compact_nodes(&reply.nodes6),
                        }
                    }
                    "announce_peer" => {
                        if let Ok(announce_query) =
                            serde_bencode::from_bytes::<TestAnnouncePeerQuery>(&buffer[..len])
                        {
                            if let Some(tx) = observation_tx.as_ref() {
                                let _ = tx.send(TestKrpcObservation::AnnouncePeer(
                                    announce_query.a.clone(),
                                ));
                            }
                        }
                        KrpcResponseBody {
                            id: ByteBuf::from(test_node_id(100).to_vec()),
                            token: ByteBuf::from(reply.token.clone()),
                            ..Default::default()
                        }
                    }
                    _ => continue,
                };

                let response = KrpcResponseEnvelope {
                    t: query.t,
                    y: ByteBuf::from(b"r".to_vec()),
                    r: Some(response_body),
                };
                let Ok(payload) = serde_bencode::to_bytes(&response) else {
                    continue;
                };
                if !reply.response_delay.is_zero() {
                    tokio::time::sleep(reply.response_delay).await;
                }
                if socket.send_to(&payload, source_addr).await.is_err() {
                    break;
                }
            }
        });

        (local_addr, task)
    }

    async fn spawn_blackhole_test_krpc_server(
        bind_addr: SocketAddr,
        observation_tx: Option<mpsc::UnboundedSender<TestKrpcObservation>>,
    ) -> (SocketAddr, JoinHandle<()>) {
        let socket = UdpSocket::bind(bind_addr)
            .await
            .expect("bind blackhole krpc socket");
        let local_addr = socket.local_addr().expect("blackhole krpc local addr");

        let task = tokio::spawn(async move {
            let mut buffer = [0u8; INTERNAL_DHT_SOCKET_BUFFER];
            loop {
                let Ok((len, _source_addr)) = socket.recv_from(&mut buffer).await else {
                    break;
                };
                let Ok(query) = serde_bencode::from_bytes::<TestKrpcQuery>(&buffer[..len]) else {
                    continue;
                };
                if query.y != "q" {
                    continue;
                }

                if let Some(tx) = observation_tx.as_ref() {
                    match query.q.as_str() {
                        "ping" => {
                            let _ = tx.send(TestKrpcObservation::Ping);
                        }
                        "get_peers" => {
                            let _ = tx.send(TestKrpcObservation::GetPeers);
                        }
                        _ => {}
                    }
                }
            }
        });

        (local_addr, task)
    }

    async fn recv_announce_observation(
        observation_rx: &mut mpsc::UnboundedReceiver<TestKrpcObservation>,
    ) -> TestAnnouncePeerArgs {
        loop {
            let observation = tokio::time::timeout(Duration::from_secs(1), observation_rx.recv())
                .await
                .expect("announce observation timeout")
                .expect("announce observation");
            if let TestKrpcObservation::AnnouncePeer(args) = observation {
                return args;
            }
        }
    }

    async fn drain_observations(
        observation_rx: &mut mpsc::UnboundedReceiver<TestKrpcObservation>,
    ) -> Vec<TestKrpcObservation> {
        let mut observations = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_millis(25), observation_rx.recv()).await {
                Ok(Some(observation)) => observations.push(observation),
                _ => break,
            }
        }
        observations
    }

    fn encode_compact_peer(addr: SocketAddr) -> ByteBuf {
        match addr {
            SocketAddr::V4(addr) => {
                let mut bytes = Vec::with_capacity(6);
                bytes.extend_from_slice(&addr.ip().octets());
                bytes.extend_from_slice(&addr.port().to_be_bytes());
                ByteBuf::from(bytes)
            }
            SocketAddr::V6(addr) => {
                let mut bytes = Vec::with_capacity(18);
                bytes.extend_from_slice(&addr.ip().octets());
                bytes.extend_from_slice(&addr.port().to_be_bytes());
                ByteBuf::from(bytes)
            }
        }
    }

    fn encode_compact_nodes(addrs: &[SocketAddr]) -> ByteBuf {
        let mut bytes = Vec::new();
        for (idx, addr) in addrs.iter().enumerate() {
            let node_id = test_node_id((idx as u8).wrapping_add(1));
            match addr {
                SocketAddr::V4(addr) => {
                    bytes.extend_from_slice(&node_id);
                    bytes.extend_from_slice(&addr.ip().octets());
                    bytes.extend_from_slice(&addr.port().to_be_bytes());
                }
                SocketAddr::V6(addr) => {
                    bytes.extend_from_slice(&node_id);
                    bytes.extend_from_slice(&addr.ip().octets());
                    bytes.extend_from_slice(&addr.port().to_be_bytes());
                }
            }
        }
        ByteBuf::from(bytes)
    }

    fn test_node_id(seed: u8) -> [u8; 20] {
        [seed; 20]
    }

    fn test_bucketed_node_id_for_local(local_node_id: [u8; 20], bucket: u8, salt: u8) -> [u8; 20] {
        let mut distance = [0u8; 20];
        let bit_index = bucket.saturating_sub(1) as usize;
        let byte_index = distance.len() - 1 - (bit_index / 8);
        let bit_in_byte = bit_index % 8;
        distance[byte_index] = 1u8 << bit_in_byte;
        if byte_index < distance.len() - 1 {
            distance[distance.len() - 1] = salt;
        } else if bit_in_byte > 0 {
            distance[byte_index] |= salt & ((1u8 << bit_in_byte) - 1);
        }
        let mut node_id = [0u8; 20];
        for (idx, byte) in node_id.iter_mut().enumerate() {
            *byte = local_node_id[idx] ^ distance[idx];
        }
        node_id
    }

    fn test_bucketed_node_id(bucket: u8, salt: u8) -> [u8; 20] {
        test_bucketed_node_id_for_local([0u8; 20], bucket, salt)
    }

    fn dht_backend_env_guard() -> &'static Mutex<()> {
        static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
        GUARD.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn dht_service_config_defaults_to_internal_backend_without_override() {
        let _guard = dht_backend_env_guard().lock().expect("dht env guard");
        std::env::remove_var("SUPERSEEDR_DHT_BACKEND");

        let config = DhtServiceConfig::from_settings(&Settings::default());

        assert_eq!(config.preferred_backend, DhtBackendKind::InternalPrototype);
    }

    #[test]
    fn dht_service_config_respects_backend_override_from_env() {
        let _guard = dht_backend_env_guard().lock().expect("dht env guard");
        std::env::set_var("SUPERSEEDR_DHT_BACKEND", "compat");
        let compat_config = DhtServiceConfig::from_settings(&Settings::default());
        std::env::set_var("SUPERSEEDR_DHT_BACKEND", "disabled");
        let disabled_config = DhtServiceConfig::from_settings(&Settings::default());
        std::env::remove_var("SUPERSEEDR_DHT_BACKEND");

        assert_eq!(compat_config.preferred_backend, DhtBackendKind::Mainline);
        assert_eq!(disabled_config.preferred_backend, DhtBackendKind::Disabled);
    }

    #[tokio::test]
    async fn lookup_task_restarts_after_runtime_update() {
        let first_client: Arc<dyn DhtBackendClient> = Arc::new(FakeBackend::new(
            DhtBackendKind::Mainline,
            vec![vec!["127.0.0.1:41000".parse().expect("v4 peer")]],
        ));
        let second_client: Arc<dyn DhtBackendClient> = Arc::new(FakeBackend::new(
            DhtBackendKind::InternalPrototype,
            vec![vec!["[::1]:42000".parse().expect("v6 peer")]],
        ));
        let (runtime_tx, runtime_rx) = watch::channel(DhtRuntimeState {
            generation: 0,
            client: first_client,
        });
        let handle = DhtHandle { runtime_rx };
        let (dht_tx, mut dht_rx) = mpsc::channel(8);
        let (shutdown_tx, _) = broadcast::channel(1);
        let (trigger_tx, trigger_rx) = watch::channel(());
        let info_hash = vec![1u8; 20];

        let task = handle
            .spawn_lookup_task(info_hash, dht_tx, shutdown_tx.subscribe(), trigger_rx)
            .expect("lookup task");

        let first_batch = tokio::time::timeout(Duration::from_secs(1), dht_rx.recv())
            .await
            .expect("first batch timeout")
            .expect("first batch value");
        assert_eq!(first_batch.len(), 1);
        assert!(first_batch[0].is_ipv4());

        runtime_tx
            .send(DhtRuntimeState {
                generation: 1,
                client: second_client,
            })
            .expect("runtime update");
        trigger_tx.send(()).expect("trigger update");

        let second_batch = tokio::time::timeout(Duration::from_secs(1), dht_rx.recv())
            .await
            .expect("second batch timeout")
            .expect("second batch value");
        assert_eq!(second_batch.len(), 1);
        assert!(second_batch[0].is_ipv6());

        let _ = shutdown_tx.send(());
        task.await.expect("lookup task join");
    }

    #[tokio::test]
    async fn dht_service_reconfigure_switches_backend_and_status_generation() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = DhtService::new(
            DhtServiceConfig {
                port: 0,
                bootstrap_nodes: vec!["127.0.0.1:6881".to_string(), "[::1]:6881".to_string()],
                preferred_backend: DhtBackendKind::InternalPrototype,
                force_internal_failure: false,
            },
            shutdown_tx.subscribe(),
        )
        .await
        .expect("internal prototype service");
        let mut status_rx = service.subscribe_status();

        assert_eq!(
            status_rx.borrow().health.backend,
            DhtBackendKind::InternalPrototype
        );
        assert_eq!(status_rx.borrow().generation, 0);

        service.reconfigure(DhtServiceConfig {
            port: 0,
            bootstrap_nodes: Vec::new(),
            preferred_backend: DhtBackendKind::Disabled,
            force_internal_failure: false,
        });

        tokio::time::timeout(Duration::from_secs(1), status_rx.changed())
            .await
            .expect("status change timeout")
            .expect("status change");

        let status = status_rx.borrow().clone();
        assert_eq!(status.generation, 1);
        assert_eq!(status.health.backend, DhtBackendKind::Disabled);
        assert_eq!(
            status.health.preferred_backend,
            Some(DhtBackendKind::Disabled)
        );
        assert!(!status.health.recovery_pending);
        assert!(status.warning.is_none());

        let _ = shutdown_tx.send(());
    }

    #[cfg(feature = "dht")]
    #[tokio::test]
    async fn dht_service_startup_falls_back_to_compat_backend_when_internal_unavailable() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = DhtService::new(
            DhtServiceConfig {
                port: 0,
                bootstrap_nodes: Vec::new(),
                preferred_backend: DhtBackendKind::InternalPrototype,
                force_internal_failure: true,
            },
            shutdown_tx.subscribe(),
        )
        .await
        .expect("compat fallback service");

        let status = service.current_status();

        assert_eq!(status.health.backend, DhtBackendKind::Mainline);
        assert_eq!(
            status.health.preferred_backend,
            Some(DhtBackendKind::InternalPrototype)
        );
        assert!(status.health.enabled);
        assert!(status.health.recovery_pending);
        assert!(status
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("Falling back to compat DHT backend")));

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn internal_prototype_recovery_attempt_rebuilds_runtime() {
        let recovered = try_recover_preferred_runtime(
            &DhtServiceConfig {
                port: 0,
                bootstrap_nodes: Vec::new(),
                preferred_backend: DhtBackendKind::InternalPrototype,
                force_internal_failure: false,
            },
            7,
            None,
        )
        .await
        .expect("recovery result")
        .expect("recovered runtime");

        assert_eq!(recovered.runtime.generation, 7);
        assert_eq!(
            recovered.runtime.client.backend_kind(),
            DhtBackendKind::InternalPrototype
        );
    }

    #[cfg(feature = "dht")]
    #[tokio::test]
    async fn internal_prototype_recovery_attempt_restores_preferred_backend_after_compat_fallback()
    {
        let config = DhtServiceConfig {
            port: 0,
            bootstrap_nodes: Vec::new(),
            preferred_backend: DhtBackendKind::InternalPrototype,
            force_internal_failure: false,
        };

        {
            let mut forced_config = config.clone();
            forced_config.force_internal_failure = true;
            let built = build_runtime(&forced_config, 0, false, None)
                .await
                .expect("compat fallback runtime");
            assert_eq!(
                built.runtime.client.backend_kind(),
                DhtBackendKind::Mainline
            );
            assert!(built
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("internal DHT backend unavailable")));
        }

        let recovered = try_recover_preferred_runtime(&config, 1, None)
            .await
            .expect("recovery result")
            .expect("recovered runtime");

        assert_eq!(recovered.runtime.generation, 1);
        assert_eq!(
            recovered.runtime.client.backend_kind(),
            DhtBackendKind::InternalPrototype
        );
    }

    #[tokio::test]
    async fn internal_prototype_recovery_state_preserves_discovered_routes() {
        let info_hash = [35u8; 20];
        let discovered_peer = "127.0.0.1:49161".parse().expect("discovered peer");
        let token = vec![1u8, 2, 3, 4];
        let (leaf_addr, leaf_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("leaf bind addr"),
            TestKrpcReply {
                values: vec![discovered_peer],
                token: token.clone(),
                ..Default::default()
            },
        )
        .await;
        let (bootstrap_addr, bootstrap_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                nodes: vec![leaf_addr],
                ..Default::default()
            },
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());
        assert_eq!(
            client.query_get_peers(info_hash).await,
            vec![discovered_peer]
        );

        let recovery_state = client
            .export_recovery_state()
            .await
            .expect("internal recovery state");

        bootstrap_task.abort();

        let (recovered_client, warning) =
            InternalPrototypeClient::bind_with_recovery(0, &[], Some(recovery_state))
                .await
                .expect("recovered client");
        assert!(warning.is_none());
        assert_eq!(
            recovered_client.query_get_peers(info_hash).await,
            vec![discovered_peer]
        );

        leaf_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_recovery_state_preserves_announce_tokens() {
        let info_hash = [36u8; 20];
        let token = vec![9u8, 8, 7, 6];
        let (observation_tx, mut observation_rx) = mpsc::unbounded_channel();
        let (bootstrap_addr, bootstrap_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                token: token.clone(),
                ..Default::default()
            },
            Some(observation_tx),
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());
        let _ = client.query_get_peers(info_hash).await;
        let recovery_state = client
            .export_recovery_state()
            .await
            .expect("internal recovery state");
        let _ = drain_observations(&mut observation_rx).await;

        let (recovered_client, warning) =
            InternalPrototypeClient::bind_with_recovery(0, &[], Some(recovery_state))
                .await
                .expect("recovered client");
        assert!(warning.is_none());
        assert!(recovered_client.announce_peer(info_hash, Some(51413)).await);

        let args = recv_announce_observation(&mut observation_rx).await;
        assert_eq!(args.info_hash.as_ref(), info_hash);
        assert_eq!(args.port, 51413);
        assert_eq!(args.implied_port, None);
        assert_eq!(args.token.as_ref(), token.as_slice());

        bootstrap_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_service_reports_bound_udp_family_health() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = DhtService::new(
            DhtServiceConfig {
                port: 0,
                bootstrap_nodes: vec!["127.0.0.1:6881".to_string(), "[::1]:6881".to_string()],
                preferred_backend: DhtBackendKind::InternalPrototype,
                force_internal_failure: false,
            },
            shutdown_tx.subscribe(),
        )
        .await
        .expect("internal prototype service");

        let status = service.current_status();

        assert_eq!(status.health.backend, DhtBackendKind::InternalPrototype);
        assert_eq!(
            status.health.preferred_backend,
            Some(DhtBackendKind::InternalPrototype)
        );
        assert!(status.health.enabled);
        assert!(!status.health.recovery_pending);
        assert!(status.health.bound_family_count >= 1);
        assert!(status.health.local_addr.is_some());
        assert_eq!(status.health.ipv4_bootstrap_nodes, 1);
        assert_eq!(status.health.ipv6_bootstrap_nodes, 1);
        if let (Some(ipv4), Some(ipv6)) =
            (status.health.ipv4_local_addr, status.health.ipv6_local_addr)
        {
            assert_eq!(ipv4.port(), ipv6.port());
        }

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn internal_prototype_probe_counts_responsive_bootstrap_nodes() {
        let (bootstrap_addr, bootstrap_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                values: Vec::new(),
                nodes: Vec::new(),
                nodes6: Vec::new(),
                token: Vec::new(),
                response_delay: Duration::ZERO,
            },
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());

        let probe = client.probe_bootstrap_nodes().await;

        assert_eq!(probe.ipv4.len(), 1);
        assert!(probe.ipv4.contains(&bootstrap_addr));
        assert!(probe.ipv6.is_empty());

        bootstrap_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_health_snapshot_uses_cached_bootstrap_probe_state() {
        let (observation_tx, mut observation_rx) = mpsc::unbounded_channel();
        let (bootstrap_addr, bootstrap_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply::default(),
            Some(observation_tx),
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());
        let _ = drain_observations(&mut observation_rx).await;

        let health = client.health_snapshot().await;

        assert_eq!(health.responsive_ipv4_bootstrap_nodes, 1);
        assert_eq!(health.responsive_ipv6_bootstrap_nodes, 0);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), observation_rx.recv())
                .await
                .is_err(),
            "health snapshot should not probe bootstrap nodes directly"
        );

        bootstrap_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_bind_warms_routing_cache_from_find_node() {
        let routed_node = "127.0.0.1:49031".parse().expect("routed node");
        let (bootstrap_addr, bootstrap_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                values: Vec::new(),
                nodes: vec![routed_node],
                nodes6: Vec::new(),
                token: Vec::new(),
                response_delay: Duration::ZERO,
            },
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());

        let health = client.health_snapshot().await;

        assert_eq!(health.exported_bootstrap_nodes, 2);
        assert_eq!(
            health.dht_size_estimate,
            Some(DhtSizeEstimate {
                node_count: 2,
                std_dev: None,
            })
        );
        bootstrap_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_maintenance_tick_refreshes_routes_from_bootstrap_nodes() {
        let routed_node = "127.0.0.1:49041".parse().expect("routed node");
        let (observation_tx, mut observation_rx) = mpsc::unbounded_channel();
        let (bootstrap_addr, bootstrap_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                values: Vec::new(),
                nodes: vec![routed_node],
                nodes6: Vec::new(),
                token: Vec::new(),
                response_delay: Duration::ZERO,
            },
            Some(observation_tx),
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());
        let initial_observations = drain_observations(&mut observation_rx).await;
        assert!(initial_observations.contains(&TestKrpcObservation::FindNode));
        assert!(initial_observations.contains(&TestKrpcObservation::Ping));

        client.maintenance_tick().await;

        let maintenance_observations = drain_observations(&mut observation_rx).await;
        assert!(maintenance_observations.contains(&TestKrpcObservation::FindNode));
        assert!(maintenance_observations.contains(&TestKrpcObservation::Ping));

        bootstrap_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_warm_routes_promotes_responsive_downstream_nodes() {
        let (downstream_addr, downstream_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("downstream bind addr"),
            TestKrpcReply {
                values: Vec::new(),
                nodes: Vec::new(),
                nodes6: Vec::new(),
                token: Vec::new(),
                response_delay: Duration::ZERO,
            },
            None,
        )
        .await;
        let (bootstrap_addr, bootstrap_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                values: Vec::new(),
                nodes: vec![downstream_addr],
                nodes6: Vec::new(),
                token: Vec::new(),
                response_delay: Duration::ZERO,
            },
            None,
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());

        let active_routes = client.active_routes.lock().await;
        assert!(active_routes.contains(bootstrap_addr));
        assert!(active_routes.contains(downstream_addr));
        drop(active_routes);

        let health = client.health_snapshot().await;
        assert!(
            health.active_ipv4_routes >= 2,
            "warm route follow-up should promote downstream responders into active routes"
        );

        bootstrap_task.abort();
        downstream_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_warm_routes_promote_second_hop_downstream_nodes() {
        let (second_hop_addr, second_hop_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("second hop bind addr"),
            TestKrpcReply {
                values: Vec::new(),
                nodes: Vec::new(),
                nodes6: Vec::new(),
                token: Vec::new(),
                response_delay: Duration::ZERO,
            },
            None,
        )
        .await;
        let (first_hop_addr, first_hop_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("first hop bind addr"),
            TestKrpcReply {
                values: Vec::new(),
                nodes: vec![second_hop_addr],
                nodes6: Vec::new(),
                token: Vec::new(),
                response_delay: Duration::ZERO,
            },
            None,
        )
        .await;
        let (bootstrap_addr, bootstrap_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                values: Vec::new(),
                nodes: vec![first_hop_addr],
                nodes6: Vec::new(),
                token: Vec::new(),
                response_delay: Duration::ZERO,
            },
            None,
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());

        let active_routes = client.active_routes.lock().await;
        assert!(active_routes.contains(bootstrap_addr));
        assert!(active_routes.contains(first_hop_addr));
        assert!(
            active_routes.contains(second_hop_addr),
            "deeper warm follow-up should promote second-hop responders into active routes"
        );

        bootstrap_task.abort();
        first_hop_task.abort();
        second_hop_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_maintenance_tick_evicts_failed_discovered_routes() {
        let (responsive_tx, mut responsive_rx) = mpsc::unbounded_channel();
        let (responsive_addr, responsive_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("responsive bind addr"),
            TestKrpcReply::default(),
            Some(responsive_tx),
        )
        .await;
        let (blackhole_tx, mut blackhole_rx) = mpsc::unbounded_channel();
        let (blackhole_addr, blackhole_task) = spawn_blackhole_test_krpc_server(
            "127.0.0.1:0".parse().expect("blackhole bind addr"),
            Some(blackhole_tx),
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[]).await.expect("client");
        assert!(warning.is_none());
        client
            .record_discovered_nodes(&[
                InternalCompactNode {
                    id: test_node_id(40),
                    addr: responsive_addr,
                },
                InternalCompactNode {
                    id: test_node_id(41),
                    addr: blackhole_addr,
                },
            ])
            .await;
        client.record_lookup_success(responsive_addr, None).await;

        for _ in 0..INTERNAL_DHT_MAX_FAILURES_PER_NODE {
            client.maintenance_tick().await;
        }

        let ordered = client
            .discovered_nodes
            .lock()
            .await
            .snapshot_for_family(false, None);
        assert!(ordered.contains(&responsive_addr));
        assert!(!ordered.contains(&blackhole_addr));

        let responsive_observations = drain_observations(&mut responsive_rx).await;
        assert!(responsive_observations
            .iter()
            .any(|observation| matches!(observation, TestKrpcObservation::Ping)));

        let blackhole_observations = drain_observations(&mut blackhole_rx).await;
        assert_eq!(
            blackhole_observations
                .iter()
                .filter(|observation| matches!(observation, TestKrpcObservation::GetPeers))
                .count(),
            0
        );
        assert_eq!(
            blackhole_observations
                .iter()
                .filter(|observation| matches!(observation, TestKrpcObservation::Ping))
                .count(),
            usize::from(INTERNAL_DHT_MAX_FAILURES_PER_NODE)
        );

        responsive_task.abort();
        blackhole_task.abort();
    }

    #[tokio::test]
    async fn seed_family_nodes_prefers_cached_routes_before_bootstrap_nodes() {
        let cached_addr = "127.0.0.1:49051".parse().expect("cached addr");
        let bootstrap_addr = "127.0.0.1:49052".parse().expect("bootstrap addr");
        let (client, warning) = InternalPrototypeClient::bind(0, &[]).await.expect("client");
        assert!(warning.is_none());

        client
            .record_discovered_nodes(&[InternalCompactNode {
                id: test_node_id(1),
                addr: cached_addr,
            }])
            .await;
        let pending = client
            .seed_family_nodes(&HashSet::from([bootstrap_addr]), false, Some([0u8; 20]))
            .await;

        assert_eq!(
            pending.into_iter().collect::<Vec<_>>(),
            vec![cached_addr, bootstrap_addr]
        );
    }

    #[tokio::test]
    async fn seed_family_nodes_keeps_bootstrap_reserve_when_cache_is_full() {
        let bootstrap_addr = "127.0.0.1:49062".parse().expect("bootstrap addr");
        let cached_nodes = (0..INTERNAL_DHT_SEED_NODE_LIMIT)
            .map(|idx| InternalCompactNode {
                id: test_node_id((idx as u8).wrapping_add(1)),
                addr: SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    49070 + u16::try_from(idx).expect("cached port fits"),
                ),
            })
            .collect::<Vec<_>>();
        let (client, warning) = InternalPrototypeClient::bind(0, &[]).await.expect("client");
        assert!(warning.is_none());
        client.record_discovered_nodes(&cached_nodes).await;

        let pending = client
            .seed_family_nodes(&HashSet::from([bootstrap_addr]), false, Some([0u8; 20]))
            .await
            .into_iter()
            .collect::<Vec<_>>();

        assert_eq!(pending.len(), INTERNAL_DHT_SEED_NODE_LIMIT);
        assert!(pending.contains(&bootstrap_addr));
        assert_eq!(pending[0], cached_nodes[0].addr);
    }

    #[tokio::test]
    async fn seed_family_nodes_keeps_bootstrap_out_of_initial_wave_when_frontier_is_deep() {
        let bootstrap_addr = "127.0.0.1:49082".parse().expect("bootstrap addr");
        let cached_nodes = (0..INTERNAL_DHT_SEED_NODE_LIMIT)
            .map(|idx| InternalCompactNode {
                id: test_node_id((idx as u8).wrapping_add(20)),
                addr: SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    49090 + u16::try_from(idx).expect("cached port fits"),
                ),
            })
            .collect::<Vec<_>>();
        let (client, warning) = InternalPrototypeClient::bind(0, &[]).await.expect("client");
        assert!(warning.is_none());
        client.record_discovered_nodes(&cached_nodes).await;
        {
            let mut active_routes = client.active_routes.lock().await;
            for idx in 0..INTERNAL_DHT_INITIAL_QUERY_FANOUT {
                let addr = SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    49160 + u16::try_from(idx).expect("frontier port fits"),
                );
                active_routes.record_success(
                    addr,
                    Some(test_node_id((idx as u8).wrapping_add(80))),
                );
            }
        }

        let pending = client
            .seed_family_nodes(&HashSet::from([bootstrap_addr]), false, Some([1u8; 20]))
            .await
            .into_iter()
            .collect::<Vec<_>>();

        assert!(
            pending
                .iter()
                .take(INTERNAL_DHT_INITIAL_QUERY_FANOUT)
                .all(|addr| *addr != bootstrap_addr),
            "bootstrap node should stay out of the initial query wave once cached routes are deep"
        );
        assert!(pending.contains(&bootstrap_addr));
    }

    #[tokio::test]
    async fn seed_family_nodes_uses_bootstrap_in_initial_wave_when_frontier_is_thin_even_if_cache_is_deep(
    ) {
        let bootstrap_addr = "127.0.0.1:49083".parse().expect("bootstrap addr");
        let cached_nodes = (0..INTERNAL_DHT_SEED_NODE_LIMIT)
            .map(|idx| InternalCompactNode {
                id: test_node_id((idx as u8).wrapping_add(60)),
                addr: SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    49210 + u16::try_from(idx).expect("cached port fits"),
                ),
            })
            .collect::<Vec<_>>();
        let (client, warning) = InternalPrototypeClient::bind(0, &[]).await.expect("client");
        assert!(warning.is_none());
        client.record_discovered_nodes(&cached_nodes).await;

        let pending = client
            .seed_family_nodes(&HashSet::from([bootstrap_addr]), false, Some([3u8; 20]))
            .await
            .into_iter()
            .collect::<Vec<_>>();

        assert!(
            pending
                .iter()
                .take(INTERNAL_DHT_INITIAL_QUERY_FANOUT)
                .any(|addr| *addr == bootstrap_addr),
            "bootstrap node should enter the initial query wave when the fast frontier is thin even if the broader cache is deep"
        );
    }

    #[tokio::test]
    async fn seed_family_nodes_excludes_broad_cached_routes_for_mature_ipv4_fast_frontier() {
        let bootstrap_addr: SocketAddr = "127.0.0.1:49084".parse().expect("bootstrap addr");
        let discovered_only_addr: SocketAddr =
            "127.0.0.1:49085".parse().expect("discovered addr");
        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("bind internal prototype");
        assert!(warning.is_none());

        {
            let mut active_routes = client.active_routes.lock().await;
            for idx in 0..INTERNAL_DHT_FAST_ACTIVE_FRONTIER_LIMIT {
                let addr = SocketAddr::from(([127, 0, 0, 1], 49100 + idx as u16));
                active_routes.record_lookup_success(addr, Some(test_node_id((idx + 1) as u8)));
            }
        }

        {
            let mut discovered_nodes = client.discovered_nodes.lock().await;
            discovered_nodes.insert(InternalCompactNode {
                id: test_node_id(91),
                addr: discovered_only_addr,
            });
        }

        let pending = client
            .seed_family_nodes(&HashSet::from([bootstrap_addr]), false, Some([4u8; 20]))
            .await;

        assert!(
            !pending.contains(&discovered_only_addr),
            "mature ipv4 warm seeds should come from the fast frontier/active routes, not the broad discovered cache"
        );
    }

    #[tokio::test]
    async fn seed_family_nodes_includes_bootstrap_in_initial_wave_when_cache_is_shallow() {
        let bootstrap_addr = "127.0.0.1:49102".parse().expect("bootstrap addr");
        let cached_nodes = (0..INTERNAL_DHT_INITIAL_QUERY_FANOUT.saturating_sub(1))
            .map(|idx| InternalCompactNode {
                id: test_node_id((idx as u8).wrapping_add(40)),
                addr: SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    49110 + u16::try_from(idx).expect("cached port fits"),
                ),
            })
            .collect::<Vec<_>>();
        let (client, warning) = InternalPrototypeClient::bind(0, &[]).await.expect("client");
        assert!(warning.is_none());
        client.record_discovered_nodes(&cached_nodes).await;

        let pending = client
            .seed_family_nodes(&HashSet::from([bootstrap_addr]), false, Some([2u8; 20]))
            .await
            .into_iter()
            .collect::<Vec<_>>();

        assert!(
            pending
                .iter()
                .take(INTERNAL_DHT_INITIAL_QUERY_FANOUT)
                .any(|addr| *addr == bootstrap_addr),
            "bootstrap node should help fill the initial query wave when cached routes are shallow"
        );
    }

    #[tokio::test]
    async fn seed_family_nodes_prefers_fast_ipv4_frontier_before_broader_routes() {
        let closer_addr = "127.0.0.1:49121".parse().expect("closer addr");
        let steadier_addr = "127.0.0.1:49122".parse().expect("steadier addr");
        let cached_addr = "127.0.0.1:49123".parse().expect("cached addr");
        let (client, warning) = InternalPrototypeClient::bind(0, &[]).await.expect("client");
        assert!(warning.is_none());

        client
            .record_discovered_nodes(&[InternalCompactNode {
                id: test_node_id(20),
                addr: cached_addr,
            }])
            .await;

        {
            let mut active_routes = client.active_routes.lock().await;
            active_routes.record_success(closer_addr, Some(test_node_id(1)));
            active_routes.record_success(closer_addr, Some(test_node_id(1)));
            active_routes.record_success(steadier_addr, Some(test_node_id(8)));
            active_routes.record_success(steadier_addr, Some(test_node_id(8)));
            active_routes.record_success(steadier_addr, Some(test_node_id(8)));
        }

        let pending = client
            .seed_family_nodes(&HashSet::new(), false, Some([0u8; 20]))
            .await
            .into_iter()
            .collect::<Vec<_>>();

        assert_eq!(pending[0], closer_addr);
        assert_eq!(pending[1], steadier_addr);
        assert!(pending.contains(&cached_addr));
    }

    #[tokio::test]
    async fn seed_family_nodes_prefers_non_bootstrap_active_routes_before_bootstrap_routes() {
        let bootstrap_addr = "127.0.0.1:49124".parse().expect("bootstrap addr");
        let non_bootstrap_addr = "127.0.0.1:49125".parse().expect("non-bootstrap addr");
        let (client, warning) = InternalPrototypeClient::bind(0, &[]).await.expect("client");
        assert!(warning.is_none());

        {
            let mut active_routes = client.active_routes.lock().await;
            active_routes.record_success(bootstrap_addr, Some(test_node_id(4)));
            active_routes.record_success(bootstrap_addr, Some(test_node_id(4)));
            active_routes.record_success(non_bootstrap_addr, Some(test_node_id(5)));
            active_routes.record_success(non_bootstrap_addr, Some(test_node_id(5)));
        }

        let pending = client
            .seed_family_nodes(&HashSet::from([bootstrap_addr]), false, Some([0u8; 20]))
            .await
            .into_iter()
            .collect::<Vec<_>>();

        assert_eq!(pending[0], non_bootstrap_addr);
        assert!(pending.contains(&bootstrap_addr));
    }

    #[tokio::test]
    async fn seed_family_nodes_prefers_responsive_bootstrap_nodes() {
        let (responsive_addr, responsive_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("responsive bind addr"),
            TestKrpcReply::default(),
        )
        .await;
        let (unresponsive_addr, unresponsive_task) = spawn_blackhole_test_krpc_server(
            "127.0.0.1:0".parse().expect("unresponsive bind addr"),
            None,
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(
            0,
            &[responsive_addr.to_string(), unresponsive_addr.to_string()],
        )
        .await
        .expect("client");
        assert!(warning.is_none());

        let pending = client
            .seed_family_nodes(
                &HashSet::from([unresponsive_addr, responsive_addr]),
                false,
                Some([0u8; 20]),
            )
            .await
            .into_iter()
            .collect::<Vec<_>>();

        assert_eq!(pending[0], responsive_addr);
        assert!(pending.contains(&unresponsive_addr));

        responsive_task.abort();
        unresponsive_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_query_walks_bootstrap_nodes_to_collect_peers() {
        let discovered_peer = "127.0.0.1:49001".parse().expect("discovered peer");
        let (leaf_addr, leaf_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("leaf bind addr"),
            TestKrpcReply {
                values: vec![discovered_peer],
                nodes: Vec::new(),
                nodes6: Vec::new(),
                token: Vec::new(),
                response_delay: Duration::ZERO,
            },
        )
        .await;
        let (bootstrap_addr, bootstrap_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                values: Vec::new(),
                nodes: vec![leaf_addr],
                nodes6: Vec::new(),
                token: Vec::new(),
                response_delay: Duration::ZERO,
            },
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());

        let peers = client.query_get_peers([7u8; 20]).await;

        assert_eq!(peers, vec![discovered_peer]);

        bootstrap_task.abort();
        leaf_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_get_peers_streams_partial_batches() {
        let info_hash = [0u8; 20];
        let first_peer = "127.0.0.1:49111".parse().expect("first peer");
        let second_peer = "127.0.0.1:49112".parse().expect("second peer");
        let (first_leaf_addr, first_leaf_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("first leaf bind addr"),
            TestKrpcReply {
                values: vec![first_peer],
                ..Default::default()
            },
        )
        .await;
        let (second_leaf_addr, second_leaf_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("second leaf bind addr"),
            TestKrpcReply {
                values: vec![second_peer],
                ..Default::default()
            },
        )
        .await;
        let (bootstrap_addr, bootstrap_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                nodes: vec![first_leaf_addr, second_leaf_addr],
                ..Default::default()
            },
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());

        let mut stream = client.get_peers(info_hash);
        let first_batch = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("first batch timeout")
            .unwrap_or_default();
        let second_batch = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("second batch timeout")
            .unwrap_or_default();

        assert_eq!(first_batch.len(), 1);
        assert_eq!(second_batch.len(), 1);
        let streamed = first_batch
            .into_iter()
            .chain(second_batch.into_iter())
            .collect::<HashSet<_>>();
        assert_eq!(streamed, HashSet::from([first_peer, second_peer]));

        bootstrap_task.abort();
        first_leaf_task.abort();
        second_leaf_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_initial_fanout_streams_fast_bootstrap_before_slow_cached_seed() {
        let info_hash = [33u8; 20];
        let discovered_peer = "127.0.0.1:49141".parse().expect("discovered peer");
        let (slow_seed_addr, slow_seed_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("slow seed bind addr"),
            TestKrpcReply {
                response_delay: Duration::from_millis(200),
                ..Default::default()
            },
        )
        .await;
        let (bootstrap_addr, bootstrap_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                values: vec![discovered_peer],
                ..Default::default()
            },
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());
        client
            .record_discovered_nodes(&[InternalCompactNode {
                id: test_node_id(33),
                addr: slow_seed_addr,
            }])
            .await;

        let mut stream = client.get_peers(info_hash);
        let first_batch = tokio::time::timeout(Duration::from_millis(150), stream.next())
            .await
            .expect("fast bootstrap batch timeout")
            .unwrap_or_default();

        assert_eq!(first_batch, vec![discovered_peer]);

        bootstrap_task.abort();
        slow_seed_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_initial_fanout_queries_short_bootstrap_frontier_immediately() {
        let info_hash = [36u8; 20];
        let discovered_peer = "127.0.0.1:49161".parse().expect("discovered peer");
        let (slow_seed_tx, mut slow_seed_rx) = mpsc::unbounded_channel();
        let (slow_seed_addr, slow_seed_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("slow seed bind addr"),
            TestKrpcReply {
                response_delay: Duration::from_millis(200),
                ..Default::default()
            },
            Some(slow_seed_tx),
        )
        .await;
        let (bootstrap_tx, mut bootstrap_rx) = mpsc::unbounded_channel();
        let (bootstrap_addr, bootstrap_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                values: vec![discovered_peer],
                ..Default::default()
            },
            Some(bootstrap_tx),
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());
        let _ = drain_observations(&mut bootstrap_rx).await;
        client
            .record_discovered_nodes(&[InternalCompactNode {
                id: test_node_id(36),
                addr: slow_seed_addr,
            }])
            .await;

        let mut stream = client.get_peers(info_hash);

        assert_eq!(
            tokio::time::timeout(Duration::from_millis(60), slow_seed_rx.recv())
                .await
                .expect("slow seed observation timeout"),
            Some(TestKrpcObservation::GetPeers)
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(60), bootstrap_rx.recv())
                .await
                .expect("bootstrap observation timeout"),
            Some(TestKrpcObservation::GetPeers)
        );

        let first_batch = tokio::time::timeout(Duration::from_millis(150), stream.next())
            .await
            .expect("bootstrap batch timeout")
            .unwrap_or_default();
        assert_eq!(first_batch, vec![discovered_peer]);

        bootstrap_task.abort();
        slow_seed_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_family_socket_allows_overlapping_queries() {
        let (observation_tx, mut observation_rx) = mpsc::unbounded_channel();
        let (slow_addr, slow_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("slow bind addr"),
            TestKrpcReply {
                response_delay: Duration::from_millis(200),
                ..Default::default()
            },
            Some(observation_tx),
        )
        .await;
        let (fast_addr, fast_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("fast bind addr"),
            TestKrpcReply::default(),
        )
        .await;

        let socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind local family socket");
        let family_socket = InternalPrototypeFamilySocket::new(socket);
        let slow_socket = family_socket.clone();

        let slow_lookup = tokio::spawn(async move {
            slow_socket
                .get_peers(slow_addr, &test_node_id(95), &[3u8; 20])
                .await
        });

        let observation = tokio::time::timeout(Duration::from_millis(100), observation_rx.recv())
            .await
            .expect("slow query observation timeout")
            .expect("slow query observation");
        assert_eq!(observation, TestKrpcObservation::GetPeers);

        let fast_result = tokio::time::timeout(
            Duration::from_millis(100),
            family_socket.ping(fast_addr, &test_node_id(96)),
        )
        .await
        .expect("fast query should not block behind another in-flight request");
        assert!(fast_result);

        assert!(slow_lookup.await.expect("slow lookup join").is_some());

        slow_task.abort();
        fast_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_family_socket_cleans_up_canceled_queries() {
        let (observation_tx, mut observation_rx) = mpsc::unbounded_channel();
        let (blackhole_addr, blackhole_task) = spawn_blackhole_test_krpc_server(
            "127.0.0.1:0".parse().expect("blackhole bind addr"),
            Some(observation_tx),
        )
        .await;
        let socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind local family socket");
        let family_socket = InternalPrototypeFamilySocket::new(socket);
        let canceled_socket = family_socket.clone();

        let canceled_query = tokio::spawn(async move {
            canceled_socket
                .get_peers(blackhole_addr, &test_node_id(97), &[4u8; 20])
                .await
        });

        let observation = tokio::time::timeout(Duration::from_millis(100), observation_rx.recv())
            .await
            .expect("canceled query observation timeout")
            .expect("canceled query observation");
        assert_eq!(observation, TestKrpcObservation::GetPeers);

        canceled_query.abort();
        let _ = canceled_query.await;

        tokio::time::timeout(Duration::from_millis(100), async {
            loop {
                if family_socket.inflight_query_count() == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("canceled query should release inflight slot");

        blackhole_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_family_socket_ignores_unreachable_peer_errors() {
        let closed_addr = {
            let closed_socket = UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("bind closed probe socket");
            closed_socket.local_addr().expect("closed probe addr")
        };
        let (live_addr, live_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("live bind addr"),
            TestKrpcReply::default(),
        )
        .await;

        let socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind local family socket");
        let family_socket = InternalPrototypeFamilySocket::new(socket);
        let unreachable_socket = family_socket.clone();

        let unreachable_lookup = tokio::spawn(async move {
            unreachable_socket
                .get_peers(closed_addr, &test_node_id(101), &[6u8; 20])
                .await
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let live_result = tokio::time::timeout(
            Duration::from_millis(150),
            family_socket.ping(live_addr, &test_node_id(102)),
        )
        .await
        .expect("live query should survive unreachable peer errors");
        assert!(live_result);

        let _ = unreachable_lookup.await;
        live_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_coalesces_concurrent_get_peers_requests() {
        let info_hash = [29u8; 20];
        let discovered_peer = "127.0.0.1:49091".parse().expect("discovered peer");
        let (observation_tx, mut observation_rx) = mpsc::unbounded_channel();
        let (bootstrap_addr, bootstrap_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                values: vec![discovered_peer],
                ..Default::default()
            },
            Some(observation_tx),
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());
        let _ = drain_observations(&mut observation_rx).await;

        let mut stream_a = client.get_peers(info_hash);
        let mut stream_b = client.get_peers(info_hash);
        let (batch_a, batch_b) =
            tokio::join!(async { stream_a.next().await.unwrap_or_default() }, async {
                stream_b.next().await.unwrap_or_default()
            });

        assert_eq!(batch_a, vec![discovered_peer]);
        assert_eq!(batch_b, vec![discovered_peer]);

        let observations = drain_observations(&mut observation_rx).await;
        assert_eq!(
            observations
                .iter()
                .filter(|observation| matches!(observation, TestKrpcObservation::GetPeers))
                .count(),
            1
        );

        bootstrap_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_coalesced_requests_share_partial_batches() {
        let info_hash = [31u8; 20];
        let first_peer = "127.0.0.1:49121".parse().expect("first peer");
        let second_peer = "127.0.0.1:49122".parse().expect("second peer");
        let (observation_tx, mut observation_rx) = mpsc::unbounded_channel();
        let (first_leaf_addr, first_leaf_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("first leaf bind addr"),
            TestKrpcReply {
                values: vec![first_peer],
                ..Default::default()
            },
        )
        .await;
        let (second_leaf_addr, second_leaf_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("second leaf bind addr"),
            TestKrpcReply {
                values: vec![second_peer],
                ..Default::default()
            },
        )
        .await;
        let (bootstrap_addr, bootstrap_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                nodes: vec![first_leaf_addr, second_leaf_addr],
                ..Default::default()
            },
            Some(observation_tx),
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());
        let _ = drain_observations(&mut observation_rx).await;

        let mut stream_a = client.get_peers(info_hash);
        let mut stream_b = client.get_peers(info_hash);

        let (first_batch_a, first_batch_b) =
            tokio::join!(async { stream_a.next().await.unwrap_or_default() }, async {
                stream_b.next().await.unwrap_or_default()
            });
        let (second_batch_a, second_batch_b) =
            tokio::join!(async { stream_a.next().await.unwrap_or_default() }, async {
                stream_b.next().await.unwrap_or_default()
            });

        let mut observed_a = vec![
            first_batch_a
                .first()
                .copied()
                .expect("first streamed peer for stream a"),
            second_batch_a
                .first()
                .copied()
                .expect("second streamed peer for stream a"),
        ];
        let mut observed_b = vec![
            first_batch_b
                .first()
                .copied()
                .expect("first streamed peer for stream b"),
            second_batch_b
                .first()
                .copied()
                .expect("second streamed peer for stream b"),
        ];
        observed_a.sort_unstable_by_key(|addr| addr.to_string());
        observed_b.sort_unstable_by_key(|addr| addr.to_string());

        let mut expected = vec![first_peer, second_peer];
        expected.sort_unstable_by_key(|addr| addr.to_string());

        assert_eq!(observed_a, expected);
        assert_eq!(observed_b, expected);

        let observations = drain_observations(&mut observation_rx).await;
        assert_eq!(
            observations
                .iter()
                .filter(|observation| matches!(observation, TestKrpcObservation::GetPeers))
                .count(),
            1
        );

        bootstrap_task.abort();
        first_leaf_task.abort();
        second_leaf_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_stops_lookup_when_all_subscribers_drop() {
        let info_hash = [32u8; 20];
        let discovered_peer = "127.0.0.1:49131".parse().expect("discovered peer");
        let (leaf_tx, mut leaf_rx) = mpsc::unbounded_channel();
        let (leaf_addr, leaf_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("leaf bind addr"),
            TestKrpcReply {
                values: vec![discovered_peer],
                ..Default::default()
            },
            Some(leaf_tx),
        )
        .await;
        let (bootstrap_tx, mut bootstrap_rx) = mpsc::unbounded_channel();
        let (bootstrap_addr, bootstrap_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                nodes: vec![leaf_addr],
                response_delay: Duration::from_millis(200),
                ..Default::default()
            },
            Some(bootstrap_tx),
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());
        let _ = drain_observations(&mut bootstrap_rx).await;

        drop(client.get_peers(info_hash));

        tokio::time::sleep(Duration::from_millis(300)).await;

        let bootstrap_observations = drain_observations(&mut bootstrap_rx).await;
        assert!(
            bootstrap_observations
                .iter()
                .filter(|observation| matches!(observation, TestKrpcObservation::GetPeers))
                .count()
                <= 1
        );

        let leaf_observations = drain_observations(&mut leaf_rx).await;
        assert!(
            leaf_observations
                .iter()
                .all(|observation| !matches!(observation, TestKrpcObservation::GetPeers)),
            "lookup should stop before querying downstream nodes once all subscribers drop"
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let health = client.health_snapshot().await;
                if health.inflight_lookups == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("lookup cache should clear after subscribers drop");

        bootstrap_task.abort();
        leaf_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_query_prefers_closer_node_peers_from_response_order() {
        let info_hash = [255u8; 20];
        let closer_peers = (0..INTERNAL_DHT_MAX_RETURNED_PEERS)
            .map(|idx| {
                SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    50000 + u16::try_from(idx).expect("peer port fits"),
                )
            })
            .collect::<Vec<_>>();
        let (far_tx, mut far_rx) = mpsc::unbounded_channel();
        let (far_addr, far_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("far bind addr"),
            TestKrpcReply::default(),
            Some(far_tx),
        )
        .await;
        let (close_tx, mut close_rx) = mpsc::unbounded_channel();
        let (close_addr, close_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("close bind addr"),
            TestKrpcReply {
                values: closer_peers.clone(),
                ..Default::default()
            },
            Some(close_tx),
        )
        .await;
        let (bootstrap_addr, bootstrap_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                values: Vec::new(),
                nodes: vec![far_addr, close_addr],
                nodes6: Vec::new(),
                token: Vec::new(),
                response_delay: Duration::ZERO,
            },
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());
        let _ = drain_observations(&mut close_rx).await;
        let _ = drain_observations(&mut far_rx).await;

        let peers = client.query_get_peers(info_hash).await;

        assert_eq!(peers, closer_peers);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), close_rx.recv())
                .await
                .expect("close observation timeout"),
            Some(TestKrpcObservation::GetPeers)
        );
        let _ = drain_observations(&mut far_rx).await;

        bootstrap_task.abort();
        close_task.abort();
        far_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_hedges_slow_closer_node_with_next_candidate() {
        let info_hash = [34u8; 20];
        let faster_peer = "127.0.0.1:49151".parse().expect("faster peer");
        let (close_tx, mut close_rx) = mpsc::unbounded_channel();
        let (close_addr, close_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("close bind addr"),
            TestKrpcReply {
                response_delay: Duration::from_millis(300),
                ..Default::default()
            },
            Some(close_tx),
        )
        .await;
        let (far_tx, mut far_rx) = mpsc::unbounded_channel();
        let (far_addr, far_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("far bind addr"),
            TestKrpcReply {
                values: vec![faster_peer],
                ..Default::default()
            },
            Some(far_tx),
        )
        .await;
        let (bootstrap_addr, bootstrap_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                values: Vec::new(),
                nodes: vec![close_addr, far_addr],
                nodes6: Vec::new(),
                token: Vec::new(),
                response_delay: Duration::ZERO,
            },
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());
        let _ = drain_observations(&mut close_rx).await;
        let _ = drain_observations(&mut far_rx).await;

        let mut stream = client.get_peers(info_hash);
        let first_batch = tokio::time::timeout(Duration::from_millis(200), stream.next())
            .await
            .expect("hedged batch timeout")
            .unwrap_or_default();

        assert_eq!(first_batch, vec![faster_peer]);
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(50), far_rx.recv())
                .await
                .expect("far hedge observation timeout"),
            Some(TestKrpcObservation::GetPeers)
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(50), close_rx.recv())
                .await
                .expect("close observation timeout"),
            Some(TestKrpcObservation::GetPeers)
        );

        bootstrap_task.abort();
        close_task.abort();
        far_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_keeps_querying_after_partial_batch_while_slow_seed_is_inflight() {
        let info_hash = [35u8; 20];
        let bootstrap_peer = "127.0.0.1:49161".parse().expect("bootstrap peer");
        let downstream_peer = "127.0.0.1:49162".parse().expect("downstream peer");
        let (leaf_tx, mut leaf_rx) = mpsc::unbounded_channel();
        let (leaf_addr, leaf_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("leaf bind addr"),
            TestKrpcReply {
                values: vec![downstream_peer],
                response_delay: Duration::from_millis(50),
                ..Default::default()
            },
            Some(leaf_tx),
        )
        .await;
        let (slow_seed_tx, mut slow_seed_rx) = mpsc::unbounded_channel();
        let (slow_seed_addr, slow_seed_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("slow seed bind addr"),
            TestKrpcReply {
                response_delay: Duration::from_millis(300),
                ..Default::default()
            },
            Some(slow_seed_tx),
        )
        .await;
        let (bootstrap_addr, bootstrap_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                values: vec![bootstrap_peer],
                nodes: vec![leaf_addr],
                ..Default::default()
            },
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());
        let _ = drain_observations(&mut leaf_rx).await;
        let _ = drain_observations(&mut slow_seed_rx).await;
        client
            .record_discovered_nodes(&[InternalCompactNode {
                id: test_node_id(36),
                addr: slow_seed_addr,
            }])
            .await;

        let mut stream = client.get_peers(info_hash);
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), slow_seed_rx.recv())
                .await
                .expect("slow seed observation timeout"),
            Some(TestKrpcObservation::GetPeers)
        );

        assert_eq!(
            tokio::time::timeout(Duration::from_millis(150), leaf_rx.recv())
                .await
                .expect("leaf observation timeout"),
            Some(TestKrpcObservation::GetPeers)
        );

        let started_at = tokio::time::Instant::now();
        let first_batch = tokio::time::timeout(Duration::from_millis(150), stream.next())
            .await
            .expect("first partial batch timeout")
            .unwrap_or_default();
        let second_batch = tokio::time::timeout(Duration::from_millis(150), stream.next())
            .await
            .expect("second partial batch timeout")
            .unwrap_or_default();

        let observed = first_batch
            .into_iter()
            .chain(second_batch.into_iter())
            .collect::<HashSet<_>>();
        assert_eq!(observed, HashSet::from([bootstrap_peer, downstream_peer]));
        assert!(
            started_at.elapsed() < Duration::from_millis(250),
            "both streamed batches should arrive before the slow seed could unblock the old scheduler"
        );

        bootstrap_task.abort();
        slow_seed_task.abort();
        leaf_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_replaces_completed_seed_before_hedge_delay() {
        let info_hash = [36u8; 20];
        let downstream_peer = "127.0.0.1:49171".parse().expect("downstream peer");
        let (leaf_tx, mut leaf_rx) = mpsc::unbounded_channel();
        let (leaf_addr, leaf_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("leaf bind addr"),
            TestKrpcReply {
                values: vec![downstream_peer],
                ..Default::default()
            },
            Some(leaf_tx),
        )
        .await;
        let (slow_seed_tx, mut slow_seed_rx) = mpsc::unbounded_channel();
        let (slow_seed_addr, slow_seed_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("slow seed bind addr"),
            TestKrpcReply {
                response_delay: Duration::from_millis(300),
                ..Default::default()
            },
            Some(slow_seed_tx),
        )
        .await;
        let (bootstrap_addr, bootstrap_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                values: Vec::new(),
                nodes: vec![leaf_addr],
                ..Default::default()
            },
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());
        let _ = drain_observations(&mut leaf_rx).await;
        let _ = drain_observations(&mut slow_seed_rx).await;
        client
            .record_discovered_nodes(&[InternalCompactNode {
                id: test_node_id(37),
                addr: slow_seed_addr,
            }])
            .await;

        let mut stream = client.get_peers(info_hash);

        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), slow_seed_rx.recv())
                .await
                .expect("slow seed observation timeout"),
            Some(TestKrpcObservation::GetPeers)
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(50), leaf_rx.recv())
                .await
                .expect("leaf should be queried before hedge delay expires"),
            Some(TestKrpcObservation::GetPeers)
        );

        let first_batch = tokio::time::timeout(Duration::from_millis(150), stream.next())
            .await
            .expect("downstream peer batch timeout")
            .unwrap_or_default();
        assert_eq!(first_batch, vec![downstream_peer]);

        bootstrap_task.abort();
        slow_seed_task.abort();
        leaf_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_prioritizes_newly_discovered_closer_nodes_ahead_of_pending_bootstrap(
    ) {
        let info_hash = [37u8; 20];
        let (leaf_tx, mut leaf_rx) = mpsc::unbounded_channel();
        let (leaf_addr, leaf_task) = spawn_blackhole_test_krpc_server(
            "127.0.0.1:0".parse().expect("leaf bind addr"),
            Some(leaf_tx),
        )
        .await;
        let (slow_tx, mut slow_rx) = mpsc::unbounded_channel();
        let (slow_addr, slow_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("slow bind addr"),
            TestKrpcReply {
                response_delay: Duration::from_millis(300),
                ..Default::default()
            },
            Some(slow_tx),
        )
        .await;
        let (bootstrap_tx, mut bootstrap_rx) = mpsc::unbounded_channel();
        let (bootstrap_addr, bootstrap_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply::default(),
            Some(bootstrap_tx),
        )
        .await;
        let (fast_seed_addr, fast_seed_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("fast seed bind addr"),
            TestKrpcReply {
                nodes: vec![leaf_addr],
                ..Default::default()
            },
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());
        let _ = drain_observations(&mut bootstrap_rx).await;
        client
            .record_discovered_nodes(&[
                InternalCompactNode {
                    id: test_node_id(38),
                    addr: fast_seed_addr,
                },
                InternalCompactNode {
                    id: test_node_id(39),
                    addr: slow_addr,
                },
            ])
            .await;

        let lookup = tokio::spawn({
            let client = client.clone();
            async move { client.query_get_peers(info_hash).await }
        });

        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), slow_rx.recv())
                .await
                .expect("slow seed observation timeout"),
            Some(TestKrpcObservation::GetPeers)
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), leaf_rx.recv())
                .await
                .expect("closer node query timeout"),
            Some(TestKrpcObservation::GetPeers)
        );

        lookup.abort();
        let _ = lookup.await;
        bootstrap_task.abort();
        slow_task.abort();
        fast_seed_task.abort();
        leaf_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_reuses_cached_nodes_after_bootstrap_goes_away() {
        let discovered_peer = "127.0.0.1:49011".parse().expect("discovered peer");
        let (leaf_addr, leaf_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("leaf bind addr"),
            TestKrpcReply {
                values: vec![discovered_peer],
                nodes: Vec::new(),
                nodes6: Vec::new(),
                token: Vec::new(),
                response_delay: Duration::ZERO,
            },
        )
        .await;
        let (bootstrap_addr, bootstrap_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                values: Vec::new(),
                nodes: vec![leaf_addr],
                nodes6: Vec::new(),
                token: Vec::new(),
                response_delay: Duration::ZERO,
            },
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());

        let first_peers = client.query_get_peers([9u8; 20]).await;
        assert_eq!(first_peers, vec![discovered_peer]);

        bootstrap_task.abort();

        let second_peers = client.query_get_peers([9u8; 20]).await;
        assert_eq!(second_peers, vec![discovered_peer]);

        leaf_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_reuses_recent_cached_peer_results() {
        let info_hash = [30u8; 20];
        let discovered_peer = "127.0.0.1:49101".parse().expect("discovered peer");
        let (observation_tx, mut observation_rx) = mpsc::unbounded_channel();
        let (bootstrap_addr, bootstrap_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                values: vec![discovered_peer],
                ..Default::default()
            },
            Some(observation_tx),
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());
        let _ = drain_observations(&mut observation_rx).await;

        let mut first_stream = client.get_peers(info_hash);
        let first_peers = first_stream.next().await.unwrap_or_default();
        assert_eq!(first_peers, vec![discovered_peer]);
        let observations = drain_observations(&mut observation_rx).await;
        assert_eq!(
            observations
                .iter()
                .filter(|observation| matches!(observation, TestKrpcObservation::GetPeers))
                .count(),
            1
        );

        bootstrap_task.abort();

        let mut second_stream = client.get_peers(info_hash);
        let second_peers = second_stream.next().await.unwrap_or_default();
        assert_eq!(second_peers, vec![discovered_peer]);

        let health = client.health_snapshot().await;
        assert_eq!(health.cached_lookup_results, 1);
        assert_eq!(health.inflight_lookups, 0);
    }

    #[tokio::test]
    async fn internal_prototype_query_walks_ipv6_nodes_to_collect_peers() {
        let Ok(ipv6_probe_socket) = UdpSocket::bind("[::1]:0").await else {
            return;
        };
        drop(ipv6_probe_socket);

        let discovered_peer = "[::1]:49021".parse().expect("discovered peer");
        let (leaf_addr, leaf_task) = spawn_test_krpc_server(
            "[::1]:0".parse().expect("leaf bind addr"),
            TestKrpcReply {
                values: vec![discovered_peer],
                nodes: Vec::new(),
                nodes6: Vec::new(),
                token: Vec::new(),
                response_delay: Duration::ZERO,
            },
        )
        .await;
        let (bootstrap_addr, bootstrap_task) = spawn_test_krpc_server(
            "[::1]:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                values: Vec::new(),
                nodes: Vec::new(),
                nodes6: vec![leaf_addr],
                token: Vec::new(),
                response_delay: Duration::ZERO,
            },
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());

        let peers = client.query_get_peers([11u8; 20]).await;

        assert_eq!(peers, vec![discovered_peer]);

        bootstrap_task.abort();
        leaf_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_announce_peer_uses_cached_token_with_explicit_port() {
        let info_hash = [13u8; 20];
        let announce_token = vec![1, 2, 3, 4];
        let (observation_tx, mut observation_rx) = mpsc::unbounded_channel();
        let (bootstrap_addr, bootstrap_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                token: announce_token.clone(),
                ..Default::default()
            },
            Some(observation_tx),
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());

        assert!(client.announce_peer(info_hash, Some(51413)).await);

        let args = recv_announce_observation(&mut observation_rx).await;
        assert_eq!(args.info_hash.as_ref(), info_hash);
        assert_eq!(args.port, 51413);
        assert_eq!(args.implied_port, None);
        assert_eq!(args.token.as_ref(), announce_token.as_slice());

        bootstrap_task.abort();
    }

    #[tokio::test]
    async fn internal_prototype_announce_peer_uses_implied_port_when_unspecified() {
        let info_hash = [17u8; 20];
        let announce_token = vec![9, 8, 7];
        let (observation_tx, mut observation_rx) = mpsc::unbounded_channel();
        let (bootstrap_addr, bootstrap_task) = spawn_observing_test_krpc_server(
            "[::1]:0".parse().expect("bootstrap bind addr"),
            TestKrpcReply {
                token: announce_token.clone(),
                ..Default::default()
            },
            Some(observation_tx),
        )
        .await;

        let (client, warning) = InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()])
            .await
            .expect("client");
        assert!(warning.is_none());

        assert!(client.announce_peer(info_hash, None).await);

        let args = recv_announce_observation(&mut observation_rx).await;
        assert_eq!(args.info_hash.as_ref(), info_hash);
        assert_eq!(args.port, 0);
        assert_eq!(args.implied_port, Some(1));
        assert_eq!(args.token.as_ref(), announce_token.as_slice());

        bootstrap_task.abort();
    }

    #[test]
    fn discovered_nodes_prefer_closer_known_ids_for_target() {
        let mut nodes = InternalPrototypeDiscoveredNodes::default();
        let closer = InternalCompactNode {
            id: test_node_id(1),
            addr: "127.0.0.1:40001".parse().expect("closer addr"),
        };
        let farther = InternalCompactNode {
            id: test_node_id(250),
            addr: "127.0.0.1:40002".parse().expect("farther addr"),
        };
        nodes.insert_all([farther, closer]);

        let ordered = nodes.snapshot_for_family(false, Some([0u8; 20]));

        assert_eq!(ordered, vec![closer.addr, farther.addr]);
    }

    #[test]
    fn discovered_nodes_demote_failed_nodes_even_when_closer() {
        let mut nodes = InternalPrototypeDiscoveredNodes::default();
        let closer = InternalCompactNode {
            id: test_node_id(1),
            addr: "127.0.0.1:40101".parse().expect("closer addr"),
        };
        let farther = InternalCompactNode {
            id: test_node_id(2),
            addr: "127.0.0.1:40102".parse().expect("farther addr"),
        };
        nodes.insert_all([closer, farther]);
        nodes.record_failure(closer.addr);

        let ordered = nodes.snapshot_for_family(false, Some([0u8; 20]));

        assert_eq!(ordered, vec![farther.addr, closer.addr]);
    }

    #[test]
    fn discovered_nodes_evict_routes_after_repeated_failures() {
        let mut nodes = InternalPrototypeDiscoveredNodes::default();
        let route = InternalCompactNode {
            id: test_node_id(5),
            addr: "127.0.0.1:40111".parse().expect("route addr"),
        };
        nodes.insert_all([route]);

        for _ in 0..INTERNAL_DHT_MAX_FAILURES_PER_NODE {
            nodes.record_failure(route.addr);
        }

        let ordered = nodes.snapshot_for_family(false, Some([0u8; 20]));

        assert!(ordered.is_empty());
    }

    #[test]
    fn active_routes_prefer_closer_routes_when_target_known() {
        let closer_addr = "127.0.0.1:40141".parse().expect("closer addr");
        let steadier_addr = "127.0.0.1:40142".parse().expect("steadier addr");
        let mut routes = InternalPrototypeActiveRoutes::default();

        routes.record_success(closer_addr, Some(test_node_id(1)));
        routes.record_success(steadier_addr, Some(test_node_id(8)));
        routes.record_success(steadier_addr, Some(test_node_id(8)));

        let ordered = routes.snapshot_for_family(false, Some([0u8; 20]));

        assert_eq!(ordered, vec![closer_addr, steadier_addr]);
    }

    #[test]
    fn active_routes_prefer_more_successful_routes_when_target_unknown() {
        let closer_addr = "127.0.0.1:40143".parse().expect("closer addr");
        let steadier_addr = "127.0.0.1:40144".parse().expect("steadier addr");
        let mut routes = InternalPrototypeActiveRoutes::default();

        routes.record_success(closer_addr, Some(test_node_id(1)));
        routes.record_success(steadier_addr, Some(test_node_id(8)));
        routes.record_success(steadier_addr, Some(test_node_id(8)));

        let ordered = routes.snapshot_for_family(false, None);

        assert_eq!(ordered, vec![steadier_addr, closer_addr]);
    }

    #[test]
    fn active_route_frontier_prefers_closer_proven_routes_for_target() {
        let closer_addr = "127.0.0.1:40145".parse().expect("closer addr");
        let steadier_addr = "127.0.0.1:40146".parse().expect("steadier addr");
        let mut routes = InternalPrototypeActiveRoutes::default();

        routes.record_lookup_success(closer_addr, Some(test_node_id(1)));
        routes.record_lookup_success(closer_addr, Some(test_node_id(1)));
        routes.record_lookup_success(steadier_addr, Some(test_node_id(8)));
        routes.record_lookup_success(steadier_addr, Some(test_node_id(8)));
        routes.record_lookup_success(steadier_addr, Some(test_node_id(8)));

        let ordered = routes.snapshot_fast_frontier_for_family(false, Some([0u8; 20]));

        assert_eq!(ordered, vec![closer_addr, steadier_addr]);
    }

    #[test]
    fn active_route_frontier_falls_back_to_general_active_routes_when_thin() {
        let known_addr = "127.0.0.1:40149".parse().expect("known addr");
        let unknown_addr = "127.0.0.1:40150".parse().expect("unknown addr");
        let mut routes = InternalPrototypeActiveRoutes::default();

        routes.record_success(known_addr, Some(test_node_id(3)));
        routes.record_success(unknown_addr, None);

        let ordered = routes.snapshot_fast_frontier_for_family(false, Some([0u8; 20]));

        assert_eq!(ordered, vec![known_addr, unknown_addr]);
    }

    #[test]
    fn active_route_frontier_prefers_lookup_proven_routes_before_route_only_routes() {
        let proven_addr = "127.0.0.1:40153".parse().expect("proven addr");
        let route_only_addr = "127.0.0.1:40154".parse().expect("route-only addr");
        let mut routes = InternalPrototypeActiveRoutes::default();

        routes.record_lookup_success(proven_addr, Some(test_node_id(4)));
        routes.record_success(route_only_addr, Some(test_node_id(1)));
        routes.record_success(route_only_addr, Some(test_node_id(1)));
        routes.record_success(route_only_addr, Some(test_node_id(1)));

        let ordered = routes.snapshot_fast_frontier_for_family(false, Some([0u8; 20]));

        assert_eq!(ordered, vec![proven_addr, route_only_addr]);
    }

    #[test]
    fn mature_ipv4_lookups_get_small_fast_fanout_boost() {
        assert_eq!(
            initial_family_query_fanout(
                false,
                "lookup",
                INTERNAL_DHT_FAST_ACTIVE_FRONTIER_READY_FLOOR
            ),
            INTERNAL_DHT_IPV4_FAST_LOOKUP_QUERY_FANOUT
        );
        assert_eq!(
            family_max_concurrent_queries(
                false,
                "lookup",
                INTERNAL_DHT_FAST_ACTIVE_FRONTIER_READY_FLOOR,
                true
            ),
            INTERNAL_DHT_IPV4_FAST_LOOKUP_QUERY_FANOUT
        );
    }

    #[test]
    fn cold_or_non_ipv4_lookups_keep_default_fanout() {
        assert_eq!(
            initial_family_query_fanout(
                false,
                "lookup",
                INTERNAL_DHT_FAST_ACTIVE_FRONTIER_READY_FLOOR - 1
            ),
            INTERNAL_DHT_INITIAL_QUERY_FANOUT
        );
        assert_eq!(
            initial_family_query_fanout(
                true,
                "lookup",
                INTERNAL_DHT_FAST_ACTIVE_FRONTIER_READY_FLOOR
            ),
            INTERNAL_DHT_INITIAL_QUERY_FANOUT
        );
        assert_eq!(
            family_max_concurrent_queries(
                false,
                "lookup",
                INTERNAL_DHT_FAST_ACTIVE_FRONTIER_READY_FLOOR,
                false
            ),
            INTERNAL_DHT_MAX_CONCURRENT_FAMILY_QUERIES
        );
    }

    #[test]
    fn active_routes_evict_routes_that_turn_noisy() {
        let addr = "127.0.0.1:40151".parse().expect("active route addr");
        let mut routes = InternalPrototypeActiveRoutes::default();
        routes.record_success(addr, Some(test_node_id(11)));

        routes.record_failure(addr);
        routes.record_failure(addr);
        routes.record_failure(addr);
        routes.record_failure(addr);
        routes.record_failure(addr);
        routes.record_failure(addr);

        let ordered = routes.snapshot_for_family(false, Some([0u8; 20]));

        assert!(ordered.is_empty());
    }

    #[test]
    fn active_routes_soft_failures_decay_success_before_evicting_route() {
        let addr = "127.0.0.1:40152".parse().expect("soft failure addr");
        let mut routes = InternalPrototypeActiveRoutes::default();
        routes.record_success(addr, Some(test_node_id(12)));
        routes.record_success(addr, Some(test_node_id(12)));

        routes.record_soft_failure(addr);
        assert_eq!(routes.snapshot_for_family(false, None), vec![addr]);

        routes.record_soft_failure(addr);
        assert_eq!(routes.snapshot_for_family(false, None), vec![addr]);

        for _ in 0..5 {
            routes.record_soft_failure(addr);
        }
        assert!(routes.snapshot_for_family(false, None).is_empty());
    }

    #[test]
    fn active_routes_reject_weaker_new_lookup_routes_when_full() {
        let mut routes = InternalPrototypeActiveRoutes::default();
        let existing_addrs = (0..INTERNAL_DHT_IPV4_ACTIVE_ROUTE_LIMIT)
            .map(|idx| format!("127.0.2.{}:{}", (idx % 250) + 1, 41000 + idx))
            .map(|addr| addr.parse::<SocketAddr>().expect("existing addr"))
            .collect::<Vec<_>>();

        for (idx, addr) in existing_addrs.iter().copied().enumerate() {
            routes.record_lookup_success(addr, Some(test_node_id((idx % 200) as u8 + 1)));
            routes.record_lookup_success(addr, Some(test_node_id((idx % 200) as u8 + 1)));
        }

        let candidate_addr = "127.0.3.1:42001".parse().expect("candidate addr");
        routes.record_lookup_success(candidate_addr, Some(test_node_id(201)));

        let ordered = routes.snapshot_for_family(false, None);

        assert_eq!(ordered.len(), INTERNAL_DHT_IPV4_ACTIVE_ROUTE_LIMIT);
        assert!(!ordered.contains(&candidate_addr));
    }

    #[test]
    fn active_routes_admit_lookup_proven_new_bucket_when_full() {
        let mut routes = InternalPrototypeActiveRoutes::default();
        routes.set_ipv4_local_node_id([0u8; 20]);
        let bucket_count = INTERNAL_DHT_IPV4_ACTIVE_ROUTE_LIMIT / INTERNAL_DHT_IPV4_K_BUCKET_SIZE;
        for bucket in 1..=bucket_count {
            for idx in 0..INTERNAL_DHT_IPV4_K_BUCKET_SIZE {
                let addr = format!(
                    "127.0.4.{}:{}",
                    ((bucket - 1) * INTERNAL_DHT_IPV4_K_BUCKET_SIZE + idx) % 250 + 1,
                    42000 + (bucket - 1) * INTERNAL_DHT_IPV4_K_BUCKET_SIZE + idx
                )
                .parse::<SocketAddr>()
                .expect("existing addr");
                let bucket_key = (bucket as u8) * 8;
                let node_id = test_bucketed_node_id(bucket_key, idx as u8 + 1);
                routes.record_lookup_success(addr, Some(node_id));
                routes.record_lookup_success(addr, Some(node_id));
            }
        }

        let candidate_addr = "127.0.5.1:43001".parse().expect("candidate addr");
        routes.record_lookup_success(
            candidate_addr,
            Some(test_bucketed_node_id((bucket_count as u8 + 1) * 8, 99)),
        );

        assert_eq!(routes.family_count(false), INTERNAL_DHT_IPV4_ACTIVE_ROUTE_LIMIT);
        assert!(routes.contains(candidate_addr));
    }

    #[test]
    fn active_routes_do_not_admit_route_only_new_bucket_when_full() {
        let mut routes = InternalPrototypeActiveRoutes::default();
        routes.set_ipv4_local_node_id([0u8; 20]);
        let bucket_count = INTERNAL_DHT_IPV4_ACTIVE_ROUTE_LIMIT / INTERNAL_DHT_IPV4_K_BUCKET_SIZE;
        for bucket in 1..=bucket_count {
            for idx in 0..INTERNAL_DHT_IPV4_K_BUCKET_SIZE {
                let addr = format!(
                    "127.0.6.{}:{}",
                    ((bucket - 1) * INTERNAL_DHT_IPV4_K_BUCKET_SIZE + idx) % 250 + 1,
                    44000 + (bucket - 1) * INTERNAL_DHT_IPV4_K_BUCKET_SIZE + idx
                )
                .parse::<SocketAddr>()
                .expect("existing addr");
                let bucket_key = (bucket as u8) * 8;
                let node_id = test_bucketed_node_id(bucket_key, idx as u8 + 1);
                routes.record_lookup_success(addr, Some(node_id));
                routes.record_lookup_success(addr, Some(node_id));
            }
        }

        let candidate_addr = "127.0.7.1:45001".parse().expect("candidate addr");
        routes.record_success(
            candidate_addr,
            Some(test_bucketed_node_id((bucket_count as u8 + 1) * 8, 99)),
        );

        assert_eq!(routes.family_count(false), INTERNAL_DHT_IPV4_ACTIVE_ROUTE_LIMIT);
        assert!(!routes.contains(candidate_addr));
    }

    #[test]
    fn active_routes_cap_each_ipv4_bucket() {
        let mut routes = InternalPrototypeActiveRoutes::default();
        routes.set_ipv4_local_node_id([0u8; 20]);

        for idx in 0..(INTERNAL_DHT_IPV4_K_BUCKET_SIZE + 5) {
            let addr = format!("127.0.8.{}:{}", (idx % 250) + 1, 46000 + idx)
                .parse::<SocketAddr>()
                .expect("bucket addr");
            let node_id = test_bucketed_node_id(16, idx as u8 + 1);
            routes.record_lookup_success(addr, Some(node_id));
            routes.record_lookup_success(addr, Some(node_id));
        }

        assert_eq!(routes.family_count(false), INTERNAL_DHT_IPV4_K_BUCKET_SIZE);
    }

    #[tokio::test]
    async fn query_success_reuses_discovered_node_id_for_active_route_promotion() {
        let addr = "127.0.0.1:40161".parse().expect("route addr");
        let (client, warning) = InternalPrototypeClient::bind(0, &[]).await.expect("client");
        assert!(warning.is_none());
        client
            .record_discovered_nodes(&[InternalCompactNode {
                id: test_node_id(14),
                addr,
            }])
            .await;

        client.record_lookup_success(addr, None).await;

        let discovered_nodes = client.discovered_nodes.lock().await;
        assert_eq!(discovered_nodes.node_id_for(addr), Some(test_node_id(14)));
        drop(discovered_nodes);

        let active_routes = client.active_routes.lock().await;
        let ordered = active_routes.snapshot_fast_frontier_for_family(false, Some([0u8; 20]));
        assert_eq!(ordered, vec![addr]);
    }

    #[tokio::test]
    async fn route_refresh_success_does_not_promote_new_active_route() {
        let addr = "127.0.0.1:40162".parse().expect("refresh route addr");
        let (client, warning) = InternalPrototypeClient::bind(0, &[]).await.expect("client");
        assert!(warning.is_none());
        client
            .record_discovered_nodes(&[InternalCompactNode {
                id: test_node_id(16),
                addr,
            }])
            .await;

        client.record_route_refresh_success(addr, None).await;

        let active_routes = client.active_routes.lock().await;
        assert_eq!(
            active_routes.snapshot_for_family(false, Some([0u8; 20])),
            vec![addr]
        );
        drop(active_routes);

        let discovered_nodes = client.discovered_nodes.lock().await;
        assert_eq!(discovered_nodes.node_id_for(addr), Some(test_node_id(16)));
    }

    #[tokio::test]
    async fn route_refresh_success_only_refills_when_active_pool_is_thin() {
        let refill_addr = "127.0.0.1:40171".parse().expect("refill route addr");
        let existing_addrs = (0..INTERNAL_DHT_ACTIVE_ROUTE_REFILL_FLOOR)
            .map(|idx| format!("127.0.1.{}:{}", idx + 1, 40200 + idx))
            .map(|addr| addr.parse::<SocketAddr>().expect("existing addr"))
            .collect::<Vec<_>>();
        let (client, warning) = InternalPrototypeClient::bind(0, &[]).await.expect("client");
        assert!(warning.is_none());
        let local_node_id = client.node_id;
        client
            .record_discovered_nodes(&[InternalCompactNode {
                id: test_node_id(17),
                addr: refill_addr,
            }])
            .await;

        for (idx, addr) in existing_addrs.iter().copied().enumerate() {
            let bucket_key = ((idx % 8) as u8 + 1) * 8;
            client
                .record_lookup_success(
                    addr,
                    Some(test_bucketed_node_id_for_local(
                        local_node_id,
                        bucket_key,
                        (idx / 8) as u8 + 1,
                    )),
                )
                .await;
        }

        client.record_route_refresh_success(refill_addr, None).await;

        let active_routes = client.active_routes.lock().await;
        assert!(
            !active_routes
                .snapshot_for_family(false, Some([0u8; 20]))
                .contains(&refill_addr)
        );
    }

    #[test]
    fn announce_tokens_prefer_low_failure_high_success_recent_records() {
        let info_hash = [21u8; 20];
        let preferred_addr = "127.0.0.1:40121".parse().expect("preferred token addr");
        let noisy_addr = "127.0.0.1:40122".parse().expect("noisy token addr");
        let mut tokens = InternalPrototypeAnnounceTokens::default();
        tokens.insert(noisy_addr, info_hash, vec![1]);
        tokens.insert(preferred_addr, info_hash, vec![2]);
        tokens.record_failure(noisy_addr, info_hash);
        tokens.record_success(preferred_addr, info_hash);

        let ordered = tokens.snapshot_for_family(info_hash, false);

        assert_eq!(ordered.len(), 2);
        assert_eq!(ordered[0].addr, preferred_addr);
        assert_eq!(ordered[1].addr, noisy_addr);
    }

    #[test]
    fn announce_tokens_evict_records_after_repeated_failures() {
        let info_hash = [22u8; 20];
        let addr = "127.0.0.1:40131".parse().expect("announce token addr");
        let mut tokens = InternalPrototypeAnnounceTokens::default();
        tokens.insert(addr, info_hash, vec![3, 4, 5]);

        for _ in 0..INTERNAL_DHT_MAX_FAILURES_PER_NODE {
            tokens.record_failure(addr, info_hash);
        }

        let ordered = tokens.snapshot_for_family(info_hash, false);

        assert!(ordered.is_empty());
    }

    #[tokio::test]
    async fn internal_prototype_health_reports_discovered_node_count_as_size_estimate() {
        let (client, warning) = InternalPrototypeClient::bind(0, &[]).await.expect("client");
        assert!(warning.is_none());
        client
            .record_discovered_nodes(&[
                InternalCompactNode {
                    id: test_node_id(3),
                    addr: "127.0.0.1:40201".parse().expect("v4 node"),
                },
                InternalCompactNode {
                    id: test_node_id(4),
                    addr: "[::1]:40202".parse().expect("v6 node"),
                },
            ])
            .await;

        let health = client.health_snapshot().await;

        assert_eq!(health.exported_bootstrap_nodes, 2);
        assert_eq!(
            health.dht_size_estimate,
            Some(DhtSizeEstimate {
                node_count: 2,
                std_dev: None,
            })
        );
        assert_eq!(health.cached_ipv4_routes, 1);
        assert_eq!(health.cached_ipv6_routes, 1);
        assert_eq!(health.active_ipv4_routes, 0);
        assert_eq!(health.active_ipv6_routes, 0);
        assert_eq!(health.cached_ipv4_announce_tokens, 0);
        assert_eq!(health.cached_ipv6_announce_tokens, 0);
    }

    #[tokio::test]
    async fn internal_prototype_health_reports_active_routes_by_family() {
        let (client, warning) = InternalPrototypeClient::bind(0, &[]).await.expect("client");
        assert!(warning.is_none());
        client
            .record_lookup_success(
                "127.0.0.1:40221".parse().expect("v4 active route"),
                Some(test_node_id(12)),
            )
            .await;
        client
            .record_lookup_success(
                "[::1]:40222".parse().expect("v6 active route"),
                Some(test_node_id(13)),
            )
            .await;

        let health = client.health_snapshot().await;

        assert_eq!(health.active_ipv4_routes, 1);
        assert_eq!(health.active_ipv6_routes, 1);
    }

    #[tokio::test]
    async fn lookup_failures_do_not_immediately_demote_active_routes() {
        let (client, warning) = InternalPrototypeClient::bind(0, &[]).await.expect("client");
        assert!(warning.is_none());
        let addr = "127.0.0.1:40223".parse().expect("lookup route");

        client.record_lookup_success(addr, Some(test_node_id(15))).await;
        client.record_lookup_failure(addr).await;

        let health = client.health_snapshot().await;

        assert_eq!(health.active_ipv4_routes, 1);
        assert_eq!(health.cached_ipv4_routes, 1);
    }

    #[tokio::test]
    async fn internal_prototype_health_reports_cached_announce_tokens_by_family() {
        let info_hash = [23u8; 20];
        let (client, warning) = InternalPrototypeClient::bind(0, &[]).await.expect("client");
        assert!(warning.is_none());
        client
            .record_announce_token(
                "127.0.0.1:40211".parse().expect("v4 token addr"),
                info_hash,
                &[1, 2, 3],
            )
            .await;
        client
            .record_announce_token(
                "[::1]:40212".parse().expect("v6 token addr"),
                info_hash,
                &[4, 5, 6],
            )
            .await;

        let health = client.health_snapshot().await;

        assert_eq!(health.cached_ipv4_announce_tokens, 1);
        assert_eq!(health.cached_ipv6_announce_tokens, 1);
    }

    #[tokio::test]
    async fn internal_prototype_health_reports_inflight_queries_by_family() {
        let info_hash = [24u8; 20];
        let (ipv4_tx, mut ipv4_rx) = mpsc::unbounded_channel();
        let (ipv4_addr, ipv4_task) = spawn_observing_test_krpc_server(
            "127.0.0.1:0".parse().expect("ipv4 bind addr"),
            TestKrpcReply {
                response_delay: Duration::from_millis(250),
                ..Default::default()
            },
            Some(ipv4_tx),
        )
        .await;
        let Ok(ipv6_probe_socket) = UdpSocket::bind("[::1]:0").await else {
            ipv4_task.abort();
            return;
        };
        drop(ipv6_probe_socket);
        let (ipv6_tx, mut ipv6_rx) = mpsc::unbounded_channel();
        let (ipv6_addr, ipv6_task) = spawn_observing_test_krpc_server(
            "[::1]:0".parse().expect("ipv6 bind addr"),
            TestKrpcReply {
                response_delay: Duration::from_millis(250),
                ..Default::default()
            },
            Some(ipv6_tx),
        )
        .await;
        let (client, warning) = InternalPrototypeClient::bind(0, &[]).await.expect("client");
        assert!(warning.is_none());
        client
            .record_discovered_nodes(&[
                InternalCompactNode {
                    id: test_node_id(25),
                    addr: ipv4_addr,
                },
                InternalCompactNode {
                    id: test_node_id(26),
                    addr: ipv6_addr,
                },
            ])
            .await;

        let _stream = client.get_peers(info_hash);

        let ipv4_observation = tokio::time::timeout(Duration::from_millis(100), ipv4_rx.recv())
            .await
            .expect("ipv4 observation timeout")
            .expect("ipv4 observation");
        assert_eq!(ipv4_observation, TestKrpcObservation::GetPeers);

        let ipv6_observation = tokio::time::timeout(Duration::from_millis(100), ipv6_rx.recv())
            .await
            .expect("ipv6 observation timeout")
            .expect("ipv6 observation");
        assert_eq!(ipv6_observation, TestKrpcObservation::GetPeers);

        let health = client.health_snapshot().await;

        assert_eq!(health.inflight_lookups, 1);
        assert_eq!(health.inflight_ipv4_queries, 1);
        assert_eq!(health.inflight_ipv6_queries, 1);

        ipv4_task.abort();
        ipv6_task.abort();
    }

    #[test]
    fn internal_prototype_ignores_unparseable_bootstrap_nodes() {
        let state = InternalPrototypeState::from_bootstrap_nodes(&[
            "127.0.0.1:6881".to_string(),
            "[::1]:6881".to_string(),
            "not-an-address".to_string(),
        ]);

        assert_eq!(state.ipv4_bootstrap_nodes.len(), 1);
        assert_eq!(state.ipv6_bootstrap_nodes.len(), 1);
    }

    proptest! {
        #[test]
        fn internal_prototype_keeps_ipv4_and_ipv6_bootstrap_nodes_separated(
            nodes in proptest::collection::vec((any::<bool>(), 1u16..=65535, 1u16..=65535), 0..32)
        ) {
            let raw_nodes = nodes
                .iter()
                .enumerate()
                .map(|(idx, (is_v6, port_a, port_b))| {
                    if *is_v6 {
                        format!("[2001:db8::{:x}]:{}", idx + 1, port_a)
                    } else {
                        format!("10.0.{}.{}:{}", idx % 200, (idx / 200) + 1, port_b)
                    }
                })
                .collect::<Vec<_>>();
            let state = InternalPrototypeState::from_bootstrap_nodes(&raw_nodes);
            let expected_v4 = raw_nodes.iter().filter(|node| !node.starts_with('[')).count();
            let expected_v6 = raw_nodes.iter().filter(|node| node.starts_with('[')).count();

            prop_assert_eq!(state.ipv4_bootstrap_nodes.len(), expected_v4);
            prop_assert_eq!(state.ipv6_bootstrap_nodes.len(), expected_v6);
        }
    }

    #[test]
    fn backend_override_aliases_parse_to_expected_variants() {
        assert_eq!(
            DhtBackendKind::from_override("mainline"),
            Some(DhtBackendKind::Mainline)
        );
        assert_eq!(
            DhtBackendKind::from_override("internal-prototype"),
            Some(DhtBackendKind::InternalPrototype)
        );
        assert_eq!(
            DhtBackendKind::from_override("off"),
            Some(DhtBackendKind::Disabled)
        );
        assert_eq!(DhtBackendKind::from_override("unknown"), None);
    }

    #[test]
    fn sanitize_dht_size_estimate_drops_non_finite_std_dev() {
        let sanitized = sanitize_dht_size_estimate((42, f64::NAN));

        assert_eq!(sanitized.node_count, 42);
        assert_eq!(sanitized.std_dev, None);
    }
}
