// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Platform-neutral application actions and effects.

pub(crate) mod health;
mod metadata;
mod services;
pub(crate) use services::ServiceObservation;
pub(crate) mod preview;
mod removal;

pub(crate) use removal::{reconcile_removed_catalog, remove_torrent_from_state};

use std::collections::HashMap;

use super::torrent_manager_protocol::{FileProbeBatchResult, ManagerEvent};
use super::{
    has_effectively_incomplete_torrents, set_automatic_torrent_sort,
    sort_and_filter_torrent_list_state, torrent_is_effectively_incomplete, AppState,
};
use crate::app::{FilePriority, TorrentMetrics};
use crate::config::TorrentSortColumn;
use crate::persistence::StorageError;
use crate::telemetry::ui_telemetry::UiTelemetry;
use crate::torrent_file::Torrent;

pub(crate) enum AppAction {
    ServiceObserved(ServiceObservation),
    CheckpointCompleted {
        revision: u64,
        result: Result<(), String>,
    },
    TorrentAdded,
    ManagerMetrics(Box<TorrentMetrics>),
    ManagerEvent(ManagerEvent),
}

pub enum AppEffect {
    RefreshRss,
    CheckpointRequested,
    MetadataLoaded {
        info_hash: Vec<u8>,
        torrent: Box<Torrent>,
        file_priorities: HashMap<usize, FilePriority>,
    },
    TorrentRemoved {
        info_hash: Vec<u8>,
        result: Result<(), String>,
        was_present: bool,
        recovery: Option<crate::config::TorrentSettings>,
    },
    TorrentCompleted {
        info_hash: Vec<u8>,
        torrent_name: String,
    },
    DataAvailabilityFault {
        info_hash: Vec<u8>,
        piece_index: u32,
        error: StorageError,
        availability_changed: bool,
    },
    ProcessFileProbeBatch {
        info_hash: Vec<u8>,
        result: FileProbeBatchResult,
    },
}

