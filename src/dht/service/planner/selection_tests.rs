use super::super::*;
use super::test_support::*;
use super::*;
use proptest::prelude::*;

#[test]
fn demand_lookup_plan_varies_by_demand_class() {
    let metadata = DemandLookupPlan::for_demand(DhtDemandState {
        awaiting_metadata: true,
        connected_peers: 0,
    });
    let no_peers = DemandLookupPlan::for_demand(DhtDemandState {
        awaiting_metadata: false,
        connected_peers: 0,
    });
    let routine = DemandLookupPlan::for_demand(DhtDemandState {
        awaiting_metadata: false,
        connected_peers: 3,
    });

    assert_eq!(
        metadata.max_wall_time,
        DHT_AWAITING_METADATA_SLICE_WALL_TIME
    );
    assert_eq!(
        no_peers.max_wall_time,
        DHT_NO_CONNECTED_PEERS_SLICE_WALL_TIME
    );
    assert_eq!(routine.max_wall_time, DHT_ROUTINE_SLICE_WALL_TIME);
    assert!(!metadata.stop_after_first_batch);
    assert!(!no_peers.stop_after_first_batch);
    assert!(routine.stop_after_first_batch);
    assert!(metadata.unique_peer_cap > no_peers.unique_peer_cap);
    assert!(no_peers.unique_peer_cap > routine.unique_peer_cap);
}

#[test]
fn routine_lookup_plan_expands_when_metrics_need_swarm_support() {
    let demand = DhtDemandState {
        awaiting_metadata: false,
        connected_peers: 3,
    };
    let downloading = DemandLookupPlan::for_demand_with_metrics(
        demand,
        DhtDemandMetrics {
            accepting_new_peers: true,
            total_pieces: 100,
            completed_pieces: 10,
            connected_peers: 2,
            downloading_peers: 0,
            download_speed_bps: 0,
            ..Default::default()
        },
    );
    let seeding = DemandLookupPlan::for_demand_with_metrics(
        demand,
        DhtDemandMetrics {
            accepting_new_peers: true,
            complete: true,
            total_pieces: 100,
            completed_pieces: 100,
            connected_peers: 8,
            peers_interested_in_us: 3,
            unchoked_upload_peers: 1,
            ..Default::default()
        },
    );

    for plan in [downloading, seeding] {
        assert_eq!(plan.class, DemandSliceClass::RoutineRefresh);
        assert_eq!(plan.max_wall_time, DHT_ROUTINE_SUPPORT_SLICE_WALL_TIME);
        assert_eq!(plan.idle_timeout, DHT_ROUTINE_SUPPORT_SLICE_IDLE_TIMEOUT);
        assert_eq!(
            plan.unique_peer_cap,
            DHT_ROUTINE_SUPPORT_SLICE_UNIQUE_PEER_CAP
        );
        assert!(!plan.stop_after_first_batch);
    }
}

#[test]
fn demand_lookup_plan_boosts_metadata_and_swarm_support_without_global_cap_change() {
    let now = test_now();
    let metadata = DueDemandCandidate {
        info_hash: hash_index(201),
        demand: DhtDemandState {
            awaiting_metadata: true,
            connected_peers: 0,
        },
        metrics: DhtDemandMetrics::default(),
        next_eligible_at: now,
        subscriber_count: 1,
    };
    let swarm_support = DueDemandCandidate {
        info_hash: hash_index(202),
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 4,
        },
        metrics: DhtDemandMetrics {
            accepting_new_peers: true,
            complete: true,
            total_pieces: 100,
            completed_pieces: 100,
            connected_peers: 4,
            peers_interested_in_us: 3,
            unchoked_upload_peers: 1,
            ..Default::default()
        },
        next_eligible_at: now,
        subscriber_count: 1,
    };

    let metadata_plan = DemandLookupPlan::for_candidate(
        metadata,
        &HashMap::new(),
        DemandSelectionReason::OverdueScarce,
        DemandPlannerIdleSpeedProbeStatus::default(),
        now,
    );
    let swarm_plan = DemandLookupPlan::for_candidate(
        swarm_support,
        &HashMap::new(),
        DemandSelectionReason::SwarmSupport,
        DemandPlannerIdleSpeedProbeStatus::default(),
        now,
    );

    assert_eq!(metadata_plan.power_multiplier, 2);
    assert_eq!(
        metadata_plan.unique_peer_cap,
        DHT_AWAITING_METADATA_SLICE_UNIQUE_PEER_CAP * 2
    );
    assert_eq!(swarm_plan.power_multiplier, 2);
    assert_eq!(
        swarm_plan.unique_peer_cap,
        DHT_ROUTINE_SUPPORT_SLICE_UNIQUE_PEER_CAP * 2
    );
}

#[test]
fn demand_lookup_plan_boosts_only_productive_no_peer_candidates() {
    let now = test_now();
    let cold_hash = hash_index(203);
    let useful_hash = hash_index(204);
    let strong_hash = hash_index(205);
    let no_peer_candidate = |info_hash| DueDemandCandidate {
        info_hash,
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 0,
        },
        metrics: DhtDemandMetrics::default(),
        next_eligible_at: now,
        subscriber_count: 1,
    };
    let mut planner_state = HashMap::new();
    planner_state.insert(
        useful_hash,
        DemandPlannerState {
            last_started_at: Some(now - Duration::from_secs(90)),
            last_finished_at: Some(now - Duration::from_secs(80)),
            last_useful_yield_at: Some(now - Duration::from_secs(80)),
            last_unique_peers: 4,
        },
    );
    planner_state.insert(
        strong_hash,
        DemandPlannerState {
            last_started_at: Some(now - Duration::from_secs(20)),
            last_finished_at: Some(now - Duration::from_secs(15)),
            last_useful_yield_at: Some(now - Duration::from_secs(15)),
            last_unique_peers: DHT_DEMAND_STRONG_YIELD_BOOST_MIN_UNIQUE_PEERS,
        },
    );

    let cold = DemandLookupPlan::for_candidate(
        no_peer_candidate(cold_hash),
        &planner_state,
        DemandSelectionReason::OverdueScarce,
        DemandPlannerIdleSpeedProbeStatus::default(),
        now,
    );
    let useful = DemandLookupPlan::for_candidate(
        no_peer_candidate(useful_hash),
        &planner_state,
        DemandSelectionReason::UsefulYieldHistory,
        DemandPlannerIdleSpeedProbeStatus::default(),
        now,
    );
    let strong = DemandLookupPlan::for_candidate(
        no_peer_candidate(strong_hash),
        &planner_state,
        DemandSelectionReason::UsefulYieldHistory,
        DemandPlannerIdleSpeedProbeStatus::default(),
        now,
    );

    assert_eq!(cold.power_multiplier, 1);
    assert_eq!(
        cold.unique_peer_cap,
        DHT_NO_CONNECTED_PEERS_SLICE_UNIQUE_PEER_CAP
    );
    assert_eq!(useful.power_multiplier, 2);
    assert_eq!(
        useful.unique_peer_cap,
        DHT_NO_CONNECTED_PEERS_SLICE_UNIQUE_PEER_CAP * 2
    );
    assert_eq!(strong.power_multiplier, 3);
    assert_eq!(
        strong.unique_peer_cap,
        DHT_NO_CONNECTED_PEERS_SLICE_UNIQUE_PEER_CAP * 3
    );
}

