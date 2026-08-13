// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(dead_code, unused_imports)]

pub mod anomaly;
pub mod bep42;
pub mod bootstrap;
pub mod health;
pub mod inbound;
pub mod krpc;
pub mod lookup;
pub mod peer_store;
pub mod persist;
pub mod public_addr;
pub mod routing;
mod scheduler;
pub mod service;
pub mod test_support;
pub mod token;
pub mod transport;
pub mod types;

use std::collections::{HashMap, HashSet};
use std::future::{pending, Future};
use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio::time::timeout;

const DHT_BOOTSTRAP_DNS_CONCURRENCY: usize = 8;

pub use health::{DhtAnomalySummary, DhtHealthSnapshot};
pub use krpc::{
    decode_compact_nodes, decode_compact_peers, encode_compact_nodes, encode_compact_peer,
    KrpcAnnouncePeerArgs, KrpcErrorBody, KrpcErrorEnvelope, KrpcFindNodeArgs, KrpcGetPeersArgs,
    KrpcPingArgs, KrpcQueryEnvelope, KrpcQueryKind, KrpcResponseBody, KrpcResponseEnvelope,
};
pub use lookup::{LookupConfig, LookupKind, LookupRequest, LookupTarget};
pub use persist::{
    PersistedRoutingNode, PersistedRoutingTable, PersistedStateEnvelope, PersistenceConfig,
};
pub use types::{
    AddressFamily, Bep42State, CompactNode, CompactPeer, FixedLengthError, InfoHash, LookupId,
    NodeId, NodeRecord, NodeTrust, TransactionId,
};

use crate::dht::bep42::{classify_node, random_secure_node_id_for_ipv4};
use crate::dht::bootstrap::{BootstrapConfig, BootstrapCoordinator};
use crate::dht::health::DhtHealthSnapshot as InternalHealthSnapshot;
use crate::dht::inbound::{InboundAction, InboundActor, InboundConfig, InboundRequestContext};
use crate::dht::lookup::{LookupManager, LookupQualitySnapshot, LookupState, LookupUpdate};
use crate::dht::peer_store::{PeerStore, PeerStoreConfig};
use crate::dht::persist::PersistenceManager;
use crate::dht::public_addr::PublicAddressObserver;
use crate::dht::routing::{InsertOutcome, RoutingActor, RoutingConfig};
use crate::dht::token::{TokenConfig, TokenService};
use crate::dht::transport::{TransportActor, TransportConfig, TransportEvent, TransportReply};
use crate::networking::runtime::NetworkLease;

const MAX_CACHED_RESPONDER_TARGETS: usize = 256;
const MAX_CACHED_RESPONDERS_PER_TARGET: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub local_node_id: NodeId,
    pub allow_public_ipv4_identity: bool,
    pub bootstrap_nodes: Vec<SocketAddr>,
    pub bootstrap_sources: Vec<String>,
    pub ipv4_bind_addr: Option<SocketAddr>,
    pub ipv6_bind_addr: Option<SocketAddr>,
    pub persistence: Option<PersistenceConfig>,
}

#[derive(Debug)]
struct ActiveLookup {
    family: AddressFamily,
    state: LookupState,
    peer_tx: mpsc::UnboundedSender<Vec<SocketAddr>>,
    mode: LookupRunMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LookupRunMode {
    Active,
    Draining,
}

impl LookupRunMode {
    fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }

    fn is_draining(self) -> bool {
        matches!(self, Self::Draining)
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum LookupTaskOutcome {
    Reply(TransportReply),
    SoftTimeout,
    Timeout,
}

#[derive(Debug)]
struct LookupTaskResult {
    lookup_id: LookupId,
    family: AddressFamily,
    transaction_id: TransactionId,
    outcome: LookupTaskOutcome,
}

pub(crate) struct AnnouncePeerJob {
    local_node_id: NodeId,
    info_hash: InfoHash,
    port: Option<u16>,
    targets: Vec<AnnouncePeerTarget>,
}

struct AnnouncePeerTarget {
    transport: TransportActor,
    addr: SocketAddr,
}

impl AnnouncePeerJob {
    pub(crate) async fn run(self) -> bool {
        let mut tasks = JoinSet::new();
        for target in self.targets {
            let local_node_id = self.local_node_id;
            let info_hash = self.info_hash;
            let port = self.port;
            tasks.spawn(async move {
                announce_peer_to_target(
                    target.transport,
                    target.addr,
                    local_node_id,
                    info_hash,
                    port,
                )
                .await
                .unwrap_or(false)
            });
        }

        let mut announced = false;
        while let Some(result) = tasks.join_next().await {
            announced |= result.unwrap_or(false);
        }
        announced
    }
}

#[derive(Debug)]
pub struct Runtime {
    network_lease: NetworkLease,
    config: RuntimeConfig,
    ipv4_transport: Option<TransportActor>,
    ipv6_transport: Option<TransportActor>,
    ipv4_events: Option<mpsc::UnboundedReceiver<TransportEvent>>,
    ipv6_events: Option<mpsc::UnboundedReceiver<TransportEvent>>,
    ipv4_routing: RoutingActor,
    ipv6_routing: RoutingActor,
    ipv4_inbound: InboundActor,
    ipv6_inbound: InboundActor,
    token_service: TokenService,
    peer_store: PeerStore,
    public_addresses: PublicAddressObserver,
    bootstrap: BootstrapCoordinator,
    lookup_manager: LookupManager,
    active_lookups: HashMap<LookupId, ActiveLookup>,
    maintenance_lookup_receivers: HashMap<LookupId, mpsc::UnboundedReceiver<Vec<SocketAddr>>>,
    closest_responder_cache: HashMap<(AddressFamily, NodeId), Vec<NodeRecord>>,
    pending_probe_targets: HashSet<(AddressFamily, SocketAddr)>,
    next_lookup_id: u64,
    lookup_result_tx: mpsc::UnboundedSender<LookupTaskResult>,
    lookup_result_rx: mpsc::UnboundedReceiver<LookupTaskResult>,
    persistence_manager: Option<PersistenceManager>,
    responsive_bootstrap_nodes: HashSet<SocketAddr>,
    inbound_query_count: usize,
    recent_lookup_success_count: usize,
}

impl Runtime {
    pub async fn bind(network_lease: &NetworkLease, config: RuntimeConfig) -> io::Result<Self> {
        network_lease.ensure_valid().map_err(io::Error::other)?;
        let now = Instant::now();
        let wall_clock = SystemTime::now();

        let mut ipv4_routing = RoutingActor::new(
            config.local_node_id,
            RoutingConfig {
                family: AddressFamily::Ipv4,
                ..RoutingConfig::default()
            },
            now,
        );
        let mut ipv6_routing = RoutingActor::new(
            config.local_node_id,
            RoutingConfig {
                family: AddressFamily::Ipv6,
                ..RoutingConfig::default()
            },
            now,
        );

        let persistence_manager = config.persistence.clone().map(PersistenceManager::new);
        if let Some(manager) = &persistence_manager {
            if let Some(snapshot) = manager.load_snapshot(wall_clock)? {
                if snapshot.node_id == config.local_node_id {
                    for node in manager.restore_nodes(&snapshot.ipv4_routes, now) {
                        let _ = ipv4_routing.table_mut().insert(node, now);
                    }
                    for node in manager.restore_nodes(&snapshot.ipv6_routes, now) {
                        let _ = ipv6_routing.table_mut().insert(node, now);
                    }
                }
            }
        }

        if config.ipv4_bind_addr.is_none() && config.ipv6_bind_addr.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "DHT runtime requires at least one bind address",
            ));
        }

        let (ipv4_transport, ipv4_events) = if let Some(bind_addr) = config.ipv4_bind_addr {
            match TransportActor::bind(
                network_lease,
                TransportConfig {
                    family: AddressFamily::Ipv4,
                    bind_addr,
                    ..TransportConfig::default()
                },
            )
            .await
            {
                Ok((transport, events)) => (Some(transport), Some(events)),
                Err(error) if config.ipv6_bind_addr.is_some() => {
                    tracing::warn!(
                        bind_addr = %bind_addr,
                        error = %error,
                        "DHT IPv4 bind failed; trying IPv6"
                    );
                    (None, None)
                }
                Err(error) => return Err(error),
            }
        } else {
            (None, None)
        };

