// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::{mpsc, oneshot};

use super::{
    observe_action_effect_reduction, AddressFamily, DemandSliceClass, DemandSliceStopReason,
    DhtCommand, InfoHash, LookupId, StartedLookup,
};

pub(super) struct DhtRuntimeLookupFamilyRequest {
    pub(super) info_hash: InfoHash,
    pub(super) family: AddressFamily,
    pub(super) slice_class: DemandSliceClass,
    pub(super) record_metrics: bool,
    pub(super) merged_tx: mpsc::UnboundedSender<Vec<SocketAddr>>,
    pub(super) lookup_ids: Arc<StdMutex<Vec<LookupId>>>,
    pub(super) first_batch_seen: Arc<AtomicBool>,
    pub(super) accepting_families: Arc<AtomicBool>,
}

pub(super) enum DhtRuntimeCommandAction {
    StartGetPeers {
        info_hash: InfoHash,
        response_tx: oneshot::Sender<Result<StartedLookup, String>>,
    },
    StartGetPeersFamily(DhtRuntimeLookupFamilyRequest),
    CancelLookups {
        lookup_ids: Vec<LookupId>,
    },
    ParkDemandLookups {
        info_hash: InfoHash,
        slice_class: DemandSliceClass,
        stop_reason: DemandSliceStopReason,
        total_peers: usize,
        unique_peers: HashSet<SocketAddr>,
        lookup_ids: Arc<StdMutex<Vec<LookupId>>>,
    },
    FinalizeDrainedDemandLookups {
        info_hash: InfoHash,
    },
    AnnouncePeer {
        info_hash: InfoHash,
        port: Option<u16>,
        response_tx: oneshot::Sender<bool>,
    },
}

pub(super) enum DhtRuntimeCommandEffect {
    StartGetPeers {
        info_hash: InfoHash,
        response_tx: oneshot::Sender<Result<StartedLookup, String>>,
    },
    AttachLookupFamily(DhtRuntimeLookupFamilyRequest),
    CancelLookups {
        lookup_ids: Vec<LookupId>,
    },
    ParkDemandLookups {
        info_hash: InfoHash,
        slice_class: DemandSliceClass,
        stop_reason: DemandSliceStopReason,
        total_peers: usize,
        unique_peers: HashSet<SocketAddr>,
        lookup_ids: Arc<StdMutex<Vec<LookupId>>>,
    },
    FinalizeDrainedDemandLookups {
        info_hash: InfoHash,
    },
    AnnouncePeer {
        info_hash: InfoHash,
        port: Option<u16>,
        response_tx: oneshot::Sender<bool>,
    },
    StartDueDemands,
}

#[derive(Default)]
pub(super) struct DhtRuntimeCommandReduction {
    pub(super) effects: Vec<DhtRuntimeCommandEffect>,
}

pub(super) struct DhtRuntimeCommandModel;

impl DhtRuntimeCommandAction {
    fn kind(&self) -> &'static str {
        match self {
            DhtRuntimeCommandAction::StartGetPeers { .. } => "start_get_peers",
            DhtRuntimeCommandAction::StartGetPeersFamily(_) => "start_get_peers_family",
            DhtRuntimeCommandAction::CancelLookups { .. } => "cancel_lookups",
            DhtRuntimeCommandAction::ParkDemandLookups { .. } => "park_demand_lookups",
            DhtRuntimeCommandAction::FinalizeDrainedDemandLookups { .. } => {
                "finalize_drained_demand_lookups"
            }
            DhtRuntimeCommandAction::AnnouncePeer { .. } => "announce_peer",
        }
    }
}

impl DhtRuntimeCommandEffect {
    fn kind(&self) -> &'static str {
        match self {
            DhtRuntimeCommandEffect::StartGetPeers { .. } => "start_get_peers",
            DhtRuntimeCommandEffect::AttachLookupFamily(_) => "attach_lookup_family",
            DhtRuntimeCommandEffect::CancelLookups { .. } => "cancel_lookups",
            DhtRuntimeCommandEffect::ParkDemandLookups { .. } => "park_demand_lookups",
            DhtRuntimeCommandEffect::FinalizeDrainedDemandLookups { .. } => {
                "finalize_drained_demand_lookups"
            }
            DhtRuntimeCommandEffect::AnnouncePeer { .. } => "announce_peer",
            DhtRuntimeCommandEffect::StartDueDemands => "start_due_demands",
        }
    }
}

