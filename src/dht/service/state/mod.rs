// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::net::SocketAddr;
use std::time::Instant;

use super::{
    DemandPlannerAction, DemandPlannerModel, DemandPlannerReduction, DemandSliceMetrics,
    DemandSubscriberRegistry, DhtServiceConfig, NetworkScopeId, RecentUniquePeers,
    DHT_UNIQUE_PEERS_FOUND_WINDOW,
};

mod demand_command;
mod service;

pub(in crate::dht::service) use demand_command::{DhtDemandCommandAction, DhtDemandCommandEffect};
pub(in crate::dht::service) use service::{
    DhtServiceAction, DhtServiceEffect, DhtServiceModel, DhtServiceReduction,
};

pub(super) struct DhtServiceState {
    pub(super) service: DhtServiceModel,
    pub(super) demand_planner: DemandPlannerModel,
    pub(super) demand_subscribers: DemandSubscriberRegistry,
    pub(super) slice_metrics: DemandSliceMetrics,
    pub(super) recent_unique_peers: RecentUniquePeers,
    runtime_retry: Option<DhtRuntimeRetry>,
}

pub(super) struct DhtRuntimeRetry {
    pub(super) config: DhtServiceConfig,
    pub(super) scope_id: NetworkScopeId,
    pub(super) attempt: u8,
    pub(super) due: Instant,
}

impl DhtServiceState {
    pub(super) fn new(config: DhtServiceConfig, generation: u64, warning: Option<String>) -> Self {
        Self {
            service: DhtServiceModel::new(config, generation, warning),
            demand_planner: DemandPlannerModel::new(Instant::now()),
            demand_subscribers: DemandSubscriberRegistry::new(),
            slice_metrics: DemandSliceMetrics::default(),
            recent_unique_peers: RecentUniquePeers::new(DHT_UNIQUE_PEERS_FOUND_WINDOW),
            runtime_retry: None,
        }
    }

    pub(super) fn schedule_runtime_retry(
        &mut self,
        config: DhtServiceConfig,
        scope_id: NetworkScopeId,
        attempt: u8,
        due: Instant,
    ) {
        self.runtime_retry = Some(DhtRuntimeRetry {
            config,
            scope_id,
            attempt,
            due,
        });
    }

    pub(super) fn cancel_runtime_retry(&mut self) {
        self.runtime_retry = None;
    }

    pub(super) fn runtime_retry_due(&self) -> Option<Instant> {
        self.runtime_retry.as_ref().map(|retry| retry.due)
    }

    pub(super) fn take_runtime_retry(&mut self) -> Option<DhtRuntimeRetry> {
        self.runtime_retry.take()
    }

    pub(super) fn has_draining_demands(&self) -> bool {
        self.demand_planner.has_draining_demands()
    }

    pub(super) fn record_recent_peers(&mut self, peers: &[SocketAddr]) {
        self.recent_unique_peers.record_batch(Instant::now(), peers);
    }

    pub(super) fn expire_recent_peers(&mut self) {
        let _ = self.recent_unique_peers.unique_count(Instant::now());
    }

    pub(super) fn update_service_action(
        &mut self,
        action: DhtServiceAction,
    ) -> DhtServiceReduction {
        self.service.update(action)
    }

    pub(in crate::dht::service) fn update_demand_planner_action(
        &mut self,
        action: DemandPlannerAction<'_>,
    ) -> DemandPlannerReduction {
        self.demand_planner.update(action)
    }
}