        let (ipv6_transport, ipv6_events) = if let Some(bind_addr) = config.ipv6_bind_addr {
            match TransportActor::bind(
                network_lease,
                TransportConfig {
                    family: AddressFamily::Ipv6,
                    bind_addr,
                    ..TransportConfig::default()
                },
            )
            .await
            {
                Ok((transport, events)) => (Some(transport), Some(events)),
                Err(error) => {
                    if ipv4_transport.is_none() {
                        return Err(error);
                    }
                    tracing::warn!(
                        bind_addr = %bind_addr,
                        error = %error,
                        "DHT IPv6 bind failed; continuing with IPv4 only"
                    );
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        let (lookup_result_tx, lookup_result_rx) = mpsc::unbounded_channel();

        let bootstrap_nodes = config.bootstrap_nodes.clone();

        Ok(Self {
            network_lease: network_lease.clone(),
            config,
            ipv4_transport,
            ipv6_transport,
            ipv4_events,
            ipv6_events,
            ipv4_routing,
            ipv6_routing,
            ipv4_inbound: InboundActor::new(InboundConfig {
                family: AddressFamily::Ipv4,
                ..InboundConfig::default()
            }),
            ipv6_inbound: InboundActor::new(InboundConfig {
                family: AddressFamily::Ipv6,
                ..InboundConfig::default()
            }),
            token_service: TokenService::new(TokenConfig::default(), now),
            peer_store: PeerStore::new(PeerStoreConfig::default()),
            public_addresses: PublicAddressObserver::default(),
            bootstrap: BootstrapCoordinator::new(BootstrapConfig {
                bootstrap_nodes,
                ..BootstrapConfig::default()
            }),
            lookup_manager: LookupManager::new(LookupConfig::default()),
            active_lookups: HashMap::new(),
            maintenance_lookup_receivers: HashMap::new(),
            closest_responder_cache: HashMap::new(),
            pending_probe_targets: HashSet::new(),
            next_lookup_id: 1,
            lookup_result_tx,
            lookup_result_rx,
            persistence_manager,
            responsive_bootstrap_nodes: HashSet::new(),
            inbound_query_count: 0,
            recent_lookup_success_count: 0,
        })
    }

    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    pub fn local_node_id(&self) -> NodeId {
        self.config.local_node_id
    }

    pub fn family_bound(&self, family: AddressFamily) -> bool {
        self.transport_for(family).is_some()
    }

    pub fn ipv4_local_addr(&self) -> Option<SocketAddr> {
        self.ipv4_transport
            .as_ref()
            .and_then(|transport| transport.local_addr().ok())
    }

    pub fn ipv6_local_addr(&self) -> Option<SocketAddr> {
        self.ipv6_transport
            .as_ref()
            .and_then(|transport| transport.local_addr().ok())
    }

    pub fn bound_family_count(&self) -> usize {
        usize::from(self.ipv4_transport.is_some()) + usize::from(self.ipv6_transport.is_some())
    }

    pub fn active_lookup_count(&self) -> usize {
        self.active_lookups
            .values()
            .filter(|active| active.mode.is_active())
            .count()
    }

    pub fn active_user_lookup_count(&self) -> usize {
        self.active_lookups
            .iter()
            .filter(|(lookup_id, active)| {
                active.mode.is_active()
                    && !self.maintenance_lookup_receivers.contains_key(lookup_id)
            })
            .count()
    }

    pub fn is_lookup_active(&self, lookup_id: LookupId) -> bool {
        self.active_lookups.contains_key(&lookup_id)
    }

    pub fn draining_lookup_count(&self) -> usize {
        self.active_lookups
            .values()
            .filter(|active| active.mode.is_draining())
            .count()
    }

    pub fn inflight_query_counts(&self) -> (usize, usize) {
        let ipv4 = self
            .ipv4_transport
            .as_ref()
            .map(TransportActor::inflight_query_count)
            .unwrap_or_default();
        let ipv6 = self
            .ipv6_transport
            .as_ref()
            .map(TransportActor::inflight_query_count)
            .unwrap_or_default();
        (ipv4, ipv6)
    }

    pub fn lookup_quality_snapshot(&self, lookup_id: LookupId) -> Option<LookupQualitySnapshot> {
        self.active_lookups
            .get(&lookup_id)
            .map(|active| active.state.quality_snapshot())
    }

    pub fn active_route_count(&self, family: AddressFamily) -> usize {
        let now = Instant::now();
        match family {
            AddressFamily::Ipv4 => self.ipv4_routing.table().snapshot(now).nodes.len(),
            AddressFamily::Ipv6 => self.ipv6_routing.table().snapshot(now).nodes.len(),
        }
    }

    pub fn health_snapshot(&self) -> DhtHealthSnapshot {
        let now = Instant::now();
        let ipv4_snapshot = self.ipv4_routing.table().snapshot(now);
        let ipv6_snapshot = self.ipv6_routing.table().snapshot(now);
        let mut health = InternalHealthSnapshot::from_parts(
            self.ipv4_transport.as_ref(),
            self.ipv6_transport.as_ref(),
            Some(&ipv4_snapshot),
            Some(&ipv6_snapshot),
            Some(&self.peer_store),
        );
        let (responsive_total, responsive_ipv4, responsive_ipv6) =
            self.responsive_bootstrap_counts();
        health.bootstrap_responsive_count = responsive_total;
        health.bootstrap_responsive_ipv4_count = responsive_ipv4;
        health.bootstrap_responsive_ipv6_count = responsive_ipv6;
        health.inbound_query_rate = self.inbound_query_count;
        health.recent_lookup_success_rate = self.recent_lookup_success_count;
        health.confirmed_public_addr_ipv4 =
            self.public_addresses.confirmed_for(AddressFamily::Ipv4);
        health.confirmed_public_addr_ipv6 =
            self.public_addresses.confirmed_for(AddressFamily::Ipv6);
        health
    }

    fn record_responsive_bootstrap(&mut self, addr: SocketAddr) {
        if self.config.bootstrap_nodes.contains(&addr) {
            self.responsive_bootstrap_nodes.insert(addr);
        }
    }

    fn responsive_bootstrap_counts(&self) -> (usize, usize, usize) {
        let mut total = 0usize;
        let mut ipv4 = 0usize;
        let mut ipv6 = 0usize;

        for addr in &self.responsive_bootstrap_nodes {
            if !self.config.bootstrap_nodes.contains(addr) {
                continue;
            }
            total = total.saturating_add(1);
            if addr.is_ipv4() {
                ipv4 = ipv4.saturating_add(1);
            } else {
                ipv6 = ipv6.saturating_add(1);
            }
        }

        (total, ipv4, ipv6)
    }

    pub async fn save_state(&self) -> io::Result<()> {
        let Some(manager) = &self.persistence_manager else {
            return Ok(());
        };
        let now = Instant::now();
        let wall_clock = SystemTime::now();
        let ipv4_snapshot = self.ipv4_routing.table().snapshot(now);
        let ipv6_snapshot = self.ipv6_routing.table().snapshot(now);
        let snapshot = manager.build_snapshot(
            self.config.local_node_id,
            &ipv4_snapshot,
            &ipv6_snapshot,
            wall_clock,
        );
        manager.save_snapshot(&snapshot)
    }

    pub async fn shutdown_for_rebind(&mut self, wait: Duration) {
        let lookup_ids = self.active_lookups.keys().copied().collect::<Vec<_>>();
        for lookup_id in lookup_ids {
            self.cancel_lookup(lookup_id);
        }

        let transports = [self.ipv4_transport.take(), self.ipv6_transport.take()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        self.ipv4_events = None;
        self.ipv6_events = None;

        for transport in &transports {
            transport.cancel_all_inflight_queries();
        }
        for transport in &transports {
            transport.shutdown().await;
        }

        let deadline = Instant::now() + wait;
        while transports
            .iter()
            .any(|transport| transport.actor_ref_count() > 1)
        {
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub async fn bootstrap_startup(&mut self) -> io::Result<()> {
        self.refresh_bootstrap_nodes_if_empty().await;

        let families = [AddressFamily::Ipv4, AddressFamily::Ipv6]
            .into_iter()
            .filter(|family| self.family_bound(*family))
            .collect::<Vec<_>>();

        for plan in self
            .bootstrap
            .startup_plan(self.config.local_node_id, families)
        {
            self.start_internal_find_node(plan.family, plan.target)
                .await?;
        }
        Ok(())
    }

    pub async fn run_maintenance(&mut self) -> io::Result<()> {
        self.cleanup_closed_lookups();
        self.refresh_bootstrap_nodes_if_empty().await;
        let now = Instant::now();
        let local_node_id = self.config.local_node_id;

        for family in [AddressFamily::Ipv4, AddressFamily::Ipv6] {
            if !self.family_bound(family) {
                continue;
            }

            let pending_probes = self.take_pending_probe_targets(family, 8);
            self.ping_nodes(family, &pending_probes).await?;

            let routing = self.routing_for_family(family).clone();
            let plan = self
                .bootstrap
                .maintenance_plan(family, &routing, local_node_id, now);

            self.ping_nodes(family, &plan.ping_targets).await?;

            if let Some(target) = plan.self_lookup_target {
                self.start_internal_find_node(family, target).await?;
            }

            for target in plan.refresh_targets {
                self.start_internal_find_node(family, target).await?;
            }
        }

        Ok(())
    }

    pub async fn start_lookup(
        &mut self,
        family: AddressFamily,
        kind: LookupKind,
        target: LookupTarget,
    ) -> io::Result<(LookupId, mpsc::UnboundedReceiver<Vec<SocketAddr>>)> {
        self.cleanup_closed_lookups();
        self.refresh_bootstrap_nodes_if_empty().await;

        if self.transport_for(family).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "transport not bound for requested family",
            ));
        }

        let lookup_id = LookupId(self.next_lookup_id);
        self.next_lookup_id = self.next_lookup_id.saturating_add(1);
        let request = LookupRequest {
            lookup_id,
            kind,
            target,
        };
        let target_node_id = request.target.as_node_id();
        let now = Instant::now();
        let routing_snapshot = match family {
            AddressFamily::Ipv4 => self.ipv4_routing.table().snapshot(now),
            AddressFamily::Ipv6 => self.ipv6_routing.table().snapshot(now),
        };
        let cached_responders = self
            .closest_responder_cache
            .get(&(family, target_node_id))
            .cloned()
            .unwrap_or_default();
        let state = self.lookup_manager.start(
            request,
            family,
            &routing_snapshot,
            &self.config.bootstrap_nodes,
            &cached_responders,
            now,
        );
        let (peer_tx, peer_rx) = mpsc::unbounded_channel();
        if state.is_finished() || state.next_candidates().is_empty() {
            return Ok((lookup_id, peer_rx));
        }

        self.active_lookups.insert(
            lookup_id,
            ActiveLookup {
                family,
                state,
                peer_tx,
                mode: LookupRunMode::Active,
            },
        );
        self.pump_lookup(lookup_id).await?;
        Ok((lookup_id, peer_rx))
    }

    async fn refresh_bootstrap_nodes_if_empty(&mut self) {
        if !self.config.bootstrap_nodes.is_empty() || self.config.bootstrap_sources.is_empty() {
            return;
        }

        let bootstrap_nodes =
            resolve_bootstrap_sources(&self.network_lease, &self.config.bootstrap_sources).await;
        if bootstrap_nodes.is_empty() {
            return;
        }

        self.config.bootstrap_nodes = bootstrap_nodes.clone();
        self.bootstrap.set_bootstrap_nodes(bootstrap_nodes);
    }

    pub async fn start_lookup_with_state(
        &mut self,
        mut state: LookupState,
    ) -> io::Result<(LookupId, mpsc::UnboundedReceiver<Vec<SocketAddr>>)> {
        self.cleanup_closed_lookups();

        let family = state.family();
        if self.transport_for(family).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "transport not bound for requested family",
            ));
        }

