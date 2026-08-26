// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::net::SocketAddr;
use std::time::Instant;

use tokio::sync::{mpsc, oneshot};

use super::super::{
    observe_action_effect_reduction, DemandPlannerAction, DemandPlannerEffect, DemandSliceClass,
    DemandSubscriberAction, DemandSubscriberEffect, DhtCommand, DhtDemandMetrics, DhtDemandState,
    InfoHash,
};
use super::DhtServiceState;

pub(in crate::dht::service) enum DhtDemandCommandAction {
    Register {
        info_hash: InfoHash,
        demand: DhtDemandState,
        subscriber_tx: mpsc::UnboundedSender<Vec<SocketAddr>>,
        response_tx: oneshot::Sender<Option<u64>>,
        now: Instant,
    },
    Update {
        info_hash: InfoHash,
        demand: DhtDemandState,
        now: Instant,
    },
    UpdateMetrics {
        info_hash: InfoHash,
        metrics: DhtDemandMetrics,
    },
    Unregister {
        info_hash: InfoHash,
        subscriber_id: u64,
        now: Instant,
    },
    PruneDeadSubscribers {
        info_hash: InfoHash,
        subscriber_ids: Vec<u64>,
        now: Instant,
    },
    PeersReceived {
        info_hash: InfoHash,
        peers: Vec<SocketAddr>,
    },
    LookupFinished {
        info_hash: InfoHash,
        slice_class: DemandSliceClass,
        total_peers: usize,
        unique_peers: usize,
        now: Instant,
    },
}

pub(in crate::dht::service) enum DhtDemandCommandEffect {
    SendRegisterResponse {
        response_tx: oneshot::Sender<Option<u64>>,
        subscriber_id: Option<u64>,
    },
    ApplySubscriberEffects(Vec<DemandSubscriberEffect>),
    ApplyPlannerEffects(Vec<DemandPlannerEffect>),
    StartDueDemands,
}

#[derive(Default)]
pub(in crate::dht::service) struct DhtDemandCommandReduction {
    pub(in crate::dht::service) effects: Vec<DhtDemandCommandEffect>,
}

impl DhtServiceState {
    pub(in crate::dht::service) fn update_demand_command_from_command(
        &mut self,
        command: DhtCommand,
        now: Instant,
    ) -> Option<DhtDemandCommandReduction> {
        let action = match command {
            DhtCommand::RegisterDemand {
                info_hash,
                demand,
                subscriber_tx,
                response_tx,
            } => DhtDemandCommandAction::Register {
                info_hash,
                demand,
                subscriber_tx,
                response_tx,
                now,
            },
            DhtCommand::UpdateDemand { info_hash, demand } => DhtDemandCommandAction::Update {
                info_hash,
                demand,
                now,
            },
            DhtCommand::UpdateDemandMetrics { info_hash, metrics } => {
                DhtDemandCommandAction::UpdateMetrics { info_hash, metrics }
            }
            DhtCommand::UnregisterDemand {
                info_hash,
                subscriber_id,
            } => DhtDemandCommandAction::Unregister {
                info_hash,
                subscriber_id,
                now,
            },
            DhtCommand::DemandPeers { info_hash, peers } => {
                DhtDemandCommandAction::PeersReceived { info_hash, peers }
            }
            DhtCommand::DemandLookupFinished {
                info_hash,
                slice_class,
                total_peers,
                unique_peers,
            } => DhtDemandCommandAction::LookupFinished {
                info_hash,
                slice_class,
                total_peers,
                unique_peers,
                now,
            },
            DhtCommand::Reconfigure(_)
            | DhtCommand::ReconfigureAndWait { .. }
            | DhtCommand::UpdatePeerSlotUsage { .. }
            | DhtCommand::StartGetPeers { .. }
            | DhtCommand::StartGetPeersFamily { .. }
            | DhtCommand::CancelLookups { .. }
            | DhtCommand::ParkDemandLookups { .. }
            | DhtCommand::FinalizeDrainedDemandLookups { .. }
            | DhtCommand::AnnouncePeer { .. } => return None,
        };
        Some(self.update_demand_command(action))
    }

