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
pub mod routing;
pub mod service;
pub mod test_support;
pub mod token;
pub mod transport;
pub mod types;

use std::collections::{HashMap, HashSet};
use std::future::pending;
use std::io;
use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;

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

use crate::dht::health::DhtHealthSnapshot as InternalHealthSnapshot;
use crate::dht::bootstrap::{BootstrapConfig, BootstrapCoordinator};
use crate::dht::inbound::{InboundAction, InboundActor, InboundConfig, InboundRequestContext};
use crate::dht::lookup::{LookupManager, LookupState, LookupUpdate};
use crate::dht::peer_store::{PeerStore, PeerStoreConfig};
use crate::dht::persist::PersistenceManager;
use crate::dht::routing::{InsertOutcome, RoutingActor, RoutingConfig};
use crate::dht::token::{TokenConfig, TokenService};
use crate::dht::transport::{TransportActor, TransportConfig, TransportEvent, TransportReply};

const MAX_CACHED_RESPONDER_TARGETS: usize = 256;
const MAX_CACHED_RESPONDERS_PER_TARGET: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub local_node_id: NodeId,
    pub bootstrap_nodes: Vec<SocketAddr>,
    pub ipv4_bind_addr: Option<SocketAddr>,
    pub ipv6_bind_addr: Option<SocketAddr>,
    pub persistence: Option<PersistenceConfig>,
}

#[derive(Debug)]
struct ActiveLookup {
    family: AddressFamily,
    state: LookupState,
    peer_tx: mpsc::UnboundedSender<Vec<SocketAddr>>,
}

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

#[derive(Debug)]
pub struct Runtime {
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
    bootstrap_responsive_count: usize,
    inbound_query_count: usize,
    recent_lookup_success_count: usize,
}

impl Runtime {
    pub async fn bind(config: RuntimeConfig) -> io::Result<Self> {
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

        let (ipv4_transport, ipv4_events) = if let Some(bind_addr) = config.ipv4_bind_addr {
            let (transport, events) = TransportActor::bind(TransportConfig {
                family: AddressFamily::Ipv4,
                bind_addr,
                ..TransportConfig::default()
            })
            .await?;
            (Some(transport), Some(events))
        } else {
            (None, None)
        };

        let (ipv6_transport, ipv6_events) = if let Some(bind_addr) = config.ipv6_bind_addr {
            let (transport, events) = TransportActor::bind(TransportConfig {
                family: AddressFamily::Ipv6,
                bind_addr,
                ..TransportConfig::default()
            })
            .await?;
            (Some(transport), Some(events))
        } else {
            (None, None)
        };

        let (lookup_result_tx, lookup_result_rx) = mpsc::unbounded_channel();

        let bootstrap_nodes = config.bootstrap_nodes.clone();

        Ok(Self {
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
            bootstrap_responsive_count: 0,
            inbound_query_count: 0,
            recent_lookup_success_count: 0,
        })
    }

    pub fn config(&self) -> &RuntimeConfig {
        &self.config
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
        self.active_lookups.len()
    }

