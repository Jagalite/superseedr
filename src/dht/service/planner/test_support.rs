#![allow(dead_code)]

use super::super::*;
use super::*;
use proptest::prelude::*;
pub(super) fn peer(addr: &str) -> SocketAddr {
    addr.parse().expect("valid socket address")
}

pub(super) fn hash_index(index: u32) -> InfoHash {
    let mut bytes = [0u8; InfoHash::LEN];
    bytes[..4].copy_from_slice(&index.to_be_bytes());
    InfoHash::from(bytes)
}

pub(super) fn demand_for_fuzz_class(class: u8, connected_peers: u8) -> DhtDemandState {
    match class % 3 {
        0 => DhtDemandState {
            awaiting_metadata: true,
            connected_peers: usize::from(connected_peers),
        },
        1 => DhtDemandState {
            awaiting_metadata: false,
            connected_peers: 0,
        },
        _ => DhtDemandState {
            awaiting_metadata: false,
            connected_peers: usize::from(connected_peers.max(1)),
        },
    }
}

pub(super) fn count_candidate_classes(candidates: &[DueDemandCandidate]) -> DemandSlotCounts {
    let mut counts = DemandSlotCounts::default();
    for candidate in candidates {
        counts.record(DemandSliceClass::from_demand(candidate.demand));
    }
    counts
}

pub(super) fn test_now() -> Instant {
    Instant::now()
        .checked_add(Duration::from_secs(20 * 60))
        .expect("planner test clock headroom should fit in Instant")
}

pub(super) fn test_instant_saturating_sub(now: Instant, duration: Duration) -> Instant {
    now.checked_sub(duration).unwrap_or(now)
}

#[derive(Debug, Clone)]
pub(super) struct PlannerCandidateSpec {
    pub(super) index: u16,
    pub(super) demand_class: u8,
    pub(super) connected_peers: u8,
    pub(super) overdue_ms: u32,
    pub(super) subscribers: u8,
    pub(super) useful_yield_age_ms: Option<u32>,
    pub(super) last_unique_peers: u8,
}

pub(super) fn planner_candidate_strategy() -> impl Strategy<Value = PlannerCandidateSpec> {
    (
        0u16..512,
        0u8..3,
        0u8..32,
        0u32..=1_200_000,
        1u8..=8,
        prop::option::of(0u32..=1_200_000),
        0u8..=96,
    )
        .prop_map(
            |(
                index,
                demand_class,
                connected_peers,
                overdue_ms,
                subscribers,
                useful_yield_age_ms,
                last_unique_peers,
            )| PlannerCandidateSpec {
                index,
                demand_class,
                connected_peers,
                overdue_ms,
                subscribers,
                useful_yield_age_ms,
                last_unique_peers,
            },
        )
}

#[derive(Debug, Clone)]
pub(super) enum PlannerMachineOp {
    Register {
        key: u8,
        demand: DhtDemandState,
        advance_ms: u16,
    },
    Update {
        key: u8,
        demand: DhtDemandState,
        advance_ms: u16,
    },
    Unregister {
        key: u8,
        advance_ms: u16,
    },
    PlanTick {
        runtime_available: bool,
        fail_mask: u8,
        advance_ms: u16,
    },
    FinishActive {
        key: u8,
        unique_peers: u8,
        advance_ms: u16,
    },
    ParkActive {
        key: u8,
        unique_peers: u8,
        stop_reason: u8,
        advance_ms: u16,
    },
    AddDrainPeers {
        key: u8,
        peer_count: u8,
        advance_ms: u16,
    },
    FinalizeDrain {
        key: u8,
        advance_ms: u16,
    },
    DrainTick {
        runtime_ready: bool,
        advance_ms: u16,
    },
    RuntimeReset {
        advance_ms: u16,
    },
    ResetActive {
        advance_ms: u16,
    },
}

