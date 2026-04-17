// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::routing::RoutingTable;
use super::types::{AddressFamily, NodeId};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct BootstrapConfig {
    pub bootstrap_nodes: Vec<SocketAddr>,
    pub refresh_interval: Duration,
    pub ping_interval: Duration,
    pub max_refresh_lookups_per_family: usize,
    pub max_questionable_pings_per_family: usize,
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self {
            bootstrap_nodes: Vec::new(),
            refresh_interval: Duration::from_secs(15 * 60),
            ping_interval: Duration::from_secs(5 * 60),
            max_refresh_lookups_per_family: 1,
            max_questionable_pings_per_family: 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupLookupPlan {
    pub family: AddressFamily,
    pub target: NodeId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilyMaintenancePlan {
    pub self_lookup_target: Option<NodeId>,
    pub refresh_targets: Vec<NodeId>,
    pub ping_targets: Vec<SocketAddr>,
}

#[derive(Debug, Clone)]
pub struct BootstrapCoordinator {
    config: BootstrapConfig,
    last_ping_at: HashMap<AddressFamily, Instant>,
}

impl BootstrapCoordinator {
    pub fn new(config: BootstrapConfig) -> Self {
        Self {
            config,
            last_ping_at: HashMap::new(),
        }
    }

    pub fn config(&self) -> &BootstrapConfig {
        &self.config
    }

    pub fn startup_plan(
        &self,
        local_node_id: NodeId,
        families: impl IntoIterator<Item = AddressFamily>,
    ) -> Vec<StartupLookupPlan> {
        families
            .into_iter()
            .map(|family| StartupLookupPlan {
                family,
                target: local_node_id,
            })
            .collect()
    }

    pub fn maintenance_plan(
        &mut self,
        family: AddressFamily,
        routing: &RoutingTable,
        local_node_id: NodeId,
        now: Instant,
    ) -> FamilyMaintenancePlan {
        let routes_empty = routing.all_nodes().is_empty();
        let ping_due = self
            .last_ping_at
            .get(&family)
            .is_none_or(|last_ping| now.duration_since(*last_ping) >= self.config.ping_interval);

        let ping_targets = if ping_due {
            self.last_ping_at.insert(family, now);
            routing
                .questionable_nodes(self.config.max_questionable_pings_per_family, now)
                .into_iter()
                .map(|record| record.addr)
                .collect()
        } else {
            Vec::new()
        };

        let refresh_targets = if self.config.refresh_interval.is_zero() {
            Vec::new()
        } else {
            routing
                .refresh_plans(now)
                .into_iter()
                .take(self.config.max_refresh_lookups_per_family)
                .map(|plan| plan.target)
                .collect()
        };

        FamilyMaintenancePlan {
            self_lookup_target: routes_empty.then_some(local_node_id),
            refresh_targets,
            ping_targets,
        }
    }
}