impl DhtRuntimeCommandModel {
    pub(super) fn update_command(command: DhtCommand) -> Option<DhtRuntimeCommandReduction> {
        let action = match command {
            DhtCommand::StartGetPeers {
                info_hash,
                response_tx,
            } => DhtRuntimeCommandAction::StartGetPeers {
                info_hash,
                response_tx,
            },
            DhtCommand::StartGetPeersFamily {
                info_hash,
                family,
                slice_class,
                record_metrics,
                merged_tx,
                lookup_ids,
                first_batch_seen,
                accepting_families,
            } => DhtRuntimeCommandAction::StartGetPeersFamily(DhtRuntimeLookupFamilyRequest {
                info_hash,
                family,
                slice_class,
                record_metrics,
                merged_tx,
                lookup_ids,
                first_batch_seen,
                accepting_families,
            }),
            DhtCommand::CancelLookups { lookup_ids } => {
                DhtRuntimeCommandAction::CancelLookups { lookup_ids }
            }
            DhtCommand::ParkDemandLookups {
                info_hash,
                slice_class,
                stop_reason,
                total_peers,
                unique_peers,
                lookup_ids,
            } => DhtRuntimeCommandAction::ParkDemandLookups {
                info_hash,
                slice_class,
                stop_reason,
                total_peers,
                unique_peers,
                lookup_ids,
            },
            DhtCommand::FinalizeDrainedDemandLookups { info_hash } => {
                DhtRuntimeCommandAction::FinalizeDrainedDemandLookups { info_hash }
            }
            DhtCommand::AnnouncePeer {
                info_hash,
                port,
                response_tx,
            } => DhtRuntimeCommandAction::AnnouncePeer {
                info_hash,
                port,
                response_tx,
            },
            DhtCommand::Reconfigure(_)
            | DhtCommand::ReconfigureAndWait { .. }
            | DhtCommand::UpdatePeerSlotUsage { .. }
            | DhtCommand::RegisterDemand { .. }
            | DhtCommand::UpdateDemand { .. }
            | DhtCommand::UpdateDemandMetrics { .. }
            | DhtCommand::UnregisterDemand { .. }
            | DhtCommand::DemandPeers { .. }
            | DhtCommand::DemandLookupFinished { .. } => return None,
        };
        Some(Self::update(action))
    }

    pub(super) fn update(action: DhtRuntimeCommandAction) -> DhtRuntimeCommandReduction {
        let action_kind = action.kind();
        let effects = match action {
            DhtRuntimeCommandAction::StartGetPeers {
                info_hash,
                response_tx,
            } => {
                vec![DhtRuntimeCommandEffect::StartGetPeers {
                    info_hash,
                    response_tx,
                }]
            }
            DhtRuntimeCommandAction::StartGetPeersFamily(request) => {
                vec![DhtRuntimeCommandEffect::AttachLookupFamily(request)]
            }
            DhtRuntimeCommandAction::CancelLookups { lookup_ids } => {
                vec![DhtRuntimeCommandEffect::CancelLookups { lookup_ids }]
            }
            DhtRuntimeCommandAction::ParkDemandLookups {
                info_hash,
                slice_class,
                stop_reason,
                total_peers,
                unique_peers,
                lookup_ids,
            } => {
                vec![
                    DhtRuntimeCommandEffect::ParkDemandLookups {
                        info_hash,
                        slice_class,
                        stop_reason,
                        total_peers,
                        unique_peers,
                        lookup_ids,
                    },
                    DhtRuntimeCommandEffect::StartDueDemands,
                ]
            }
            DhtRuntimeCommandAction::FinalizeDrainedDemandLookups { info_hash } => {
                vec![
                    DhtRuntimeCommandEffect::FinalizeDrainedDemandLookups { info_hash },
                    DhtRuntimeCommandEffect::StartDueDemands,
                ]
            }
            DhtRuntimeCommandAction::AnnouncePeer {
                info_hash,
                port,
                response_tx,
            } => {
                vec![DhtRuntimeCommandEffect::AnnouncePeer {
                    info_hash,
                    port,
                    response_tx,
                }]
            }
        };
        observe_action_effect_reduction(
            "runtime_command",
            action_kind,
            effects.iter().map(DhtRuntimeCommandEffect::kind),
        );
        DhtRuntimeCommandReduction { effects }
    }
}