#[test]
fn demand_lookup_plan_uses_idle_speed_probe_multiplier_for_unserved_demand() {
    let now = test_now();
    let candidate = DueDemandCandidate {
        info_hash: hash_index(206),
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 0,
        },
        metrics: DhtDemandMetrics {
            accepting_new_peers: true,
            ..Default::default()
        },
        next_eligible_at: now,
        subscriber_count: 1,
    };
    let idle_probe = DemandPlannerIdleSpeedProbeStatus {
        active: true,
        demand_count: 1,
        multiplier: 4,
    };

    let plan = DemandLookupPlan::for_candidate(
        candidate,
        &HashMap::new(),
        DemandSelectionReason::IdleSpeedProbe,
        idle_probe,
        now,
    );

    assert_eq!(plan.power_multiplier, 4);
    assert_eq!(
        plan.unique_peer_cap,
        DHT_NO_CONNECTED_PEERS_SLICE_UNIQUE_PEER_CAP * 4
    );
}

#[test]
fn demand_lookup_plan_caps_any_tier_to_half_power_under_peer_pressure() {
    let now = test_now();
    let candidate = DueDemandCandidate {
        info_hash: hash_index(260),
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 0,
        },
        metrics: DhtDemandMetrics {
            accepting_new_peers: true,
            ..Default::default()
        },
        next_eligible_at: now,
        subscriber_count: 1,
    };
    let idle_probe = DemandPlannerIdleSpeedProbeStatus {
        active: true,
        demand_count: 1,
        multiplier: 4,
    };

    let plan = DemandLookupPlan::for_candidate_with_peer_cap(
        candidate,
        &HashMap::new(),
        DemandSelectionReason::IdleSpeedProbe,
        idle_probe,
        1,
        now,
    );

    assert_eq!(plan.power_multiplier, 4);
    assert_eq!(plan.power_scale_halves, 1);
    assert_eq!(plan.peer_pressure_cap_halves, 1);
    assert_eq!(
        plan.max_wall_time,
        DHT_NO_CONNECTED_PEERS_SLICE_WALL_TIME
            .checked_div(2)
            .expect("half duration")
    );
    assert_eq!(
        plan.unique_peer_cap,
        DHT_NO_CONNECTED_PEERS_SLICE_UNIQUE_PEER_CAP / 2
    );
}

#[test]
fn peer_pressure_cap_drops_fast_and_recovers_linearly() {
    let now = test_now();
    let mut cap = DemandPeerPressureCap::default();

    cap.update_usage(95, 100, now);
    assert_eq!(cap.current_scale_halves(), 1);
    assert_eq!(cap.advance(now + Duration::from_secs(1)), 1);

    cap.update_usage(50, 100, now + Duration::from_secs(1));
    assert_eq!(cap.current_scale_halves(), 1);
    assert_eq!(cap.advance(now + Duration::from_secs(30)), 1);
    assert_eq!(cap.advance(now + Duration::from_secs(31)), 2);
    assert_eq!(
        cap.advance(now + Duration::from_secs(31) + DHT_PEER_PRESSURE_CAP_RAMP_UP_INTERVAL * 6),
        DHT_DEMAND_POWER_MAX_SCALE_HALVES
    );
}

#[test]
fn idle_speed_probe_escalates_after_global_idle_with_demand() {
    let now = test_now();
    let mut probe = DemandPlannerIdleSpeedProbe::default();
    let snapshot = DemandEntrySnapshot {
        info_hash: hash_index(207),
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 0,
        },
        metrics: DhtDemandMetrics {
            accepting_new_peers: true,
            ..Default::default()
        },
        next_eligible_at: now + Duration::from_secs(120),
        subscriber_count: 1,
        in_progress: false,
        retrigger_pending: false,
        no_connected_peers_backoff_step: 0,
    };

    let initial = probe.observe(&[snapshot], now);
    assert!(!initial.active);
    assert_eq!(initial.demand_count, 1);

    let two_x = probe.observe(&[snapshot], now + DHT_IDLE_SPEED_PROBE_2X_MIN_IDLE);
    assert!(two_x.active);
    assert_eq!(two_x.multiplier, 2);

    let three_x = probe.observe(&[snapshot], now + DHT_IDLE_SPEED_PROBE_3X_MIN_IDLE);
    assert!(three_x.active);
    assert_eq!(three_x.multiplier, 3);

    let four_x = probe.observe(&[snapshot], now + DHT_IDLE_SPEED_PROBE_4X_MIN_IDLE);
    assert!(four_x.active);
    assert_eq!(four_x.multiplier, 4);

    let active_metrics = DhtDemandMetrics {
        accepting_new_peers: true,
        download_speed_bps: 1,
        ..Default::default()
    };
    let recovered = probe.observe(
        &[DemandEntrySnapshot {
            metrics: active_metrics,
            ..snapshot
        }],
        now + DHT_IDLE_SPEED_PROBE_4X_MIN_IDLE + Duration::from_secs(1),
    );
    assert!(recovered.active);
    assert_eq!(recovered.multiplier, 4);
}