        let lookup_id = LookupId(self.next_lookup_id);
        self.next_lookup_id = self.next_lookup_id.saturating_add(1);
        state.resume(lookup_id, Instant::now());
        let (peer_tx, peer_rx) = mpsc::unbounded_channel();
        if state.is_finished() || state.next_candidates().is_empty() {
            return Ok((lookup_id, peer_rx));
        }

        self.active_lookups.insert(
            lookup_id,
            ActiveLookup {
                family,
                state,
                peer_tx,
                mode: LookupRunMode::Active,
            },
        );
        self.pump_lookup(lookup_id).await?;
        Ok((lookup_id, peer_rx))
    }

    pub async fn start_get_peers(
        &mut self,
        family: AddressFamily,
        info_hash: InfoHash,
    ) -> io::Result<(LookupId, mpsc::UnboundedReceiver<Vec<SocketAddr>>)> {
        self.start_lookup(
            family,
            LookupKind::GetPeers,
            LookupTarget::InfoHash(info_hash),
        )
        .await
    }

    pub async fn start_get_peers_with_state(
        &mut self,
        state: LookupState,
    ) -> io::Result<(LookupId, mpsc::UnboundedReceiver<Vec<SocketAddr>>)> {
        let request = state.request();
        match request {
            LookupRequest {
                kind: LookupKind::GetPeers,
                target: LookupTarget::InfoHash(_),
                ..
            } => self.start_lookup_with_state(state).await,
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "lookup state is not a get_peers traversal",
            )),
        }
    }

    pub async fn start_find_node(
        &mut self,
        family: AddressFamily,
        node_id: NodeId,
    ) -> io::Result<(LookupId, mpsc::UnboundedReceiver<Vec<SocketAddr>>)> {
        self.start_lookup(family, LookupKind::FindNode, LookupTarget::Node(node_id))
            .await
    }

    pub async fn announce_peer(
        &mut self,
        family: AddressFamily,
        info_hash: InfoHash,
        port: Option<u16>,
    ) -> io::Result<bool> {
        let transport = self.transport_for(family).cloned().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "transport not bound for requested family",
            )
        })?;
        let target = NodeId::from(info_hash);
        let now = Instant::now();
        let mut candidates = match family {
            AddressFamily::Ipv4 => self.ipv4_routing.table().closest_nodes(target, 8),
            AddressFamily::Ipv6 => self.ipv6_routing.table().closest_nodes(target, 8),
        }
        .into_iter()
        .map(|record| record.addr)
        .collect::<Vec<_>>();

        if candidates.is_empty() {
            candidates.extend(
                self.config
                    .bootstrap_nodes
                    .iter()
                    .copied()
                    .filter(|addr| AddressFamily::for_addr(*addr) == family)
                    .take(8),
            );
        }

        let mut announced = false;
        for addr in candidates {
            match transport
                .get_peers(addr, self.config.local_node_id, info_hash)
                .await?
            {
                Some(TransportReply::Response(response)) => {
                    let response_body = response.r.unwrap_or_default();
                    let _ = self.routing_for_family_mut(family).record_response(
                        addr,
                        response_body.node_id(),
                        now,
                    );

                    if response_body.token.is_empty() {
                        continue;
                    }

                    if matches!(
                        transport
                            .announce_peer(
                                addr,
                                self.config.local_node_id,
                                info_hash,
                                response_body.token.as_ref(),
                                port,
                            )
                            .await?,
                        Some(TransportReply::Response(_))
                    ) {
                        announced = true;
                    }
                }
                Some(TransportReply::Error(_)) | None => {
                    let _ = self
                        .routing_for_family_mut(family)
                        .record_failure(addr, now);
                }
            }
        }

        Ok(announced)
    }

    pub(crate) fn announce_peer_job(
        &self,
        info_hash: InfoHash,
        port: Option<u16>,
    ) -> Option<AnnouncePeerJob> {
        let target = NodeId::from(info_hash);
        let mut targets = Vec::new();

        for family in [AddressFamily::Ipv4, AddressFamily::Ipv6] {
            let Some(transport) = self.transport_for(family).cloned() else {
                continue;
            };
            let mut candidates = match family {
                AddressFamily::Ipv4 => self.ipv4_routing.table().closest_nodes(target, 8),
                AddressFamily::Ipv6 => self.ipv6_routing.table().closest_nodes(target, 8),
            }
            .into_iter()
            .map(|record| record.addr)
            .collect::<Vec<_>>();

            if candidates.is_empty() {
                candidates.extend(
                    self.config
                        .bootstrap_nodes
                        .iter()
                        .copied()
                        .filter(|addr| AddressFamily::for_addr(*addr) == family)
                        .take(8),
                );
            }

            targets.extend(candidates.into_iter().map(|addr| AnnouncePeerTarget {
                transport: transport.clone(),
                addr,
            }));
        }

        (!targets.is_empty()).then_some(AnnouncePeerJob {
            local_node_id: self.config.local_node_id,
            info_hash,
            port,
            targets,
        })
    }

    pub async fn step(&mut self) -> io::Result<bool> {
        self.cleanup_closed_lookups();

        let mut processed_lookup_results = 0usize;
        while processed_lookup_results < 4 {
            match self.lookup_result_rx.try_recv() {
                Ok(result) => {
                    self.handle_lookup_result(result).await?;
                    processed_lookup_results += 1;
                }
                Err(_) => break,
            }
        }
        if processed_lookup_results > 0 {
            return Ok(true);
        }

        if self.ipv4_events.is_none()
            && self.ipv6_events.is_none()
            && self.active_lookups.is_empty()
        {
            return Ok(false);
        }

        let ipv4_event_future = async {
            match self.ipv4_events.as_mut() {
                Some(rx) => rx.recv().await.map(|event| (AddressFamily::Ipv4, event)),
                None => pending::<Option<(AddressFamily, TransportEvent)>>().await,
            }
        };
        let ipv6_event_future = async {
            match self.ipv6_events.as_mut() {
                Some(rx) => rx.recv().await.map(|event| (AddressFamily::Ipv6, event)),
                None => pending::<Option<(AddressFamily, TransportEvent)>>().await,
            }
        };

        tokio::select! {
            biased;
            result = self.lookup_result_rx.recv() => {
                match result {
                    Some(result) => {
                        self.handle_lookup_result(result).await?;
                        Ok(true)
                    }
                    None => Ok(false),
                }
            }
            event = ipv4_event_future => {
                match event {
                    Some((family, event)) => {
                        self.handle_transport_event(family, event).await?;
                        Ok(true)
                    }
                    None => {
                        self.ipv4_events = None;
                        Ok(false)
                    }
                }
            }
            event = ipv6_event_future => {
                match event {
                    Some((family, event)) => {
                        self.handle_transport_event(family, event).await?;
                        Ok(true)
                    }
                    None => {
                        self.ipv6_events = None;
                        Ok(false)
                    }
                }
            }
        }
    }

    async fn handle_transport_event(
        &mut self,
        family: AddressFamily,
        event: TransportEvent,
    ) -> io::Result<()> {
        match event {
            TransportEvent::Query { source, query } => {
                self.inbound_query_count = self.inbound_query_count.saturating_add(1);
                let now = Instant::now();
                let wall_clock = SystemTime::now();
                let local_node_id = self.config.local_node_id;
                let transport = self.transport_for(family).cloned().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotConnected, "transport unavailable")
                })?;

                let action = match family {
                    AddressFamily::Ipv4 => {
                        let ipv6_routing = self.ipv6_routing.table().clone();
                        self.ipv4_inbound.handle_query(
                            InboundRequestContext { source },
                            query,
                            local_node_id,
                            self.ipv4_routing.table_mut(),
                            Some(&ipv6_routing),
                            &mut self.token_service,
                            &mut self.peer_store,
                            now,
                            wall_clock,
                        )
                    }
                    AddressFamily::Ipv6 => {
                        let ipv4_routing = self.ipv4_routing.table().clone();
                        self.ipv6_inbound.handle_query(
                            InboundRequestContext { source },
                            query,
                            local_node_id,
                            self.ipv6_routing.table_mut(),
                            Some(&ipv4_routing),
                            &mut self.token_service,
                            &mut self.peer_store,
                            now,
                            wall_clock,
                        )
                    }
                };

                match action {
                    InboundAction::Respond(response) => {
                        transport.send_response(source, &response).await?;
                    }
                    InboundAction::Error(error) => {
                        transport.send_error(source, &error).await?;
                    }
                    InboundAction::Drop => {}
                }
            }
            TransportEvent::UnexpectedReply { source, .. } => {
                self.record_responsive_bootstrap(source);
            }
            TransportEvent::Timeout { .. } => {}
        }
        Ok(())
    }

    async fn handle_lookup_result(&mut self, result: LookupTaskResult) -> io::Result<()> {
        let now = Instant::now();
        let mut completed_addr = None;
        let mut completed_node_id = None;
        let mut discovered_nodes = Vec::new();
        let mut emitted_peers = Vec::new();
        let mut cross_family_nodes = Vec::new();
        let mut public_address_observation = None;
        let mut finished = false;
        let mut peer_tx = None;
        let mut receiver_closed = false;
        let mut draining = false;

        if let Some(active) = self.active_lookups.get_mut(&result.lookup_id) {
            draining = active.mode.is_draining();
            receiver_closed = active.peer_tx.is_closed();
            peer_tx = Some(active.peer_tx.clone());
            match result.outcome {
                LookupTaskOutcome::Reply(reply) => match reply {
                    TransportReply::Response(response) => {
                        let observed_addr = response.observed_addr();
                        let response_body = response.r.unwrap_or_default();
                        let update = active.state.handle_response(
                            result.transaction_id,
                            &response_body,
                            now,
                        );
                        if let Some(query) = update.completed_query {
                            completed_addr = Some(query.candidate.addr);
                            completed_node_id = response_body.node_id();
                            if let Some(observed_addr) = observed_addr {
                                public_address_observation =
                                    Some((query.candidate.addr, observed_addr));
                            }
                        }
                        cross_family_nodes =
                            response_body.closest_nodes(opposite_family(result.family));
                        emitted_peers = update
                            .emitted_peers
                            .into_iter()
                            .map(|peer| peer.addr)
                            .collect();
                        discovered_nodes = update.discovered_nodes;
                        finished = update.finished;
                    }
                    TransportReply::Error(_) => {
                        let update = active.state.handle_error(result.transaction_id);
                        if let Some(query) = update.completed_query {
                            completed_addr = Some(query.candidate.addr);
                        }
                        finished = update.finished;
                    }
                },
                LookupTaskOutcome::SoftTimeout => {
                    let _ = active.state.mark_soft_timeout(result.transaction_id);
                    finished = active.state.is_finished();
                }
                LookupTaskOutcome::Timeout => {
                    let update = active.state.handle_timeout(result.transaction_id);
                    if let Some(query) = update.completed_query {
                        completed_addr = Some(query.candidate.addr);
                    }
                    finished = update.finished;
                }
            }
        }

        if let Some((voter, observed_addr)) = public_address_observation {
            let confirmed = self
                .public_addresses
                .record_observation(voter, observed_addr);
            self.apply_confirmed_public_identity(confirmed);
        }

        let other_family = opposite_family(result.family);
        for node in cross_family_nodes {
            let record = NodeRecord::new(node.addr, Some(node.id), now);
            let outcome = self
                .routing_for_family_mut(other_family)
                .insert(record, now);
            if let InsertOutcome::NeedsProbe { targets } = outcome {
                self.enqueue_probe_targets(other_family, &targets);
            }
        }

        if let Some(addr) = completed_addr {
            if let Some(node_id) = completed_node_id {
                let routing = self.routing_for_family_mut(result.family);
                if !routing.record_response(addr, Some(node_id), now) {
                    let mut record = NodeRecord::new(addr, Some(node_id), now);
                    record.note_query_response(Some(node_id), now);
                    let _ = routing.insert(record, now);
                }
                self.record_responsive_bootstrap(addr);
                self.recent_lookup_success_count =
                    self.recent_lookup_success_count.saturating_add(1);
            } else {
                let _ = self
                    .routing_for_family_mut(result.family)
                    .record_failure(addr, now);
            }
        }

        let mut probe_targets = Vec::new();
        for node in discovered_nodes {
            let record = NodeRecord::new(node.addr, Some(node.id), now);
            let outcome = self
                .routing_for_family_mut(result.family)
                .insert(record, now);
            if let InsertOutcome::NeedsProbe { targets } = outcome {
                probe_targets.extend(targets);
            }
        }

        self.enqueue_probe_targets(result.family, &probe_targets);

        if let Some(peer_tx) = peer_tx {
            let send_failed = !emitted_peers.is_empty() && peer_tx.send(emitted_peers).is_err();
            if send_failed {
                receiver_closed = true;
            }
        }

        if finished || receiver_closed {
            if !draining {
                self.cancel_lookup(result.lookup_id);
            }
        } else if self
            .active_lookups
            .get(&result.lookup_id)
            .is_some_and(|active| active.mode.is_active())
        {
            self.pump_lookup(result.lookup_id).await?;
        }

        Ok(())
    }

    fn apply_confirmed_public_identity(&mut self, confirmed: Option<SocketAddr>) {
        if !self.config.allow_public_ipv4_identity {
            return;
        }

        let Some(SocketAddr::V4(public_addr)) = confirmed else {
            return;
        };
        if classify_node(SocketAddr::V4(public_addr), Some(self.config.local_node_id))
            == Bep42State::Compliant
        {
            return;
        }

        let Some(new_node_id) = random_secure_node_id_for_ipv4(*public_addr.ip()) else {
            return;
        };
        let old_node_id = self.config.local_node_id;
        if new_node_id == old_node_id {
            return;
        }

        tracing::info!(
            old_node_id = %node_id_hex(old_node_id),
            new_node_id = %node_id_hex(new_node_id),
            public_addr = %public_addr,
            "DHT rotated local node ID to match confirmed public IPv4 identity"
        );
        self.config.local_node_id = new_node_id;
        self.ipv4_routing.set_local_node_id(new_node_id);
        self.ipv6_routing.set_local_node_id(new_node_id);
        self.closest_responder_cache.clear();
    }

    async fn pump_lookup(&mut self, lookup_id: LookupId) -> io::Result<()> {
        let (family, request, candidates) = match self.active_lookups.get(&lookup_id) {
            Some(active) if active.mode.is_draining() => return Ok(()),
            Some(active) if active.peer_tx.is_closed() => {
                self.cancel_lookup(lookup_id);
                return Ok(());
            }
            Some(active) => (
                active.family,
                active.state.request(),
                active.state.next_candidates(),
            ),
            None => return Ok(()),
        };

        let transport = self
            .transport_for(family)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "transport unavailable"))?;

        for candidate in candidates {
            let sent_at = Instant::now();
            let _ = self
                .routing_for_family_mut(family)
                .record_query_sent(candidate.addr, sent_at);

            let deferred = match request.kind {
                LookupKind::FindNode => {
                    let target = request.target.as_node_id();
                    let args = krpc::KrpcFindNodeArgs::new(self.config.local_node_id, target)
                        .with_want(&self.wanted_node_families());
                    transport
                        .send_query_deferred(candidate.addr, krpc::KrpcQueryKind::FindNode, args)
                        .await
                }
                LookupKind::GetPeers => {
                    let LookupTarget::InfoHash(info_hash) = request.target else {
                        continue;
                    };
                    let args = krpc::KrpcGetPeersArgs::new(self.config.local_node_id, info_hash)
                        .with_want(&self.wanted_node_families());
                    transport
                        .send_query_deferred(candidate.addr, krpc::KrpcQueryKind::GetPeers, args)
                        .await
                }
            };

            let (transaction_id, response_rx) = match deferred {
                Ok(value) => value,
                Err(_) => {
                    if let Some(active) = self.active_lookups.get_mut(&lookup_id) {
                        active.state.discard_candidate(candidate.addr);
                    }
                    continue;
                }
            };

            let marked_inflight = if let Some(active) = self.active_lookups.get_mut(&lookup_id) {
                active
                    .state
                    .mark_inflight(transaction_id, candidate.addr, sent_at)
                    .is_some()
            } else {
                false
            };
            if !marked_inflight {
                transport.cancel_inflight_query(transaction_id);
                if let Some(active) = self.active_lookups.get_mut(&lookup_id) {
                    active.state.discard_candidate(candidate.addr);
                }
                continue;
            }

            let outcome_tx = self.lookup_result_tx.clone();
            let soft_timeout_window = transport.config().soft_query_timeout;
            let timeout_window = transport.config().query_timeout;
            let timeout_transport = transport.clone();
            tokio::spawn(async move {
                let mut response_rx = response_rx;
                let mut soft_timeout_sent = false;
                let soft_timeout_enabled = soft_timeout_window < timeout_window;
                let soft_timeout_sleep = tokio::time::sleep(soft_timeout_window);
                let hard_timeout_sleep = tokio::time::sleep(timeout_window);
                tokio::pin!(soft_timeout_sleep);
                tokio::pin!(hard_timeout_sleep);

                loop {
                    tokio::select! {
                        reply = &mut response_rx => {
                            let outcome = match reply {
                                Ok(reply) => LookupTaskOutcome::Reply(reply),
                                Err(_) => LookupTaskOutcome::Timeout,
                            };
                            let send_result = outcome_tx.send(LookupTaskResult {
                                lookup_id,
                                family,
                                transaction_id,
                                outcome,
                            });
                            if send_result.is_err() {
                                break;
                            }
                            break;
                        }
                        _ = &mut soft_timeout_sleep, if soft_timeout_enabled && !soft_timeout_sent => {
                            soft_timeout_sent = true;
                            let send_result = outcome_tx.send(LookupTaskResult {
                                lookup_id,
                                family,
                                transaction_id,
                                outcome: LookupTaskOutcome::SoftTimeout,
                            });
                            if send_result.is_err() {
                                break;
                            }
                        }
                        _ = &mut hard_timeout_sleep => {
                            timeout_transport.cancel_inflight_query(transaction_id);
                            let send_result = outcome_tx.send(LookupTaskResult {
                                lookup_id,
                                family,
                                transaction_id,
                                outcome: LookupTaskOutcome::Timeout,
                            });
                            let _ = send_result;
                            break;
                        }
                    }
                }
            });
        }

        Ok(())
    }

    fn transport_for(&self, family: AddressFamily) -> Option<&TransportActor> {
        match family {
            AddressFamily::Ipv4 => self.ipv4_transport.as_ref(),
            AddressFamily::Ipv6 => self.ipv6_transport.as_ref(),
        }
    }

    fn routing_for_family_mut(&mut self, family: AddressFamily) -> &mut routing::RoutingTable {
        match family {
            AddressFamily::Ipv4 => self.ipv4_routing.table_mut(),
            AddressFamily::Ipv6 => self.ipv6_routing.table_mut(),
        }
    }

    fn routing_for_family(&self, family: AddressFamily) -> &routing::RoutingTable {
        match family {
            AddressFamily::Ipv4 => self.ipv4_routing.table(),
            AddressFamily::Ipv6 => self.ipv6_routing.table(),
        }
    }

    fn wanted_node_families(&self) -> Vec<AddressFamily> {
        [AddressFamily::Ipv4, AddressFamily::Ipv6]
            .into_iter()
            .filter(|family| self.family_bound(*family))
            .collect()
    }

    fn cleanup_closed_lookups(&mut self) {
        let closed = self
            .active_lookups
            .iter()
            .filter_map(|(lookup_id, active)| {
                (active.mode.is_active() && active.peer_tx.is_closed()).then_some(*lookup_id)
            })
            .collect::<Vec<_>>();

        for lookup_id in closed {
            self.cancel_lookup(lookup_id);
        }
    }

    pub fn cancel_lookup(&mut self, lookup_id: LookupId) -> bool {
        self.cancel_lookup_and_take_state(lookup_id).is_some()
    }

    pub fn pause_lookup_for_drain(&mut self, lookup_id: LookupId) -> Option<LookupQualitySnapshot> {
        let active = self.active_lookups.get_mut(&lookup_id)?;
        active.mode = LookupRunMode::Draining;
        Some(active.state.quality_snapshot())
    }

    pub fn drained_lookups_ready(&self, lookup_ids: &[LookupId]) -> bool {
        lookup_ids.iter().all(|lookup_id| {
            self.active_lookups.get(lookup_id).is_none_or(|active| {
                active.mode.is_draining() && active.state.quality_snapshot().inflight_len == 0
            })
        })
    }

    pub fn finish_drained_lookup(&mut self, lookup_id: LookupId) -> Option<LookupState> {
        let active = self.active_lookups.get(&lookup_id)?;
        if !active.mode.is_draining() {
            return None;
        }

        let active = self.active_lookups.remove(&lookup_id)?;
        self.maintenance_lookup_receivers.remove(&lookup_id);
        self.cache_lookup_responders(active.family, &active.state);

        if let Some(transport) = self.transport_for(active.family).cloned() {
            for transaction_id in active.state.inflight_transaction_ids() {
                transport.cancel_inflight_query(transaction_id);
            }
        }

        let mut state = active.state;
        state.park();
        Some(state)
    }

    pub fn cancel_lookup_and_take_state(&mut self, lookup_id: LookupId) -> Option<LookupState> {
        let active = self.active_lookups.remove(&lookup_id)?;
        self.maintenance_lookup_receivers.remove(&lookup_id);
        self.cache_lookup_responders(active.family, &active.state);

        if let Some(transport) = self.transport_for(active.family).cloned() {
            for transaction_id in active.state.inflight_transaction_ids() {
                transport.cancel_inflight_query(transaction_id);
            }
        }

        let mut state = active.state;
        state.park();
        Some(state)
    }

    pub fn cancel_maintenance_lookups(&mut self) {
        let lookup_ids = self
            .maintenance_lookup_receivers
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for lookup_id in lookup_ids {
            self.cancel_lookup(lookup_id);
        }
    }

    fn cache_lookup_responders(&mut self, family: AddressFamily, state: &LookupState) {
        let responders = state.cacheable_responders(MAX_CACHED_RESPONDERS_PER_TARGET);
        if responders.is_empty() {
            return;
        }

        if self.closest_responder_cache.len() >= MAX_CACHED_RESPONDER_TARGETS {
            if let Some(evicted) = self.closest_responder_cache.keys().next().copied() {
                self.closest_responder_cache.remove(&evicted);
            }
        }

        self.closest_responder_cache
            .insert((family, state.target_id()), responders);
    }

    async fn start_internal_find_node(
        &mut self,
        family: AddressFamily,
        target: NodeId,
    ) -> io::Result<()> {
        if self.has_active_find_node(family, target) {
            return Ok(());
        }

        let (lookup_id, rx) = self.start_find_node(family, target).await?;
        if self.is_lookup_active(lookup_id) {
            self.maintenance_lookup_receivers.insert(lookup_id, rx);
        }
        Ok(())
    }

    fn has_active_find_node(&self, family: AddressFamily, target: NodeId) -> bool {
        self.active_lookups.values().any(|active| {
            active.family == family
                && active.state.request().kind == LookupKind::FindNode
                && active.state.request().target == LookupTarget::Node(target)
        })
    }

    async fn ping_nodes(
        &mut self,
        family: AddressFamily,
        targets: &[SocketAddr],
    ) -> io::Result<()> {
        let Some(transport) = self.transport_for(family).cloned() else {
            return Ok(());
        };
        let local_node_id = self.config.local_node_id;

        for &addr in targets {
            let sent_at = Instant::now();
            let _ = self
                .routing_for_family_mut(family)
                .record_query_sent(addr, sent_at);

            match transport.ping(addr, local_node_id).await {
                Ok(Some(TransportReply::Response(response))) => {
                    let now = Instant::now();
                    let node_id = response.r.as_ref().and_then(KrpcResponseBody::node_id);
                    let routing = self.routing_for_family_mut(family);
                    if !routing.record_response(addr, node_id, now) {
                        let mut record = NodeRecord::new(addr, node_id, now);
                        record.note_query_response(node_id, now);
                        let _ = routing.insert(record, now);
                    }
                    self.record_responsive_bootstrap(addr);
                }
                Ok(Some(TransportReply::Error(_))) | Ok(None) => {
                    let _ = self
                        .routing_for_family_mut(family)
                        .record_failure(addr, Instant::now());
                }
                Err(_) => {
                    let _ = self
                        .routing_for_family_mut(family)
                        .record_failure(addr, Instant::now());
                }
            }
        }

        Ok(())
    }

    fn enqueue_probe_targets(&mut self, family: AddressFamily, targets: &[SocketAddr]) {
        for &addr in targets {
            self.pending_probe_targets.insert((family, addr));
        }
    }

    fn take_pending_probe_targets(
        &mut self,
        family: AddressFamily,
        limit: usize,
    ) -> Vec<SocketAddr> {
        let mut selected = Vec::new();
        let mut retained = HashSet::with_capacity(self.pending_probe_targets.len());

        for (target_family, addr) in self.pending_probe_targets.drain() {
            if target_family == family && selected.len() < limit {
                selected.push(addr);
            } else {
                retained.insert((target_family, addr));
            }
        }

        self.pending_probe_targets = retained;
        selected
    }
}