pub(super) fn planner_machine_op_strategy() -> impl Strategy<Value = PlannerMachineOp> {
    let key = 0u8..64;
    let advance_ms = 0u16..=5_000;

    prop_oneof![
        (key.clone(), demand_strategy(), advance_ms.clone()).prop_map(
            |(key, demand, advance_ms)| PlannerMachineOp::Register {
                key,
                demand,
                advance_ms,
            }
        ),
        (key.clone(), demand_strategy(), advance_ms.clone()).prop_map(
            |(key, demand, advance_ms)| PlannerMachineOp::Update {
                key,
                demand,
                advance_ms,
            }
        ),
        (key.clone(), advance_ms.clone())
            .prop_map(|(key, advance_ms)| { PlannerMachineOp::Unregister { key, advance_ms } }),
        (any::<bool>(), any::<u8>(), advance_ms.clone()).prop_map(
            |(runtime_available, fail_mask, advance_ms)| PlannerMachineOp::PlanTick {
                runtime_available,
                fail_mask,
                advance_ms,
            },
        ),
        (key.clone(), 0u8..=96, advance_ms.clone()).prop_map(|(key, unique_peers, advance_ms)| {
            PlannerMachineOp::FinishActive {
                key,
                unique_peers,
                advance_ms,
            }
        }),
        (key.clone(), 0u8..=96, any::<u8>(), advance_ms.clone()).prop_map(
            |(key, unique_peers, stop_reason, advance_ms)| PlannerMachineOp::ParkActive {
                key,
                unique_peers,
                stop_reason,
                advance_ms,
            }
        ),
        (key.clone(), 0u8..=32, advance_ms.clone()).prop_map(|(key, peer_count, advance_ms)| {
            PlannerMachineOp::AddDrainPeers {
                key,
                peer_count,
                advance_ms,
            }
        }),
        (key, advance_ms.clone())
            .prop_map(|(key, advance_ms)| { PlannerMachineOp::FinalizeDrain { key, advance_ms } }),
        (any::<bool>(), advance_ms.clone()).prop_map(|(runtime_ready, advance_ms)| {
            PlannerMachineOp::DrainTick {
                runtime_ready,
                advance_ms,
            }
        }),
        advance_ms
            .clone()
            .prop_map(|advance_ms| PlannerMachineOp::RuntimeReset { advance_ms }),
        advance_ms.prop_map(|advance_ms| PlannerMachineOp::ResetActive { advance_ms }),
    ]
}

pub(super) fn demand_strategy() -> impl Strategy<Value = DhtDemandState> {
    (any::<bool>(), 0usize..=32).prop_map(|(awaiting_metadata, connected_peers)| DhtDemandState {
        awaiting_metadata,
        connected_peers,
    })
}

pub(super) fn active_lookup(lookup_id: LookupId, class: DemandSliceClass) -> ActiveDemandLookup {
    ActiveDemandLookup {
        lookup_ids: Arc::new(StdMutex::new(vec![lookup_id])),
        slice_class: class,
    }
}

pub(super) fn synthetic_peers(key: u8, count: u8) -> HashSet<SocketAddr> {
    (0..count)
        .map(|index| {
            SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(127, key, index, key.wrapping_add(index))),
                40_000 + u16::from(index),
            )
        })
        .collect()
}

pub(super) fn lookup_state_for_family(
    lookup_id: LookupId,
    family: AddressFamily,
    target_index: u32,
    now: Instant,
) -> LookupState {
    let bootstrap = match family {
        AddressFamily::Ipv4 => vec![peer("127.0.0.10:6881")],
        AddressFamily::Ipv6 => vec![peer("[::1]:6881")],
    };
    let routing = crate::dht::routing::RoutingSnapshot {
        family,
        buckets: Vec::new(),
        nodes: Vec::new(),
        replacement_count: 0,
        refresh_due_count: 0,
    };
    crate::dht::lookup::LookupManager::new(crate::dht::lookup::LookupConfig::default()).start(
        crate::dht::lookup::LookupRequest {
            lookup_id,
            kind: crate::dht::lookup::LookupKind::GetPeers,
            target: crate::dht::lookup::LookupTarget::InfoHash(hash_index(target_index)),
        },
        family,
        &routing,
        &bootstrap,
        &[],
        now,
    )
}