#[test]
fn idle_speed_probe_decays_after_activity_recovers() {
    let now = test_now();
    let mut probe = DemandPlannerIdleSpeedProbe::default();
    let snapshot = DemandEntrySnapshot {
        info_hash: hash_index(209),
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 0,
        },
        metrics: DhtDemandMetrics {
            accepting_new_peers: true,
            ..Default::default()
        },
        next_eligible_at: now + Duration::from_secs(120),
        subscriber_count: 1,
        in_progress: false,
        retrigger_pending: false,
        no_connected_peers_backoff_step: 0,
    };

    assert!(!probe.observe(&[snapshot], now).active);
    assert!(
        probe
            .observe(&[snapshot], now + DHT_IDLE_SPEED_PROBE_4X_MIN_IDLE)
            .active
    );

    let active_snapshot = DemandEntrySnapshot {
        metrics: DhtDemandMetrics {
            accepting_new_peers: true,
            download_speed_bps: 1,
            ..Default::default()
        },
        ..snapshot
    };

    let still_four_x = probe.observe(
        &[active_snapshot],
        now + DHT_IDLE_SPEED_PROBE_4X_MIN_IDLE + Duration::from_secs(1),
    );
    assert!(still_four_x.active);
    assert_eq!(still_four_x.multiplier, 4);

    let three_x = probe.observe(
        &[active_snapshot],
        now + DHT_IDLE_SPEED_PROBE_4X_MIN_IDLE
            + Duration::from_secs(1)
            + DHT_IDLE_SPEED_PROBE_DECAY_INTERVAL,
    );
    assert!(three_x.active);
    assert_eq!(three_x.multiplier, 3);

    let two_x = probe.observe(
        &[active_snapshot],
        now + DHT_IDLE_SPEED_PROBE_4X_MIN_IDLE
            + Duration::from_secs(1)
            + DHT_IDLE_SPEED_PROBE_DECAY_INTERVAL * 2,
    );
    assert!(two_x.active);
    assert_eq!(two_x.multiplier, 2);

    let one_x = probe.observe(
        &[active_snapshot],
        now + DHT_IDLE_SPEED_PROBE_4X_MIN_IDLE
            + Duration::from_secs(1)
            + DHT_IDLE_SPEED_PROBE_DECAY_INTERVAL * 3,
    );
    assert!(!one_x.active);
    assert_eq!(one_x.multiplier, 1);
}

#[test]
fn idle_speed_probe_holds_decay_level_when_idle_resumes() {
    let now = test_now();
    let mut probe = DemandPlannerIdleSpeedProbe::default();
    let idle_snapshot = DemandEntrySnapshot {
        info_hash: hash_index(210),
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 0,
        },
        metrics: DhtDemandMetrics {
            accepting_new_peers: true,
            ..Default::default()
        },
        next_eligible_at: now + Duration::from_secs(120),
        subscriber_count: 1,
        in_progress: false,
        retrigger_pending: false,
        no_connected_peers_backoff_step: 0,
    };

    assert!(!probe.observe(&[idle_snapshot], now).active);
    assert!(
        probe
            .observe(&[idle_snapshot], now + DHT_IDLE_SPEED_PROBE_4X_MIN_IDLE)
            .active
    );

    let active_snapshot = DemandEntrySnapshot {
        metrics: DhtDemandMetrics {
            accepting_new_peers: true,
            download_speed_bps: 1,
            ..Default::default()
        },
        ..idle_snapshot
    };
    let still_four_x = probe.observe(
        &[active_snapshot],
        now + DHT_IDLE_SPEED_PROBE_4X_MIN_IDLE + Duration::from_secs(1),
    );
    assert!(still_four_x.active);
    assert_eq!(still_four_x.multiplier, 4);

    let three_x = probe.observe(
        &[active_snapshot],
        now + DHT_IDLE_SPEED_PROBE_4X_MIN_IDLE
            + Duration::from_secs(1)
            + DHT_IDLE_SPEED_PROBE_DECAY_INTERVAL,
    );
    assert!(three_x.active);
    assert_eq!(three_x.multiplier, 3);

    let next_probe = probe.observe(
        &[idle_snapshot],
        now + DHT_IDLE_SPEED_PROBE_4X_MIN_IDLE
            + Duration::from_secs(2)
            + DHT_IDLE_SPEED_PROBE_DECAY_INTERVAL,
    );
    assert!(next_probe.active);
    assert_eq!(next_probe.multiplier, 3);
}

#[test]
fn idle_speed_probe_selects_not_yet_due_demand() {
    let now = test_now();
    let snapshot = DemandEntrySnapshot {
        info_hash: hash_index(208),
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 0,
        },
        metrics: DhtDemandMetrics {
            accepting_new_peers: true,
            ..Default::default()
        },
        next_eligible_at: now + Duration::from_secs(120),
        subscriber_count: 1,
        in_progress: false,
        retrigger_pending: false,
        no_connected_peers_backoff_step: 2,
    };
    let mut budget = DemandPlannerBudget::new(now);

    let selected = select_idle_speed_probe_launches(
        &[snapshot],
        DemandSlotCounts::default(),
        &HashSet::new(),
        &HashMap::new(),
        &HashMap::new(),
        &mut budget,
        now,
        1,
    );

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].info_hash, snapshot.info_hash);
}

#[test]
fn demand_planner_uses_spare_capacity_for_backed_off_no_peer_state() {
    let now = test_now();
    let info_hash = hash_index(67);
    let mut planner = DemandPlannerModel::new(now);
    planner.update(DemandPlannerAction::DemandRegistered {
        info_hash,
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 0,
        },
        now,
    });
    assert!(planner.scheduler.mark_in_progress(info_hash));
    let previous = planner.scheduler.entry_snapshot(info_hash);

    planner.update(DemandPlannerAction::DrainedLookupFinalized {
        info_hash,
        outcome: DrainedDemandOutcome {
            slice_class: DemandSliceClass::NoConnectedPeers,
            stop_reason: DemandSliceStopReason::IdleTimeout,
            total_peers: 0,
            unique_peers: 0,
            parked_outcome: Some(DemandParkedSliceOutcome::HealthyZeroYield),
            drain_duration_ms: 1_000,
            finalized_after_deadline: false,
            finalized_early_no_yield: true,
        },
        previous,
        now,
    });
    let backed_off = planner
        .scheduler
        .entry_snapshot(info_hash)
        .expect("demand entry");
    assert!(backed_off.no_connected_peers_backoff_step > 0);

    let spare_at = now + DHT_DEMAND_SPARE_RESEARCH_MIN_INTERVAL;
    assert!(backed_off.next_eligible_at > spare_at);
    let reduction = planner.update(DemandPlannerAction::PlanDue {
        now: spare_at,
        runtime_available: true,
    });
    let starts = reduction
        .effects
        .iter()
        .filter_map(|effect| match effect {
            DemandPlannerEffect::StartLookup(start) => Some(*start),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].candidate.info_hash, info_hash);
    assert_eq!(starts[0].plan.class, DemandSliceClass::NoConnectedPeers);
    assert_eq!(
        starts[0].selection_reason,
        DemandSelectionReason::SpareCapacity
    );
    assert!(
        planner
            .scheduler
            .entry_snapshot(info_hash)
            .expect("demand entry")
            .in_progress
    );
}
#[test]
fn demand_lookup_launch_budget_respects_active_slot_cap() {
    let mut active = HashMap::new();
    let make_ids = || Arc::new(StdMutex::new(Vec::<LookupId>::new()));
    let hash = |byte: u8| InfoHash::from([byte; InfoHash::LEN]);

    assert_eq!(
        demand_lookup_launch_budget(&active, 0),
        DHT_DEMAND_LOOKUP_SLOT_FILL_PER_TICK
    );

    for byte in 0..6u8 {
        active.insert(
            hash(byte),
            ActiveDemandLookup {
                lookup_ids: make_ids(),
                slice_class: DemandSliceClass::NoConnectedPeers,
            },
        );
    }
    assert_eq!(
        demand_lookup_launch_budget(&active, 0),
        DHT_DEMAND_LOOKUP_SLOT_FILL_PER_TICK.min(DHT_DEMAND_LOOKUP_SLOT_COUNT - 6)
    );

    for byte in 6..10u8 {
        active.insert(
            hash(byte),
            ActiveDemandLookup {
                lookup_ids: make_ids(),
                slice_class: DemandSliceClass::RoutineRefresh,
            },
        );
    }
    assert_eq!(demand_lookup_launch_budget(&active, 0), 0);
}
#[test]
fn select_due_demand_launches_respects_class_slot_caps() {
    let hash = |byte: u8| InfoHash::from([byte; InfoHash::LEN]);
    let now = test_now();
    let due = vec![
        DueDemandCandidate {
            info_hash: hash(1),
            demand: DhtDemandState {
                awaiting_metadata: true,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now,
            subscriber_count: 1,
        },
        DueDemandCandidate {
            info_hash: hash(2),
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now,
            subscriber_count: 1,
        },
        DueDemandCandidate {
            info_hash: hash(3),
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 1,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now,
            subscriber_count: 1,
        },
        DueDemandCandidate {
            info_hash: hash(4),
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 1,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now,
            subscriber_count: 1,
        },
    ];

    let mut planner_budget = DemandPlannerBudget::new(now);
    let selected = select_due_demand_launches(
        &due,
        DemandSlotCounts {
            awaiting_metadata: 0,
            no_connected_peers: DHT_NO_CONNECTED_PEERS_SLOT_CAP,
            routine_refresh: DHT_ROUTINE_LOOKUP_SLOT_CAP,
        },
        &HashMap::new(),
        &HashMap::new(),
        &mut planner_budget,
        now,
        1,
    );

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].info_hash, hash(1));
}

