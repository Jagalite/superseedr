// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(dead_code, unused_imports)]

pub mod anomaly;
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

use std::collections::HashMap;
use std::future::pending;
use std::io;
use std::net::SocketAddr;
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
use crate::dht::inbound::{InboundAction, InboundActor, InboundConfig, InboundRequestContext};
use crate::dht::lookup::{LookupManager, LookupState, LookupUpdate};
use crate::dht::peer_store::{PeerStore, PeerStoreConfig};
use crate::dht::persist::PersistenceManager;
use crate::dht::routing::{RoutingActor, RoutingConfig};
use crate::dht::token::{TokenConfig, TokenService};
use crate::dht::transport::{TransportActor, TransportConfig, TransportEvent, TransportReply};

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
    lookup_manager: LookupManager,
    active_lookups: HashMap<LookupId, ActiveLookup>,
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
            lookup_manager: LookupManager::new(LookupConfig::default()),
            active_lookups: HashMap::new(),
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

    pub async fn start_lookup(
        &mut self,
        family: AddressFamily,
        kind: LookupKind,
        target: LookupTarget,
    ) -> io::Result<(LookupId, mpsc::UnboundedReceiver<Vec<SocketAddr>>)> {
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
        let now = Instant::now();
        let routing_snapshot = match family {
            AddressFamily::Ipv4 => self.ipv4_routing.table().snapshot(now),
            AddressFamily::Ipv6 => self.ipv6_routing.table().snapshot(now),
        };
        let state = self.lookup_manager.start(
            request,
            family,
            &routing_snapshot,
            &self.config.bootstrap_nodes,
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
            result = self.lookup_result_rx.recv() => {
                match result {
                    Some(result) => {
                        self.handle_lookup_result(result).await?;
                        Ok(true)
                    }
                    None => Ok(false),
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
        let now = Instant::now();
        let mut completed_addr = None;
        let mut completed_node_id = None;
        let mut discovered_nodes = Vec::new();
        let mut emitted_peers = Vec::new();
        let mut finished = false;
        let mut peer_tx = None;

        if let Some(active) = self.active_lookups.get_mut(&result.lookup_id) {
            peer_tx = Some(active.peer_tx.clone());
            match result.outcome {
                LookupTaskOutcome::Reply(reply) => match reply {
                    TransportReply::Response(response) => {
                        let response_body = response.r.unwrap_or_default();
                        let update =
                            active.state.handle_response(result.transaction_id, &response_body, now);
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
                LookupTaskOutcome::Timeout => {
                    let update = active.state.handle_timeout(result.transaction_id);
                    if let Some(query) = update.completed_query {
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

        for node in discovered_nodes {
            let record = NodeRecord::new(node.addr, Some(node.id), now);
            let _ = self.routing_for_family_mut(result.family).insert(record, now);
        }

        if let Some(peer_tx) = peer_tx {
            if !emitted_peers.is_empty() {
                let _ = peer_tx.send(emitted_peers);
            }
        }

        if finished {
            self.active_lookups.remove(&result.lookup_id);
        } else if self.active_lookups.contains_key(&result.lookup_id) {
            self.pump_lookup(result.lookup_id).await?;
        }

        Ok(())
    }

    async fn pump_lookup(&mut self, lookup_id: LookupId) -> io::Result<()> {
        let (family, request, candidates) = match self.active_lookups.get(&lookup_id) {
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

            if let Some(active) = self.active_lookups.get_mut(&lookup_id) {
                let _ = active
                    .state
                    .mark_inflight(transaction_id, candidate.addr, Instant::now());
            }

            let outcome_tx = self.lookup_result_tx.clone();
            let timeout_window = transport.config().query_timeout;
            tokio::spawn(async move {
                let outcome = match timeout(timeout_window, response_rx).await {
                    Ok(Ok(reply)) => LookupTaskOutcome::Reply(reply),
                    Ok(Err(_)) | Err(_) => LookupTaskOutcome::Timeout,
                };
                let _ = outcome_tx.send(LookupTaskResult {
                    lookup_id,
                    family,
                    transaction_id,
                    outcome,
                });
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
}