pub(super) fn insert_synthetic_drain(
    draining_demands: &mut HashMap<InfoHash, DrainingDemandLookup>,
    info_hash: InfoHash,
    key: u8,
    lookup_id: LookupId,
    slice_class: DemandSliceClass,
    unique_peers: u8,
    now: Instant,
) {
    insert_synthetic_drain_with_stop_reason(
        draining_demands,
        info_hash,
        key,
        lookup_id,
        slice_class,
        DemandSliceStopReason::WallTime,
        unique_peers,
        now,
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn insert_synthetic_drain_with_stop_reason(
    draining_demands: &mut HashMap<InfoHash, DrainingDemandLookup>,
    info_hash: InfoHash,
    key: u8,
    lookup_id: LookupId,
    slice_class: DemandSliceClass,
    stop_reason: DemandSliceStopReason,
    unique_peers: u8,
    now: Instant,
) {
    let unique_peers = synthetic_peers(key, unique_peers);
    let unique_peer_count = unique_peers.len();
    let parked_outcome = slice_class.parked_slice_outcome(stop_reason, unique_peer_count, false);
    let duration = demand_drain_duration(
        slice_class,
        stop_reason,
        Some(parked_outcome),
        unique_peer_count,
    )
    .unwrap_or(Duration::from_secs(1));
    draining_demands.insert(
        info_hash,
        DrainingDemandLookup {
            lookup_ids: vec![lookup_id],
            slice_class,
            stop_reason,
            started_at: now,
            total_peers: unique_peer_count,
            initial_unique_peers: unique_peer_count,
            unique_peers,
            deadline: now + duration,
            no_late_yield_deadline: now
                + demand_drain_no_late_yield_grace(slice_class).min(duration),
            initial_inflight_queries: 1,
            score: 1,
        },
    );
}

pub(super) fn prop_stop_reason(code: u8) -> DemandSliceStopReason {
    match code % 4 {
        0 => DemandSliceStopReason::WallTime,
        1 => DemandSliceStopReason::IdleTimeout,
        2 => DemandSliceStopReason::FirstBatch,
        _ => DemandSliceStopReason::UniquePeerCap,
    }
}

pub(super) struct PlannerMachine {
    pub(super) now: Instant,
    pub(super) planner: DemandPlannerModel,
    pub(super) next_lookup_id: u64,
}

impl PlannerMachine {
    pub(super) fn new() -> Self {
        let now = Instant::now();
        Self {
            now,
            planner: DemandPlannerModel::new(now),
            next_lookup_id: 1,
        }
    }

    pub(super) fn advance(&mut self, advance_ms: u16) {
        self.now += Duration::from_millis(u64::from(advance_ms));
    }

    pub(super) fn plan_tick(&mut self, runtime_available: bool, fail_mask: u8) {
        let reduction = self.planner.update(DemandPlannerAction::PlanDue {
            now: self.now,
            runtime_available,
        });

        let mut launch_index = 0u8;
        for effect in reduction.effects {
            let DemandPlannerEffect::StartLookup(start) = effect else {
                continue;
            };
            let fail_start = (fail_mask & (1 << (launch_index % 8))) != 0;
            launch_index = launch_index.wrapping_add(1);
            if fail_start {
                self.planner.update(DemandPlannerAction::LookupStartFailed {
                    info_hash: start.candidate.info_hash,
                    slice_class: start.plan.class,
                    now: self.now,
                });
                continue;
            }
            let lookup_id = LookupId(self.next_lookup_id);
            self.next_lookup_id = self.next_lookup_id.saturating_add(1);
            self.planner.update(DemandPlannerAction::LookupStarted {
                info_hash: start.candidate.info_hash,
                slice_class: start.plan.class,
                lookup_ids: active_lookup(lookup_id, start.plan.class).lookup_ids,
            });
        }
    }

    pub(super) fn finish_active(&mut self, key: u8, unique_peers: u8) {
        let info_hash = hash_index(u32::from(key));
        let Some(active) = self.planner.active.get(&info_hash) else {
            return;
        };
        let slice_class = active.slice_class;
        self.planner.update(DemandPlannerAction::LookupFinished {
            info_hash,
            slice_class,
            total_peers: usize::from(unique_peers),
            unique_peers: usize::from(unique_peers),
            now: self.now,
        });
    }

    pub(super) fn park_active(&mut self, key: u8, unique_peers: u8, stop_reason: u8) {
        let info_hash = hash_index(u32::from(key));
        let Some(active) = self.planner.active.get(&info_hash).cloned() else {
            return;
        };
        let stop_reason = prop_stop_reason(stop_reason);
        let requested = self
            .planner
            .update(DemandPlannerAction::LookupParkRequested {
                info_hash,
                slice_class: active.slice_class,
                stop_reason,
                total_peers: usize::from(unique_peers),
                unique_peers: synthetic_peers(key, unique_peers),
                lookup_ids: active.lookup_ids,
            });
        for effect in requested.effects {
            let DemandPlannerEffect::AdmitDrain(admit) = effect else {
                continue;
            };
            let unique_peer_count = admit.unique_peers.len();
            let admit_drain =
                unique_peer_count > 0 || admit.slice_class != DemandSliceClass::RoutineRefresh;
            let parked_outcome = if admit_drain {
                let lookup_id = admit
                    .lookup_ids
                    .lock()
                    .expect("test lookup id lock")
                    .first()
                    .copied()
                    .unwrap_or(LookupId(0));
                insert_synthetic_drain_with_stop_reason(
                    &mut self.planner.draining_demands,
                    admit.info_hash,
                    key,
                    lookup_id,
                    admit.slice_class,
                    admit.stop_reason,
                    unique_peers,
                    self.now,
                );
                Some(admit.slice_class.parked_slice_outcome(
                    admit.stop_reason,
                    unique_peer_count,
                    false,
                ))
            } else {
                None
            };
            let drain_admission = self
                .planner
                .draining_demands
                .get(&admit.info_hash)
                .map(demand_drain_admission_snapshot);
            self.planner
                .update(DemandPlannerAction::LookupParkResolved {
                    info_hash: admit.info_hash,
                    slice_class: admit.slice_class,
                    stop_reason: admit.stop_reason,
                    total_peers: admit.total_peers,
                    unique_peers: unique_peer_count,
                    parked_outcome,
                    drain_admission,
                    previous: admit.previous,
                    now: self.now,
                });
        }
    }

    pub(super) fn finalize_drain(&mut self, key: u8) {
        self.finalize_drain_hash(hash_index(u32::from(key)));
    }

    pub(super) fn finalize_drain_hash(&mut self, info_hash: InfoHash) {
        let Some(drain) = self.planner.draining_demands.remove(&info_hash) else {
            return;
        };
        let unique_peers = drain.unique_peer_count();
        let previous = self.planner.scheduler.entry_snapshot(info_hash);
        let parked_outcome =
            drain
                .slice_class
                .parked_slice_outcome(drain.stop_reason, unique_peers, false);
        self.planner
            .update(DemandPlannerAction::DrainedLookupFinalized {
                info_hash,
                outcome: DrainedDemandOutcome {
                    slice_class: drain.slice_class,
                    stop_reason: drain.stop_reason,
                    total_peers: drain.total_peers,
                    unique_peers,
                    parked_outcome: Some(parked_outcome),
                    drain_duration_ms: drain.duration_ms(self.now),
                    finalized_after_deadline: self.now >= drain.deadline,
                    finalized_early_no_yield: false,
                },
                previous,
                now: self.now,
            });
    }

    pub(super) fn apply(&mut self, op: PlannerMachineOp) {
        let advance_ms = match &op {
            PlannerMachineOp::Register { advance_ms, .. }
            | PlannerMachineOp::Update { advance_ms, .. }
            | PlannerMachineOp::Unregister { advance_ms, .. }
            | PlannerMachineOp::PlanTick { advance_ms, .. }
            | PlannerMachineOp::FinishActive { advance_ms, .. }
            | PlannerMachineOp::ParkActive { advance_ms, .. }
            | PlannerMachineOp::AddDrainPeers { advance_ms, .. }
            | PlannerMachineOp::FinalizeDrain { advance_ms, .. }
            | PlannerMachineOp::DrainTick { advance_ms, .. }
            | PlannerMachineOp::RuntimeReset { advance_ms }
            | PlannerMachineOp::ResetActive { advance_ms } => *advance_ms,
        };
        self.advance(advance_ms);

        match op {
            PlannerMachineOp::Register { key, demand, .. } => {
                self.planner.update(DemandPlannerAction::DemandRegistered {
                    info_hash: hash_index(u32::from(key)),
                    demand,
                    now: self.now,
                });
            }
            PlannerMachineOp::Update { key, demand, .. } => {
                let info_hash = hash_index(u32::from(key));
                let reduction = self.planner.update(DemandPlannerAction::DemandUpdated {
                    info_hash,
                    demand,
                    now: self.now,
                });
                for effect in reduction.effects {
                    if let DemandPlannerEffect::FinalizeDrainingLookup(_) = effect {
                        self.finalize_drain(key);
                    }
                }
            }
            PlannerMachineOp::Unregister { key, .. } => {
                let info_hash = hash_index(u32::from(key));
                self.planner
                    .update(DemandPlannerAction::DemandSubscriberRemoved { info_hash });
            }
            PlannerMachineOp::PlanTick {
                runtime_available,
                fail_mask,
                ..
            } => self.plan_tick(runtime_available, fail_mask),
            PlannerMachineOp::FinishActive {
                key, unique_peers, ..
            } => self.finish_active(key, unique_peers),
            PlannerMachineOp::ParkActive {
                key,
                unique_peers,
                stop_reason,
                ..
            } => self.park_active(key, unique_peers, stop_reason),
            PlannerMachineOp::AddDrainPeers {
                key, peer_count, ..
            } => {
                let peers = synthetic_peers(key.wrapping_add(1), peer_count)
                    .into_iter()
                    .collect::<Vec<_>>();
                self.planner.update(DemandPlannerAction::PeersReceived {
                    info_hash: hash_index(u32::from(key)),
                    peers: &peers,
                });
            }
            PlannerMachineOp::FinalizeDrain { key, .. } => self.finalize_drain(key),
            PlannerMachineOp::DrainTick { runtime_ready, .. } => {
                let runtime_ready = self
                    .planner
                    .draining_demands
                    .keys()
                    .copied()
                    .map(|info_hash| (info_hash, runtime_ready))
                    .collect();
                let reduction = self.planner.update(DemandPlannerAction::DrainTick {
                    now: self.now,
                    runtime_ready,
                });
                for effect in reduction.effects {
                    if let DemandPlannerEffect::FinalizeDrainingLookup(finalize) = effect {
                        self.finalize_drain_hash(finalize.info_hash);
                    }
                }
            }
            PlannerMachineOp::RuntimeReset { .. } => {
                self.planner
                    .update(DemandPlannerAction::RuntimeReset { now: self.now });
            }
            PlannerMachineOp::ResetActive { .. } => {
                self.planner.active.clear();
                self.planner.draining_demands.clear();
                self.planner.scheduler.reset_active(self.now);
            }
        }
    }

    pub(super) fn assert_invariants(&self) -> Result<(), TestCaseError> {
        let mut occupied = HashSet::new();
        let mut lookup_ids = HashSet::new();
        for (&info_hash, active) in &self.planner.active {
            prop_assert!(occupied.insert(info_hash));
            let snapshot = self
                .planner
                .scheduler
                .entry_snapshot(info_hash)
                .expect("active demand must have scheduler entry");
            prop_assert!(snapshot.in_progress);
            let active_ids = active.lookup_ids.lock().expect("test lookup id lock");
            prop_assert_eq!(active_ids.len(), 1);
            for lookup_id in active_ids.iter().copied() {
                prop_assert!(lookup_ids.insert(lookup_id));
            }
        }

        for (&info_hash, drain) in &self.planner.draining_demands {
            prop_assert!(occupied.insert(info_hash));
            let snapshot = self
                .planner
                .scheduler
                .entry_snapshot(info_hash)
                .expect("draining demand must have scheduler entry");
            prop_assert!(snapshot.in_progress);
            prop_assert!(!drain.lookup_ids.is_empty());
            for lookup_id in drain.lookup_ids.iter().copied() {
                prop_assert!(lookup_ids.insert(lookup_id));
            }
            prop_assert!(drain.deadline >= drain.started_at);
            prop_assert!(drain.no_late_yield_deadline <= drain.deadline);
            prop_assert!(drain.unique_peer_count() >= drain.initial_unique_peers);
            prop_assert!(drain.late_unique_peer_count() <= drain.unique_peer_count());
            prop_assert!(drain.total_peers >= drain.unique_peer_count());
            prop_assert!(drain.initial_inflight_queries > 0);
        }

        let scheduler_snapshots = self.planner.scheduler.entry_snapshots();
        for snapshot in &scheduler_snapshots {
            prop_assert!(snapshot.subscriber_count > 0);
            if snapshot.in_progress {
                prop_assert!(
                    self.planner.active.contains_key(&snapshot.info_hash)
                        || self
                            .planner
                            .draining_demands
                            .contains_key(&snapshot.info_hash)
                );
            }
        }
        let expected_metadata_waiters = scheduler_snapshots
            .iter()
            .filter(|snapshot| snapshot.demand.is_awaiting_metadata())
            .count();
        prop_assert_eq!(
            self.planner.metadata_waiter_count(),
            expected_metadata_waiters
        );

        let active_counts = active_demand_lookup_slot_counts(&self.planner.active);
        prop_assert!(active_counts.awaiting_metadata <= DHT_AWAITING_METADATA_SLOT_CAP);
        prop_assert!(active_counts.no_connected_peers <= DHT_NO_CONNECTED_PEERS_SLOT_CAP);
        prop_assert!(active_counts.routine_refresh <= DHT_ROUTINE_LOOKUP_SLOT_CAP);
        prop_assert!(
            self.planner
                .active
                .len()
                .saturating_add(drain_virtual_slot_count(
                    self.planner.draining_demands.len()
                ))
                <= DHT_DEMAND_LOOKUP_SLOT_COUNT
        );

        for candidate in self.planner.scheduler.due_candidates(self.now) {
            prop_assert!(!self.planner.active.contains_key(&candidate.info_hash));
            prop_assert!(!self
                .planner
                .draining_demands
                .contains_key(&candidate.info_hash));
        }

        Ok(())
    }
}
