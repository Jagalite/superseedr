// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Snapshot preparation and durable commit observations; no physical I/O.

use super::*;

/// Checkpoint requests are snapshots. A successful older write never clears a newer request.
#[derive(Default, Debug)]
pub struct CheckpointState {
    pub requested_revision: u64,
    pub committed_revision: u64,
    pub last_error: Option<String>,
}

impl CheckpointState {
    pub fn is_dirty(&self) -> bool {
        self.requested_revision != self.committed_revision
    }

    pub(crate) fn request(&mut self) -> u64 {
        self.requested_revision = self
            .requested_revision
            .checked_add(1)
            .expect("application checkpoint revision exhausted");
        self.requested_revision
    }

    pub(crate) fn finish(&mut self, revision: u64, result: Result<(), String>) -> bool {
        if revision > self.requested_revision || revision <= self.committed_revision {
            return false;
        }
        match result {
            Ok(()) => {
                self.committed_revision = revision;
                if revision == self.requested_revision {
                    self.last_error = None;
                }
            }
            Err(error) => {
                if revision == self.requested_revision {
                    self.last_error = Some(error);
                }
            }
        }
        true
    }
}

pub(super) fn persisted_validation_status_from_metrics(
    metrics: &TorrentMetrics,
    previous_validation_status: bool,
) -> bool {
    // Metadata may not be available yet for magnet sessions; preserve prior validation
    // only for the unknown 0/0 snapshot when we also have no explicit completion signal.
    if metrics.number_of_pieces_total == 0
        && metrics.number_of_pieces_completed == 0
        && !metrics.is_complete
        && !activity_marks_torrent_complete(&metrics.activity_message)
        && !torrent_has_skipped_files(metrics)
    {
        return previous_validation_status;
    }

    metrics.is_complete || !torrent_is_effectively_incomplete(metrics)
}