#[test]
fn candidate_priority_score_keeps_metadata_above_swarm_support() {
    let now = test_now();
    let metadata = DueDemandCandidate {
        info_hash: hash_index(180),
        demand: DhtDemandState {
            awaiting_metadata: true,
            connected_peers: 0,
        },
        metrics: DhtDemandMetrics::default(),
        next_eligible_at: now,
        subscriber_count: 1,
    };
    let swarm_support = DueDemandCandidate {
        info_hash: hash_index(181),
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 4,
        },
        metrics: DhtDemandMetrics {
            accepting_new_peers: true,
            total_pieces: 100,
            completed_pieces: 50,
            connected_peers: 2,
            download_speed_bps: 0,
            ..Default::default()
        },
        next_eligible_at: now,
        subscriber_count: 1,
    };

    assert!(
        demand_candidate_priority_score(metadata, &HashMap::new(), now)
            > demand_candidate_priority_score(swarm_support, &HashMap::new(), now)
    );
}

#[test]
fn candidate_priority_score_keeps_metadata_above_max_supported_yield() {
    let now = test_now();
    let metadata = DueDemandCandidate {
        info_hash: hash_index(186),
        demand: DhtDemandState {
            awaiting_metadata: true,
            connected_peers: 0,
        },
        metrics: DhtDemandMetrics::default(),
        next_eligible_at: now,
        subscriber_count: 1,
    };
    let supported_yield = DueDemandCandidate {
        info_hash: hash_index(187),
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 8,
        },
        metrics: DhtDemandMetrics {
            accepting_new_peers: true,
            complete: true,
            total_pieces: 100,
            completed_pieces: 100,
            connected_peers: 8,
            peers_interested_in_us: 4,
            upload_speed_bps: 128_000,
            ..Default::default()
        },
        next_eligible_at: now - DHT_DEMAND_FAIRNESS_AGE,
        subscriber_count: 1,
    };
    let mut planner_state = HashMap::new();
    planner_state.insert(
        supported_yield.info_hash,
        DemandPlannerState {
            last_started_at: Some(now - Duration::from_secs(20)),
            last_finished_at: Some(now - Duration::from_secs(15)),
            last_useful_yield_at: Some(now - Duration::from_secs(15)),
            last_unique_peers: 10_000,
        },
    );

    assert!(
        demand_candidate_priority_score(metadata, &planner_state, now)
            > demand_candidate_priority_score(supported_yield, &planner_state, now)
    );
}

#[test]
fn candidate_priority_score_does_not_inflate_cold_no_peer_recovery() {
    let now = test_now();
    let no_peer = DueDemandCandidate {
        info_hash: hash_index(188),
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 0,
        },
        metrics: DhtDemandMetrics::default(),
        next_eligible_at: now,
        subscriber_count: 1,
    };
    let routine = DueDemandCandidate {
        info_hash: hash_index(189),
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 4,
        },
        metrics: DhtDemandMetrics::default(),
        next_eligible_at: now,
        subscriber_count: 1,
    };

    assert_eq!(
        demand_candidate_priority_score(no_peer, &HashMap::new(), now),
        demand_candidate_priority_score(routine, &HashMap::new(), now)
    );
}

#[test]
fn select_due_demand_launches_prefers_swarm_support_over_cold_no_peer_recovery() {
    let now = test_now();
    let swarm_hash = hash_index(182);
    let no_peer_hash = hash_index(183);
    let candidates = vec![
        DueDemandCandidate {
            info_hash: no_peer_hash,
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now - Duration::from_secs(30),
            subscriber_count: 1,
        },
        DueDemandCandidate {
            info_hash: swarm_hash,
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 4,
            },
            metrics: DhtDemandMetrics {
                accepting_new_peers: true,
                total_pieces: 100,
                completed_pieces: 100,
                complete: true,
                connected_peers: 4,
                peers_interested_in_us: 3,
                unchoked_upload_peers: 1,
                ..Default::default()
            },
            next_eligible_at: now,
            subscriber_count: 1,
        },
    ];

    let selected = select_due_demand_launches(
        &candidates,
        DemandSlotCounts::default(),
        &HashMap::new(),
        &HashMap::new(),
        &mut DemandPlannerBudget::new(now),
        now,
        1,
    );

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].info_hash, swarm_hash);
}

