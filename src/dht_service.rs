// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::config::Settings;
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use std::cmp::Ordering;
use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::lookup_host;
use tokio::net::UdpSocket;
use tokio::sync::broadcast;
use tokio::sync::mpsc::{self, Sender};
use tokio::sync::Mutex;
use tokio::sync::watch;
use tokio::task::JoinHandle;
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

const DHT_LOOKUP_REFRESH_INTERVAL: Duration = Duration::from_secs(300);
const DHT_RETRY_INTERVAL: Duration = Duration::from_secs(60);
const DHT_HEALTH_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const INTERNAL_DHT_QUERY_TIMEOUT: Duration = Duration::from_millis(400);
const INTERNAL_DHT_SOCKET_BUFFER: usize = 2048;
const INTERNAL_DHT_MAX_VISITS_PER_FAMILY: usize = 8;
const INTERNAL_DHT_MAX_RETURNED_PEERS: usize = 64;
const INTERNAL_DHT_HEALTH_PROBE_LIMIT: usize = 4;
const INTERNAL_DHT_DISCOVERED_NODE_LIMIT: usize = 64;
const INTERNAL_DHT_SEED_NODE_LIMIT: usize = 16;
const INTERNAL_DHT_ROUTE_WARM_LIMIT: usize = 2;
const INTERNAL_DHT_MAX_FAILURES_PER_NODE: u16 = 3;
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
                .unwrap_or(DhtBackendKind::Mainline),
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DhtHealthSnapshot {
    pub backend: DhtBackendKind,
    pub enabled: bool,
    pub local_addr: Option<SocketAddr>,
    pub ipv4_local_addr: Option<SocketAddr>,
    pub ipv6_local_addr: Option<SocketAddr>,
    pub bound_family_count: usize,
    pub cached_ipv4_routes: usize,
    pub cached_ipv6_routes: usize,
    pub cached_ipv4_announce_tokens: usize,
    pub cached_ipv6_announce_tokens: usize,
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
}

#[derive(Debug, Clone)]
struct InternalPrototypeClient {
    state: InternalPrototypeState,
    sockets: InternalPrototypeSockets,
    node_id: [u8; 20],
    discovered_nodes: Arc<Mutex<InternalPrototypeDiscoveredNodes>>,
    announce_tokens: Arc<Mutex<InternalPrototypeAnnounceTokens>>,
}

impl InternalPrototypeClient {
    async fn bind(port: u16, bootstrap_nodes: &[String]) -> Result<(Self, Option<String>), String> {
        let mut state = resolve_bootstrap_nodes(bootstrap_nodes).await;
        let (sockets, warning) = InternalPrototypeSockets::bind(port).await?;
        state.ipv4_local_addr = sockets.ipv4_local_addr();
        state.ipv6_local_addr = sockets.ipv6_local_addr();

        let client = Self {
            state,
            sockets,
            node_id: random(),
            discovered_nodes: Arc::new(Mutex::new(InternalPrototypeDiscoveredNodes::default())),
            announce_tokens: Arc::new(Mutex::new(InternalPrototypeAnnounceTokens::default())),
        };
        client.warm_routes().await;

        Ok((client, warning))
    }

