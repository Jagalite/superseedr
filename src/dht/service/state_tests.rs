use super::test_support::*;
use super::*;

#[test]
fn runtime_retry_attempts_are_bounded_with_exponential_delays() {
    assert_eq!(next_dht_runtime_retry_attempt(None), Some(1));
    assert_eq!(next_dht_runtime_retry_attempt(Some(1)), Some(2));
    assert_eq!(next_dht_runtime_retry_attempt(Some(2)), Some(3));
    assert_eq!(next_dht_runtime_retry_attempt(Some(3)), None);
    assert_eq!(dht_runtime_retry_delay(1), Duration::from_secs(1));
    assert_eq!(dht_runtime_retry_delay(2), Duration::from_secs(2));
    assert_eq!(dht_runtime_retry_delay(3), Duration::from_secs(4));
}

#[test]
fn dht_service_model_reconfigure_success_updates_state_and_emits_followups() {
    let initial = DhtServiceConfig {
        port: 6881,
        bootstrap_nodes: vec!["198.51.100.10:6881".to_string()],
        preferred_backend: DhtBackendKind::InternalPrototype,
        force_internal_failure: false,
    };
    let next = DhtServiceConfig {
        port: 6882,
        bootstrap_nodes: vec!["203.0.113.20:6881".to_string()],
        preferred_backend: DhtBackendKind::Disabled,
        force_internal_failure: false,
    };
    let mut model = DhtServiceModel::new(initial, 7, Some("old warning".to_string()));

    let reduction = model.update(DhtServiceAction::ReconfigureSucceeded {
        config: next.clone(),
        warning: None,
    });

    assert_eq!(model.config(), &next);
    assert_eq!(model.generation(), 8);
    assert_eq!(model.warning_owned(), None);
    assert_eq!(
        reduction.effects,
        vec![
            DhtServiceEffect::ResetDemandPlanner,
            DhtServiceEffect::PublishStatus,
            DhtServiceEffect::StartDueDemands,
        ]
    );
}

#[test]
fn dht_service_model_reconfigure_request_emits_runtime_build_effect() {
    let initial = disabled_service_config();
    let next = DhtServiceConfig {
        port: 6882,
        bootstrap_nodes: vec!["203.0.113.21:6881".to_string()],
        preferred_backend: DhtBackendKind::InternalPrototype,
        force_internal_failure: false,
    };
    let mut model = DhtServiceModel::new(initial.clone(), 5, None);

    let reduction = model.update(DhtServiceAction::ReconfigureRequested {
        config: next.clone(),
    });

    assert_eq!(model.config(), &initial);
    assert_eq!(model.generation(), 5);
    assert_eq!(
        reduction.effects,
        vec![DhtServiceEffect::BuildRuntime { config: next }]
    );
}

#[test]
fn dht_service_model_reconfigure_failure_preserves_config_and_generation() {
    let initial = DhtServiceConfig {
        port: 6881,
        bootstrap_nodes: vec!["198.51.100.10:6881".to_string()],
        preferred_backend: DhtBackendKind::InternalPrototype,
        force_internal_failure: false,
    };
    let mut model = DhtServiceModel::new(initial.clone(), 3, None);

    let reduction = model.update(DhtServiceAction::ReconfigureFailed {
        warning: "runtime unavailable".to_string(),
        runtime_reset: false,
    });

    assert_eq!(model.config(), &initial);
    assert_eq!(model.generation(), 3);
    assert_eq!(
        model.warning_owned().as_deref(),
        Some("runtime unavailable")
    );
    assert_eq!(reduction.effects, vec![DhtServiceEffect::PublishStatus]);
}