    pub fn active_user_lookup_count(&self) -> usize {
        self.active_lookups
            .len()
            .saturating_sub(self.maintenance_lookup_receivers.len())
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
        health.bootstrap_responsive_count = self.bootstrap_responsive_count;
        health.inbound_query_rate = self.inbound_query_count;
        health.recent_lookup_success_rate = self.recent_lookup_success_count;
        health
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

    pub async fn bootstrap_startup(&mut self) -> io::Result<()> {
        let families = [AddressFamily::Ipv4, AddressFamily::Ipv6]
            .into_iter()
            .filter(|family| self.family_bound(*family))
            .collect::<Vec<_>>();

        for plan in self
            .bootstrap
            .startup_plan(self.config.local_node_id, families)
        {
            self.start_internal_find_node(plan.family, plan.target).await?;
        }
        Ok(())
    }

    pub async fn run_maintenance(&mut self) -> io::Result<()> {
        self.cleanup_closed_lookups();
        let now = Instant::now();
        let local_node_id = self.config.local_node_id;

        for family in [AddressFamily::Ipv4, AddressFamily::Ipv6] {
            if !self.family_bound(family) {
                continue;
            }

            let pending_probes = self.take_pending_probe_targets(family, 8);
            self.ping_nodes(family, &pending_probes).await?;

            let routing = self.routing_for_family(family).clone();
            let plan = self.bootstrap.maintenance_plan(
                family,
                &routing,
                local_node_id,
                now,
            );

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
        trace_lookup_target(
            target_node_id,
            format!(
                "start family={:?} routing_nodes={} cached_responders={} bootstrap_nodes={}",
                family,
                routing_snapshot.nodes.len(),
                cached_responders.len(),
                self.config
                    .bootstrap_nodes
                    .iter()
                    .filter(|addr| AddressFamily::for_addr(**addr) == family)
                    .count(),
            ),
        );
        let state = self.lookup_manager.start(
            request,
            family,
            &routing_snapshot,
            &self.config.bootstrap_nodes,
            &cached_responders,
            now,
        );
        let (peer_tx, peer_rx) = mpsc::unbounded_channel();
        self.active_lookups.insert(
            lookup_id,
            ActiveLookup {
                family,
                state,
                peer_tx,
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
        self.start_lookup(family, LookupKind::GetPeers, LookupTarget::InfoHash(info_hash))
            .await
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
            io::Error::new(io::ErrorKind::AddrNotAvailable, "transport not bound for requested family")
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
                    let _ = self.routing_for_family_mut(family).record_failure(addr, now);
                }
            }
        }

        Ok(announced)
    }

    pub async fn step(&mut self) -> io::Result<bool> {
        self.cleanup_closed_lookups();

        let mut processed_lookup_results = 0usize;
        while processed_lookup_results < 64 {
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
                if drop_inbound_queries_enabled() {
                    return Ok(());
                }
                self.inbound_query_count = self.inbound_query_count.saturating_add(1);
                let now = Instant::now();
                let wall_clock = SystemTime::now();
                let local_node_id = self.config.local_node_id;
                let transport = self.transport_for(family).cloned().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotConnected, "transport unavailable")
                })?;

