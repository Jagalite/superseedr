// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Platform-neutral application actions produced by torrent-manager output.

use super::manager_port::ManagerEvent;
use super::{sort_and_filter_torrent_list_state, torrent_is_effectively_incomplete, AppState};
use crate::app::TorrentMetrics;
use crate::telemetry::ui_telemetry::UiTelemetry;

pub(crate) enum AppAction {
    ManagerMetrics(Box<TorrentMetrics>),
    ManagerEvent(ManagerEvent),
}

pub(crate) enum AppEffect {
    TorrentCompleted {
        info_hash: Vec<u8>,
        torrent_name: String,
    },
    HandleManagerEvent(ManagerEvent),
}

pub(crate) fn reduce_app_action(app_state: &mut AppState, action: AppAction) -> Vec<AppEffect> {
    match action {
        AppAction::ManagerMetrics(metrics) => reduce_manager_metrics(app_state, *metrics),
        AppAction::ManagerEvent(event) => {
            if UiTelemetry::on_manager_event_metrics(app_state, &event) {
                app_state.ui.needs_redraw = true;
                Vec::new()
            } else {
                vec![AppEffect::HandleManagerEvent(event)]
            }
        }
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
pub(crate) fn finalize_manager_metrics_batch(app_state: &mut AppState) {
    sort_and_filter_torrent_list_state(app_state);
    app_state.ui.needs_redraw = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SortDirection, TorrentSortColumn};

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

        finalize_manager_metrics_batch(&mut state);

        assert!(effects.is_empty());
        assert_eq!(state.torrent_list_order, vec![info_hash.clone()]);
        assert_eq!(
            state.torrents[&info_hash].latest_state.download_speed_bps,
            42_000
        );
        assert!(state.ui.needs_redraw);
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
        finalize_manager_metrics_batch(&mut state);
        assert_eq!(state.torrent_list_order, vec![earlier_hash, later_hash]);
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
}