    async fn query_get_peers(&self, info_hash: [u8; 20]) -> Vec<SocketAddr> {
        let (ipv4_peers, ipv6_peers) = tokio::join!(
            self.query_family_get_peers(
                self.sockets.ipv4.as_ref(),
                &self.state.ipv4_bootstrap_nodes,
                info_hash,
                false,
            ),
            self.query_family_get_peers(
                self.sockets.ipv6.as_ref(),
                &self.state.ipv6_bootstrap_nodes,
                info_hash,
                true,
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
    ) -> HashSet<SocketAddr> {
        let Some(socket) = socket else {
            return HashSet::new();
        };

        let mut pending = self
            .seed_family_nodes(bootstrap_nodes, is_ipv6, Some(info_hash))
            .await;
        let mut visited = HashSet::new();
        let mut peers = HashSet::new();

        while let Some(node_addr) = pending.pop_front() {
            if !visited.insert(node_addr) {
                continue;
            }
            if visited.len() > INTERNAL_DHT_MAX_VISITS_PER_FAMILY {
                break;
            }

            let Some(response) = socket.get_peers(node_addr, &self.node_id, &info_hash).await else {
                self.record_query_failure(node_addr).await;
                continue;
            };
            self.record_query_success(node_addr, response.node_id()).await;
            self.record_announce_token(node_addr, info_hash, response.token.as_ref()).await;

            for compact_peer in response.values {
                for peer_addr in decode_compact_peers(compact_peer.as_ref(), is_ipv6) {
                    peers.insert(peer_addr);
                    if peers.len() >= INTERNAL_DHT_MAX_RETURNED_PEERS {
                        return peers;
                    }
                }
            }

            let next_nodes = if is_ipv6 {
                decode_compact_nodes(response.nodes6.as_ref(), true)
            } else {
                decode_compact_nodes(response.nodes.as_ref(), false)
            };
            self.record_discovered_nodes(&next_nodes).await;

            for next_node in next_nodes {
                if visited.contains(&next_node.addr) || pending.contains(&next_node.addr) {
                    continue;
                }
                pending.push_back(next_node.addr);
                if pending.len() + visited.len() >= INTERNAL_DHT_MAX_VISITS_PER_FAMILY {
                    break;
                }
            }
        }

        peers
    }

    async fn seed_family_nodes(
        &self,
        bootstrap_nodes: &HashSet<SocketAddr>,
        is_ipv6: bool,
        target: Option<[u8; 20]>,
    ) -> VecDeque<SocketAddr> {
        let cached_nodes = self
            .discovered_nodes
            .lock()
            .await
            .snapshot_for_family(is_ipv6, target);
        let mut pending = bootstrap_nodes.iter().copied().collect::<VecDeque<_>>();
        for cached_node in cached_nodes.into_iter().take(INTERNAL_DHT_SEED_NODE_LIMIT) {
            if !pending.contains(&cached_node) {
                pending.push_back(cached_node);
            }
        }
        pending
    }

    async fn record_discovered_nodes(&self, nodes: &[InternalCompactNode]) {
        let mut discovered_nodes = self.discovered_nodes.lock().await;
        discovered_nodes.insert_all(nodes.iter().copied());
    }

    async fn record_query_success(&self, addr: SocketAddr, node_id: Option<[u8; 20]>) {
        let mut discovered_nodes = self.discovered_nodes.lock().await;
        discovered_nodes.record_success(addr, node_id);
    }

    async fn record_query_failure(&self, addr: SocketAddr) {
        let mut discovered_nodes = self.discovered_nodes.lock().await;
        discovered_nodes.record_failure(addr);
    }

    async fn record_announce_token(&self, addr: SocketAddr, info_hash: [u8; 20], token: &[u8]) {
        if token.is_empty() {
            return;
        }
        let mut announce_tokens = self.announce_tokens.lock().await;
        announce_tokens.insert(addr, info_hash, token.to_vec());
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
                .query_family_get_peers(socket.into(), bootstrap_nodes, info_hash, is_ipv6)
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
                self.record_query_success(token.addr, None).await;
                self.announce_tokens
                    .lock()
                    .await
                    .record_success(token.addr, info_hash);
            } else {
                self.record_query_failure(token.addr).await;
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
        let (tx, rx) = mpsc::channel(2);
        let client = self.clone();
        tokio::spawn(async move {
            let peers = client.query_get_peers(info_hash).await;
            if !peers.is_empty() {
                let _ = tx.send(peers).await;
            }
        });

        Box::pin(ReceiverStream::new(rx))
    }

    fn health_snapshot(&self) -> HealthFuture {
        let client = self.clone();
        Box::pin(async move {
            let responsive = client.probe_bootstrap_nodes().await;
            let discovered_nodes = client.discovered_nodes.lock().await;
            let announce_tokens = client.announce_tokens.lock().await;
            let exported_bootstrap_nodes = discovered_nodes.total_count();
            DhtHealthSnapshot {
                backend: DhtBackendKind::InternalPrototype,
                enabled: true,
                local_addr: client.state.ipv4_local_addr.or(client.state.ipv6_local_addr),
                ipv4_local_addr: client.state.ipv4_local_addr,
                ipv6_local_addr: client.state.ipv6_local_addr,
                bound_family_count: usize::from(client.state.ipv4_local_addr.is_some())
                    + usize::from(client.state.ipv6_local_addr.is_some()),
                cached_ipv4_routes: discovered_nodes.family_count(false),
                cached_ipv6_routes: discovered_nodes.family_count(true),
                cached_ipv4_announce_tokens: announce_tokens.family_count(false),
                cached_ipv6_announce_tokens: announce_tokens.family_count(true),
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

#[derive(Debug, Default)]
struct InternalPrototypeDiscoveredNodes {
    ipv4: VecDeque<InternalPrototypeNodeRecord>,
    ipv6: VecDeque<InternalPrototypeNodeRecord>,
}

#[derive(Debug, Default)]
struct InternalPrototypeAnnounceTokens {
    ipv4: VecDeque<InternalAnnounceTokenRecord>,
    ipv6: VecDeque<InternalAnnounceTokenRecord>,
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
            recency_epoch: tokens.back().map_or(0, |last| last.recency_epoch.saturating_add(1)),
        });
        while tokens.len() > INTERNAL_DHT_TOKEN_CACHE_LIMIT {
            tokens.pop_front();
        }
    }

    fn has_family_token(&self, info_hash: [u8; 20], is_ipv6: bool) -> bool {
        self.snapshot_for_family(info_hash, is_ipv6).first().is_some()
    }

    fn snapshot_for_family(&self, info_hash: [u8; 20], is_ipv6: bool) -> Vec<InternalAnnounceTokenRecord> {
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

impl InternalPrototypeDiscoveredNodes {
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

    fn get_or_insert_record(&mut self, addr: SocketAddr) -> Option<&mut InternalPrototypeNodeRecord> {
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

        family_nodes.iter_mut().find(|existing| existing.addr == addr)
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
        let distance_order = compare_node_distance(left.node_id.as_ref(), right.node_id.as_ref(), target);
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
    socket: Arc<UdpSocket>,
    gate: Arc<Mutex<()>>,
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
        Self {
            socket: Arc::new(socket),
            gate: Arc::new(Mutex::new(())),
        }
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.socket.local_addr().ok()
    }

    async fn ping(&self, target: SocketAddr, node_id: &[u8; 20]) -> bool {
        self.send_query(
            target,
            "ping",
            PingArgs { id: node_id.as_ref() },
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
        self.send_query(
            target,
            "find_node",
            FindNodeArgs {
                id: node_id.as_ref(),
                target: lookup_target.as_ref(),
            },
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

    async fn send_query<A>(&self, target: SocketAddr, query: &'static str, args: A) -> Option<KrpcResponseBody>
    where
        A: Serialize,
    {
        let transaction_id = random::<u32>().to_be_bytes();
        let payload = serde_bencode::to_bytes(&KrpcQueryEnvelope {
            t: transaction_id.as_slice(),
            y: "q",
            q: query,
            a: args,
        })
        .ok()?;

        let _guard = self.gate.lock().await;
        self.socket.send_to(&payload, target).await.ok()?;

        let mut buffer = [0u8; INTERNAL_DHT_SOCKET_BUFFER];
        timeout(INTERNAL_DHT_QUERY_TIMEOUT, async {
            loop {
                let (len, source_addr) = self.socket.recv_from(&mut buffer).await.ok()?;
                if source_addr != target {
                    continue;
                }

                let response =
                    serde_bencode::from_bytes::<KrpcResponseEnvelope>(&buffer[..len]).ok()?;
                if response.t.as_ref() != transaction_id.as_slice() {
                    continue;
                }
                if response.y.as_ref() != b"r" {
                    return None;
                }

                return response.r;
            }
        })
        .await
        .ok()
        .flatten()
    }
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

    async fn warm_family_routes(
        &self,
        socket: Option<&InternalPrototypeFamilySocket>,
        bootstrap_nodes: &HashSet<SocketAddr>,
        is_ipv6: bool,
    ) -> Vec<InternalCompactNode> {
        let Some(socket) = socket else {
            return Vec::new();
        };

        let mut discovered = Vec::new();
        for bootstrap_node in bootstrap_nodes
            .iter()
            .copied()
            .take(INTERNAL_DHT_ROUTE_WARM_LIMIT)
        {
            let Some(response) = socket.find_node(bootstrap_node, &self.node_id, &self.node_id).await else {
                self.record_query_failure(bootstrap_node).await;
                continue;
            };
            self.record_query_success(bootstrap_node, response.node_id()).await;

            let nodes = if is_ipv6 {
                decode_compact_nodes(response.nodes6.as_ref(), true)
            } else {
                decode_compact_nodes(response.nodes.as_ref(), false)
            };
            discovered.extend(nodes);
        }

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

        let ipv6 = match UdpSocket::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port)).await
        {
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

        let ipv4 = match UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), ipv4_port)).await
        {
            Ok(socket) => Some(InternalPrototypeFamilySocket::new(socket)),
            Err(error) if ipv6.is_some() && error.kind() == io::ErrorKind::AddrInUse => None,
            Err(error) => {
                warnings.push(format!("IPv4 UDP bind failed: {}", error));
                None
            }
        };

        if ipv4.is_none() && ipv6.is_none() {
            return Err("Failed to bind IPv4 and IPv6 UDP sockets for internal DHT backend.".to_string());
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
        self.ipv4.as_ref().and_then(InternalPrototypeFamilySocket::local_addr)
    }

    fn ipv6_local_addr(&self) -> Option<SocketAddr> {
        self.ipv6.as_ref().and_then(InternalPrototypeFamilySocket::local_addr)
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
        let initial = build_runtime(&config, 0, false).await?;
        let initial_status = build_status(&initial.runtime, initial.warning.clone()).await;
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

                tokio::select! {
                    _ = shutdown_rx.recv() => break,
                    _ = async {
                        while let Some(peers) = peers_stream.next().await {
                            if dht_tx.send(peers).await.is_err() {
                                return;
                            }
                        }
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

    pub async fn announce_peer(&self, info_hash: Vec<u8>, port: Option<u16>) -> bool {
        let Ok(info_hash) = <[u8; 20]>::try_from(info_hash) else {
            return false;
        };
        let runtime = self.runtime_rx.borrow().clone();
        runtime.client.announce_peer(info_hash, port).await
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
                        config = new_config;
                        current_generation = current_generation.saturating_add(1);
                        match build_runtime(&config, current_generation, true).await {
                            Ok(next_runtime) => {
                                let status = build_status(&next_runtime.runtime, next_runtime.warning).await;
                                let _ = runtime_tx.send(next_runtime.runtime);
                                let _ = status_tx.send(status);
                            }
                            Err(error) => {
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
                if let Ok(Some(next_runtime)) = try_recover_preferred_runtime(&config, current_generation.saturating_add(1)).await {
                    current_generation = next_runtime.runtime.generation;
                    let status = build_status(&next_runtime.runtime, next_runtime.warning.clone()).await;
                    let _ = runtime_tx.send(next_runtime.runtime);
                    let _ = status_tx.send(status);
                }
            }
            _ = health_interval.tick() => {
                let runtime = runtime_tx.borrow().clone();
                let warning = status_tx.borrow().warning.clone();
                let status = build_status(&runtime, warning).await;
                let _ = status_tx.send(status);
            }
        }
    }
}

async fn build_status(runtime: &DhtRuntimeState, warning: Option<String>) -> DhtStatus {
    DhtStatus {
        generation: runtime.generation,
        warning,
        health: runtime.client.health_snapshot().await,
    }
}

async fn build_runtime(
    config: &DhtServiceConfig,
    generation: u64,
    allow_disabled_fallback: bool,
) -> Result<BuiltRuntime, String> {
    let (client, warning) = match config.preferred_backend {
        DhtBackendKind::Disabled => (
            Arc::new(DisabledDhtClient) as Arc<dyn DhtBackendClient>,
            None,
        ),
        DhtBackendKind::InternalPrototype => {
            build_internal_runtime(config, allow_disabled_fallback).await?
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
) -> Result<(Arc<dyn DhtBackendClient>, Option<String>), String> {
    match InternalPrototypeClient::bind(config.port, &config.bootstrap_nodes).await {
        Ok((client, warning)) => Ok((Arc::new(client) as Arc<dyn DhtBackendClient>, warning)),
        Err(error) if allow_disabled_fallback => Ok((
            Arc::new(DisabledDhtClient) as Arc<dyn DhtBackendClient>,
            Some(format!(
                "Warning: internal DHT backend unavailable ({}). Running with DHT disabled until reconfigured.",
                error
            )),
        )),
        Err(error) => Err(error),
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
) -> Result<Option<BuiltRuntime>, String> {
    match config.preferred_backend {
        DhtBackendKind::InternalPrototype => match build_internal_runtime(config, false).await {
            Ok((client, warning)) => Ok(Some(BuiltRuntime {
                runtime: DhtRuntimeState {
                    generation,
                    client,
                },
                warning,
            })),
            Err(_) => Ok(None),
        },
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
) -> Result<Option<BuiltRuntime>, String> {
    match config.preferred_backend {
        DhtBackendKind::InternalPrototype => match build_internal_runtime(config, false).await {
            Ok((client, warning)) => Ok(Some(BuiltRuntime {
                runtime: DhtRuntimeState { generation, client },
                warning,
            })),
            Err(_) => Ok(None),
        },
        DhtBackendKind::Disabled | DhtBackendKind::Mainline => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
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
        AnnouncePeer(TestAnnouncePeerArgs),
    }

    #[derive(Debug, Clone, Default)]
    struct TestKrpcReply {
        values: Vec<SocketAddr>,
        nodes: Vec<SocketAddr>,
        nodes6: Vec<SocketAddr>,
        token: Vec<u8>,
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
        let socket = UdpSocket::bind(bind_addr).await.expect("bind test krpc socket");
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
                    "ping" => KrpcResponseBody::default(),
                    "find_node" => KrpcResponseBody {
                        id: ByteBuf::from(test_node_id(98).to_vec()),
                        token: ByteBuf::from(reply.token.clone()),
                        values: Vec::new(),
                        nodes: encode_compact_nodes(&reply.nodes),
                        nodes6: encode_compact_nodes(&reply.nodes6),
                    },
                    "get_peers" => KrpcResponseBody {
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
                    },
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
                if socket.send_to(&payload, source_addr).await.is_err() {
                    break;
                }
            }
        });

        (local_addr, task)
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
        });

        tokio::time::timeout(Duration::from_secs(1), status_rx.changed())
            .await
            .expect("status change timeout")
            .expect("status change");

        let status = status_rx.borrow().clone();
        assert_eq!(status.generation, 1);
        assert_eq!(status.health.backend, DhtBackendKind::Disabled);
        assert!(status.warning.is_none());

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn internal_prototype_recovery_attempt_rebuilds_runtime() {
        let recovered = try_recover_preferred_runtime(
            &DhtServiceConfig {
                port: 0,
                bootstrap_nodes: Vec::new(),
                preferred_backend: DhtBackendKind::InternalPrototype,
            },
            7,
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

    #[tokio::test]
    async fn internal_prototype_service_reports_bound_udp_family_health() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = DhtService::new(
            DhtServiceConfig {
                port: 0,
                bootstrap_nodes: vec!["127.0.0.1:6881".to_string(), "[::1]:6881".to_string()],
                preferred_backend: DhtBackendKind::InternalPrototype,
            },
            shutdown_tx.subscribe(),
        )
        .await
        .expect("internal prototype service");

        let status = service.current_status();

        assert_eq!(status.health.backend, DhtBackendKind::InternalPrototype);
        assert!(status.health.enabled);
        assert!(status.health.bound_family_count >= 1);
        assert!(status.health.local_addr.is_some());
        assert_eq!(status.health.ipv4_bootstrap_nodes, 1);
        assert_eq!(status.health.ipv6_bootstrap_nodes, 1);
        if let (Some(ipv4), Some(ipv6)) = (status.health.ipv4_local_addr, status.health.ipv6_local_addr) {
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
            },
        )
        .await;

        let (client, warning) =
            InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()]).await.expect("client");
        assert!(warning.is_none());

        let probe = client.probe_bootstrap_nodes().await;

        assert_eq!(probe.ipv4.len(), 1);
        assert!(probe.ipv4.contains(&bootstrap_addr));
        assert!(probe.ipv6.is_empty());

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
            },
        )
        .await;

        let (client, warning) =
            InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()]).await.expect("client");
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
    async fn internal_prototype_query_walks_bootstrap_nodes_to_collect_peers() {
        let discovered_peer = "127.0.0.1:49001".parse().expect("discovered peer");
        let (leaf_addr, leaf_task) = spawn_test_krpc_server(
            "127.0.0.1:0".parse().expect("leaf bind addr"),
            TestKrpcReply {
                values: vec![discovered_peer],
                nodes: Vec::new(),
                nodes6: Vec::new(),
                token: Vec::new(),
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
            },
        )
        .await;

        let (client, warning) =
            InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()]).await.expect("client");
        assert!(warning.is_none());

        let peers = client.query_get_peers([7u8; 20]).await;

        assert_eq!(peers, vec![discovered_peer]);

        bootstrap_task.abort();
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
            },
        )
        .await;

        let (client, warning) =
            InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()]).await.expect("client");
        assert!(warning.is_none());