#[test]
fn dht_service_model_reconfigure_failure_resets_dependents_when_runtime_was_lost() {
    let initial = DhtServiceConfig {
        port: 6881,
        bootstrap_nodes: vec!["198.51.100.10:6881".to_string()],
        preferred_backend: DhtBackendKind::InternalPrototype,
        force_internal_failure: false,
    };
    let mut model = DhtServiceModel::new(initial.clone(), 3, None);

    let reduction = model.update(DhtServiceAction::ReconfigureFailed {
        warning: "runtime unavailable".to_string(),
        runtime_reset: true,
    });

    assert_eq!(model.config(), &initial);
    assert_eq!(model.generation(), 3);
    assert_eq!(
        model.warning_owned().as_deref(),
        Some("runtime unavailable")
    );
    assert_eq!(
        reduction.effects,
        vec![
            DhtServiceEffect::ResetDemandPlanner,
            DhtServiceEffect::PublishStatus,
            DhtServiceEffect::StartDueDemands,
        ]
    );
}
#[test]
fn dht_service_model_runtime_warning_only_publishes_status() {
    let config = disabled_service_config();
    let mut model = DhtServiceModel::new(config.clone(), 11, None);

    let reduction = model.update(DhtServiceAction::RuntimeWarning {
        warning: "maintenance failed".to_string(),
    });

    assert_eq!(model.config(), &config);
    assert_eq!(model.generation(), 11);
    assert_eq!(model.warning_owned().as_deref(), Some("maintenance failed"));
    assert_eq!(reduction.effects, vec![DhtServiceEffect::PublishStatus]);
}
#[test]
fn dht_service_state_initializes_helper_models() {
    let config = disabled_service_config();
    let mut state = DhtServiceState::new(config.clone(), 42, Some("initial warning".to_string()));

    assert_eq!(state.service.config(), &config);
    assert_eq!(state.service.generation(), 42);
    assert_eq!(
        state.service.warning_owned().as_deref(),
        Some("initial warning")
    );
    assert!(!state.has_draining_demands());
    assert!(state.demand_subscribers.subscribers.is_empty());

    state.record_recent_peers(&[peer("198.51.100.30:6881")]);
    assert_eq!(state.recent_unique_peers.unique_count(Instant::now()), 1);
    state.expire_recent_peers();
}

#[test]
fn dht_service_state_reduces_demand_commands_only() {
    let config = disabled_service_config();
    let mut state = DhtServiceState::new(config, 0, None);
    let info_hash = hash_index(89);
    let (subscriber_tx, _subscriber_rx) = mpsc::unbounded_channel();
    let (response_tx, mut response_rx) = oneshot::channel();
    let demand = DhtDemandState {
        awaiting_metadata: false,
        connected_peers: 0,
    };

    let reduction = state
        .update_demand_command_from_command(
            DhtCommand::RegisterDemand {
                info_hash,
                demand,
                subscriber_tx,
                response_tx,
            },
            Instant::now(),
        )
        .expect("demand command reduction");

    assert_eq!(state.demand_subscribers.subscriber_count(info_hash), 1);
    let mut effects = reduction.effects.into_iter();
    let Some(DhtDemandCommandEffect::SendRegisterResponse {
        response_tx,
        subscriber_id,
    }) = effects.next()
    else {
        panic!("expected register response effect");
    };
    assert_eq!(subscriber_id, Some(1));
    response_tx.send(subscriber_id).expect("send subscriber id");
    assert_eq!(response_rx.try_recv(), Ok(Some(1)));

    let (lookup_response_tx, _lookup_response_rx) = oneshot::channel();
    assert!(state
        .update_demand_command_from_command(
            DhtCommand::StartGetPeers {
                info_hash,
                response_tx: lookup_response_tx,
            },
            Instant::now(),
        )
        .is_none());
}