pub(crate) fn reduce_app_action(app_state: &mut AppState, action: AppAction) -> Vec<AppEffect> {
    match action {
        AppAction::ServiceObserved(observation) => {
            services::reduce_service_observation(app_state, observation)
        }
        AppAction::CheckpointCompleted { revision, result } => {
            let previous_error = app_state.checkpoint.last_error.clone();
            if app_state.checkpoint.finish(revision, result) {
                if let Some(error) = &app_state.checkpoint.last_error {
                    app_state.system_error = Some(error.clone());
                } else if app_state.system_error == previous_error {
                    app_state.system_error = None;
                }
                app_state.ui.needs_redraw = true;
            }
            Vec::new()
        }
        AppAction::TorrentAdded => {
            set_automatic_torrent_sort(app_state, TorrentSortColumn::Down);
            Vec::new()
        }
        AppAction::ManagerMetrics(metrics) => reduce_manager_metrics(app_state, *metrics),
        AppAction::ManagerEvent(ManagerEvent::MetadataLoaded { info_hash, torrent }) => {
            let file_priorities = metadata::reduce_metadata_loaded(app_state, &info_hash, &torrent);
            vec![AppEffect::MetadataLoaded {
                info_hash,
                torrent,
                file_priorities,
            }]
        }
        AppAction::ManagerEvent(ManagerEvent::DeletionComplete(info_hash, result)) => {
            let recovery = if result.is_err() {
                app_state
                    .torrents
                    .get(&info_hash)
                    .map(removal::cleanup_recovery_entry)
            } else {
                app_state.cleanup_failures.remove(&info_hash);
                None
            };
            let was_present = remove_torrent_from_state(app_state, &info_hash);
            if let Err(error) = &result {
                app_state
                    .cleanup_failures
                    .insert(info_hash.clone(), error.clone());
                app_state.system_error = Some(format!("Torrent cleanup failed: {error}"));
                app_state.ui.needs_redraw = true;
            }
            vec![AppEffect::TorrentRemoved {
                info_hash,
                result,
                was_present,
                recovery,
            }]
        }
        AppAction::ManagerEvent(ManagerEvent::DataAvailabilityFault {
            info_hash,
            piece_index,
            error,
        }) => {
            let mut availability_changed = false;
            if let Some(torrent) = app_state.torrents.get_mut(&info_hash) {
                availability_changed = torrent.latest_state.data_available;
                torrent.latest_state.data_available = false;
            }
            app_state.ui.needs_redraw = true;
            vec![AppEffect::DataAvailabilityFault {
                info_hash,
                piece_index,
                error,
                availability_changed,
            }]
        }
        AppAction::ManagerEvent(ManagerEvent::FileProbeBatchResult { info_hash, result }) => {
            vec![AppEffect::ProcessFileProbeBatch { info_hash, result }]
        }
        AppAction::ManagerEvent(
            event @ (ManagerEvent::DiskReadStarted { .. }
            | ManagerEvent::DiskReadFinished
            | ManagerEvent::DiskWriteStarted { .. }
            | ManagerEvent::DiskWriteCompleted { .. }
            | ManagerEvent::DiskWriteFinished { .. }
            | ManagerEvent::DiskIoBackoff { .. }
            | ManagerEvent::PeerDiscovered { .. }
            | ManagerEvent::PeerConnected { .. }
            | ManagerEvent::PeerDisconnected { .. }
            | ManagerEvent::BlockReceived { .. }
            | ManagerEvent::BlockSent { .. }),
        ) => {
            if UiTelemetry::on_manager_event_metrics(app_state, &event) {
                app_state.ui.needs_redraw = true;
            }
            Vec::new()
        }
        #[cfg(feature = "synthetic-load")]
        AppAction::ManagerEvent(
            ManagerEvent::PeerConnectAttempted { .. }
            | ManagerEvent::PeerConnectEstablished { .. }
            | ManagerEvent::PeerConnectFailed { .. }
            | ManagerEvent::PeerSessionFailed
            | ManagerEvent::SyntheticProbeCompleted { .. },
        ) => Vec::new(),
    }
}

fn reduce_manager_metrics(app_state: &mut AppState, metrics: TorrentMetrics) -> Vec<AppEffect> {
    let info_hash = metrics.info_hash.clone();
    let was_complete = app_state
        .torrents
        .get(&info_hash)
        .map(|torrent| !torrent_is_effectively_incomplete(&torrent.latest_state))
        .unwrap_or(false);

    UiTelemetry::on_metrics(app_state, metrics);
    if !app_state.torrent_list_order.contains(&info_hash) {
        app_state.torrent_list_order.push(info_hash.clone());
    }
    app_state.ui.needs_redraw = true;

    let Some(torrent) = app_state.torrents.get(&info_hash) else {
        return Vec::new();
    };
    if was_complete || torrent_is_effectively_incomplete(&torrent.latest_state) {
        Vec::new()
    } else {
        vec![AppEffect::TorrentCompleted {
            info_hash,
            torrent_name: torrent.latest_state.torrent_name.clone(),
        }]
    }
}