                let action = match family {
                    AddressFamily::Ipv4 => self.ipv4_inbound.handle_query(
                        InboundRequestContext { source },
                        query,
                        local_node_id,
                        self.ipv4_routing.table_mut(),
                        &mut self.token_service,
                        &mut self.peer_store,
                        now,
                        wall_clock,
                    ),
                    AddressFamily::Ipv6 => self.ipv6_inbound.handle_query(
                        InboundRequestContext { source },
                        query,
                        local_node_id,
                        self.ipv6_routing.table_mut(),
                        &mut self.token_service,
                        &mut self.peer_store,
                        now,
                        wall_clock,
                    ),
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
                if self.config.bootstrap_nodes.iter().any(|addr| *addr == source) {
                    self.bootstrap_responsive_count =
                        self.bootstrap_responsive_count.saturating_add(1);
                }
            }
            TransportEvent::Timeout { .. } => {}
        }
        Ok(())
    }

    async fn handle_lookup_result(&mut self, result: LookupTaskResult) -> io::Result<()> {
        trace_lookup_result(result.transaction_id, "runtime_handle");
        let now = Instant::now();
        let mut completed_addr = None;
        let mut completed_node_id = None;
        let mut discovered_nodes = Vec::new();
        let mut emitted_peers = Vec::new();
        let mut finished = false;
        let mut peer_tx = None;
        let mut receiver_closed = false;

        if let Some(active) = self.active_lookups.get_mut(&result.lookup_id) {
            receiver_closed = active.peer_tx.is_closed();
            peer_tx = Some(active.peer_tx.clone());
            let target_node_id = active.state.target_id();
            match result.outcome {
                LookupTaskOutcome::Reply(reply) => match reply {
                    TransportReply::Response(response) => {
                        let response_body = response.r.unwrap_or_default();
                        let update =
                            active.state.handle_response(result.transaction_id, &response_body, now);
                        trace_lookup_target(
                            target_node_id,
                            format!(
                                "response family={:?} tx={:?} from={} node_id={} nodes={} peers={} token={}",
                                result.family,
                                result.transaction_id,
                                update
                                    .completed_query
                                    .as_ref()
                                    .map(|query| query.candidate.addr.to_string())
                                    .unwrap_or_else(|| "<unknown>".to_string()),
                                response_body
                                    .node_id()
                                    .map(|node_id| node_id_hex(node_id))
                                    .unwrap_or_else(|| "<none>".to_string()),
                                update.discovered_nodes.len(),
                                update.emitted_peers.len(),
                                response_body.token.len(),
                            ),
                        );
                        if let Some(query) = update.completed_query {
                            completed_addr = Some(query.candidate.addr);
                            completed_node_id = response_body.node_id();
                        }
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
                    if let Some(query) = active.state.mark_soft_timeout(result.transaction_id) {
                        trace_lookup_target(
                            target_node_id,
                            format!(
                                "soft-timeout family={:?} tx={:?} from={}",
                                result.family, result.transaction_id, query.candidate.addr
                            ),
                        );
                    }
                    finished = active.state.is_finished();
                }
                LookupTaskOutcome::Timeout => {
                    let update = active.state.handle_timeout(result.transaction_id);
                    if let Some(query) = update.completed_query {
                        trace_lookup_target(
                            target_node_id,
                            format!(
                                "timeout family={:?} tx={:?} from={}",
                                result.family, result.transaction_id, query.candidate.addr
                            ),
                        );
                        completed_addr = Some(query.candidate.addr);
                    }
                    finished = update.finished;
                }
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
                if self.config.bootstrap_nodes.iter().any(|bootstrap| *bootstrap == addr) {
                    self.bootstrap_responsive_count =
                        self.bootstrap_responsive_count.saturating_add(1);
                }
                self.recent_lookup_success_count =
                    self.recent_lookup_success_count.saturating_add(1);
            } else {
                let _ = self.routing_for_family_mut(result.family).record_failure(addr, now);
            }
        }

        let mut probe_targets = Vec::new();
        for node in discovered_nodes {
            let record = NodeRecord::new(node.addr, Some(node.id), now);
            let outcome = self.routing_for_family_mut(result.family).insert(record, now);
            if let InsertOutcome::NeedsProbe { targets } = outcome {
                probe_targets.extend(targets);
            }
        }

        self.enqueue_probe_targets(result.family, &probe_targets);

        if let Some(peer_tx) = peer_tx {
            if !emitted_peers.is_empty() {
                if peer_tx.send(emitted_peers).is_err() {
                    receiver_closed = true;
                }
            }
        }

        if finished || receiver_closed {
            if let Some(active) = self.active_lookups.get(&result.lookup_id) {
                trace_lookup_target(
                    active.state.target_id(),
                    format!(
                        "finish family={:?} responders_cached={} receiver_closed={}",
                        active.family,
                        active
                            .state
                            .cacheable_responders(MAX_CACHED_RESPONDERS_PER_TARGET)
                            .len(),
                        receiver_closed,
                    ),
                );
            }
            self.cancel_lookup(result.lookup_id);
        } else if self.active_lookups.contains_key(&result.lookup_id) {
            self.pump_lookup(result.lookup_id).await?;
        }

        Ok(())
    }

    async fn pump_lookup(&mut self, lookup_id: LookupId) -> io::Result<()> {
        let (family, request, candidates) = match self.active_lookups.get(&lookup_id) {
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

        let transport = self.transport_for(family).cloned().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "transport unavailable")
        })?;

        for candidate in candidates {
            let target_node_id = request.target.as_node_id();
            trace_lookup_target(
                target_node_id,
                format!(
                    "send family={:?} kind={:?} addr={} node_id={} bep42={:?}",
                    family,
                    request.kind,
                    candidate.addr,
                    candidate
                        .node_id
                        .map(node_id_hex)
                        .unwrap_or_else(|| "<none>".to_string()),
                    candidate.bep42,
                ),
            );
            let sent_at = Instant::now();
            let _ = self
                .routing_for_family_mut(family)
                .record_query_sent(candidate.addr, sent_at);

            if ping_visit_enabled() {
                let ping_transport = transport.clone();
                let ping_addr = candidate.addr;
                let ping_family = family;
                let ping_target = target_node_id;
                let local_node_id = self.config.local_node_id;
                tokio::spawn(async move {
                    let outcome = match ping_transport.ping(ping_addr, local_node_id).await {
                        Ok(Some(TransportReply::Response(_))) => "pong",
                        Ok(Some(TransportReply::Error(_))) => "error",
                        Ok(None) => "timeout",
                        Err(_) => "send_error",
                    };
                    trace_lookup_target(
                        ping_target,
                        format!(
                            "visit-ping family={:?} addr={} outcome={}",
                            ping_family, ping_addr, outcome
                        ),
                    );
                });
            }

            let deferred = match request.kind {
                LookupKind::FindNode => {
                    let target = request.target.as_node_id();
                    transport
                        .send_query_deferred(
                            candidate.addr,
                            krpc::KrpcQueryKind::FindNode,
                            krpc::KrpcFindNodeArgs::new(self.config.local_node_id, target),
                        )
                        .await
                }
                LookupKind::GetPeers => {
                    let LookupTarget::InfoHash(info_hash) = request.target else {
                        continue;
                    };
                    transport
                        .send_query_deferred(
                            candidate.addr,
                            krpc::KrpcQueryKind::GetPeers,
                            krpc::KrpcGetPeersArgs::new(self.config.local_node_id, info_hash),
                        )
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
                            trace_lookup_result(transaction_id, "task_ready");
                            let send_result = outcome_tx.send(LookupTaskResult {
                                lookup_id,
                                family,
                                transaction_id,
                                outcome,
                            });
                            if send_result.is_ok() {
                                trace_lookup_result(transaction_id, "task_sent");
                            } else {
                                trace_lookup_result(transaction_id, "task_dropped");
                            }
                            break;
                        }
                        _ = &mut soft_timeout_sleep, if soft_timeout_enabled && !soft_timeout_sent => {
                            soft_timeout_sent = true;
                            trace_lookup_result(transaction_id, "task_ready");
                            let send_result = outcome_tx.send(LookupTaskResult {
                                lookup_id,
                                family,
                                transaction_id,
                                outcome: LookupTaskOutcome::SoftTimeout,
                            });
                            if send_result.is_ok() {
                                trace_lookup_result(transaction_id, "task_sent");
                            } else {
                                trace_lookup_result(transaction_id, "task_dropped");
                                break;
                            }
                        }
                        _ = &mut hard_timeout_sleep => {
                            timeout_transport.cancel_inflight_query(transaction_id);
                            trace_lookup_result(transaction_id, "task_ready");
                            let send_result = outcome_tx.send(LookupTaskResult {
                                lookup_id,
                                family,
                                transaction_id,
                                outcome: LookupTaskOutcome::Timeout,
                            });
                            if send_result.is_ok() {
                                trace_lookup_result(transaction_id, "task_sent");
                            } else {
                                trace_lookup_result(transaction_id, "task_dropped");
                            }
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

    fn cleanup_closed_lookups(&mut self) {
        let closed = self
            .active_lookups
            .iter()
            .filter_map(|(lookup_id, active)| active.peer_tx.is_closed().then_some(*lookup_id))
            .collect::<Vec<_>>();

        for lookup_id in closed {
            self.cancel_lookup(lookup_id);
        }
    }

    pub fn cancel_lookup(&mut self, lookup_id: LookupId) -> bool {
        let Some(active) = self.active_lookups.remove(&lookup_id) else {
            return false;
        };
        self.maintenance_lookup_receivers.remove(&lookup_id);
        self.cache_lookup_responders(active.family, &active.state);

        if let Some(transport) = self.transport_for(active.family).cloned() {
            for transaction_id in active.state.inflight_transaction_ids() {
                transport.cancel_inflight_query(transaction_id);
            }
        }

        true
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
        self.maintenance_lookup_receivers.insert(lookup_id, rx);
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
            let _ = self.routing_for_family_mut(family).record_query_sent(addr, sent_at);

            match transport.ping(addr, local_node_id).await? {
                Some(TransportReply::Response(response)) => {
                    let now = Instant::now();
                    let node_id = response.r.as_ref().and_then(KrpcResponseBody::node_id);
                    let routing = self.routing_for_family_mut(family);
                    if !routing.record_response(addr, node_id, now) {
                        let mut record = NodeRecord::new(addr, node_id, now);
                        record.note_query_response(node_id, now);
                        let _ = routing.insert(record, now);
                    }
                    if self.config.bootstrap_nodes.iter().any(|bootstrap| *bootstrap == addr) {
                        self.bootstrap_responsive_count =
                            self.bootstrap_responsive_count.saturating_add(1);
                    }
                }
                Some(TransportReply::Error(_)) | None => {
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

fn trace_lookup_target(target: NodeId, message: impl AsRef<str>) {
    static TRACE_ENABLED: OnceLock<bool> = OnceLock::new();
    static TRACE_TARGET: OnceLock<Option<String>> = OnceLock::new();

    let enabled = *TRACE_ENABLED.get_or_init(|| std::env::var_os("SUPERSEEDR_INTERNAL_TRACE").is_some());
    if !enabled {
        return;
    }

    let target_filter = TRACE_TARGET.get_or_init(|| {
        std::env::var("SUPERSEEDR_INTERNAL_TRACE_TARGET")
            .ok()
            .map(|value| value.to_ascii_lowercase())
    });

    if let Some(target_filter) = target_filter {
        if node_id_hex(target).to_ascii_lowercase() != *target_filter {
            return;
        }
    }

    eprintln!(
        "[internal-trace target={}] {}",
        node_id_hex(target),
        message.as_ref()
    );
}

fn node_id_hex(node_id: NodeId) -> String {
    hex::encode(node_id.as_ref())
}

fn ping_visit_enabled() -> bool {
    static PING_VISIT_ENABLED: OnceLock<bool> = OnceLock::new();
    *PING_VISIT_ENABLED.get_or_init(|| std::env::var_os("SUPERSEEDR_INTERNAL_TRACE_PING").is_some())
}

fn drop_inbound_queries_enabled() -> bool {
    static DROP_INBOUND: OnceLock<bool> = OnceLock::new();
    *DROP_INBOUND
        .get_or_init(|| std::env::var_os("SUPERSEEDR_INTERNAL_DROP_INBOUND_QUERIES").is_some())
}

fn trace_lookup_result(transaction_id: TransactionId, stage: &str) {
    static TRACE_RESULTS: OnceLock<bool> = OnceLock::new();
    let enabled =
        *TRACE_RESULTS.get_or_init(|| std::env::var_os("SUPERSEEDR_INTERNAL_TRACE_RESULTS").is_some());
    if !enabled {
        return;
    }

    eprintln!(
        "[internal-trace-result tx={} stage={}]",
        hex::encode(transaction_id.as_ref()),
        stage
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dht::krpc::{decode_message, KrpcInboundMessage, KrpcIncomingQuery};
    use crate::dht::test_support::{seeded_info_hash, seeded_node_id};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::{Arc, Mutex};
    use tokio::net::UdpSocket;
    use tokio::task::JoinHandle;
    use tokio::time::timeout;

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
        let responder_addr = socket
            .local_addr()
            .expect("replay responder local addr");
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

    #[tokio::test]
    async fn runtime_scripted_network_replay_reaches_peers() {
        let query_log = Arc::new(Mutex::new(Vec::new()));

        let bootstrap_a_socket = UdpSocket::bind(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
        ))
        .await
        .expect("bind bootstrap A");
        let bootstrap_a_addr = bootstrap_a_socket.local_addr().expect("bootstrap A addr");

        let bootstrap_b_socket = UdpSocket::bind(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
        ))
        .await
        .expect("bind bootstrap B");
        let bootstrap_b_addr = bootstrap_b_socket.local_addr().expect("bootstrap B addr");

        let branch_a_socket = UdpSocket::bind(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
        ))
        .await
        .expect("bind branch A");
        let branch_a_addr = branch_a_socket.local_addr().expect("branch A addr");

        let branch_b_socket = UdpSocket::bind(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
        ))
        .await
        .expect("bind branch B");
        let branch_b_addr = branch_b_socket.local_addr().expect("branch B addr");

        let terminal_socket = UdpSocket::bind(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
        ))
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

        let mut runtime = Runtime::bind(RuntimeConfig {
            local_node_id: seeded_node_id(0x01),
            bootstrap_nodes: vec![bootstrap_a_addr, bootstrap_b_addr],
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
        assert_eq!(peers.len(), expected_peers.len(), "unexpected peer batch size");
        for expected in &expected_peers {
            assert!(
                peers.contains(expected),
                "missing replay peer {expected}"
            );
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
                .any(|entry| entry.responder == branch_a_addr && entry.kind == KrpcQueryKind::GetPeers),
            "runtime never queried first-hop branch responder"
        );
        assert!(
            log.iter()
                .any(|entry| entry.responder == terminal_addr && entry.kind == KrpcQueryKind::GetPeers),
            "runtime never reached terminal peer responder"
        );

        for handle in handles {
            handle.abort();
            let _ = handle.await;
        }
    }
}
