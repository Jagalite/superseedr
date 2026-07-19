// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::Instant;

use tokio::sync::{broadcast, watch};

use crate::networking::runtime::NetworkHandle;

use super::{
    apply_demand_planner_effects_for_state, apply_dht_demand_command_effects,
    apply_dht_lifecycle_effects, apply_dht_runtime_command_effects, apply_dht_service_effects,
    publish_wave_telemetry, start_due_demands_for_state, ActiveRuntime, DemandPlannerAction,
    DhtCommand, DhtCommandReceiver, DhtCommandSender, DhtLifecycleAction, DhtLifecycleModel,
    DhtRuntimeCommandModel, DhtServiceAction, DhtServiceConfig, DhtServiceState, DhtStatus,
    DhtWaveTelemetry, NodeId, DHT_DEMAND_DRAIN_POLL_INTERVAL, DHT_DEMAND_SCHEDULER_INTERVAL,
    DHT_HEALTH_REFRESH_INTERVAL, DHT_MAINTENANCE_INTERVAL,
};

#[derive(Debug)]
pub(in crate::dht::service) enum LoopEvent {
    Shutdown,
    Command(DhtCommand),
    DrainTick,
    DemandTick,
    MaintenanceTick,
    HealthTick,
    RuntimeStep(Result<bool, String>),
    CommandClosed,
}

