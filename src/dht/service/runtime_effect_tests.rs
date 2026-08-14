use super::test_support::*;
use super::*;

#[test]
fn runtime_build_retry_classification_only_accepts_address_in_use() {
    let scope_id = NetworkScopeId::for_test(7);
    let occupied = DhtRuntimeBuildError::bind(
        scope_id,
        std::io::Error::new(std::io::ErrorKind::AddrInUse, "occupied test port"),
    );
    let denied = DhtRuntimeBuildError::bind(
        scope_id,
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied test bind"),
    );

    assert_eq!(occupied.retry_scope_id(), Some(scope_id));
    assert_eq!(denied.retry_scope_id(), None);
}

#[tokio::test]
async fn permanent_runtime_build_failure_does_not_schedule_retry() {
    let initial_config = disabled_service_config();
    let mut service_state = DhtServiceState::new(initial_config.clone(), 0, None);
    let requested_config = DhtServiceConfig {
        port: 0,
        bootstrap_nodes: Vec::new(),
        preferred_backend: DhtBackendKind::InternalPrototype,
        force_internal_failure: true,
    };
    let reduction = service_state.update_service_action(DhtServiceAction::ReconfigureRequested {
        config: requested_config,
    });
    let (status_tx, _status_rx) = watch::channel(initial_disabled_status(&initial_config));
    let (command_tx, _command_rx) = mpsc::unbounded_channel();
    let mut active_runtime = None;

    apply_dht_service_effects(
        &test_network_handle(),
        reduction.effects,
        &mut service_state,
        &mut active_runtime,
        &status_tx,
        &command_tx,
        NodeId::from([13u8; NodeId::LEN]),
    )
    .await;

    assert!(service_state.runtime_retry_due().is_none());
}

#[tokio::test]
async fn start_get_peers_lookup_without_runtime_returns_empty_lookup() {
    let (command_tx, _command_rx) = mpsc::unbounded_channel();
    let mut planner = DemandPlannerModel::new(Instant::now());

    let started = start_get_peers_lookup(
        None,
        &command_tx,
        &mut planner,
        None,
        hash_index(73),
        DemandSliceClass::RoutineRefresh,
        false,
    )
    .await
    .expect("empty lookup should succeed");

    assert!(started
        .lookup_ids
        .lock()
        .expect("test lookup ids")
        .is_empty());
    assert!(!started.accepting_families.load(Ordering::Acquire));
}

#[tokio::test]
async fn start_get_peers_lookup_without_seed_candidates_returns_empty_lookup() {
    let (command_tx, _command_rx) = mpsc::unbounded_channel();
    let mut planner = DemandPlannerModel::new(Instant::now());
    let mut active_runtime = local_ipv4_active_runtime_without_bootstrap().await;

    let started = start_get_peers_lookup(
        Some(&mut active_runtime),
        &command_tx,
        &mut planner,
        None,
        hash_index(74),
        DemandSliceClass::NoConnectedPeers,
        false,
    )
    .await
    .expect("seedless runtime should return an empty lookup");

    assert!(started
        .lookup_ids
        .lock()
        .expect("test lookup ids")
        .is_empty());
    assert_eq!(active_runtime.runtime.active_lookup_count(), 0);
    assert!(!started.accepting_families.load(Ordering::Acquire));
}

#[tokio::test]
async fn start_due_demands_treats_empty_runtime_lookup_as_start_failure() {
    let mut active_runtime = local_ipv4_active_runtime_without_bootstrap().await;
    let config = disabled_service_config();
    let mut service_state = DhtServiceState::new(config, 0, None);
    let info_hash = hash_index(75);
    let now = Instant::now();
    let (command_tx, _command_rx) = mpsc::unbounded_channel();

    service_state.update_demand_planner_action(DemandPlannerAction::DemandRegistered {
        info_hash,
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 0,
        },
        now,
    });

    start_due_demands(Some(&mut active_runtime), &command_tx, &mut service_state).await;

    let snapshot = service_state
        .demand_planner
        .scheduler
        .entry_snapshot(info_hash)
        .expect("registered demand remains tracked");
    assert!(!snapshot.in_progress);
    assert!(service_state.demand_planner.active.is_empty());
    assert!(service_state.demand_planner.pending_starts.is_empty());
}