#[test]
fn select_due_demand_launches_allows_high_yield_routine_to_beat_cold_no_peer() {
    let now = test_now();
    let routine_hash = hash_index(184);
    let no_peer_hash = hash_index(185);
    let candidates = vec![
        DueDemandCandidate {
            info_hash: no_peer_hash,
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now,
            subscriber_count: 1,
        },
        DueDemandCandidate {
            info_hash: routine_hash,
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 6,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now,
            subscriber_count: 1,
        },
    ];
    let mut planner_state = HashMap::new();
    planner_state.insert(
        routine_hash,
        DemandPlannerState {
            last_started_at: Some(now - Duration::from_secs(20)),
            last_finished_at: Some(now - Duration::from_secs(15)),
            last_useful_yield_at: Some(now - Duration::from_secs(15)),
            last_unique_peers: 320,
        },
    );

    let selected = select_due_demand_launches(
        &candidates,
        DemandSlotCounts::default(),
        &HashMap::new(),
        &planner_state,
        &mut DemandPlannerBudget::new(now),
        now,
        1,
    );

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].info_hash, routine_hash);
}

#[test]
fn select_due_demand_launches_does_not_bypass_routine_cap_for_high_yield() {
    let now = test_now();
    let routine_hash = hash_index(190);
    let no_peer_hash = hash_index(191);
    let candidates = vec![
        DueDemandCandidate {
            info_hash: routine_hash,
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 6,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now,
            subscriber_count: 1,
        },
        DueDemandCandidate {
            info_hash: no_peer_hash,
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now,
            subscriber_count: 1,
        },
    ];
    let mut planner_state = HashMap::new();
    planner_state.insert(
        routine_hash,
        DemandPlannerState {
            last_started_at: Some(now - Duration::from_secs(20)),
            last_finished_at: Some(now - Duration::from_secs(15)),
            last_useful_yield_at: Some(now - Duration::from_secs(15)),
            last_unique_peers: 320,
        },
    );

    let selected = select_due_demand_launches(
        &candidates,
        DemandSlotCounts {
            awaiting_metadata: 0,
            no_connected_peers: 0,
            routine_refresh: DHT_ROUTINE_LOOKUP_SLOT_CAP,
        },
        &HashMap::new(),
        &planner_state,
        &mut DemandPlannerBudget::new(now),
        now,
        1,
    );

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].info_hash, no_peer_hash);
}

#[test]
fn select_due_demand_launches_prefers_reusable_parked_crawls_within_class() {
    let hash = |byte: u8| InfoHash::from([byte; InfoHash::LEN]);
    let now = test_now();
    let due = vec![
        DueDemandCandidate {
            info_hash: hash(1),
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now - Duration::from_secs(30),
            subscriber_count: 1,
        },
        DueDemandCandidate {
            info_hash: hash(2),
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now - Duration::from_secs(10),
            subscriber_count: 1,
        },
    ];

    let mut parked_crawls = HashMap::new();
    let mut crawl = DemandCrawlState::new(now, DemandSliceClass::NoConnectedPeers);
    let manager =
        crate::dht::lookup::LookupManager::new(crate::dht::lookup::LookupConfig::default());
    let routing = crate::dht::routing::RoutingSnapshot {
        family: AddressFamily::Ipv4,
        buckets: Vec::new(),
        nodes: Vec::new(),
        replacement_count: 0,
        refresh_due_count: 0,
    };
    crawl.ipv4 = Some(manager.start(
        crate::dht::lookup::LookupRequest {
            lookup_id: LookupId(1),
            kind: crate::dht::lookup::LookupKind::GetPeers,
            target: crate::dht::lookup::LookupTarget::InfoHash(hash(2)),
        },
        AddressFamily::Ipv4,
        &routing,
        &[],
        &[],
        now,
    ));
    parked_crawls.insert(hash(2), crawl);

    let mut planner_budget = DemandPlannerBudget::new(now);
    let selected = select_due_demand_launches(
        &due,
        DemandSlotCounts::default(),
        &parked_crawls,
        &HashMap::new(),
        &mut planner_budget,
        now,
        1,
    );

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].info_hash, hash(2));
}

#[test]
fn select_due_demand_launches_does_not_reuse_low_quality_parked_crawl() {
    let hash = |byte: u8| InfoHash::from([byte; InfoHash::LEN]);
    let now = test_now();
    let older_due = hash(1);
    let low_quality_parked = hash(2);
    let due = vec![
        DueDemandCandidate {
            info_hash: older_due,
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now - Duration::from_secs(30),
            subscriber_count: 1,
        },
        DueDemandCandidate {
            info_hash: low_quality_parked,
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now - Duration::from_secs(10),
            subscriber_count: 1,
        },
    ];

    let mut crawl = DemandCrawlState::new(now, DemandSliceClass::NoConnectedPeers);
    crawl.ipv4 = Some(lookup_state_for_family(
        LookupId(1),
        AddressFamily::Ipv4,
        2,
        now,
    ));
    for _ in 0..DHT_NO_CONNECTED_PEERS_STALLED_EMPTY_SLICE_RESET_THRESHOLD {
        crawl.observe_parked_slice(
            DemandSliceClass::NoConnectedPeers,
            DemandParkedSliceOutcome::HealthyZeroYield,
        );
    }
    let parked_crawls = HashMap::from([(low_quality_parked, crawl)]);

    assert!(!due_candidate_has_reusable_parked_crawl(
        &parked_crawls,
        due[1],
        now
    ));
    assert_ne!(
        candidate_selection_reason(due[1], &parked_crawls, &HashMap::new(), now),
        DemandSelectionReason::ReusableParked
    );

    let mut planner_budget = DemandPlannerBudget::new(now);
    let selected = select_due_demand_launches(
        &due,
        DemandSlotCounts::default(),
        &parked_crawls,
        &HashMap::new(),
        &mut planner_budget,
        now,
        1,
    );

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].info_hash, older_due);
}

