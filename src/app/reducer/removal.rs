// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared application-state cleanup; runtime handles belong to the app host.

use crate::app::{clamp_selected_indices_in_state, refresh_torrent_sort_after_removal, AppState};

pub(crate) fn remove_torrent_from_state(state: &mut AppState, info_hash: &[u8]) -> bool {
    let removed = state.torrents.remove(info_hash).is_some();
    state
        .torrent_list_order
        .retain(|candidate| candidate.as_slice() != info_hash);
    if removed {
        refresh_torrent_sort_after_removal(state);
    } else {
        clamp_selected_indices_in_state(state);
    }
    state.ui.needs_redraw = true;
    removed
}

/// Keep a failed payload cleanup recoverable using the existing catalog format.
/// A retained failed entry restores paused and unvalidated; only TorrentState can verify it.
pub(crate) fn reconcile_removed_catalog(
    settings: &mut crate::config::Settings,
    info_hash: &[u8],
    result: &Result<(), String>,
    can_write_catalog: bool,
    recovery: Option<crate::config::TorrentSettings>,
) {
    if !can_write_catalog {
        return;
    }
    let matches_hash = |torrent: &crate::config::TorrentSettings| {
        crate::torrent_identity::info_hash_from_torrent_source(&torrent.torrent_or_magnet)
            .as_deref()
            == Some(info_hash)
    };
    if result.is_err() {
        if let Some(torrent) = settings
            .torrents
            .iter_mut()
            .find(|torrent| matches_hash(torrent))
        {
            torrent.torrent_control_state = crate::app::TorrentControlState::Paused;
            torrent.validation_status = false;
        } else if let Some(torrent) = recovery.filter(matches_hash) {
            settings.torrents.push(torrent);
        }
    } else {
        settings.torrents.retain(|torrent| {
            !(matches_hash(torrent)
                && torrent.torrent_control_state == crate::app::TorrentControlState::Deleting
                && torrent.delete_files)
        });
    }
}

// Capture recovery data before the reducer releases the runtime projection. A newly added
// torrent may fail cleanup before its first checkpoint creates a catalog entry.
pub(super) fn cleanup_recovery_entry(
    display: &crate::app::TorrentDisplayState,
) -> crate::config::TorrentSettings {
    let metrics = &display.latest_state;
    crate::config::TorrentSettings {
        torrent_or_magnet: metrics.torrent_or_magnet.clone(),
        name: metrics.torrent_name.clone(),
        added_at_unix_secs: display.added_at_unix_secs,
        validation_status: false,
        download_path: metrics.download_path.clone(),
        container_name: metrics.container_name.clone(),
        torrent_control_state: crate::app::TorrentControlState::Paused,
        delete_files: metrics.delete_files,
        download_mode: metrics.download_mode,
        file_priorities: metrics.file_priorities.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::torrent_manager_protocol::ManagerEvent;
    use crate::app::{
        reduce_app_action, AppAction, AppEffect, TorrentDisplayState, TorrentMetrics,
    };
    use crate::config::{SortDirection, TorrentSortColumn};

    #[test]
    fn failed_deletion_preserves_error_for_executor_while_removing_runtime_projection() {
        let removed_hash = vec![0x51; 20];
        let retained_hash = vec![0x52; 20];
        let mut state = AppState {
            torrent_list_order: vec![removed_hash.clone(), retained_hash.clone()],
            torrent_sort: (TorrentSortColumn::Name, SortDirection::Ascending),
            torrent_sort_pinned: true,
            ..Default::default()
        };
        for hash in [&removed_hash, &retained_hash] {
            state.torrents.insert(
                hash.clone(),
                TorrentDisplayState {
                    latest_state: TorrentMetrics {
                        info_hash: hash.clone(),
                        torrent_name: hex::encode(hash),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            );
        }
        state.ui.selected_torrent_index = 1;
        let effects = reduce_app_action(
            &mut state,
            AppAction::ManagerEvent(ManagerEvent::DeletionComplete(
                removed_hash.clone(),
                Err("fixture storage unavailable".into()),
            )),
        );

        assert!(!state.torrents.contains_key(&removed_hash));
        assert_eq!(state.torrent_list_order, vec![retained_hash.clone()]);
        assert_eq!(state.ui.selected_torrent_index, 0);
        assert_eq!(
            state.torrent_sort,
            (TorrentSortColumn::Name, SortDirection::Ascending)
        );
        assert!(matches!(effects.as_slice(), [AppEffect::TorrentRemoved {
            info_hash, result: Err(error), was_present: true, ..
        }] if info_hash == &removed_hash && error == "fixture storage unavailable"));

        let repeated = reduce_app_action(
            &mut state,
            AppAction::ManagerEvent(ManagerEvent::DeletionComplete(removed_hash, Ok(()))),
        );
        assert!(matches!(
            repeated.as_slice(),
            [AppEffect::TorrentRemoved {
                was_present: false,
                ..
            }]
        ));
        assert_eq!(state.torrent_list_order, vec![retained_hash]);
        assert_eq!(state.ui.selected_torrent_index, 0);
    }
}