pub(in crate::dht::service) fn command_event(maybe_command: Option<DhtCommand>) -> LoopEvent {
    match maybe_command {
        Some(command) => LoopEvent::Command(command),
        None => LoopEvent::CommandClosed,
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::dht::service) async fn run_service(
    network_handle: NetworkHandle,
    config: DhtServiceConfig,
    local_node_id: NodeId,
    mut active_runtime: Option<ActiveRuntime>,
    warning: Option<String>,
    status_tx: watch::Sender<DhtStatus>,
    wave_telemetry_tx: watch::Sender<DhtWaveTelemetry>,
    command_tx: DhtCommandSender,
    mut command_rx: DhtCommandReceiver,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let mut local_node_id = local_node_id;
    let mut demand_tick = tokio::time::interval(DHT_DEMAND_SCHEDULER_INTERVAL);
    demand_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut drain_interval = tokio::time::interval(DHT_DEMAND_DRAIN_POLL_INTERVAL);
    drain_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut maintenance_interval = tokio::time::interval(DHT_MAINTENANCE_INTERVAL);
    let mut health_interval = tokio::time::interval(DHT_HEALTH_REFRESH_INTERVAL);
    let mut service_state = DhtServiceState::new(config, status_tx.borrow().generation, warning);

    loop {
        if let Some(active) = active_runtime.as_ref() {
            if let Some(due) = active.startup_bootstrap_due {
                let reduction =
                    DhtLifecycleModel::update(DhtLifecycleAction::StartupBootstrapDue {
                        now: Instant::now(),
                        due,
                        active_user_lookup_count: active.runtime.active_user_lookup_count(),
                    });
                apply_dht_lifecycle_effects(
                    &network_handle,
                    reduction.effects,
                    &mut service_state,
                    &mut active_runtime,
                    &status_tx,
                    &command_tx,
                    local_node_id,
                )
                .await;
            }
        }

        let event = if let Some(active) = active_runtime.as_mut() {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => LoopEvent::Shutdown,
                _ = drain_interval.tick(), if service_state.has_draining_demands() => LoopEvent::DrainTick,
                maybe_command = command_rx.recv() => command_event(maybe_command),
                _ = demand_tick.tick() => LoopEvent::DemandTick,
                _ = maintenance_interval.tick() => LoopEvent::MaintenanceTick,
                _ = health_interval.tick() => LoopEvent::HealthTick,
                step_result = active.runtime.step() => LoopEvent::RuntimeStep(step_result.map_err(|error| error.to_string())),
            }
        } else {
            tokio::select! {
                _ = shutdown_rx.recv() => LoopEvent::Shutdown,
                _ = drain_interval.tick(), if service_state.has_draining_demands() => LoopEvent::DrainTick,
                maybe_command = command_rx.recv() => command_event(maybe_command),
                _ = demand_tick.tick() => LoopEvent::DemandTick,
                _ = maintenance_interval.tick() => LoopEvent::MaintenanceTick,
                _ = health_interval.tick() => LoopEvent::HealthTick,
            }
        };

        match event {
            LoopEvent::Shutdown | LoopEvent::CommandClosed => {
                let reduction = DhtLifecycleModel::update(DhtLifecycleAction::Shutdown);
                apply_dht_lifecycle_effects(
                    &network_handle,
                    reduction.effects,
                    &mut service_state,
                    &mut active_runtime,
                    &status_tx,
                    &command_tx,
                    local_node_id,
                )
                .await;
                break;
            }
            LoopEvent::Command(DhtCommand::Reconfigure(new_config)) => {
                let reduction =
                    service_state.update_service_action(DhtServiceAction::ReconfigureRequested {
                        config: new_config,
                    });
                apply_dht_service_effects(
                    &network_handle,
                    reduction.effects,
                    &mut service_state,
                    &mut active_runtime,
                    &status_tx,
                    &command_tx,
                    local_node_id,
                )
                .await;
            }
            LoopEvent::Command(DhtCommand::UpdatePeerSlotUsage {
                total_peers,
                max_connected_peers,
            }) => {
                let reduction = service_state.update_demand_planner_action(
                    DemandPlannerAction::PeerSlotUsageUpdated {
                        total_peers,
                        max_connected_peers,
                        now: Instant::now(),
                    },
                );
                apply_demand_planner_effects_for_state(
                    active_runtime.as_mut(),
                    &command_tx,
                    &mut service_state,
                    reduction.effects,
                );
            }
            LoopEvent::Command(
                command @ (DhtCommand::RegisterDemand { .. }
                | DhtCommand::UpdateDemand { .. }
                | DhtCommand::UpdateDemandMetrics { .. }
                | DhtCommand::UnregisterDemand { .. }
                | DhtCommand::DemandPeers { .. }
                | DhtCommand::DemandLookupFinished { .. }),
            ) => {
                let reduction = service_state
                    .update_demand_command_from_command(command, Instant::now())
                    .expect("demand command must reduce");
                apply_dht_demand_command_effects(
                    reduction.effects,
                    &mut service_state,
                    &mut active_runtime,
                    &command_tx,
                )
                .await;
            }
            LoopEvent::Command(
                command @ (DhtCommand::StartGetPeers { .. }
                | DhtCommand::StartGetPeersFamily { .. }
                | DhtCommand::CancelLookups { .. }
                | DhtCommand::ParkDemandLookups { .. }
                | DhtCommand::FinalizeDrainedDemandLookups { .. }
                | DhtCommand::AnnouncePeer { .. }),
            ) => {
                let reduction = DhtRuntimeCommandModel::update_command(command)
                    .expect("runtime command must reduce");
                apply_dht_runtime_command_effects(
                    reduction.effects,
                    &mut active_runtime,
                    &command_tx,
                    &mut service_state,
                )
                .await;
            }
            LoopEvent::DrainTick => {
                let runtime_ready = service_state
                    .demand_planner
                    .drain_runtime_readiness(active_runtime.as_ref());
                let reduction =
                    service_state.update_demand_planner_action(DemandPlannerAction::DrainTick {
                        now: Instant::now(),
                        runtime_ready,
                    });
                let finalized_any = apply_demand_planner_effects_for_state(
                    active_runtime.as_mut(),
                    &command_tx,
                    &mut service_state,
                    reduction.effects,
                );
                if finalized_any {
                    start_due_demands_for_state(
                        &mut active_runtime,
                        &command_tx,
                        &mut service_state,
                    )
                    .await;
                }
            }
            LoopEvent::DemandTick => {
                start_due_demands_for_state(&mut active_runtime, &command_tx, &mut service_state)
                    .await;
            }
            LoopEvent::MaintenanceTick => {
                let reduction = DhtLifecycleModel::update(DhtLifecycleAction::MaintenanceTick {
                    active_user_lookup_count: active_runtime
                        .as_ref()
                        .map(|active| active.runtime.active_user_lookup_count()),
                });
                apply_dht_lifecycle_effects(
                    &network_handle,
                    reduction.effects,
                    &mut service_state,
                    &mut active_runtime,
                    &status_tx,
                    &command_tx,
                    local_node_id,
                )
                .await;
            }
            LoopEvent::HealthTick => {
                let reduction = DhtLifecycleModel::update(DhtLifecycleAction::HealthTick);
                apply_dht_lifecycle_effects(
                    &network_handle,
                    reduction.effects,
                    &mut service_state,
                    &mut active_runtime,
                    &status_tx,
                    &command_tx,
                    local_node_id,
                )
                .await;
            }
            LoopEvent::RuntimeStep(Ok(_)) => {
                if let Some(active) = active_runtime.as_ref() {
                    local_node_id = active.runtime.local_node_id();
                }
            }
            LoopEvent::RuntimeStep(Err(error)) => {
                let reduction = DhtLifecycleModel::update(DhtLifecycleAction::RuntimeStepFailed {
                    warning: format!("DHT runtime step failed: {error}"),
                });
                apply_dht_lifecycle_effects(
                    &network_handle,
                    reduction.effects,
                    &mut service_state,
                    &mut active_runtime,
                    &status_tx,
                    &command_tx,
                    local_node_id,
                )
                .await;
            }
        }

        publish_wave_telemetry(
            &wave_telemetry_tx,
            active_runtime.as_ref(),
            &mut service_state.recent_unique_peers,
            service_state
                .demand_planner
                .current_power_scale_halves(Instant::now()),
        );
    }
}