#[tokio::test]
async fn runtime_backed_park_lookup_moves_active_state_to_parked_crawl() {
    let mut active_runtime = local_ipv4_active_runtime().await;
    let info_hash = hash_index(76);
    let (lookup_id, peer_rx) = active_runtime
        .runtime
        .start_get_peers(AddressFamily::Ipv4, info_hash)
        .await
        .expect("start runtime lookup");
    let _keep_receiver_open = peer_rx;
    assert_eq!(active_runtime.runtime.active_user_lookup_count(), 1);

    let lookup_ids = Arc::new(StdMutex::new(vec![lookup_id]));
    let mut parked_crawls = HashMap::new();
    let parked_outcome = park_lookup_ids(
        Some(&mut active_runtime),
        &mut parked_crawls,
        info_hash,
        DemandSliceClass::NoConnectedPeers,
        Some(DemandSliceStopReason::WallTime),
        1,
        lookup_ids.clone(),
    );

    assert_eq!(
        parked_outcome,
        Some(DemandParkedSliceOutcome::HealthyLowYield)
    );
    assert!(lookup_ids.lock().expect("test lookup ids").is_empty());
    assert_eq!(active_runtime.runtime.active_lookup_count(), 0);
    let parked = take_parked_family_state(
        &mut parked_crawls,
        None,
        info_hash,
        AddressFamily::Ipv4,
        DemandSliceClass::NoConnectedPeers,
    )
    .expect("parked runtime state");
    assert_eq!(parked.family(), AddressFamily::Ipv4);
    assert!(!parked_crawls.contains_key(&info_hash));
}
#[tokio::test]
async fn runtime_backed_drain_lookup_pauses_and_force_finalize_finishes_state() {
    let mut active_runtime = local_ipv4_active_runtime().await;
    let info_hash = hash_index(77);
    let (lookup_id, peer_rx) = active_runtime
        .runtime
        .start_get_peers(AddressFamily::Ipv4, info_hash)
        .await
        .expect("start runtime lookup");
    let _keep_receiver_open = peer_rx;

    let mut planner = DemandPlannerModel::new(Instant::now());
    planner.update(DemandPlannerAction::DemandRegistered {
        info_hash,
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 0,
        },
        now: Instant::now(),
    });
    assert!(planner.scheduler.mark_in_progress(info_hash));
    let (command_tx, _command_rx) = mpsc::unbounded_channel();
    let lookup_ids = Arc::new(StdMutex::new(vec![lookup_id]));
    let parked_outcome = planner.drain_lookup_ids(
        Some(&mut active_runtime),
        &command_tx,
        info_hash,
        DemandSliceClass::NoConnectedPeers,
        DemandSliceStopReason::WallTime,
        3,
        synthetic_peers(77, 3),
        lookup_ids,
    );

    assert_eq!(parked_outcome, Some(DemandParkedSliceOutcome::UsefulYield));
    assert_eq!(active_runtime.runtime.active_lookup_count(), 0);
    assert_eq!(active_runtime.runtime.draining_lookup_count(), 1);
    assert!(planner.draining_demands.contains_key(&info_hash));

    let mut slice_metrics = DemandSliceMetrics::default();
    let finalized = finish_drained_demand_lookup(
        Some(&mut active_runtime),
        &mut planner,
        &command_tx,
        &mut slice_metrics,
        info_hash,
        true,
    );

    assert!(finalized);
    assert_eq!(active_runtime.runtime.draining_lookup_count(), 0);
    assert!(!planner.draining_demands.contains_key(&info_hash));
    assert!(
        !planner
            .scheduler
            .entry_snapshot(info_hash)
            .expect("demand entry")
            .in_progress
    );
    assert!(planner.parked_crawls.contains_key(&info_hash));
    assert_eq!(slice_metrics.no_connected_peers.wall_time_stops, 1);
    assert_eq!(slice_metrics.no_connected_peers.unique_peers_yielded, 3);
}
#[tokio::test]
async fn runtime_backed_cancel_draining_effect_removes_runtime_lookup() {
    let mut active_runtime = local_ipv4_active_runtime().await;
    let info_hash = hash_index(78);
    let (lookup_id, peer_rx) = active_runtime
        .runtime
        .start_get_peers(AddressFamily::Ipv4, info_hash)
        .await
        .expect("start runtime lookup");
    let _keep_receiver_open = peer_rx;

    let mut planner = DemandPlannerModel::new(Instant::now());
    planner.update(DemandPlannerAction::DemandRegistered {
        info_hash,
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 0,
        },
        now: Instant::now(),
    });
    assert!(planner.scheduler.mark_in_progress(info_hash));
    let (command_tx, _command_rx) = mpsc::unbounded_channel();
    let parked_outcome = planner.drain_lookup_ids(
        Some(&mut active_runtime),
        &command_tx,
        info_hash,
        DemandSliceClass::NoConnectedPeers,
        DemandSliceStopReason::WallTime,
        3,
        synthetic_peers(78, 3),
        Arc::new(StdMutex::new(vec![lookup_id])),
    );
    assert_eq!(parked_outcome, Some(DemandParkedSliceOutcome::UsefulYield));
    assert_eq!(active_runtime.runtime.draining_lookup_count(), 1);

    let removal = planner.update(DemandPlannerAction::DemandSubscriberRemoved { info_hash });
    let mut slice_metrics = DemandSliceMetrics::default();
    apply_demand_planner_effects(
        Some(&mut active_runtime),
        &mut planner,
        &command_tx,
        &mut slice_metrics,
        removal.effects,
    );

    assert_eq!(active_runtime.runtime.draining_lookup_count(), 0);
    assert_eq!(active_runtime.runtime.active_lookup_count(), 0);
    assert!(planner.scheduler.entry_snapshot(info_hash).is_none());
    assert!(!planner.draining_demands.contains_key(&info_hash));
}
#[tokio::test]
async fn attach_lookup_family_ignores_closed_acceptance_and_unbound_family() {
    let mut active_runtime = local_ipv4_active_runtime().await;
    let mut planner = DemandPlannerModel::new(Instant::now());
    let mut metrics = DemandSliceMetrics::default();
    let (merged_tx, _merged_rx) = mpsc::unbounded_channel();
    let lookup_ids = Arc::new(StdMutex::new(Vec::new()));
    let first_batch_seen = Arc::new(AtomicBool::new(false));

    attach_lookup_family(
        Some(&mut active_runtime),
        &mut planner,
        Some(&mut metrics),
        hash_index(79),
        AddressFamily::Ipv4,
        DemandSliceClass::NoConnectedPeers,
        merged_tx.clone(),
        lookup_ids.clone(),
        first_batch_seen.clone(),
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .expect("closed accepting flag is not an error");
    assert!(lookup_ids.lock().expect("test lookup ids").is_empty());
    assert_eq!(active_runtime.runtime.active_lookup_count(), 0);

    attach_lookup_family(
        Some(&mut active_runtime),
        &mut planner,
        Some(&mut metrics),
        hash_index(79),
        AddressFamily::Ipv6,
        DemandSliceClass::NoConnectedPeers,
        merged_tx,
        lookup_ids.clone(),
        first_batch_seen,
        Arc::new(AtomicBool::new(true)),
    )
    .await
    .expect("unbound family is not an error");
    assert!(lookup_ids.lock().expect("test lookup ids").is_empty());
    assert_eq!(active_runtime.runtime.active_lookup_count(), 0);
    assert_eq!(metrics.no_connected_peers.fresh_starts, 0);
    assert_eq!(metrics.no_connected_peers.resumed_starts, 0);
}
#[tokio::test]
async fn attach_lookup_family_records_fresh_and_resumed_state() {
    let mut active_runtime = local_ipv4_active_runtime().await;
    let mut planner = DemandPlannerModel::new(Instant::now());
    let mut metrics = DemandSliceMetrics::default();
    let (merged_tx, _merged_rx) = mpsc::unbounded_channel();
    let lookup_ids = Arc::new(StdMutex::new(Vec::new()));
    let first_batch_seen = Arc::new(AtomicBool::new(false));

    attach_lookup_family(
        Some(&mut active_runtime),
        &mut planner,
        Some(&mut metrics),
        hash_index(80),
        AddressFamily::Ipv4,
        DemandSliceClass::NoConnectedPeers,
        merged_tx.clone(),
        lookup_ids.clone(),
        first_batch_seen.clone(),
        Arc::new(AtomicBool::new(true)),
    )
    .await
    .expect("fresh attach");
    assert_eq!(lookup_ids.lock().expect("test lookup ids").len(), 1);
    assert_eq!(metrics.no_connected_peers.fresh_starts, 1);
    assert_eq!(metrics.no_connected_peers.resumed_starts, 0);

    let parked_hash = hash_index(81);
    store_parked_lookup_states(
        &mut planner.parked_crawls,
        parked_hash,
        DemandSliceClass::NoConnectedPeers,
        Some(DemandSliceStopReason::WallTime),
        1,
        vec![lookup_state_for_family(
            LookupId(81),
            AddressFamily::Ipv4,
            81,
            Instant::now(),
        )],
    );
    let resumed_lookup_ids = Arc::new(StdMutex::new(Vec::new()));
    attach_lookup_family(
        Some(&mut active_runtime),
        &mut planner,
        Some(&mut metrics),
        parked_hash,
        AddressFamily::Ipv4,
        DemandSliceClass::NoConnectedPeers,
        merged_tx,
        resumed_lookup_ids.clone(),
        first_batch_seen,
        Arc::new(AtomicBool::new(true)),
    )
    .await
    .expect("resumed attach");

    assert_eq!(resumed_lookup_ids.lock().expect("test lookup ids").len(), 1);
    assert_eq!(metrics.no_connected_peers.fresh_starts, 1);
    assert_eq!(metrics.no_connected_peers.resumed_starts, 1);
    assert!(!planner.parked_crawls.contains_key(&parked_hash));
}
#[tokio::test]
async fn runtime_backed_start_skips_lookup_when_no_seed_candidates_exist() {
    let mut active_runtime = local_ipv4_active_runtime_without_bootstrap().await;
    let info_hash = hash_index(82);
    let (lookup_id, peer_rx) = active_runtime
        .runtime
        .start_get_peers(AddressFamily::Ipv4, info_hash)
        .await
        .expect("start runtime lookup");
    let _keep_receiver_open = peer_rx;
    assert!(!active_runtime.runtime.is_lookup_active(lookup_id));
    assert!(active_runtime
        .runtime
        .lookup_quality_snapshot(lookup_id)
        .is_none());

    let mut planner = DemandPlannerModel::new(Instant::now());
    let (command_tx, _command_rx) = mpsc::unbounded_channel();
    let parked_outcome = planner.drain_lookup_ids(
        Some(&mut active_runtime),
        &command_tx,
        info_hash,
        DemandSliceClass::NoConnectedPeers,
        DemandSliceStopReason::WallTime,
        1,
        synthetic_peers(82, 1),
        Arc::new(StdMutex::new(vec![lookup_id])),
    );

    assert!(parked_outcome.is_none());
    assert!(planner.draining_demands.is_empty());
    assert_eq!(active_runtime.runtime.active_lookup_count(), 0);
    assert!(!planner.parked_crawls.contains_key(&info_hash));
}
#[tokio::test]
async fn runtime_backed_drain_rejection_parks_lookup_when_score_is_not_productive() {
    let mut active_runtime = local_ipv4_active_runtime().await;
    let info_hash = hash_index(83);
    let (lookup_id, peer_rx) = active_runtime
        .runtime
        .start_get_peers(AddressFamily::Ipv4, info_hash)
        .await
        .expect("start runtime lookup");
    let _keep_receiver_open = peer_rx;

    let mut planner = DemandPlannerModel::new(Instant::now());
    let (command_tx, _command_rx) = mpsc::unbounded_channel();
    let parked_outcome = planner.drain_lookup_ids(
        Some(&mut active_runtime),
        &command_tx,
        info_hash,
        DemandSliceClass::NoConnectedPeers,
        DemandSliceStopReason::IdleTimeout,
        0,
        HashSet::new(),
        Arc::new(StdMutex::new(vec![lookup_id])),
    );

    assert!(parked_outcome.is_none());
    assert!(planner.draining_demands.is_empty());
    assert_eq!(active_runtime.runtime.active_lookup_count(), 0);
    assert!(planner.parked_crawls.contains_key(&info_hash));
}
#[tokio::test]
async fn runtime_backed_drain_replaces_previous_drain_for_same_demand() {
    let mut active_runtime = local_ipv4_active_runtime().await;
    let info_hash = hash_index(84);
    let (first_lookup_id, first_rx) = active_runtime
        .runtime
        .start_get_peers(AddressFamily::Ipv4, info_hash)
        .await
        .expect("start first runtime lookup");
    let _keep_first_receiver_open = first_rx;

    let mut planner = DemandPlannerModel::new(Instant::now());
    let (command_tx, _command_rx) = mpsc::unbounded_channel();
    assert_eq!(
        planner.drain_lookup_ids(
            Some(&mut active_runtime),
            &command_tx,
            info_hash,
            DemandSliceClass::NoConnectedPeers,
            DemandSliceStopReason::WallTime,
            3,
            synthetic_peers(84, 3),
            Arc::new(StdMutex::new(vec![first_lookup_id])),
        ),
        Some(DemandParkedSliceOutcome::UsefulYield)
    );
    assert_eq!(active_runtime.runtime.draining_lookup_count(), 1);

    let (second_lookup_id, second_rx) = active_runtime
        .runtime
        .start_get_peers(AddressFamily::Ipv4, info_hash)
        .await
        .expect("start second runtime lookup");
    let _keep_second_receiver_open = second_rx;
    assert_eq!(
        planner.drain_lookup_ids(
            Some(&mut active_runtime),
            &command_tx,
            info_hash,
            DemandSliceClass::NoConnectedPeers,
            DemandSliceStopReason::WallTime,
            3,
            synthetic_peers(85, 3),
            Arc::new(StdMutex::new(vec![second_lookup_id])),
        ),
        Some(DemandParkedSliceOutcome::UsefulYield)
    );

    assert_eq!(active_runtime.runtime.draining_lookup_count(), 1);
    assert!(active_runtime
        .runtime
        .lookup_quality_snapshot(first_lookup_id)
        .is_none());
    assert!(planner
        .draining_demands
        .get(&info_hash)
        .expect("replacement drain")
        .lookup_ids
        .contains(&second_lookup_id));
}
#[tokio::test]
async fn finalize_drained_lookup_not_ready_keeps_drain_when_not_forced() {
    let mut active_runtime = local_ipv4_active_runtime().await;
    let info_hash = hash_index(86);
    let (lookup_id, peer_rx) = active_runtime
        .runtime
        .start_get_peers(AddressFamily::Ipv4, info_hash)
        .await
        .expect("start runtime lookup");
    let _keep_receiver_open = peer_rx;

    let mut planner = DemandPlannerModel::new(Instant::now());
    let (command_tx, _command_rx) = mpsc::unbounded_channel();
    assert_eq!(
        planner.drain_lookup_ids(
            Some(&mut active_runtime),
            &command_tx,
            info_hash,
            DemandSliceClass::NoConnectedPeers,
            DemandSliceStopReason::WallTime,
            3,
            synthetic_peers(86, 3),
            Arc::new(StdMutex::new(vec![lookup_id])),
        ),
        Some(DemandParkedSliceOutcome::UsefulYield)
    );

    let outcome =
        planner.finalize_drained_lookup(Some(&mut active_runtime), &command_tx, info_hash, false);

    assert!(outcome.is_none());
    assert!(planner.draining_demands.contains_key(&info_hash));
    assert_eq!(active_runtime.runtime.draining_lookup_count(), 1);
}