#[test]
fn dht_demand_command_register_and_unregister_emit_subscriber_effects() {
    let config = disabled_service_config();
    let mut state = DhtServiceState::new(config, 0, None);
    let info_hash = hash_index(90);
    let (subscriber_tx, _subscriber_rx) = mpsc::unbounded_channel();
    let (response_tx, mut response_rx) = oneshot::channel();
    let demand = DhtDemandState {
        awaiting_metadata: true,
        connected_peers: 0,
    };

    let reduction = state.update_demand_command(DhtDemandCommandAction::Register {
        info_hash,
        demand,
        subscriber_tx,
        response_tx,
        now: Instant::now(),
    });

    assert_eq!(state.demand_subscribers.subscriber_count(info_hash), 1);
    let mut effects = reduction.effects.into_iter();
    let Some(DhtDemandCommandEffect::SendRegisterResponse {
        response_tx,
        subscriber_id,
    }) = effects.next()
    else {
        panic!("expected register response effect");
    };
    assert_eq!(subscriber_id, Some(1));
    response_tx.send(subscriber_id).expect("send subscriber id");
    assert_eq!(response_rx.try_recv(), Ok(Some(1)));

    let Some(DhtDemandCommandEffect::ApplySubscriberEffects(subscriber_effects)) = effects.next()
    else {
        panic!("expected subscriber effects");
    };
    assert_eq!(subscriber_effects.len(), 1);
    assert!(matches!(
        subscriber_effects.as_slice(),
        [DemandSubscriberEffect::Registered {
            info_hash: registered_hash,
            demand: registered_demand,
            subscriber_id: 1,
        }] if *registered_hash == info_hash && *registered_demand == demand
    ));
    assert!(matches!(
        effects.next(),
        Some(DhtDemandCommandEffect::ApplyPlannerEffects(_))
    ));
    assert!(matches!(
        effects.next(),
        Some(DhtDemandCommandEffect::StartDueDemands)
    ));
    assert!(effects.next().is_none());

    let metrics = DhtDemandMetrics {
        accepting_new_peers: true,
        total_pieces: 80,
        completed_pieces: 20,
        connected_peers: 3,
        upload_speed_bps: 32_000,
        ..Default::default()
    };
    let reduction =
        state.update_demand_command(DhtDemandCommandAction::UpdateMetrics { info_hash, metrics });
    let mut effects = reduction.effects.into_iter();
    let Some(DhtDemandCommandEffect::ApplyPlannerEffects(planner_effects)) = effects.next() else {
        panic!("expected planner effects");
    };
    assert!(planner_effects.is_empty());
    assert!(effects.next().is_none());
    assert_eq!(
        state
            .demand_planner
            .scheduler
            .entry_snapshot(info_hash)
            .expect("demand entry")
            .metrics,
        metrics
    );

    let reduction = state.update_demand_command(DhtDemandCommandAction::Unregister {
        info_hash,
        subscriber_id: 1,
        now: Instant::now(),
    });

    assert_eq!(state.demand_subscribers.subscriber_count(info_hash), 0);
    let mut effects = reduction.effects.into_iter();
    let Some(DhtDemandCommandEffect::ApplySubscriberEffects(subscriber_effects)) = effects.next()
    else {
        panic!("expected subscriber removal effects");
    };
    assert!(matches!(
        subscriber_effects.as_slice(),
        [DemandSubscriberEffect::SubscriberRemoved { info_hash: removed_hash }]
            if *removed_hash == info_hash
    ));
    assert!(matches!(
        effects.next(),
        Some(DhtDemandCommandEffect::ApplyPlannerEffects(_))
    ));
    assert!(effects.next().is_none());
    assert!(state
        .demand_planner
        .scheduler
        .entry_snapshot(info_hash)
        .is_none());
}

#[test]
fn dht_demand_command_peer_and_finish_actions_emit_planner_followups() {
    let config = disabled_service_config();
    let mut state = DhtServiceState::new(config, 0, None);
    let info_hash = hash_index(91);
    let (subscriber_tx, _subscriber_rx) = mpsc::unbounded_channel();
    let (response_tx, _response_rx) = oneshot::channel();

    let _ = state.update_demand_command(DhtDemandCommandAction::Register {
        info_hash,
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 0,
        },
        subscriber_tx,
        response_tx,
        now: Instant::now(),
    });

    let peers = vec![peer("127.0.0.91:6881")];
    let reduction = state.update_demand_command(DhtDemandCommandAction::PeersReceived {
        info_hash,
        peers: peers.clone(),
    });

    assert_eq!(state.recent_unique_peers.unique_count(Instant::now()), 1);
    let mut effects = reduction.effects.into_iter();
    assert!(matches!(
        effects.next(),
        Some(DhtDemandCommandEffect::ApplyPlannerEffects(_))
    ));
    let Some(DhtDemandCommandEffect::ApplySubscriberEffects(subscriber_effects)) = effects.next()
    else {
        panic!("expected subscriber delivery effects");
    };
    assert!(matches!(
        subscriber_effects.as_slice(),
        [DemandSubscriberEffect::DeliverPeers {
            info_hash: delivered_hash,
            peers: delivered_peers,
            deliveries,
        }] if *delivered_hash == info_hash
            && delivered_peers == &peers
            && deliveries.len() == 1
    ));
    assert!(effects.next().is_none());

    let reduction = state.update_demand_command(DhtDemandCommandAction::LookupFinished {
        info_hash,
        slice_class: DemandSliceClass::NoConnectedPeers,
        total_peers: 1,
        unique_peers: 1,
        now: Instant::now(),
    });

    let mut effects = reduction.effects.into_iter();
    assert!(matches!(
        effects.next(),
        Some(DhtDemandCommandEffect::ApplyPlannerEffects(_))
    ));
    assert!(matches!(
        effects.next(),
        Some(DhtDemandCommandEffect::StartDueDemands)
    ));
    assert!(effects.next().is_none());
}