pub(crate) fn prepare_checkpoint(
    client_configs: &mut Settings,
    app_state: &mut AppState,
    startup_deferred_load_queue: &VecDeque<Vec<u8>>,
    observed_unix_secs: u64,
) -> PersistPayload {
    let revision = app_state.checkpoint.request();
    client_configs.lifetime_downloaded =
        app_state.lifetime_downloaded_from_config + app_state.session_total_downloaded;
    client_configs.lifetime_uploaded =
        app_state.lifetime_uploaded_from_config + app_state.session_total_uploaded;

    client_configs.torrent_sort_column = app_state.torrent_sort.0;
    client_configs.torrent_sort_direction = app_state.torrent_sort.1;
    client_configs.torrent_sort_pinned = app_state.torrent_sort_pinned;
    client_configs.peer_sort_column = app_state.peer_sort.0;
    client_configs.peer_sort_direction = app_state.peer_sort.1;
    client_configs.peer_sort_pinned = app_state.peer_sort_pinned;
    client_configs.ui_refresh_rate = app_state.data_rate;
    client_configs.peer_stream_visualization = app_state.ui.visualization_focus.peer_stream;
    client_configs.disk_health_visualization = app_state.ui.visualization_focus.disk_health;
    client_configs.dht_visualization = app_state.ui.visualization_focus.dht;
    let old_validation_statuses: HashMap<String, bool> = client_configs
        .torrents
        .iter()
        .map(|cfg| (cfg.torrent_or_magnet.clone(), cfg.validation_status))
        .collect();
    let old_added_at_unix_secs: HashMap<String, Option<u64>> = client_configs
        .torrents
        .iter()
        .map(|cfg| (cfg.torrent_or_magnet.clone(), cfg.added_at_unix_secs))
        .collect();
    let previous_torrents = client_configs.torrents.clone();
    let deferred_hashes: HashSet<Vec<u8>> = startup_deferred_load_queue.iter().cloned().collect();
    let pending_preview_info_hash = app_state.pending_magnet_preview_info_hash.clone();
    let is_pending_preview =
        |info_hash: &[u8]| pending_preview_info_hash.as_deref() == Some(info_hash);
    let mut persisted_info_hashes: HashSet<Vec<u8>> = app_state
        .torrents
        .keys()
        .filter(|info_hash| !is_pending_preview(info_hash.as_slice()))
        .cloned()
        .collect();

    let mut persisted_torrents: Vec<TorrentSettings> = app_state
        .torrents
        .iter()
        .filter_map(|(info_hash, torrent)| {
            if is_pending_preview(info_hash) {
                return None;
            }

            let torrent_state = &torrent.latest_state;
            let previous_validation_status = old_validation_statuses
                .get(&torrent_state.torrent_or_magnet)
                .copied()
                .unwrap_or(false);

            let final_validation_status =
                persisted_validation_status_from_metrics(torrent_state, previous_validation_status);

            Some(TorrentSettings {
                torrent_or_magnet: torrent_state.torrent_or_magnet.clone(),
                name: torrent_state.torrent_name.clone(),
                added_at_unix_secs: torrent.added_at_unix_secs.or_else(|| {
                    old_added_at_unix_secs
                        .get(&torrent_state.torrent_or_magnet)
                        .copied()
                        .flatten()
                }),
                validation_status: final_validation_status,
                download_path: torrent_state.download_path.clone(),
                container_name: torrent_state.container_name.clone(),
                torrent_control_state: torrent_state.torrent_control_state.clone(),
                delete_files: torrent_state.delete_files,
                file_priorities: torrent_state.file_priorities.clone(),
            })
        })
        .collect();

    for torrent in previous_torrents {
        let Some(info_hash) = info_hash_from_torrent_source(&torrent.torrent_or_magnet) else {
            continue;
        };

        if (deferred_hashes.contains(&info_hash)
            || app_state.cleanup_failures.contains_key(&info_hash))
            && persisted_info_hashes.insert(info_hash)
        {
            persisted_torrents.push(torrent);
        }
    }

    client_configs.torrents = persisted_torrents;

    const RSS_HISTORY_LIMIT: usize = 1000;
    if app_state.rss_runtime.history.len() > RSS_HISTORY_LIMIT {
        let overflow = app_state.rss_runtime.history.len() - RSS_HISTORY_LIMIT;
        app_state.rss_runtime.history.drain(0..overflow);
    }

    let rss_state = RssPersistedState {
        history: app_state.rss_runtime.history.clone(),
        last_sync_at: app_state.rss_runtime.last_sync_at.clone(),
        feed_errors: app_state.rss_runtime.feed_errors.clone(),
    };

    let network_history = if !client_configs.persist_graph_history
        || app_state.network_history_restore_pending
    {
        None
    } else {
        app_state.network_history_state.rollups = app_state.network_history_rollups.to_snapshot();
        app_state.network_history_state.updated_at_unix = observed_unix_secs;
        app_state.next_network_history_persist_request_id = app_state
            .next_network_history_persist_request_id
            .saturating_add(1);
        Some(NetworkHistoryPersistRequest {
            request_id: app_state.next_network_history_persist_request_id,
            state: app_state.network_history_state.clone(),
        })
    };

    let activity_history =
        if !client_configs.persist_graph_history || app_state.activity_history_restore_pending {
            None
        } else {
            app_state
                .activity_history_rollups
                .sync_snapshots_to_state(&mut app_state.activity_history_state);
            app_state.activity_history_state.updated_at_unix = observed_unix_secs;
            app_state.next_activity_history_persist_request_id = app_state
                .next_activity_history_persist_request_id
                .saturating_add(1);
            Some(ActivityHistoryPersistRequest {
                request_id: app_state.next_activity_history_persist_request_id,
                // Native storage already omits zero samples. Compact before the
                // snapshot is retained by the persistence channel and cloned by its worker.
                #[cfg(not(target_arch = "wasm32"))]
                state: crate::persistence::activity_history::sparse_state_for_persistence(
                    &app_state.activity_history_state,
                ),
                #[cfg(target_arch = "wasm32")]
                state: app_state.activity_history_state.clone(),
            })
        };

    PersistPayload {
        revision,
        settings: client_configs.clone(),
        rss_state,
        network_history,
        activity_history,
    }
}

pub(super) fn apply_network_history_persist_result(
    app_state: &mut AppState,
    request_id: u64,
    success: bool,
) {
    if success && app_state.pending_network_history_persist_request_id == Some(request_id) {
        app_state.network_history_dirty = false;
        app_state.pending_network_history_persist_request_id = None;
    }
}

pub(super) fn apply_activity_history_persist_result(
    app_state: &mut AppState,
    request_id: u64,
    success: bool,
) {
    if success && app_state.pending_activity_history_persist_request_id == Some(request_id) {
        app_state.activity_history_dirty = false;
        app_state.pending_activity_history_persist_request_id = None;
    }
}

pub(super) fn should_persist_network_history_on_interval(app_state: &AppState) -> bool {
    app_state.network_history_dirty
        || app_state.activity_history_dirty
        || !app_state.network_history_state.tiers.second_1s.is_empty()
        || !app_state
            .activity_history_state
            .cpu
            .tiers
            .second_1s
            .is_empty()
}

