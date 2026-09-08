// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::{ActiveRuntime, BootstrapSummary, DhtBackendKind, DHT_DEMAND_POWER_BASE_SCALE_HALVES};
pub use crate::dht_model::{DhtHealthSnapshot, DhtSizeEstimate, DhtStatus, DhtWaveTelemetry};

#[derive(Debug)]
pub(in crate::dht::service) struct RecentUniquePeers {
    window: Duration,
    events: VecDeque<(Instant, SocketAddr)>,
    last_seen: HashMap<SocketAddr, Instant>,
}

impl RecentUniquePeers {
    pub(in crate::dht::service) fn new(window: Duration) -> Self {
        Self {
            window,
            events: VecDeque::new(),
            last_seen: HashMap::new(),
        }
    }

    pub(in crate::dht::service) fn record_batch(&mut self, now: Instant, peers: &[SocketAddr]) {
        self.evict_expired(now);
        for &peer in peers {
            self.events.push_back((now, peer));
            self.last_seen.insert(peer, now);
        }
    }

    fn evict_expired(&mut self, now: Instant) {
        while let Some((seen_at, peer)) = self.events.front().copied() {
            if now.saturating_duration_since(seen_at) < self.window {
                break;
            }
            self.events.pop_front();
            if self.last_seen.get(&peer).copied() == Some(seen_at) {
                self.last_seen.remove(&peer);
            }
        }
    }

    pub(in crate::dht::service) fn unique_count(&mut self, now: Instant) -> usize {
        self.evict_expired(now);
        self.last_seen.len()
    }
}

pub(super) fn build_status(
    active_runtime: Option<&ActiveRuntime>,
    backend: DhtBackendKind,
    preferred_backend: DhtBackendKind,
    warning: Option<String>,
    generation: u64,
    bootstrap: BootstrapSummary,
) -> DhtStatus {
    let mut health = DhtHealthSnapshot {
        backend,
        preferred_backend: Some(preferred_backend),
        enabled: !matches!(backend, DhtBackendKind::Disabled),
        exported_bootstrap_nodes: bootstrap.total,
        ipv4_bootstrap_nodes: bootstrap.ipv4,
        ipv6_bootstrap_nodes: bootstrap.ipv6,
        ..Default::default()
    };

    if let Some(active_runtime) = active_runtime {
        let runtime_health = active_runtime.runtime.health_snapshot();
        let ipv4_local_addr = active_runtime.runtime.ipv4_local_addr();
        let ipv6_local_addr = active_runtime.runtime.ipv6_local_addr();
        health.local_addr = ipv4_local_addr.or(ipv6_local_addr);
        health.ipv4_local_addr = ipv4_local_addr;
        health.ipv6_local_addr = ipv6_local_addr;
        health.bound_family_count = active_runtime.runtime.bound_family_count();
        health.cached_ipv4_routes = runtime_health.routing_nodes_ipv4;
        health.cached_ipv6_routes = runtime_health.routing_nodes_ipv6;
        health.active_ipv4_routes = runtime_health.routing_nodes_ipv4;
        health.active_ipv6_routes = runtime_health.routing_nodes_ipv6;
        health.inflight_lookups = active_runtime.runtime.active_lookup_count();
        health.inflight_ipv4_queries = runtime_health.inflight_queries_ipv4;
        health.inflight_ipv6_queries = runtime_health.inflight_queries_ipv6;
        health.public_addr = runtime_health
            .confirmed_public_addr_ipv4
            .or(runtime_health.confirmed_public_addr_ipv6);
        health.server_mode = Some(health.bound_family_count > 0);

        health.responsive_ipv4_bootstrap_nodes = runtime_health
            .bootstrap_responsive_ipv4_count
            .min(active_runtime.bootstrap.ipv4);
        health.responsive_ipv6_bootstrap_nodes = runtime_health
            .bootstrap_responsive_ipv6_count
            .min(active_runtime.bootstrap.ipv6);
    }

    DhtStatus {
        generation,
        warning,
        health,
    }
}

pub(super) fn publish_status(
    status_tx: &watch::Sender<DhtStatus>,
    active_runtime: Option<&ActiveRuntime>,
    warning: Option<String>,
    generation: u64,
    preferred_backend: DhtBackendKind,
    configured_bootstrap: BootstrapSummary,
) {
    let backend = active_runtime
        .map(|active| active.backend)
        .unwrap_or(DhtBackendKind::Disabled);
    let bootstrap = active_runtime
        .map(|active| active.bootstrap)
        .unwrap_or(configured_bootstrap);
    let _ = status_tx.send(build_status(
        active_runtime,
        backend,
        preferred_backend,
        warning,
        generation,
        bootstrap,
    ));
}

pub(super) fn build_wave_telemetry(
    active_runtime: Option<&ActiveRuntime>,
    unique_peers_found_last_10s: usize,
    demand_power_scale_halves: u8,
) -> DhtWaveTelemetry {
    let demand_power_scale_halves = demand_power_scale_halves.max(1);
    let demand_power_multiplier =
        demand_power_scale_halves.div_ceil(DHT_DEMAND_POWER_BASE_SCALE_HALVES);
    let Some(active_runtime) = active_runtime else {
        return DhtWaveTelemetry {
            unique_peers_found_last_10s,
            demand_power_multiplier,
            demand_power_scale_halves,
            ..DhtWaveTelemetry::default()
        };
    };

    let (inflight_ipv4_queries, inflight_ipv6_queries) =
        active_runtime.runtime.inflight_query_counts();

    DhtWaveTelemetry {
        active_lookups: active_runtime.runtime.active_lookup_count(),
        active_user_lookups: active_runtime.runtime.active_user_lookup_count(),
        inflight_ipv4_queries,
        inflight_ipv6_queries,
        unique_peers_found_last_10s,
        demand_power_multiplier,
        demand_power_scale_halves,
    }
}

pub(super) fn publish_wave_telemetry(
    wave_telemetry_tx: &watch::Sender<DhtWaveTelemetry>,
    active_runtime: Option<&ActiveRuntime>,
    recent_unique_peers: &mut RecentUniquePeers,
    demand_power_scale_halves: u8,
) {
    let telemetry = build_wave_telemetry(
        active_runtime,
        recent_unique_peers.unique_count(Instant::now()),
        demand_power_scale_halves,
    );
    if *wave_telemetry_tx.borrow() != telemetry {
        let _ = wave_telemetry_tx.send(telemetry);
    }
}