fn node_id_hex(node_id: NodeId) -> String {
    hex::encode(node_id.as_ref())
}

fn opposite_family(family: AddressFamily) -> AddressFamily {
    match family {
        AddressFamily::Ipv4 => AddressFamily::Ipv6,
        AddressFamily::Ipv6 => AddressFamily::Ipv4,
    }
}

pub(crate) async fn resolve_bootstrap_sources(
    network_lease: &NetworkLease,
    bootstrap_sources: &[String],
) -> Vec<SocketAddr> {
    resolve_bootstrap_sources_with(bootstrap_sources, |host, port| {
        let network_lease = network_lease.clone();
        async move { network_lease.resolve(&host, port).await.ok() }
    })
    .await
}

async fn resolve_bootstrap_sources_with<F, Fut>(
    bootstrap_sources: &[String],
    mut resolve: F,
) -> Vec<SocketAddr>
where
    F: FnMut(String, u16) -> Fut,
    Fut: Future<Output = Option<Vec<SocketAddr>>> + Send + 'static,
{
    let mut sources = bootstrap_sources
        .iter()
        .enumerate()
        .filter_map(|(index, bootstrap)| {
            let (host, port) = split_host_port(bootstrap)?;
            Some((index, host.to_string(), port))
        });
    let mut resolutions = JoinSet::new();
    let mut completed = Vec::new();

    loop {
        while resolutions.len() < DHT_BOOTSTRAP_DNS_CONCURRENCY {
            let Some((index, host, port)) = sources.next() else {
                break;
            };
            let resolution = resolve(host, port);
            resolutions.spawn(async move { (index, resolution.await) });
        }

        let Some(result) = resolutions.join_next().await else {
            break;
        };
        if let Ok((index, Some(addresses))) = result {
            completed.push((index, addresses));
        }
    }

    completed.sort_unstable_by_key(|(index, _)| *index);
    let mut resolved = Vec::new();
    let mut seen = HashSet::new();
    for (_, addresses) in completed {
        for addr in addresses {
            if seen.insert(addr) {
                resolved.push(addr);
            }
        }
    }

    resolved
}