#[derive(Clone)]
pub struct NetworkHistoryPersistRequest {
    pub request_id: u64,
    pub state: NetworkHistoryPersistedState,
}

#[derive(Clone)]
pub struct ActivityHistoryPersistRequest {
    pub request_id: u64,
    pub state: ActivityHistoryPersistedState,
}

#[derive(Clone)]
pub struct PersistPayload {
    pub revision: u64,
    pub settings: Settings,
    pub rss_state: RssPersistedState,
    pub network_history: Option<NetworkHistoryPersistRequest>,
    pub activity_history: Option<ActivityHistoryPersistRequest>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_graph_history_skips_checkpoint_snapshots() {
        let mut settings = Settings {
            persist_graph_history: false,
            ..Default::default()
        };
        let mut state = AppState::default();
        state.network_history_dirty = true;
        state.activity_history_dirty = true;
        let snapshot = prepare_checkpoint(&mut settings, &mut state, &VecDeque::new(), 42);
        assert!(snapshot.network_history.is_none());
        assert!(snapshot.activity_history.is_none());
        assert_eq!(state.next_network_history_persist_request_id, 0);
        assert_eq!(state.next_activity_history_persist_request_id, 0);
        assert!(!snapshot.settings.persist_graph_history);

        settings.persist_graph_history = true;
        let snapshot = prepare_checkpoint(&mut settings, &mut state, &VecDeque::new(), 43);
        assert!(snapshot.network_history.is_some());
        assert!(snapshot.activity_history.is_some());
    }