        let first_peers = client.query_get_peers([9u8; 20]).await;
        assert_eq!(first_peers, vec![discovered_peer]);

        bootstrap_task.abort();

        let second_peers = client.query_get_peers([9u8; 20]).await;
        assert_eq!(second_peers, vec![discovered_peer]);

        leaf_task.abort();
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
            },
        )
        .await;

        let (client, warning) =
            InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()]).await.expect("client");
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

        let (client, warning) =
            InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()]).await.expect("client");
        assert!(warning.is_none());

        assert!(client.announce_peer(info_hash, Some(51413)).await);

        let observation = tokio::time::timeout(Duration::from_secs(1), observation_rx.recv())
            .await
            .expect("announce observation timeout")
            .expect("announce observation");
        let TestKrpcObservation::AnnouncePeer(args) = observation;
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

        let (client, warning) =
            InternalPrototypeClient::bind(0, &[bootstrap_addr.to_string()]).await.expect("client");
        assert!(warning.is_none());

        assert!(client.announce_peer(info_hash, None).await);

        let observation = tokio::time::timeout(Duration::from_secs(1), observation_rx.recv())
            .await
            .expect("announce observation timeout")
            .expect("announce observation");
        let TestKrpcObservation::AnnouncePeer(args) = observation;
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
        assert_eq!(health.cached_ipv4_announce_tokens, 0);
        assert_eq!(health.cached_ipv6_announce_tokens, 0);
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
