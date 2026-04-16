// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::peer_store::PeerStore;
use super::routing::RoutingSnapshot;
use super::transport::TransportActor;
use super::types::{Bep42State, NodeTrust};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DhtAnomalySummary {
    pub suspicious_nodes: usize,
    pub non_compliant_nodes: usize,
    pub dead_referrals: usize,
    pub id_churn_events: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DhtHealthSnapshot {
    pub ipv4_bound: bool,
    pub ipv6_bound: bool,
    pub inflight_queries_ipv4: usize,
    pub inflight_queries_ipv6: usize,
    pub routing_nodes_ipv4: usize,
    pub routing_nodes_ipv6: usize,
    pub replacement_nodes_ipv4: usize,
    pub replacement_nodes_ipv6: usize,
    pub refresh_due_buckets_ipv4: usize,
    pub refresh_due_buckets_ipv6: usize,
    pub peer_store_size: usize,
    pub bootstrap_responsive_count: usize,
    pub inbound_query_rate: usize,
    pub recent_lookup_success_rate: usize,
    pub anomalies: DhtAnomalySummary,
}

impl DhtHealthSnapshot {
    pub fn from_parts(
        ipv4_transport: Option<&TransportActor>,
        ipv6_transport: Option<&TransportActor>,
        ipv4_routing: Option<&RoutingSnapshot>,
        ipv6_routing: Option<&RoutingSnapshot>,
        peer_store: Option<&PeerStore>,
    ) -> Self {
        let anomalies = summarize_anomalies(ipv4_routing, ipv6_routing);
        Self {
            ipv4_bound: ipv4_transport.and_then(|transport| transport.local_addr().ok()).is_some(),
            ipv6_bound: ipv6_transport.and_then(|transport| transport.local_addr().ok()).is_some(),
            inflight_queries_ipv4: ipv4_transport.map(TransportActor::inflight_query_count).unwrap_or_default(),
            inflight_queries_ipv6: ipv6_transport.map(TransportActor::inflight_query_count).unwrap_or_default(),
            routing_nodes_ipv4: ipv4_routing.map(|snapshot| snapshot.nodes.len()).unwrap_or_default(),
            routing_nodes_ipv6: ipv6_routing.map(|snapshot| snapshot.nodes.len()).unwrap_or_default(),
            replacement_nodes_ipv4: ipv4_routing.map(|snapshot| snapshot.replacement_count).unwrap_or_default(),
            replacement_nodes_ipv6: ipv6_routing.map(|snapshot| snapshot.replacement_count).unwrap_or_default(),
            refresh_due_buckets_ipv4: ipv4_routing.map(|snapshot| snapshot.refresh_due_count).unwrap_or_default(),
            refresh_due_buckets_ipv6: ipv6_routing.map(|snapshot| snapshot.refresh_due_count).unwrap_or_default(),
            peer_store_size: peer_store.map(PeerStore::total_peer_count).unwrap_or_default(),
            bootstrap_responsive_count: 0,
            inbound_query_rate: 0,
            recent_lookup_success_rate: 0,
            anomalies,
        }
    }
}

fn summarize_anomalies(
    ipv4_routing: Option<&RoutingSnapshot>,
    ipv6_routing: Option<&RoutingSnapshot>,
) -> DhtAnomalySummary {
    let mut summary = DhtAnomalySummary::default();
    for snapshot in [ipv4_routing, ipv6_routing].into_iter().flatten() {
        for node in &snapshot.nodes {
            if node.trust == NodeTrust::Suspicious {
                summary.suspicious_nodes += 1;
            }
            if node.bep42_state == Bep42State::NonCompliant {
                summary.non_compliant_nodes += 1;
            }
            summary.dead_referrals += usize::from(node.dead_referral_count);
            summary.id_churn_events += usize::from(node.id_churn_count);
        }
    }
    summary
}
