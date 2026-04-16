// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::krpc::KrpcResponseBody;
use super::routing::{xor_distance, RoutingSnapshot};
use super::types::{
    AddressFamily, Bep42State, CompactNode, CompactPeer, InfoHash, LookupId, NodeId, NodeRecord,
    NodeTrust, TransactionId,
};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
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
            initial_concurrency: 8,
            concurrency: 4,
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

        state.seed_from_routing(routing);
        state.seed_bootstrap(bootstrap_nodes);
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

    pub fn next_candidates(&self) -> Vec<LookupCandidate> {
        let concurrency = if self.visited.is_empty() {
            self.config.initial_concurrency
        } else {
            self.config.concurrency
        };

        self.frontier
            .iter()
            .filter(|candidate| !self.visited.contains(&candidate.addr))
            .take(concurrency)
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
        };
        self.inflight.insert(transaction_id, query.clone());
        Some(query)
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

    fn seed_from_routing(&mut self, routing: &RoutingSnapshot) {
        for record in &routing.nodes {
            if record.family() != self.family {
                continue;
            }
            let insertion_order = self.next_order();
            self.insert_candidate(candidate_from_record(record, insertion_order));
        }
    }

    fn seed_bootstrap(&mut self, bootstrap_nodes: &[SocketAddr]) {
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
                live_referral_count: 0,
                dead_referral_count: 0,
                insertion_order,
                last_response_at: None,
            });
        }
    }

    fn absorb_discovered_nodes(&mut self, nodes: Vec<CompactNode>) -> Vec<CompactNode> {
        let mut accepted = Vec::new();
        for node in nodes {
            if self.visited.contains(&node.addr)
                || self.inflight.values().any(|query| query.candidate.addr == node.addr)
            {
                continue;
            }

            if self.prefix_count(node.addr) >= self.config.per_prefix_limit {
                continue;
            }

            let candidate = LookupCandidate {
                addr: node.addr,
                node_id: Some(node.id),
                trust: NodeTrust::Neutral,
                bep42: Bep42State::Unknown,
                live_referral_count: 0,
                dead_referral_count: 0,
                insertion_order: self.next_order(),
                last_response_at: None,
            };

            if self.insert_candidate(candidate) {
                accepted.push(node);
            }
        }
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
        self.closest_valid_responders.push(LookupResponder {
            addr: candidate.addr,
            node_id: candidate.node_id,
            trust: candidate.trust,
            bep42: candidate.bep42,
        });
        let target = self.target_id();
        self.closest_valid_responders.sort_by(|left, right| {
            compare_candidate_distance(left.node_id, right.node_id, &target)
        });
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
}

fn candidate_from_record(record: &NodeRecord, insertion_order: u64) -> LookupCandidate {
    LookupCandidate {
        addr: record.addr,
        node_id: record.node_id,
        trust: record.trust,
        bep42: record.bep42_state,
        live_referral_count: record.live_referral_count,
        dead_referral_count: record.dead_referral_count,
        insertion_order,
        last_response_at: record.last_query_response_at,
    }
}

fn compare_candidates(left: &LookupCandidate, right: &LookupCandidate, target: &NodeId) -> Ordering {
    compare_candidate_distance(left.node_id, right.node_id, target)
        .then_with(|| trust_rank(left.trust).cmp(&trust_rank(right.trust)))
        .then_with(|| bep42_rank(left.bep42).cmp(&bep42_rank(right.bep42)))
        .then_with(|| referral_quality_rank(left).cmp(&referral_quality_rank(right)))
        .then_with(|| response_recency_rank(left.last_response_at).cmp(&response_recency_rank(right.last_response_at)))
        .then_with(|| left.insertion_order.cmp(&right.insertion_order))
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
    (
        candidate.dead_referral_count,
        candidate.live_referral_count.wrapping_neg(),
    )
}

fn response_recency_rank(last_response_at: Option<Instant>) -> (u8, Option<Instant>) {
    match last_response_at {
        Some(at) => (0, Some(at)),
        None => (1, None),
    }
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