    pub(in crate::dht::service) fn update_demand_command(
        &mut self,
        action: DhtDemandCommandAction,
    ) -> DhtDemandCommandReduction {
        let action_kind = action.kind();
        let reduction =
            match action {
                DhtDemandCommandAction::Register {
                    info_hash,
                    demand,
                    subscriber_tx,
                    response_tx,
                    now,
                } => {
                    let reduction =
                        self.demand_subscribers
                            .update(DemandSubscriberAction::Register {
                                info_hash,
                                demand,
                                subscriber_tx,
                            });
                    let planner_effects =
                        self.reduce_subscriber_planner_followups(&reduction.effects, now);
                    DhtDemandCommandReduction {
                        effects: vec![
                            DhtDemandCommandEffect::SendRegisterResponse {
                                response_tx,
                                subscriber_id: reduction.subscriber_id,
                            },
                            DhtDemandCommandEffect::ApplySubscriberEffects(reduction.effects),
                            DhtDemandCommandEffect::ApplyPlannerEffects(planner_effects),
                            DhtDemandCommandEffect::StartDueDemands,
                        ],
                    }
                }
                DhtDemandCommandAction::Update {
                    info_hash,
                    demand,
                    now,
                } => {
                    let reduction =
                        self.update_demand_planner_action(DemandPlannerAction::DemandUpdated {
                            info_hash,
                            demand,
                            now,
                        });
                    DhtDemandCommandReduction {
                        effects: vec![
                            DhtDemandCommandEffect::ApplyPlannerEffects(reduction.effects),
                            DhtDemandCommandEffect::StartDueDemands,
                        ],
                    }
                }
                DhtDemandCommandAction::UpdateMetrics { info_hash, metrics } => {
                    let reduction = self.update_demand_planner_action(
                        DemandPlannerAction::DemandMetricsUpdated { info_hash, metrics },
                    );
                    DhtDemandCommandReduction {
                        effects: vec![DhtDemandCommandEffect::ApplyPlannerEffects(
                            reduction.effects,
                        )],
                    }
                }
                DhtDemandCommandAction::Unregister {
                    info_hash,
                    subscriber_id,
                    now,
                } => {
                    let reduction =
                        self.demand_subscribers
                            .update(DemandSubscriberAction::Unregister {
                                info_hash,
                                subscriber_id,
                            });
                    let planner_effects =
                        self.reduce_subscriber_planner_followups(&reduction.effects, now);
                    DhtDemandCommandReduction {
                        effects: vec![
                            DhtDemandCommandEffect::ApplySubscriberEffects(reduction.effects),
                            DhtDemandCommandEffect::ApplyPlannerEffects(planner_effects),
                        ],
                    }
                }
                DhtDemandCommandAction::PruneDeadSubscribers {
                    info_hash,
                    subscriber_ids,
                    now,
                } => {
                    let reduction = self.demand_subscribers.update(
                        DemandSubscriberAction::PruneDeadSubscribers {
                            info_hash,
                            subscriber_ids,
                        },
                    );
                    let planner_effects =
                        self.reduce_subscriber_planner_followups(&reduction.effects, now);
                    DhtDemandCommandReduction {
                        effects: vec![
                            DhtDemandCommandEffect::ApplySubscriberEffects(reduction.effects),
                            DhtDemandCommandEffect::ApplyPlannerEffects(planner_effects),
                        ],
                    }
                }
                DhtDemandCommandAction::PeersReceived { info_hash, peers } => {
                    self.record_recent_peers(&peers);
                    let planner_reduction =
                        self.update_demand_planner_action(DemandPlannerAction::PeersReceived {
                            info_hash,
                            peers: &peers,
                        });
                    let subscriber_reduction = self
                        .demand_subscribers
                        .update(DemandSubscriberAction::DeliverPeers { info_hash, peers });
                    DhtDemandCommandReduction {
                        effects: vec![
                            DhtDemandCommandEffect::ApplyPlannerEffects(planner_reduction.effects),
                            DhtDemandCommandEffect::ApplySubscriberEffects(
                                subscriber_reduction.effects,
                            ),
                        ],
                    }
                }
                DhtDemandCommandAction::LookupFinished {
                    info_hash,
                    slice_class,
                    total_peers,
                    unique_peers,
                    now,
                } => {
                    let reduction =
                        self.update_demand_planner_action(DemandPlannerAction::LookupFinished {
                            info_hash,
                            slice_class,
                            total_peers,
                            unique_peers,
                            now,
                        });
                    DhtDemandCommandReduction {
                        effects: vec![
                            DhtDemandCommandEffect::ApplyPlannerEffects(reduction.effects),
                            DhtDemandCommandEffect::StartDueDemands,
                        ],
                    }
                }
            };
        observe_action_effect_reduction(
            "demand_command",
            action_kind,
            reduction.effects.iter().map(DhtDemandCommandEffect::kind),
        );
        reduction
    }

    fn reduce_subscriber_planner_followups(
        &mut self,
        effects: &[DemandSubscriberEffect],
        now: Instant,
    ) -> Vec<DemandPlannerEffect> {
        let mut planner_effects = Vec::new();
        for effect in effects {
            let reduction = match effect {
                DemandSubscriberEffect::Registered {
                    info_hash, demand, ..
                } => self.update_demand_planner_action(DemandPlannerAction::DemandRegistered {
                    info_hash: *info_hash,
                    demand: *demand,
                    now,
                }),
                DemandSubscriberEffect::SubscriberRemoved { info_hash } => self
                    .update_demand_planner_action(DemandPlannerAction::DemandSubscriberRemoved {
                        info_hash: *info_hash,
                    }),
                DemandSubscriberEffect::DeliverPeers { .. } => continue,
            };
            planner_effects.extend(reduction.effects);
        }
        planner_effects
    }
}

impl DhtDemandCommandAction {
    fn kind(&self) -> &'static str {
        match self {
            DhtDemandCommandAction::Register { .. } => "register",
            DhtDemandCommandAction::Update { .. } => "update",
            DhtDemandCommandAction::UpdateMetrics { .. } => "update_metrics",
            DhtDemandCommandAction::Unregister { .. } => "unregister",
            DhtDemandCommandAction::PruneDeadSubscribers { .. } => "prune_dead_subscribers",
            DhtDemandCommandAction::PeersReceived { .. } => "peers_received",
            DhtDemandCommandAction::LookupFinished { .. } => "lookup_finished",
        }
    }
}

impl DhtDemandCommandEffect {
    fn kind(&self) -> &'static str {
        match self {
            DhtDemandCommandEffect::SendRegisterResponse { .. } => "send_register_response",
            DhtDemandCommandEffect::ApplySubscriberEffects(_) => "apply_subscriber_effects",
            DhtDemandCommandEffect::ApplyPlannerEffects(_) => "apply_planner_effects",
            DhtDemandCommandEffect::StartDueDemands => "start_due_demands",
        }
    }
}