#[test]
fn dht_demand_command_prune_dead_subscribers_updates_planner_state() {
    let config = disabled_service_config();
    let mut state = DhtServiceState::new(config, 0, None);
    let info_hash = hash_index(93);
    let (subscriber_tx, _subscriber_rx) = mpsc::unbounded_channel();
    let (response_tx, _response_rx) = oneshot::channel();

    let _ = state.update_demand_command(DhtDemandCommandAction::Register {
        info_hash,
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 0,
        },
        subscriber_tx,
        response_tx,
        now: Instant::now(),
    });
    assert_eq!(state.demand_subscribers.subscriber_count(info_hash), 1);
    assert!(state
        .demand_planner
        .scheduler
        .entry_snapshot(info_hash)
        .is_some());

    let reduction = state.update_demand_command(DhtDemandCommandAction::PruneDeadSubscribers {
        info_hash,
        subscriber_ids: vec![1],
        now: Instant::now(),
    });

    assert_eq!(state.demand_subscribers.subscriber_count(info_hash), 0);
    assert!(state
        .demand_planner
        .scheduler
        .entry_snapshot(info_hash)
        .is_none());
    let mut effects = reduction.effects.into_iter();
    let Some(DhtDemandCommandEffect::ApplySubscriberEffects(subscriber_effects)) = effects.next()
    else {
        panic!("expected subscriber effects");
    };
    assert!(matches!(
        subscriber_effects.as_slice(),
        [DemandSubscriberEffect::SubscriberRemoved { info_hash: removed_hash }]
            if *removed_hash == info_hash
    ));
    assert!(matches!(
        effects.next(),
        Some(DhtDemandCommandEffect::ApplyPlannerEffects(_))
    ));
    assert!(effects.next().is_none());
}

#[test]
fn dht_demand_subscriber_effect_delivery_failure_prunes_through_reducer() {
    let config = disabled_service_config();
    let mut state = DhtServiceState::new(config, 0, None);
    let info_hash = hash_index(94);
    let (dead_tx, dead_rx) = mpsc::unbounded_channel();
    drop(dead_rx);
    let (response_tx, _response_rx) = oneshot::channel();

    let _ = state.update_demand_command(DhtDemandCommandAction::Register {
        info_hash,
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 0,
        },
        subscriber_tx: dead_tx,
        response_tx,
        now: Instant::now(),
    });
    assert_eq!(state.demand_subscribers.subscriber_count(info_hash), 1);

    let reduction = state.update_demand_command(DhtDemandCommandAction::PeersReceived {
        info_hash,
        peers: vec![peer("127.0.0.94:6881")],
    });
    let subscriber_effects = reduction
        .effects
        .into_iter()
        .find_map(|effect| match effect {
            DhtDemandCommandEffect::ApplySubscriberEffects(effects) => Some(effects),
            _ => None,
        })
        .expect("subscriber delivery effects");
    let (command_tx, _command_rx) = mpsc::unbounded_channel();

    apply_demand_subscriber_effects(&mut state, None, &command_tx, subscriber_effects);

    assert_eq!(state.demand_subscribers.subscriber_count(info_hash), 0);
    assert!(state
        .demand_planner
        .scheduler
        .entry_snapshot(info_hash)
        .is_none());
}