/// Finalizes a batch of manager metrics after every changed receiver has been reduced.
///
/// Sorting once per drain keeps the shared reducer independent of transport shape and avoids
/// repeating an O(n log n) list rebuild for every torrent in a native or browser frame.
pub(crate) fn finalize_manager_metrics_batch(
    app_state: &mut AppState,
    select_upload_priority_if_all_complete: bool,
) {
    if select_upload_priority_if_all_complete && !has_effectively_incomplete_torrents(app_state) {
        set_automatic_torrent_sort(app_state, TorrentSortColumn::Up);
        return;
    }
    sort_and_filter_torrent_list_state(app_state);
    app_state.ui.needs_redraw = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::FilePriority;
    use crate::config::{SortDirection, TorrentSortColumn};

    #[test]
    fn repeated_data_faults_preserve_error_but_only_report_the_first_availability_change() {
        let info_hash = vec![0x61; 20];
        let mut state = AppState::default();
        reduce_app_action(
            &mut state,
            AppAction::ManagerMetrics(Box::new(TorrentMetrics {
                info_hash: info_hash.clone(),
                torrent_name: "Fictional Recovery Garden".into(),
                data_available: true,
                ..Default::default()
            })),
        );
        for expected_change in [true, false] {
            let effects = reduce_app_action(
                &mut state,
                AppAction::ManagerEvent(ManagerEvent::DataAvailabilityFault {
                    info_hash: info_hash.clone(),
                    piece_index: 3,
                    error: StorageError::UnexpectedType,
                }),
            );
            assert!(!state.torrents[&info_hash].latest_state.data_available);
            assert!(
                matches!(effects.as_slice(), [AppEffect::DataAvailabilityFault {
                piece_index: 3, error: StorageError::UnexpectedType, availability_changed, ..
            }] if *availability_changed == expected_change)
            );
        }
    }

    #[test]
    fn manager_metrics_use_the_shared_telemetry_and_batch_sort_path() {
        let mut state = AppState::default();
        let info_hash = vec![0x2a; 20];
        let effects = reduce_app_action(
            &mut state,
            AppAction::ManagerMetrics(Box::new(TorrentMetrics {
                info_hash: info_hash.clone(),
                torrent_name: "Fictional Kernel Garden".to_string(),
                number_of_pieces_total: 10,
                number_of_pieces_completed: 4,
                download_speed_bps: 42_000,
                ..TorrentMetrics::default()
            })),
        );

        finalize_manager_metrics_batch(&mut state, false);

        assert!(effects.is_empty());
        assert_eq!(state.torrent_list_order, vec![info_hash.clone()]);
        assert_eq!(
            state.torrents[&info_hash].latest_state.download_speed_bps,
            42_000
        );
        assert!(state.ui.needs_redraw);
    }

    #[test]
    fn metrics_batch_sort_preserves_the_selected_row() {
        for selected_index in [0, 1] {
            let selected_hash = vec![0x31; 20];
            let faster_hash = vec![0x32; 20];
            let mut state = AppState {
                torrent_sort: (TorrentSortColumn::Down, SortDirection::Descending),
                torrent_list_order: vec![selected_hash.clone(), faster_hash.clone()],
                ..AppState::default()
            };
            state.ui.selected_torrent_index = selected_index;
            for (info_hash, torrent_name, download_speed_bps) in [
                (selected_hash.clone(), "Fictional Quiet Kernel", 1_000),
                (faster_hash.clone(), "Fictional Swift Kernel", 20_000),
            ] {
                reduce_app_action(
                    &mut state,
                    AppAction::ManagerMetrics(Box::new(TorrentMetrics {
                        info_hash,
                        torrent_name: torrent_name.to_string(),
                        download_speed_bps,
                        number_of_pieces_total: 10,
                        number_of_pieces_completed: 5,
                        ..TorrentMetrics::default()
                    })),
                );
            }

            finalize_manager_metrics_batch(&mut state, false);

            assert_eq!(state.torrent_list_order, vec![faster_hash, selected_hash]);
            assert_eq!(state.ui.selected_torrent_index, selected_index);
        }
    }

    #[test]
    fn manager_metric_batch_sorts_once_after_all_updates_are_reduced() {
        let mut state = AppState {
            torrent_sort: (TorrentSortColumn::Name, SortDirection::Ascending),
            ..AppState::default()
        };
        let later_hash = vec![0x31; 20];
        let earlier_hash = vec![0x32; 20];

        for (info_hash, torrent_name) in [
            (later_hash.clone(), "Zephyr Archive"),
            (earlier_hash.clone(), "Amber Archive"),
        ] {
            reduce_app_action(
                &mut state,
                AppAction::ManagerMetrics(Box::new(TorrentMetrics {
                    info_hash,
                    torrent_name: torrent_name.to_string(),
                    ..TorrentMetrics::default()
                })),
            );
        }

        assert_eq!(
            state.torrent_list_order,
            vec![later_hash.clone(), earlier_hash.clone()]
        );
        finalize_manager_metrics_batch(&mut state, false);
        assert_eq!(state.torrent_list_order, vec![earlier_hash, later_hash]);
    }

    #[test]
    fn initial_manager_metrics_with_only_skipped_files_emit_completion() {
        let mut state = AppState::default();
        let info_hash = vec![0x33; 20];

        let effects = reduce_app_action(
            &mut state,
            AppAction::ManagerMetrics(Box::new(TorrentMetrics {
                info_hash: info_hash.clone(),
                torrent_name: "Fictional Skipped Orchard".to_string(),
                number_of_pieces_total: 10,
                file_priorities: std::collections::HashMap::from([(0, FilePriority::Skip)]),
                ..TorrentMetrics::default()
            })),
        );

        assert!(matches!(
            effects.as_slice(),
            [AppEffect::TorrentCompleted {
                info_hash: completed_hash,
                torrent_name,
            }] if completed_hash == &info_hash && torrent_name == "Fictional Skipped Orchard"
        ));
        assert_eq!(
            state.torrents[&info_hash].latest_state.file_priorities,
            std::collections::HashMap::from([(0, FilePriority::Skip)])
        );
    }

    #[test]
    fn manager_event_telemetry_is_reduced_without_a_platform_effect() {
        let mut state = AppState::default();
        let info_hash = vec![0x2b; 20];
        reduce_app_action(
            &mut state,
            AppAction::ManagerMetrics(Box::new(TorrentMetrics {
                info_hash: info_hash.clone(),
                torrent_name: "Imaginary Packet Orchard".to_string(),
                ..TorrentMetrics::default()
            })),
        );

        let effects = reduce_app_action(
            &mut state,
            AppAction::ManagerEvent(ManagerEvent::PeerConnected {
                info_hash: info_hash.clone(),
            }),
        );

        assert!(effects.is_empty());
        assert_eq!(state.torrents[&info_hash].peers_connected_this_tick, 1);
    }

    #[test]
    fn torrent_added_replaces_a_manual_sort_with_download_priority() {
        let mut state = AppState {
            torrent_sort: (TorrentSortColumn::Name, SortDirection::Ascending),
            torrent_sort_pinned: true,
            ..AppState::default()
        };

        let effects = reduce_app_action(&mut state, AppAction::TorrentAdded);

        assert!(effects.is_empty());
        assert_eq!(
            state.torrent_sort,
            (TorrentSortColumn::Down, SortDirection::Descending)
        );
        assert!(!state.torrent_sort_pinned);
    }

    #[test]
    fn final_incomplete_torrent_completion_selects_upload_priority() {
        let mut state = AppState::default();
        let info_hash = vec![0x41; 20];
        reduce_app_action(
            &mut state,
            AppAction::ManagerMetrics(Box::new(TorrentMetrics {
                info_hash: info_hash.clone(),
                torrent_name: "Fictional Packet Meadow".to_string(),
                number_of_pieces_total: 10,
                number_of_pieces_completed: 9,
                ..TorrentMetrics::default()
            })),
        );
        finalize_manager_metrics_batch(&mut state, false);
        state.torrent_sort = (TorrentSortColumn::Name, SortDirection::Ascending);
        state.torrent_sort_pinned = true;

        let batch_started_with_incomplete_torrents = has_effectively_incomplete_torrents(&state);
        reduce_app_action(
            &mut state,
            AppAction::ManagerMetrics(Box::new(TorrentMetrics {
                info_hash,
                torrent_name: "Fictional Packet Meadow".to_string(),
                number_of_pieces_total: 10,
                number_of_pieces_completed: 10,
                is_complete: true,
                ..TorrentMetrics::default()
            })),
        );
        finalize_manager_metrics_batch(&mut state, batch_started_with_incomplete_torrents);

        assert_eq!(
            state.torrent_sort,
            (TorrentSortColumn::Up, SortDirection::Descending)
        );
        assert!(!state.torrent_sort_pinned);
    }

    #[test]
    fn partial_completion_preserves_the_manual_sort() {
        let mut state = AppState::default();
        let completed_hash = vec![0x42; 20];
        let remaining_hash = vec![0x43; 20];
        for (info_hash, torrent_name) in [
            (completed_hash.clone(), "Fictional Packet Orchard"),
            (remaining_hash, "Fictional Packet Grove"),
        ] {
            reduce_app_action(
                &mut state,
                AppAction::ManagerMetrics(Box::new(TorrentMetrics {
                    info_hash,
                    torrent_name: torrent_name.to_string(),
                    number_of_pieces_total: 10,
                    number_of_pieces_completed: 5,
                    ..TorrentMetrics::default()
                })),
            );
        }
        finalize_manager_metrics_batch(&mut state, false);
        state.torrent_sort = (TorrentSortColumn::Name, SortDirection::Ascending);
        state.torrent_sort_pinned = true;

        let batch_started_with_incomplete_torrents = has_effectively_incomplete_torrents(&state);
        reduce_app_action(
            &mut state,
            AppAction::ManagerMetrics(Box::new(TorrentMetrics {
                info_hash: completed_hash,
                torrent_name: "Fictional Packet Orchard".to_string(),
                number_of_pieces_total: 10,
                number_of_pieces_completed: 10,
                is_complete: true,
                ..TorrentMetrics::default()
            })),
        );
        finalize_manager_metrics_batch(&mut state, batch_started_with_incomplete_torrents);

        assert_eq!(
            state.torrent_sort,
            (TorrentSortColumn::Name, SortDirection::Ascending)
        );
        assert!(state.torrent_sort_pinned);
    }

    #[test]
    fn a_new_incomplete_torrent_in_the_completion_batch_keeps_download_priority() {
        let mut state = AppState::default();
        let finishing_hash = vec![0x44; 20];
        reduce_app_action(
            &mut state,
            AppAction::ManagerMetrics(Box::new(TorrentMetrics {
                info_hash: finishing_hash.clone(),
                torrent_name: "Fictional Byte Prairie".to_string(),
                number_of_pieces_total: 10,
                number_of_pieces_completed: 9,
                ..TorrentMetrics::default()
            })),
        );
        finalize_manager_metrics_batch(&mut state, false);

        reduce_app_action(&mut state, AppAction::TorrentAdded);
        let batch_started_with_incomplete_torrents = has_effectively_incomplete_torrents(&state);
        reduce_app_action(
            &mut state,
            AppAction::ManagerMetrics(Box::new(TorrentMetrics {
                info_hash: finishing_hash,
                torrent_name: "Fictional Byte Prairie".to_string(),
                number_of_pieces_total: 10,
                number_of_pieces_completed: 10,
                is_complete: true,
                ..TorrentMetrics::default()
            })),
        );
        reduce_app_action(
            &mut state,
            AppAction::ManagerMetrics(Box::new(TorrentMetrics {
                info_hash: vec![0x45; 20],
                torrent_name: "Fictional Byte Valley".to_string(),
                number_of_pieces_total: 10,
                number_of_pieces_completed: 0,
                ..TorrentMetrics::default()
            })),
        );
        finalize_manager_metrics_batch(&mut state, batch_started_with_incomplete_torrents);

        assert_eq!(
            state.torrent_sort,
            (TorrentSortColumn::Down, SortDirection::Descending)
        );
        assert!(!state.torrent_sort_pinned);
    }
}