    #[test]
    fn stale_metrics_cannot_revert_accepted_file_priorities_at_checkpoint() {
        for (accepted, stale) in [
            (HashMap::from([(0, FilePriority::High)]), HashMap::new()),
            (HashMap::new(), HashMap::from([(0, FilePriority::High)])),
        ] {
            let info_hash = vec![0x42; 20];
            let source = format!("magnet:?xt=urn:btih:{}", hex::encode(&info_hash));
            let mut settings = Settings {
                torrents: vec![TorrentSettings {
                    torrent_or_magnet: source.clone(),
                    file_priorities: accepted.clone(),
                    ..Default::default()
                }],
                ..Default::default()
            };
            let mut state = AppState::default();
            state.torrents.insert(
                info_hash.clone(),
                TorrentDisplayState {
                    latest_state: TorrentMetrics {
                        torrent_or_magnet: source,
                        file_priorities: accepted.clone(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            );
            crate::telemetry::ui_telemetry::UiTelemetry::on_metrics(
                &mut state,
                TorrentMetrics {
                    info_hash,
                    file_priorities: stale,
                    ..Default::default()
                },
            );

            // Repeated saves must not make a stale sample authoritative either.
            for _ in 0..2 {
                let snapshot = prepare_checkpoint(&mut settings, &mut state, &VecDeque::new(), 1);
                assert_eq!(snapshot.settings.torrents[0].file_priorities, accepted);
                assert_eq!(settings.torrents[0].file_priorities, accepted);
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_checkpoint_sparsifies_history_without_changing_live_points_or_rollups() {
        use crate::persistence::activity_history::{ActivityHistoryPoint, ActivityHistorySeries};
        let mut settings = Settings::default();
        let mut state = AppState::default();
        let mut series = ActivityHistorySeries::default();
        for ts in 1..=75 {
            state.activity_history_rollups.cpu.ingest_second_sample(
                &mut series,
                ts,
                if ts % 7 == 0 { 14 } else { 0 },
                0,
            );
        }
        state.activity_history_state.cpu = series.clone();
        let mut idle = ActivityHistorySeries::default();
        idle.tiers.second_1s.push(ActivityHistoryPoint {
            ts_unix: 75,
            primary: 0,
            secondary: 0,
        });
        state
            .activity_history_state
            .torrents
            .insert("idle-history".into(), idle);
        state
            .activity_history_state
            .torrents
            .insert("active-history".into(), series.clone());
        let original = state.activity_history_state.clone();
        let payload = prepare_checkpoint(&mut settings, &mut state, &VecDeque::new(), 76);
        let sparse = payload.activity_history.unwrap().state;
        assert_eq!(state.activity_history_state.cpu, original.cpu);
        assert_eq!(state.activity_history_state.torrents, original.torrents);
        assert_eq!(sparse.cpu.rollups, state.activity_history_state.cpu.rollups);
        assert_eq!(sparse.cpu.rollups.second_to_minute.count, 15);
        assert_eq!(
            sparse.cpu.tiers.second_1s.first(),
            series.tiers.second_1s.first()
        );
        assert_eq!(
            sparse.cpu.tiers.second_1s.last(),
            series.tiers.second_1s.last()
        );
        assert!(sparse.torrents.contains_key("idle-history"));
        assert!(sparse.torrents.contains_key("active-history"));
        assert_eq!(sparse.updated_at_unix, 76);
        assert_eq!(
            sparse,
            crate::persistence::activity_history::sparse_state_for_persistence(&sparse)
        );
    }

    #[test]
    fn pending_activity_restore_omits_checkpoint_history() {
        let mut settings = Settings::default();
        let mut state = AppState {
            activity_history_restore_pending: true,
            ..Default::default()
        };
        let before = state.next_activity_history_persist_request_id;
        let payload = prepare_checkpoint(&mut settings, &mut state, &VecDeque::new(), 76);
        assert!(payload.activity_history.is_none());
        assert_eq!(state.next_activity_history_persist_request_id, before);
    }

    #[test]
    fn first_checkpoint_preserves_new_runtime_torrent_priorities() {
        let mut settings = Settings::default();
        let mut state = AppState::default();
        let priorities = HashMap::from([(0, FilePriority::Skip)]);
        state.torrents.insert(
            vec![0x43; 20],
            TorrentDisplayState {
                latest_state: TorrentMetrics {
                    torrent_or_magnet: format!("magnet:?xt=urn:btih:{}", "43".repeat(20)),
                    file_priorities: priorities.clone(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        let snapshot = prepare_checkpoint(&mut settings, &mut state, &VecDeque::new(), 1);
        assert_eq!(snapshot.settings.torrents[0].file_priorities, priorities);
    }

    #[test]
    fn old_completion_cannot_clear_a_newer_checkpoint_or_its_failure() {
        let mut state = CheckpointState::default();
        let first = state.request();
        let second = state.request();
        assert!(state.finish(second, Err("storage offline".into())));
        assert!(state.finish(first, Ok(())));
        assert!(state.is_dirty());
        assert_eq!(state.last_error.as_deref(), Some("storage offline"));
        assert!(state.finish(second, Ok(())));
        assert!(!state.is_dirty());
        assert_eq!(state.last_error, None);
        assert!(!state.finish(second, Err("late failure".into())));
        assert_eq!(state.last_error, None);
    }

    #[test]
    fn an_unissued_checkpoint_completion_is_ignored() {
        let mut state = CheckpointState::default();
        state.request();
        assert!(!state.finish(20, Ok(())));
        assert!(state.is_dirty());
        assert_eq!(state.committed_revision, 0);
    }

    #[test]
    fn snapshot_time_is_the_host_observation_and_restore_is_not_overwritten() {
        let mut settings = Settings::default();
        let mut state = AppState {
            network_history_restore_pending: true,
            ..Default::default()
        };
        let snapshot = prepare_checkpoint(&mut settings, &mut state, &VecDeque::new(), 1234);
        assert!(snapshot.network_history.is_none());
        assert_eq!(
            snapshot.activity_history.unwrap().state.updated_at_unix,
            1234
        );
        assert!(state.checkpoint.is_dirty());
    }
    #[test]
    fn failed_payload_deletion_retains_an_unvalidated_paused_catalog_entry() {
        let hash = vec![0x31; 20];
        let torrent = TorrentSettings {
            torrent_or_magnet: format!("magnet:?xt=urn:btih:{}", hex::encode(&hash)),
            name: "Fictional Recovery Set".into(),
            torrent_control_state: TorrentControlState::Deleting,
            delete_files: true,
            validation_status: true,
            ..Default::default()
        };
        let mut settings = Settings {
            torrents: vec![torrent],
            ..Default::default()
        };
        let mut state = AppState::default();
        state
            .cleanup_failures
            .insert(hash.clone(), "storage offline".into());
        crate::app::reducer::reconcile_removed_catalog(
            &mut settings,
            &hash,
            &Err("storage offline".into()),
            true,
            None,
        );
        let snapshot = prepare_checkpoint(&mut settings, &mut state, &VecDeque::new(), 1);
        assert_eq!(snapshot.settings.torrents.len(), 1);
        assert_eq!(
            snapshot.settings.torrents[0].torrent_control_state,
            TorrentControlState::Paused
        );
        assert!(!snapshot.settings.torrents[0].validation_status);
    }
}