fn split_host_port(address: &str) -> Option<(&str, u16)> {
    let (host, port) = address.rsplit_once(':')?;
    let host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    Some((host, port.parse().ok()?))
}

async fn announce_peer_to_target(
    transport: TransportActor,
    addr: SocketAddr,
    local_node_id: NodeId,
    info_hash: InfoHash,
    port: Option<u16>,
) -> io::Result<bool> {
    let Some(TransportReply::Response(response)) =
        transport.get_peers(addr, local_node_id, info_hash).await?
    else {
        return Ok(false);
    };

    let response_body = response.r.unwrap_or_default();
    if response_body.token.is_empty() {
        return Ok(false);
    }

    Ok(matches!(
        transport
            .announce_peer(
                addr,
                local_node_id,
                info_hash,
                response_body.token.as_ref(),
                port,
            )
            .await?,
        Some(TransportReply::Response(_))
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dht::krpc::{decode_message, KrpcInboundMessage, KrpcIncomingQuery};
    use crate::dht::routing::RoutingSnapshot;
    use crate::dht::test_support::{seeded_info_hash, seeded_node_id};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::net::UdpSocket;
    use tokio::task::JoinHandle;
    use tokio::time::{sleep, timeout};

    async fn bind_test_runtime(config: RuntimeConfig) -> io::Result<Runtime> {
        let (network_handle, network_lease) = crate::networking::runtime::test_network_lease();
        let runtime = Runtime::bind(&network_lease, config).await;
        std::mem::forget(network_handle);
        runtime
    }

    #[tokio::test(start_paused = true)]
    async fn bootstrap_sources_resolve_concurrently_and_preserve_configured_order() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum_active = Arc::new(AtomicUsize::new(0));
        let sources = vec![
            "alpha.invalid:4101".to_string(),
            "beta.invalid:4102".to_string(),
            "gamma.invalid:4103".to_string(),
        ];

        let resolved = resolve_bootstrap_sources_with(&sources, {
            let active = active.clone();
            let maximum_active = maximum_active.clone();
            move |_host, port| {
                let active = active.clone();
                let maximum_active = maximum_active.clone();
                async move {
                    let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum_active.fetch_max(now_active, Ordering::SeqCst);
                    sleep(Duration::from_millis(u64::from(4104 - port))).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Some(vec![SocketAddr::new(
                        IpAddr::V4(Ipv4Addr::new(192, 0, 2, (port - 4100) as u8)),
                        port,
                    )])
                }
            }
        })
        .await;

        assert!(maximum_active.load(Ordering::SeqCst) > 1);
        assert_eq!(
            resolved.iter().map(SocketAddr::port).collect::<Vec<_>>(),
            vec![4101, 4102, 4103]
        );
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum ReplayBehavior {
        Referrals(Vec<CompactNode>),
        Peers(Vec<CompactPeer>),
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct QueryLogEntry {
        responder: SocketAddr,
        source: SocketAddr,
        kind: KrpcQueryKind,
    }

    async fn spawn_replay_responder(
        socket: UdpSocket,
        node_id: NodeId,
        behavior: ReplayBehavior,
        query_log: Arc<Mutex<Vec<QueryLogEntry>>>,
    ) -> JoinHandle<()> {
        let responder_addr = socket.local_addr().expect("replay responder local addr");
        tokio::spawn(async move {
            let mut buffer = [0u8; 2048];
            loop {
                let (len, source) = match socket.recv_from(&mut buffer).await {
                    Ok(result) => result,
                    Err(_) => break,
                };

                let Ok(message) = decode_message(&buffer[..len]) else {
                    continue;
                };
                let KrpcInboundMessage::Query(query) = message else {
                    continue;
                };

                query_log
                    .lock()
                    .expect("replay query log lock")
                    .push(QueryLogEntry {
                        responder: responder_addr,
                        source,
                        kind: query.kind(),
                    });

                let response = match query {
                    KrpcIncomingQuery::Ping { transaction_id, .. } => {
                        Some(KrpcResponseEnvelope::new(
                            transaction_id.as_ref(),
                            KrpcResponseBody::pong(node_id),
                        ))
                    }
                    KrpcIncomingQuery::FindNode { transaction_id, .. } => {
                        let nodes = match &behavior {
                            ReplayBehavior::Referrals(nodes) => nodes.as_slice(),
                            ReplayBehavior::Peers(_) => &[],
                        };
                        Some(KrpcResponseEnvelope::new(
                            transaction_id.as_ref(),
                            KrpcResponseBody::with_nodes(node_id, nodes, AddressFamily::Ipv4),
                        ))
                    }
                    KrpcIncomingQuery::GetPeers { transaction_id, .. } => Some(match &behavior {
                        ReplayBehavior::Referrals(nodes) => KrpcResponseEnvelope::new(
                            transaction_id.as_ref(),
                            KrpcResponseBody::with_closest_nodes(
                                node_id,
                                nodes,
                                AddressFamily::Ipv4,
                                b"rt",
                            ),
                        ),
                        ReplayBehavior::Peers(peers) => KrpcResponseEnvelope::new(
                            transaction_id.as_ref(),
                            KrpcResponseBody::with_peers(node_id, peers, b"rt"),
                        ),
                    }),
                    KrpcIncomingQuery::AnnouncePeer { transaction_id, .. } => {
                        Some(KrpcResponseEnvelope::new(
                            transaction_id.as_ref(),
                            KrpcResponseBody::pong(node_id),
                        ))
                    }
                };

                if let Some(response) = response {
                    let Ok(payload) = serde_bencode::to_bytes(&response) else {
                        continue;
                    };
                    let _ = socket.send_to(&payload, source).await;
                }
            }
        })
    }

    async fn spawn_delayed_get_peers_responder(
        socket: UdpSocket,
        node_id: NodeId,
        response_body: KrpcResponseBody,
        delay: Duration,
        query_log: Arc<Mutex<Vec<QueryLogEntry>>>,
    ) -> JoinHandle<()> {
        let responder_addr = socket.local_addr().expect("delayed responder local addr");
        tokio::spawn(async move {
            let mut buffer = [0u8; 2048];
            loop {
                let (len, source) = match socket.recv_from(&mut buffer).await {
                    Ok(result) => result,
                    Err(_) => break,
                };

                let Ok(message) = decode_message(&buffer[..len]) else {
                    continue;
                };
                let KrpcInboundMessage::Query(query) = message else {
                    continue;
                };

                query_log
                    .lock()
                    .expect("delayed query log lock")
                    .push(QueryLogEntry {
                        responder: responder_addr,
                        source,
                        kind: query.kind(),
                    });

                let transaction_id = match query {
                    KrpcIncomingQuery::GetPeers { transaction_id, .. } => transaction_id,
                    KrpcIncomingQuery::Ping { transaction_id, .. } => {
                        let response = KrpcResponseEnvelope::new(
                            transaction_id.as_ref(),
                            KrpcResponseBody::pong(node_id),
                        );
                        if let Ok(payload) = serde_bencode::to_bytes(&response) {
                            let _ = socket.send_to(&payload, source).await;
                        }
                        continue;
                    }
                    KrpcIncomingQuery::FindNode { transaction_id, .. }
                    | KrpcIncomingQuery::AnnouncePeer { transaction_id, .. } => {
                        let response = KrpcResponseEnvelope::new(
                            transaction_id.as_ref(),
                            KrpcResponseBody::pong(node_id),
                        );
                        if let Ok(payload) = serde_bencode::to_bytes(&response) {
                            let _ = socket.send_to(&payload, source).await;
                        }
                        continue;
                    }
                };

                sleep(delay).await;
                let response =
                    KrpcResponseEnvelope::new(transaction_id.as_ref(), response_body.clone());
                if let Ok(payload) = serde_bencode::to_bytes(&response) {
                    let _ = socket.send_to(&payload, source).await;
                }
            }
        })
    }

    async fn wait_for_query(
        query_log: Arc<Mutex<Vec<QueryLogEntry>>>,
        responder: SocketAddr,
        kind: KrpcQueryKind,
    ) {
        timeout(Duration::from_secs(2), async {
            loop {
                if query_log
                    .lock()
                    .expect("query log lock")
                    .iter()
                    .any(|entry| entry.responder == responder && entry.kind == kind)
                {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for query");
    }

    #[tokio::test]
    async fn runtime_bind_requires_at_least_one_transport() {
        let error = bind_test_runtime(RuntimeConfig {
            local_node_id: seeded_node_id(0x01),
            allow_public_ipv4_identity: false,
            bootstrap_nodes: Vec::new(),
            bootstrap_sources: Vec::new(),
            ipv4_bind_addr: None,
            ipv6_bind_addr: None,
            persistence: None,
        })
        .await
        .expect_err("runtime bind without an address family should fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn runtime_bind_supports_ipv6_only_policy() {
        let runtime = match bind_test_runtime(RuntimeConfig {
            local_node_id: seeded_node_id(0x21),
            allow_public_ipv4_identity: false,
            bootstrap_nodes: Vec::new(),
            bootstrap_sources: Vec::new(),
            ipv4_bind_addr: None,
            ipv6_bind_addr: Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0)),
            persistence: None,
        })
        .await
        {
            Ok(runtime) => runtime,
            Err(error) if ipv6_test_bind_unavailable(&error) => return,
            Err(error) => panic!("bind IPv6-only DHT runtime: {error}"),
        };

        assert!(!runtime.family_bound(AddressFamily::Ipv4));
        assert!(runtime.family_bound(AddressFamily::Ipv6));
        assert_eq!(runtime.bound_family_count(), 1);
    }

    #[tokio::test]
    async fn runtime_bind_continues_without_ipv6_when_ipv6_port_is_unavailable() {
        let occupied_ipv6 =
            match UdpSocket::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0)).await {
                Ok(socket) => socket,
                Err(error) if ipv6_test_bind_unavailable(&error) => return,
                Err(error) => panic!("bind occupied IPv6 test socket: {error}"),
            };
        let occupied_addr = occupied_ipv6
            .local_addr()
            .expect("occupied IPv6 local addr");

        let runtime = bind_test_runtime(RuntimeConfig {
            local_node_id: seeded_node_id(0x02),
            allow_public_ipv4_identity: false,
            bootstrap_nodes: Vec::new(),
            bootstrap_sources: Vec::new(),
            ipv4_bind_addr: Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)),
            ipv6_bind_addr: Some(occupied_addr),
            persistence: None,
        })
        .await
        .expect("runtime should start with IPv4 when IPv6 bind fails");

        assert!(runtime.family_bound(AddressFamily::Ipv4));
        assert!(!runtime.family_bound(AddressFamily::Ipv6));
    }

    #[tokio::test]
    async fn runtime_does_not_register_lookup_without_seed_candidates() {
        let mut runtime = bind_test_runtime(RuntimeConfig {
            local_node_id: seeded_node_id(0x03),
            allow_public_ipv4_identity: false,
            bootstrap_nodes: Vec::new(),
            bootstrap_sources: Vec::new(),
            ipv4_bind_addr: Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)),
            ipv6_bind_addr: None,
            persistence: None,
        })
        .await
        .expect("bind runtime");

        let (lookup_id, mut peer_rx) = runtime
            .start_get_peers(AddressFamily::Ipv4, seeded_info_hash(0x04))
            .await
            .expect("empty lookup should not fail");

        assert!(!runtime.is_lookup_active(lookup_id));
        assert_eq!(runtime.active_lookup_count(), 0);
        assert!(runtime.lookup_quality_snapshot(lookup_id).is_none());
        assert!(peer_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn runtime_tracks_unique_responsive_bootstrap_nodes_by_family() {
        let bootstrap_ipv4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 6881);
        let bootstrap_ipv6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 6881);
        let non_bootstrap = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 6882);
        let mut runtime = bind_test_runtime(RuntimeConfig {
            local_node_id: seeded_node_id(0x13),
            allow_public_ipv4_identity: false,
            bootstrap_nodes: vec![bootstrap_ipv4, bootstrap_ipv6],
            bootstrap_sources: Vec::new(),
            ipv4_bind_addr: Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)),
            ipv6_bind_addr: None,
            persistence: None,
        })
        .await
        .expect("bind runtime");

        runtime.record_responsive_bootstrap(bootstrap_ipv4);
        runtime.record_responsive_bootstrap(bootstrap_ipv4);
        runtime.record_responsive_bootstrap(bootstrap_ipv6);
        runtime.record_responsive_bootstrap(non_bootstrap);

        let health = runtime.health_snapshot();
        assert_eq!(health.bootstrap_responsive_count, 2);
        assert_eq!(health.bootstrap_responsive_ipv4_count, 1);
        assert_eq!(health.bootstrap_responsive_ipv6_count, 1);
    }

    #[tokio::test]
    async fn runtime_rotates_local_node_id_after_confirmed_public_ipv4() {
        let mut runtime = bind_test_runtime(RuntimeConfig {
            local_node_id: seeded_node_id(0x04),
            allow_public_ipv4_identity: true,
            bootstrap_nodes: Vec::new(),
            bootstrap_sources: Vec::new(),
            ipv4_bind_addr: Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)),
            ipv6_bind_addr: None,
            persistence: None,
        })
        .await
        .expect("bind runtime");
        let old_node_id = runtime.local_node_id();
        let public_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(45, 67, 89, 10)), 6881);
        let mut route = NodeRecord::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(45, 67, 89, 20)), 6881),
            Some(seeded_node_id(0x44)),
            Instant::now(),
        );
        route.note_query_response(Some(seeded_node_id(0x44)), Instant::now());
        assert!(matches!(
            runtime
                .ipv4_routing
                .table_mut()
                .insert(route, Instant::now()),
            InsertOutcome::Inserted
        ));

        runtime.apply_confirmed_public_identity(Some(public_addr));

        assert_ne!(runtime.local_node_id(), old_node_id);
        assert_eq!(
            classify_node(public_addr, Some(runtime.local_node_id())),
            Bep42State::Compliant
        );
        assert_eq!(
            runtime.ipv4_routing.table().local_node_id(),
            runtime.local_node_id()
        );
        assert_eq!(runtime.active_route_count(AddressFamily::Ipv4), 1);
    }

    #[tokio::test]
    async fn runtime_keeps_configured_local_node_id_when_public_identity_disabled() {
        let mut runtime = bind_test_runtime(RuntimeConfig {
            local_node_id: seeded_node_id(0x05),
            allow_public_ipv4_identity: false,
            bootstrap_nodes: Vec::new(),
            bootstrap_sources: Vec::new(),
            ipv4_bind_addr: Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)),
            ipv6_bind_addr: None,
            persistence: None,
        })
        .await
        .expect("bind runtime");
        let old_node_id = runtime.local_node_id();
        let public_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(45, 67, 89, 11)), 6881);

        runtime.apply_confirmed_public_identity(Some(public_addr));

        assert_eq!(runtime.local_node_id(), old_node_id);
    }

    fn ipv6_test_bind_unavailable(error: &io::Error) -> bool {
        matches!(
            error.kind(),
            io::ErrorKind::AddrNotAvailable
                | io::ErrorKind::Unsupported
                | io::ErrorKind::PermissionDenied
        )
    }

    #[tokio::test]
    async fn runtime_re_resolves_bootstrap_sources_when_initial_resolution_was_empty() {
        let query_log = Arc::new(Mutex::new(Vec::new()));
        let bootstrap_socket = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("bind bootstrap");
        let bootstrap_addr = bootstrap_socket.local_addr().expect("bootstrap addr");
        let terminal_peers = [CompactPeer {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 43000),
        }];
        let handle = spawn_replay_responder(
            bootstrap_socket,
            seeded_node_id(0x70),
            ReplayBehavior::Peers(terminal_peers.to_vec()),
            query_log.clone(),
        )
        .await;

        let mut runtime = bind_test_runtime(RuntimeConfig {
            local_node_id: seeded_node_id(0x71),
            allow_public_ipv4_identity: false,
            bootstrap_nodes: Vec::new(),
            bootstrap_sources: vec![bootstrap_addr.to_string()],
            ipv4_bind_addr: Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)),
            ipv6_bind_addr: None,
            persistence: None,
        })
        .await
        .expect("bind runtime");

        let (_lookup_id, mut peer_rx) = runtime
            .start_get_peers(AddressFamily::Ipv4, seeded_info_hash(0x72))
            .await
            .expect("start get_peers lookup");
        assert_eq!(runtime.config.bootstrap_nodes, vec![bootstrap_addr]);

        let peers = timeout(Duration::from_secs(2), async {
            loop {
                tokio::select! {
                    maybe_batch = peer_rx.recv() => {
                        return maybe_batch.expect("peer receiver closed before bootstrap reply");
                    }
                    step_result = runtime.step() => {
                        let active = step_result.expect("runtime step");
                        assert!(active, "runtime step loop terminated before bootstrap reply");
                    }
                }
            }
        })
        .await
        .expect("timed out waiting for bootstrap source lookup");

        assert_eq!(
            peers,
            terminal_peers
                .iter()
                .map(|peer| peer.addr)
                .collect::<Vec<_>>()
        );
        wait_for_query(query_log, bootstrap_addr, KrpcQueryKind::GetPeers).await;
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn runtime_scripted_network_replay_reaches_peers() {
        let query_log = Arc::new(Mutex::new(Vec::new()));

        let bootstrap_a_socket =
            UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .await
                .expect("bind bootstrap A");
        let bootstrap_a_addr = bootstrap_a_socket.local_addr().expect("bootstrap A addr");

        let bootstrap_b_socket =
            UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .await
                .expect("bind bootstrap B");
        let bootstrap_b_addr = bootstrap_b_socket.local_addr().expect("bootstrap B addr");

        let branch_a_socket = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("bind branch A");
        let branch_a_addr = branch_a_socket.local_addr().expect("branch A addr");

        let branch_b_socket = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("bind branch B");
        let branch_b_addr = branch_b_socket.local_addr().expect("branch B addr");

        let terminal_socket = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("bind terminal");
        let terminal_addr = terminal_socket.local_addr().expect("terminal addr");

        let bootstrap_referrals = [
            CompactNode {
                id: seeded_node_id(0x20),
                addr: branch_a_addr,
            },
            CompactNode {
                id: seeded_node_id(0x21),
                addr: branch_b_addr,
            },
        ];
        let branch_a_referrals = [CompactNode {
            id: seeded_node_id(0x30),
            addr: terminal_addr,
        }];
        let branch_b_referrals = [CompactNode {
            id: seeded_node_id(0x30),
            addr: terminal_addr,
        }];
        let terminal_peers = [
            CompactPeer {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 41000),
            },
            CompactPeer {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 41001),
            },
        ];

        let handles = vec![
            spawn_replay_responder(
                bootstrap_a_socket,
                seeded_node_id(0x10),
                ReplayBehavior::Referrals(bootstrap_referrals.to_vec()),
                query_log.clone(),
            )
            .await,
            spawn_replay_responder(
                bootstrap_b_socket,
                seeded_node_id(0x11),
                ReplayBehavior::Referrals(bootstrap_referrals.to_vec()),
                query_log.clone(),
            )
            .await,
            spawn_replay_responder(
                branch_a_socket,
                seeded_node_id(0x20),
                ReplayBehavior::Referrals(branch_a_referrals.to_vec()),
                query_log.clone(),
            )
            .await,
            spawn_replay_responder(
                branch_b_socket,
                seeded_node_id(0x21),
                ReplayBehavior::Referrals(branch_b_referrals.to_vec()),
                query_log.clone(),
            )
            .await,
            spawn_replay_responder(
                terminal_socket,
                seeded_node_id(0x30),
                ReplayBehavior::Peers(terminal_peers.to_vec()),
                query_log.clone(),
            )
            .await,
        ];

        let mut runtime = bind_test_runtime(RuntimeConfig {
            local_node_id: seeded_node_id(0x01),
            allow_public_ipv4_identity: false,
            bootstrap_nodes: vec![bootstrap_a_addr, bootstrap_b_addr],
            bootstrap_sources: Vec::new(),
            ipv4_bind_addr: Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)),
            ipv6_bind_addr: None,
            persistence: None,
        })
        .await
        .expect("bind runtime");

        let info_hash = seeded_info_hash(0x44);
        let (_lookup_id, mut peer_rx) = runtime
            .start_get_peers(AddressFamily::Ipv4, info_hash)
            .await
            .expect("start get_peers lookup");

        let peers = timeout(Duration::from_secs(2), async {
            loop {
                tokio::select! {
                    maybe_batch = peer_rx.recv() => {
                        return maybe_batch.expect("peer receiver closed before replay completed");
                    }
                    step_result = runtime.step() => {
                        let active = step_result.expect("runtime step");
                        assert!(active, "runtime step loop terminated before replay completed");
                    }
                }
            }
        })
        .await
        .expect("timed out waiting for runtime replay peers");

        let expected_peers = terminal_peers
            .iter()
            .map(|peer| peer.addr)
            .collect::<Vec<_>>();
        assert_eq!(
            peers.len(),
            expected_peers.len(),
            "unexpected peer batch size"
        );
        for expected in &expected_peers {
            assert!(peers.contains(expected), "missing replay peer {expected}");
        }

        let log = query_log.lock().expect("replay query log lock").clone();
        assert!(
            log.iter().any(|entry| {
                entry.responder == bootstrap_a_addr && entry.kind == KrpcQueryKind::GetPeers
            }) || log.iter().any(|entry| {
                entry.responder == bootstrap_b_addr && entry.kind == KrpcQueryKind::GetPeers
            }),
            "runtime never queried bootstrap responders during replay"
        );
        assert!(
            log.iter()
                .any(|entry| entry.responder == branch_a_addr
                    && entry.kind == KrpcQueryKind::GetPeers),
            "runtime never queried first-hop branch responder"
        );
        assert!(
            log.iter()
                .any(|entry| entry.responder == terminal_addr
                    && entry.kind == KrpcQueryKind::GetPeers),
            "runtime never reached terminal peer responder"
        );
        let get_peers_targets = log
            .iter()
            .filter(|entry| entry.kind == KrpcQueryKind::GetPeers)
            .map(|entry| entry.responder)
            .collect::<HashSet<_>>();
        let get_peers_query_count = log
            .iter()
            .filter(|entry| entry.kind == KrpcQueryKind::GetPeers)
            .count();
        assert_eq!(
            get_peers_targets.len(),
            get_peers_query_count,
            "scripted traversal should not issue duplicate get_peers queries"
        );
        assert!(
            get_peers_query_count <= 5,
            "scripted traversal used {get_peers_query_count} queries for {} peers",
            expected_peers.len()
        );

        for handle in handles {
            handle.abort();
            let _ = handle.await;
        }
    }

    #[tokio::test]
    async fn runtime_bind_restores_persisted_routes_only_for_matching_node_id() {
        let temp_dir = tempfile::tempdir().expect("temp dht persistence dir");
        let path = temp_dir.path().join("dht_state.json");
        let local_node_id = seeded_node_id(0x51);
        let manager = PersistenceManager::new(PersistenceConfig {
            path: path.clone(),
            max_age: Duration::from_secs(60),
        });
        let route = NodeRecord::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 45151),
            Some(seeded_node_id(0x52)),
            Instant::now(),
        );
        let empty_ipv6 = RoutingSnapshot {
            family: AddressFamily::Ipv6,
            buckets: Vec::new(),
            nodes: Vec::new(),
            replacement_count: 0,
            refresh_due_count: 0,
        };
        let ipv4_routes = RoutingSnapshot {
            family: AddressFamily::Ipv4,
            buckets: Vec::new(),
            nodes: vec![route],
            replacement_count: 0,
            refresh_due_count: 0,
        };
        let snapshot =
            manager.build_snapshot(local_node_id, &ipv4_routes, &empty_ipv6, SystemTime::now());
        manager
            .save_snapshot(&snapshot)
            .expect("save persisted dht state");

        let matching = bind_test_runtime(RuntimeConfig {
            local_node_id,
            allow_public_ipv4_identity: false,
            bootstrap_nodes: Vec::new(),
            bootstrap_sources: Vec::new(),
            ipv4_bind_addr: Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)),
            ipv6_bind_addr: None,
            persistence: Some(PersistenceConfig {
                path: path.clone(),
                max_age: Duration::from_secs(60),
            }),
        })
        .await
        .expect("bind matching runtime");
        assert_eq!(matching.active_route_count(AddressFamily::Ipv4), 1);
        assert_eq!(matching.active_route_count(AddressFamily::Ipv6), 0);

        let mismatched = bind_test_runtime(RuntimeConfig {
            local_node_id: seeded_node_id(0x53),
            allow_public_ipv4_identity: false,
            bootstrap_nodes: Vec::new(),
            bootstrap_sources: Vec::new(),
            ipv4_bind_addr: Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)),
            ipv6_bind_addr: None,
            persistence: Some(PersistenceConfig {
                path,
                max_age: Duration::from_secs(60),
            }),
        })
        .await
        .expect("bind mismatched runtime");
        assert_eq!(mismatched.active_route_count(AddressFamily::Ipv4), 0);
        assert_eq!(mismatched.active_route_count(AddressFamily::Ipv6), 0);
    }

    #[tokio::test]
    async fn draining_lookup_accepts_late_peers_without_pumping_more_queries() {
        let query_log = Arc::new(Mutex::new(Vec::new()));

        let bootstrap_socket = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("bind bootstrap");
        let bootstrap_addr = bootstrap_socket.local_addr().expect("bootstrap addr");

        let branch_socket = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("bind branch");
        let branch_addr = branch_socket.local_addr().expect("branch addr");

        let terminal_peers = [
            CompactPeer {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 42000),
            },
            CompactPeer {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 42001),
            },
        ];
        let branch_referral = [CompactNode {
            id: seeded_node_id(0x40),
            addr: branch_addr,
        }];
        let mut response_body = KrpcResponseBody::with_closest_nodes(
            seeded_node_id(0x10),
            &branch_referral,
            AddressFamily::Ipv4,
            b"rt",
        );
        response_body.values = terminal_peers
            .iter()
            .copied()
            .map(encode_compact_peer)
            .collect();

        let handles = vec![
            spawn_delayed_get_peers_responder(
                bootstrap_socket,
                seeded_node_id(0x10),
                response_body,
                Duration::from_millis(100),
                query_log.clone(),
            )
            .await,
            spawn_replay_responder(
                branch_socket,
                seeded_node_id(0x40),
                ReplayBehavior::Peers(Vec::new()),
                query_log.clone(),
            )
            .await,
        ];

        let mut runtime = bind_test_runtime(RuntimeConfig {
            local_node_id: seeded_node_id(0x01),
            allow_public_ipv4_identity: false,
            bootstrap_nodes: vec![bootstrap_addr],
            bootstrap_sources: Vec::new(),
            ipv4_bind_addr: Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)),
            ipv6_bind_addr: None,
            persistence: None,
        })
        .await
        .expect("bind runtime");

        let info_hash = seeded_info_hash(0x45);
        let (lookup_id, mut peer_rx) = runtime
            .start_get_peers(AddressFamily::Ipv4, info_hash)
            .await
            .expect("start get_peers lookup");

        wait_for_query(query_log.clone(), bootstrap_addr, KrpcQueryKind::GetPeers).await;
        assert!(runtime.pause_lookup_for_drain(lookup_id).is_some());
        assert_eq!(runtime.active_lookup_count(), 0);
        assert_eq!(runtime.draining_lookup_count(), 1);
        assert!(!runtime.drained_lookups_ready(&[lookup_id]));

        let peers = timeout(Duration::from_secs(2), async {
            loop {
                tokio::select! {
                    maybe_batch = peer_rx.recv() => {
                        return maybe_batch.expect("peer receiver closed before drained reply");
                    }
                    step_result = runtime.step() => {
                        let active = step_result.expect("runtime step");
                        assert!(active, "runtime step loop terminated before drained reply");
                    }
                }
            }
        })
        .await
        .expect("timed out waiting for drained peer reply");

        let expected_peers = terminal_peers
            .iter()
            .map(|peer| peer.addr)
            .collect::<Vec<_>>();
        assert_eq!(peers, expected_peers);
        assert!(runtime.drained_lookups_ready(&[lookup_id]));
        assert!(
            query_log
                .lock()
                .expect("query log lock")
                .iter()
                .all(|entry| entry.responder != branch_addr),
            "draining lookup should not pump discovered branch candidates"
        );

        let drained_state = runtime
            .finish_drained_lookup(lookup_id)
            .expect("finished drained lookup state");
        assert_eq!(drained_state.quality_snapshot().received_peer_count, 2);
        assert_eq!(runtime.active_lookup_count(), 0);
        assert_eq!(runtime.draining_lookup_count(), 0);

        for handle in handles {
            handle.abort();
            let _ = handle.await;
        }
    }
}