#[test]
fn select_due_demand_launches_prefers_recently_productive_crawls_within_class() {
    let hash = |byte: u8| InfoHash::from([byte; InfoHash::LEN]);
    let now = test_now();
    let due = vec![
        DueDemandCandidate {
            info_hash: hash(1),
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now - Duration::from_secs(30),
            subscriber_count: 1,
        },
        DueDemandCandidate {
            info_hash: hash(2),
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now - Duration::from_secs(10),
            subscriber_count: 1,
        },
    ];

    let mut planner_state = HashMap::new();
    planner_state.insert(
        hash(2),
        DemandPlannerState {
            last_started_at: Some(now - Duration::from_secs(20)),
            last_finished_at: Some(now - Duration::from_secs(5)),
            last_useful_yield_at: Some(now - Duration::from_secs(5)),
            last_unique_peers: 8,
        },
    );

    let mut planner_budget = DemandPlannerBudget::new(now);
    let selected = select_due_demand_launches(
        &due,
        DemandSlotCounts::default(),
        &HashMap::new(),
        &planner_state,
        &mut planner_budget,
        now,
        1,
    );

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].info_hash, hash(2));
}
#[test]
fn select_due_demand_launches_prefers_stale_productive_crawls_within_class() {
    let hash = |byte: u8| InfoHash::from([byte; InfoHash::LEN]);
    let now = test_now();
    let due = vec![
        DueDemandCandidate {
            info_hash: hash(1),
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now - Duration::from_secs(60),
            subscriber_count: 1,
        },
        DueDemandCandidate {
            info_hash: hash(2),
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now - Duration::from_secs(10),
            subscriber_count: 1,
        },
    ];

    let mut planner_state = HashMap::new();
    planner_state.insert(
        hash(2),
        DemandPlannerState {
            last_started_at: Some(now - Duration::from_secs(80)),
            last_finished_at: Some(now - Duration::from_secs(70)),
            last_useful_yield_at: Some(now - Duration::from_secs(70)),
            last_unique_peers: 8,
        },
    );

    let mut planner_budget = DemandPlannerBudget::new(now);
    let selected = select_due_demand_launches(
        &due,
        DemandSlotCounts::default(),
        &HashMap::new(),
        &planner_state,
        &mut planner_budget,
        now,
        1,
    );

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].info_hash, hash(2));
}
#[test]
fn select_due_demand_launches_fairness_age_overtakes_yield_history() {
    let hash = |byte: u8| InfoHash::from([byte; InfoHash::LEN]);
    let now = test_now();
    let due = vec![
        DueDemandCandidate {
            info_hash: hash(1),
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now - DHT_DEMAND_FAIRNESS_AGE - Duration::from_secs(1),
            subscriber_count: 1,
        },
        DueDemandCandidate {
            info_hash: hash(2),
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now - Duration::from_secs(10),
            subscriber_count: 1,
        },
    ];

    let mut planner_state = HashMap::new();
    planner_state.insert(
        hash(2),
        DemandPlannerState {
            last_started_at: Some(now - Duration::from_secs(20)),
            last_finished_at: Some(now - Duration::from_secs(5)),
            last_useful_yield_at: Some(now - Duration::from_secs(5)),
            last_unique_peers: 8,
        },
    );

    let mut planner_budget = DemandPlannerBudget::new(now);
    let selected = select_due_demand_launches(
        &due,
        DemandSlotCounts::default(),
        &HashMap::new(),
        &planner_state,
        &mut planner_budget,
        now,
        1,
    );

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].info_hash, hash(1));
}
#[test]
fn select_due_demand_launches_does_not_bypass_class_cap_for_oldest_due_candidate() {
    let hash = |byte: u8| InfoHash::from([byte; InfoHash::LEN]);
    let now = test_now();
    let due = vec![
        DueDemandCandidate {
            info_hash: hash(1),
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now - Duration::from_secs(120),
            subscriber_count: 1,
        },
        DueDemandCandidate {
            info_hash: hash(2),
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now - Duration::from_secs(10),
            subscriber_count: 1,
        },
    ];

    let mut planner_budget = DemandPlannerBudget::new(now);
    let selected = select_due_demand_launches(
        &due,
        DemandSlotCounts {
            awaiting_metadata: 0,
            no_connected_peers: DHT_NO_CONNECTED_PEERS_SLOT_CAP,
            routine_refresh: 0,
        },
        &HashMap::new(),
        &HashMap::new(),
        &mut planner_budget,
        now,
        1,
    );

    assert!(selected.is_empty());
}
#[test]
fn demand_planner_budget_caps_repeated_no_peer_launch_batches() {
    let hash = |byte: u8| InfoHash::from([byte; InfoHash::LEN]);
    let now = test_now();
    let due = (0..32u8)
        .map(|byte| DueDemandCandidate {
            info_hash: hash(byte),
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now,
            subscriber_count: 1,
        })
        .collect::<Vec<_>>();
    let mut planner_budget = DemandPlannerBudget::new(now);

    let first = select_due_demand_launches(
        &due,
        DemandSlotCounts::default(),
        &HashMap::new(),
        &HashMap::new(),
        &mut planner_budget,
        now,
        DHT_NO_CONNECTED_PEERS_SLOT_CAP,
    );
    let second = select_due_demand_launches(
        &due,
        DemandSlotCounts::default(),
        &HashMap::new(),
        &HashMap::new(),
        &mut planner_budget,
        now,
        DHT_NO_CONNECTED_PEERS_SLOT_CAP,
    );
    let third = select_due_demand_launches(
        &due,
        DemandSlotCounts::default(),
        &HashMap::new(),
        &HashMap::new(),
        &mut planner_budget,
        now,
        DHT_NO_CONNECTED_PEERS_SLOT_CAP,
    );

    assert_eq!(first.len(), DHT_NO_CONNECTED_PEERS_SLOT_CAP);
    assert_eq!(
        second.len(),
        (DHT_NO_CONNECTED_PEERS_LAUNCH_BURST as usize)
            .saturating_sub(DHT_NO_CONNECTED_PEERS_SLOT_CAP)
    );
    assert!(third.is_empty());
}
#[test]
fn demand_planner_selection_stats_report_throttled_due_candidates() {
    fn hash(index: u32) -> InfoHash {
        let mut bytes = [0u8; InfoHash::LEN];
        bytes[..4].copy_from_slice(&index.to_be_bytes());
        InfoHash::from(bytes)
    }

    let now = test_now();
    let due = (0..16u32)
        .map(|index| DueDemandCandidate {
            info_hash: hash(index),
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now - Duration::from_secs(u64::from(index + 1)),
            subscriber_count: 1,
        })
        .collect::<Vec<_>>();
    let mut planner_budget = DemandPlannerBudget::new(now);

    let selection = select_due_demand_launches_with_stats(
        &due,
        DemandSlotCounts::default(),
        &HashMap::new(),
        &HashMap::new(),
        &mut planner_budget,
        now,
        DHT_NO_CONNECTED_PEERS_SLOT_CAP,
    );

    assert_eq!(selection.launches.len(), DHT_NO_CONNECTED_PEERS_SLOT_CAP);
    assert_eq!(selection.stats.offered.no_connected_peers, 16);
    assert_eq!(
        selection.stats.launched.no_connected_peers,
        DHT_NO_CONNECTED_PEERS_SLOT_CAP
    );
    assert_eq!(
        selection.stats.throttled.no_connected_peers,
        16 - DHT_NO_CONNECTED_PEERS_SLOT_CAP
    );
    assert!(selection.stats.oldest_throttled_no_peers_ms >= 8_000);
}
#[test]
fn demand_planner_budget_refills_no_peer_tokens_over_time() {
    let now = test_now();
    let mut planner_budget = DemandPlannerBudget::new(now);

    for _ in 0..DHT_NO_CONNECTED_PEERS_LAUNCH_BURST {
        assert!(planner_budget.try_consume(DemandSliceClass::NoConnectedPeers, now));
    }
    assert!(!planner_budget.try_consume(DemandSliceClass::NoConnectedPeers, now));

    let later = now + Duration::from_secs(2);
    assert!(planner_budget.try_consume(DemandSliceClass::NoConnectedPeers, later));
    assert!(!planner_budget.try_consume(DemandSliceClass::NoConnectedPeers, later));
}
#[test]
fn exhausted_no_peer_budget_does_not_block_metadata_launches() {
    let hash = |byte: u8| InfoHash::from([byte; InfoHash::LEN]);
    let now = test_now();
    let due = vec![
        DueDemandCandidate {
            info_hash: hash(1),
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now,
            subscriber_count: 1,
        },
        DueDemandCandidate {
            info_hash: hash(2),
            demand: DhtDemandState {
                awaiting_metadata: true,
                connected_peers: 0,
            },
            metrics: DhtDemandMetrics::default(),
            next_eligible_at: now,
            subscriber_count: 1,
        },
    ];
    let mut planner_budget = DemandPlannerBudget::new(now);
    for _ in 0..DHT_NO_CONNECTED_PEERS_LAUNCH_BURST {
        assert!(planner_budget.try_consume(DemandSliceClass::NoConnectedPeers, now));
    }

    let selected = select_due_demand_launches(
        &due,
        DemandSlotCounts::default(),
        &HashMap::new(),
        &HashMap::new(),
        &mut planner_budget,
        now,
        2,
    );

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].info_hash, hash(2));
}
#[test]
fn no_peer_launch_budget_is_independent_of_catalog_size() {
    fn hash(index: u32) -> InfoHash {
        let mut bytes = [0u8; InfoHash::LEN];
        bytes[..4].copy_from_slice(&index.to_be_bytes());
        InfoHash::from(bytes)
    }

    fn immediate_launches(candidate_count: u32, now: Instant) -> usize {
        let due = (0..candidate_count)
            .map(|index| DueDemandCandidate {
                info_hash: hash(index),
                demand: DhtDemandState {
                    awaiting_metadata: false,
                    connected_peers: 0,
                },
                metrics: DhtDemandMetrics::default(),
                next_eligible_at: now,
                subscriber_count: 1,
            })
            .collect::<Vec<_>>();
        let mut planner_budget = DemandPlannerBudget::new(now);
        let mut selected_count = 0usize;

        for _ in 0..10 {
            selected_count = selected_count.saturating_add(
                select_due_demand_launches(
                    &due,
                    DemandSlotCounts::default(),
                    &HashMap::new(),
                    &HashMap::new(),
                    &mut planner_budget,
                    now,
                    DHT_DEMAND_LOOKUP_SLOT_COUNT,
                )
                .len(),
            );
        }

        selected_count
    }

    let now = test_now();
    let hundred = immediate_launches(100, now);
    let thousand = immediate_launches(1000, now);

    assert_eq!(hundred, DHT_NO_CONNECTED_PEERS_LAUNCH_BURST as usize);
    assert_eq!(thousand, hundred);
}
#[test]
fn select_spare_research_launches_uses_idle_capacity_for_backed_off_no_peer_work() {
    let hash = |byte: u8| InfoHash::from([byte; InfoHash::LEN]);
    let now = test_now();
    let snapshot = |byte: u8, demand: DhtDemandState| DemandEntrySnapshot {
        info_hash: hash(byte),
        demand,
        metrics: DhtDemandMetrics::default(),
        next_eligible_at: now + Duration::from_secs(40),
        subscriber_count: 1,
        in_progress: false,
        retrigger_pending: false,
        no_connected_peers_backoff_step: 3,
    };
    let snapshots = vec![
        snapshot(
            1,
            DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
        ),
        snapshot(
            2,
            DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
        ),
        snapshot(
            3,
            DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 4,
            },
        ),
    ];
    let mut planner_state = HashMap::new();
    planner_state.insert(
        hash(1),
        DemandPlannerState {
            last_started_at: Some(now - Duration::from_secs(35)),
            last_finished_at: Some(now - Duration::from_secs(30)),
            last_useful_yield_at: None,
            last_unique_peers: 0,
        },
    );
    planner_state.insert(
        hash(2),
        DemandPlannerState {
            last_started_at: Some(now - Duration::from_secs(10)),
            last_finished_at: Some(now - Duration::from_secs(5)),
            last_useful_yield_at: None,
            last_unique_peers: 0,
        },
    );

    let mut planner_budget = DemandPlannerBudget::new(now);
    let selected = select_spare_research_launches(
        &snapshots,
        DemandSlotCounts::default(),
        &HashMap::new(),
        &planner_state,
        &mut planner_budget,
        now,
        4,
    );

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].info_hash, hash(1));
}
#[test]
fn select_spare_research_launches_waits_when_demand_lookup_is_active() {
    let hash = |byte: u8| InfoHash::from([byte; InfoHash::LEN]);
    let now = test_now();
    let snapshots = vec![DemandEntrySnapshot {
        info_hash: hash(1),
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 0,
        },
        metrics: DhtDemandMetrics::default(),
        next_eligible_at: now + Duration::from_secs(40),
        subscriber_count: 1,
        in_progress: false,
        retrigger_pending: false,
        no_connected_peers_backoff_step: 3,
    }];

    let mut planner_budget = DemandPlannerBudget::new(now);
    let selected = select_spare_research_launches(
        &snapshots,
        DemandSlotCounts {
            awaiting_metadata: 0,
            no_connected_peers: 1,
            routine_refresh: 0,
        },
        &HashMap::new(),
        &HashMap::new(),
        &mut planner_budget,
        now,
        4,
    );

    assert!(selected.is_empty());
}
#[test]
fn candidate_selection_reason_labels_fairness_support_yield_reuse_and_due() {
    let hash = |byte: u8| InfoHash::from([byte; InfoHash::LEN]);
    let now = test_now();
    let candidate = DueDemandCandidate {
        info_hash: hash(1),
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 0,
        },
        metrics: DhtDemandMetrics::default(),
        next_eligible_at: now,
        subscriber_count: 1,
    };

    let mut parked_crawls = HashMap::new();
    let manager =
        crate::dht::lookup::LookupManager::new(crate::dht::lookup::LookupConfig::default());
    let routing = crate::dht::routing::RoutingSnapshot {
        family: AddressFamily::Ipv4,
        buckets: Vec::new(),
        nodes: Vec::new(),
        replacement_count: 0,
        refresh_due_count: 0,
    };
    let mut crawl = DemandCrawlState::new(now, DemandSliceClass::NoConnectedPeers);
    crawl.ipv4 = Some(manager.start(
        crate::dht::lookup::LookupRequest {
            lookup_id: LookupId(1),
            kind: crate::dht::lookup::LookupKind::GetPeers,
            target: crate::dht::lookup::LookupTarget::InfoHash(hash(1)),
        },
        AddressFamily::Ipv4,
        &routing,
        &[],
        &[],
        now,
    ));
    parked_crawls.insert(hash(1), crawl);

    assert_eq!(
        candidate_selection_reason(candidate, &parked_crawls, &HashMap::new(), now),
        DemandSelectionReason::ReusableParked
    );

    let mut planner_state = HashMap::new();
    planner_state.insert(
        hash(1),
        DemandPlannerState {
            last_started_at: Some(now - Duration::from_secs(10)),
            last_finished_at: Some(now - Duration::from_secs(5)),
            last_useful_yield_at: Some(now - Duration::from_secs(5)),
            last_unique_peers: 3,
        },
    );
    assert_eq!(
        candidate_selection_reason(candidate, &parked_crawls, &planner_state, now),
        DemandSelectionReason::UsefulYieldHistory
    );

    parked_crawls.clear();
    assert_eq!(
        candidate_selection_reason(candidate, &parked_crawls, &planner_state, now),
        DemandSelectionReason::UsefulYieldHistory
    );

    planner_state.clear();
    assert_eq!(
        candidate_selection_reason(candidate, &parked_crawls, &planner_state, now),
        DemandSelectionReason::OverdueScarce
    );

    let swarm_support_candidate = DueDemandCandidate {
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 2,
        },
        metrics: DhtDemandMetrics {
            accepting_new_peers: true,
            total_pieces: 100,
            completed_pieces: 50,
            connected_peers: 2,
            download_speed_bps: 0,
            ..Default::default()
        },
        ..candidate
    };
    assert_eq!(
        candidate_selection_reason(swarm_support_candidate, &parked_crawls, &planner_state, now),
        DemandSelectionReason::SwarmSupport
    );

    let no_peer_with_stale_metrics = DueDemandCandidate {
        demand: DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 0,
        },
        ..swarm_support_candidate
    };
    assert_eq!(
        candidate_selection_reason(
            no_peer_with_stale_metrics,
            &parked_crawls,
            &planner_state,
            now,
        ),
        DemandSelectionReason::OverdueScarce
    );

    let fairness_candidate = DueDemandCandidate {
        next_eligible_at: now - DHT_DEMAND_FAIRNESS_AGE,
        ..swarm_support_candidate
    };
    assert_eq!(
        candidate_selection_reason(fairness_candidate, &parked_crawls, &planner_state, now),
        DemandSelectionReason::Fairness
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        ..ProptestConfig::default()
    })]

    #[test]
    fn demand_planner_selection_fuzz_respects_caps_budget_and_stats(
        specs in prop::collection::vec(planner_candidate_strategy(), 0..96),
        active_awaiting in 0usize..=12,
        active_no_peers in 0usize..=12,
        active_routine in 0usize..=12,
        total_budget in 0usize..=12,
    ) {
        let now = test_now();
        let mut seen = HashSet::new();
        let mut due_candidates = Vec::new();
        let mut planner_state = HashMap::new();

        for spec in specs {
            if !seen.insert(spec.index) {
                continue;
            }

            let info_hash = hash_index(u32::from(spec.index));
            due_candidates.push(DueDemandCandidate {
                info_hash,
                demand: demand_for_fuzz_class(spec.demand_class, spec.connected_peers),
                metrics: DhtDemandMetrics::default(),
                next_eligible_at: test_instant_saturating_sub(
                    now,
                    Duration::from_millis(u64::from(spec.overdue_ms)),
                ),
                subscriber_count: usize::from(spec.subscribers),
            });

            if let Some(useful_yield_age_ms) = spec.useful_yield_age_ms {
                let useful_yield_at = test_instant_saturating_sub(
                    now,
                    Duration::from_millis(u64::from(useful_yield_age_ms)),
                );
                planner_state.insert(
                    info_hash,
                    DemandPlannerState {
                        last_started_at: Some(test_instant_saturating_sub(
                            useful_yield_at,
                            Duration::from_millis(250),
                        )),
                        last_finished_at: Some(useful_yield_at),
                        last_useful_yield_at: Some(useful_yield_at),
                        last_unique_peers: usize::from(spec.last_unique_peers),
                    },
                );
            }
        }

        let active_counts = DemandSlotCounts {
            awaiting_metadata: active_awaiting,
            no_connected_peers: active_no_peers,
            routine_refresh: active_routine,
        };
        let mut planner_budget = DemandPlannerBudget::new(now);
        let selection = select_due_demand_launches_with_stats(
            &due_candidates,
            active_counts,
            &HashMap::new(),
            &planner_state,
            &mut planner_budget,
            now,
            total_budget,
        );

        prop_assert!(selection.launches.len() <= total_budget);

        let input_hashes = due_candidates
            .iter()
            .map(|candidate| candidate.info_hash)
            .collect::<HashSet<_>>();
        let mut launched_hashes = HashSet::new();
        let mut launched_counts = DemandSlotCounts::default();
        for launched in &selection.launches {
            prop_assert!(input_hashes.contains(&launched.info_hash));
            prop_assert!(launched_hashes.insert(launched.info_hash));
            launched_counts.record(DemandSliceClass::from_demand(launched.demand));
        }

        prop_assert!(
            launched_counts.awaiting_metadata
                <= DHT_AWAITING_METADATA_SLOT_CAP.saturating_sub(active_awaiting)
        );
        prop_assert!(
            launched_counts.no_connected_peers
                <= DHT_NO_CONNECTED_PEERS_SLOT_CAP.saturating_sub(active_no_peers)
        );
        prop_assert!(
            launched_counts.routine_refresh
                <= DHT_ROUTINE_LOOKUP_SLOT_CAP.saturating_sub(active_routine)
        );

        let offered_counts = count_candidate_classes(&due_candidates);
        prop_assert_eq!(selection.stats.offered, offered_counts);
        prop_assert_eq!(selection.stats.launched, launched_counts);
        prop_assert_eq!(
            selection.stats.throttled.awaiting_metadata,
            offered_counts
                .awaiting_metadata
                .saturating_sub(launched_counts.awaiting_metadata)
        );
        prop_assert_eq!(
            selection.stats.throttled.no_connected_peers,
            offered_counts
                .no_connected_peers
                .saturating_sub(launched_counts.no_connected_peers)
        );
        prop_assert_eq!(
            selection.stats.throttled.routine_refresh,
            offered_counts
                .routine_refresh
                .saturating_sub(launched_counts.routine_refresh)
        );
    }
}
