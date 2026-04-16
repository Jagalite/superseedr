// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::krpc::{
    KrpcErrorEnvelope, KrpcIncomingQuery, KrpcResponseBody, KrpcResponseEnvelope,
};
use super::peer_store::PeerStore;
use super::routing::RoutingTable;
use super::token::TokenService;
use super::types::{AddressFamily, CompactNode, CompactPeer, InfoHash, NodeId, NodeRecord};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant, SystemTime};

const ERROR_PROTOCOL: i64 = 203;

#[derive(Debug, Clone)]
pub struct InboundConfig {
    pub family: AddressFamily,
    pub max_queries_per_second: usize,
    pub burst_capacity: usize,
    pub closest_nodes_limit: usize,
}

impl Default for InboundConfig {
    fn default() -> Self {
        Self {
            family: AddressFamily::Ipv4,
            max_queries_per_second: 256,
            burst_capacity: 512,
            closest_nodes_limit: 8,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundRequestContext {
    pub source: SocketAddr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundAction {
    Respond(KrpcResponseEnvelope),
    Error(KrpcErrorEnvelope),
    Drop,
}

#[derive(Debug, Clone)]
struct RateLimiter {
    last_refill_at: Instant,
    tokens: f64,
}

#[derive(Debug, Clone)]
pub struct InboundActor {
    config: InboundConfig,
    per_ip_rate_limits: HashMap<IpAddr, RateLimiter>,
}

impl InboundActor {
    pub fn new(config: InboundConfig) -> Self {
        Self {
            config,
            per_ip_rate_limits: HashMap::new(),
        }
    }

    pub fn family(&self) -> AddressFamily {
        self.config.family
    }

    pub fn config(&self) -> &InboundConfig {
        &self.config
    }

    pub fn handle_query(
        &mut self,
        ctx: InboundRequestContext,
        query: KrpcIncomingQuery,
        local_node_id: NodeId,
        routing: &mut RoutingTable,
        token_service: &mut TokenService,
        peer_store: &mut PeerStore,
        now: Instant,
        wall_clock: SystemTime,
    ) -> InboundAction {
        if ctx.source.is_ipv4() != matches!(self.config.family, AddressFamily::Ipv4) {
            return InboundAction::Drop;
        }

        if !self.allow_query(ctx.source.ip(), now) {
            return InboundAction::Drop;
        }

        let transaction_id = query.transaction_id().to_vec();
        let requester_id = match query.requester_id() {
            Some(node_id) => node_id,
            None => {
                return InboundAction::Error(KrpcErrorEnvelope::new(
                    &transaction_id,
                    ERROR_PROTOCOL,
                    "invalid node id",
                ));
            }
        };

        remember_inbound_node(routing, ctx.source, requester_id, now);

        match query {
            KrpcIncomingQuery::Ping { .. } => InboundAction::Respond(KrpcResponseEnvelope::new(
                &transaction_id,
                KrpcResponseBody::pong(local_node_id),
            )),
            KrpcIncomingQuery::FindNode { args, .. } => {
                let Ok(target) = NodeId::try_from(args.target.as_ref()) else {
                    return InboundAction::Error(KrpcErrorEnvelope::new(
                        &transaction_id,
                        ERROR_PROTOCOL,
                        "invalid target",
                    ));
                };

                let nodes = self.closest_nodes_for(routing, target, ctx.source, now);
                InboundAction::Respond(KrpcResponseEnvelope::new(
                    &transaction_id,
                    KrpcResponseBody::with_nodes(local_node_id, &nodes, self.config.family),
                ))
            }
            KrpcIncomingQuery::GetPeers { args, .. } => {
                let Ok(info_hash) = InfoHash::try_from(args.info_hash.as_ref()) else {
                    return InboundAction::Error(KrpcErrorEnvelope::new(
                        &transaction_id,
                        ERROR_PROTOCOL,
                        "invalid info_hash",
                    ));
                };

                let token = token_service.mint_for(ctx.source.ip(), now);
                let peers = peer_store.peers_for(info_hash, self.config.family, wall_clock);
                let body = if peers.is_empty() {
                    let nodes = self.closest_nodes_for(routing, info_hash.into(), ctx.source, now);
                    KrpcResponseBody::with_closest_nodes(
                        local_node_id,
                        &nodes,
                        self.config.family,
                        &token,
                    )
                } else {
                    KrpcResponseBody::with_peers(local_node_id, &peers, &token)
                };

                InboundAction::Respond(KrpcResponseEnvelope::new(&transaction_id, body))
            }
            KrpcIncomingQuery::AnnouncePeer { args, .. } => {
                let Ok(info_hash) = InfoHash::try_from(args.info_hash.as_ref()) else {
                    return InboundAction::Error(KrpcErrorEnvelope::new(
                        &transaction_id,
                        ERROR_PROTOCOL,
                        "invalid info_hash",
                    ));
                };

                if !token_service.validate_for(ctx.source.ip(), args.token.as_ref(), now) {
                    return InboundAction::Error(KrpcErrorEnvelope::new(
                        &transaction_id,
                        ERROR_PROTOCOL,
                        "invalid token",
                    ));
                }

                let port = if args.implied_port.unwrap_or_default() != 0 {
                    ctx.source.port()
                } else {
                    args.port
                };

                if port == 0 {
                    return InboundAction::Error(KrpcErrorEnvelope::new(
                        &transaction_id,
                        ERROR_PROTOCOL,
                        "invalid port",
                    ));
                }

                let peer = CompactPeer {
                    addr: SocketAddr::new(ctx.source.ip(), port),
                };
                peer_store.insert(info_hash, peer, wall_clock);

                InboundAction::Respond(KrpcResponseEnvelope::new(
                    &transaction_id,
                    KrpcResponseBody::pong(local_node_id),
                ))
            }
        }
    }

    fn allow_query(&mut self, source_ip: IpAddr, now: Instant) -> bool {
        let burst = self.config.burst_capacity.max(self.config.max_queries_per_second.max(1));
        let fill_rate = self.config.max_queries_per_second.max(1) as f64;
        let limiter = self
            .per_ip_rate_limits
            .entry(source_ip)
            .or_insert_with(|| RateLimiter {
                last_refill_at: now,
                tokens: burst as f64,
            });

        let elapsed = now.saturating_duration_since(limiter.last_refill_at);
        limiter.last_refill_at = now;
        limiter.tokens = (limiter.tokens + elapsed.as_secs_f64() * fill_rate).min(burst as f64);
        if limiter.tokens < 1.0 {
            return false;
        }

        limiter.tokens -= 1.0;
        true
    }

    fn closest_nodes_for(
        &self,
        routing: &RoutingTable,
        target: NodeId,
        source: SocketAddr,
        now: Instant,
    ) -> Vec<CompactNode> {
        routing
            .closest_good_nodes(target, self.config.closest_nodes_limit, now)
            .into_iter()
            .filter(|record| record.addr != source)
            .filter_map(|record| {
                Some(CompactNode {
                    id: record.node_id?,
                    addr: record.addr,
                })
            })
            .collect()
    }
}

fn remember_inbound_node(
    routing: &mut RoutingTable,
    source: SocketAddr,
    node_id: NodeId,
    now: Instant,
) {
    if !routing.record_inbound_query(source, Some(node_id), now) {
        let mut record = NodeRecord::new(source, Some(node_id), now);
        record.note_inbound_query(now);
        let _ = routing.insert(record, now);
    }
}
