// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native cluster execution. Runtime resources remain owned by `App`.

use super::*;

impl App {
    pub(super) fn cluster_role_label_for_state(&self) -> Option<&'static str> {
        if !self.is_shared_mode_enabled() {
            return None;
        }

        if self.is_current_shared_leader() {
            Some("Leader")
        } else if self.is_current_shared_follower() {
            Some("Follower")
        } else {
            Some("Unknown")
        }
    }

    pub(super) fn sync_cluster_role_label(&mut self) {
        self.app_state.cluster_role_label = self.cluster_role_label_for_state().map(str::to_string);
        self.app_state.cluster_runtime_label = if self.is_current_shared_follower() {
            Some("Reader".to_string())
        } else {
            None
        };
    }

    pub(super) fn should_suppress_follower_runtime_for_torrent(
        &self,
        torrent: &TorrentSettings,
    ) -> bool {
        self.is_current_shared_follower() && !torrent.validation_status
    }

    pub(super) fn display_state_from_torrent_settings(
        &self,
        torrent: &TorrentSettings,
    ) -> Option<TorrentDisplayState> {
        let info_hash = info_hash_from_torrent_source(&torrent.torrent_or_magnet)?;
        Some(TorrentDisplayState {
            latest_state: TorrentMetrics {
                torrent_control_state: torrent.torrent_control_state.clone(),
                delete_files: torrent.delete_files,
                info_hash,
                torrent_or_magnet: torrent.torrent_or_magnet.clone(),
                torrent_name: torrent.name.clone(),
                download_path: torrent
                    .download_path
                    .clone()
                    .or_else(|| self.client_configs.default_download_folder.clone()),
                container_name: torrent.container_name.clone(),
                file_priorities: torrent.file_priorities.clone(),
                is_complete: torrent.validation_status,
                activity_message: "Reader mode waiting for leader status".to_string(),
                ..Default::default()
            },
            added_at_unix_secs: torrent.added_at_unix_secs,
            ..Default::default()
        })
    }

    pub(super) fn ensure_display_only_torrent_from_settings(&mut self, torrent: &TorrentSettings) {
        let Some(display_state) = self.display_state_from_torrent_settings(torrent) else {
            return;
        };
        let info_hash = display_state.latest_state.info_hash.clone();
        if !self.app_state.torrents.contains_key(&info_hash) {
            self.app_state
                .torrents
                .insert(info_hash.clone(), display_state);
            self.app_state.torrent_list_order.push(info_hash);
            self.refresh_rss_derived();
        }
    }

    pub(super) fn apply_leader_snapshot_to_display(&mut self, snapshot: &AppOutputState) {
        let configured_torrents = self.client_configs.torrents.clone();
        for torrent in &configured_torrents {
            let Some(info_hash) = info_hash_from_torrent_source(&torrent.torrent_or_magnet) else {
                continue;
            };

            if !self.app_state.torrents.contains_key(&info_hash) {
                self.ensure_display_only_torrent_from_settings(torrent);
            }

            let has_live_runtime = self.has_live_runtime_for_torrent(&info_hash);
            let Some(runtime) = self.app_state.torrents.get_mut(&info_hash) else {
                continue;
            };
            let Some(leader_metrics) = snapshot.torrents.get(&info_hash) else {
                if !has_live_runtime {
                    runtime.latest_state.activity_message =
                        "Leader runtime unavailable".to_string();
                    runtime.latest_state.download_speed_bps = 0;
                    runtime.latest_state.upload_speed_bps = 0;
                    runtime.latest_state.bytes_downloaded_this_tick = 0;
                    runtime.latest_state.bytes_uploaded_this_tick = 0;
                }
                continue;
            };

            let keep_local_seed_runtime = has_live_runtime && runtime.latest_state.is_complete;
            if !keep_local_seed_runtime {
                runtime.latest_state = leader_metrics.clone();
            }
        }

        self.sort_and_filter_torrent_list();
        self.app_state.ui.needs_redraw = true;
    }

    pub(super) fn refresh_follower_read_model(&mut self) {
        if !self.is_current_shared_follower() {
            return;
        }

        for torrent in self.client_configs.torrents.clone() {
            if self.should_suppress_follower_runtime_for_torrent(&torrent) {
                self.ensure_display_only_torrent_from_settings(&torrent);
            }
        }

        match status::read_cluster_output_state() {
            Ok(snapshot) => {
                self.leader_status_snapshot = Some(snapshot.clone());
                self.apply_leader_snapshot_to_display(&snapshot);
            }
            Err(error) => {
                tracing_event!(
                    Level::DEBUG,
                    "Follower could not read leader status snapshot yet: {}",
                    error
                );
                self.leader_status_snapshot = None;
            }
        }
    }

    pub fn is_shared_mode_enabled(&self) -> bool {
        self.shared_mode_enabled
    }

    pub fn is_current_shared_leader(&self) -> bool {
        matches!(self.current_cluster_role, Some(AppClusterRole::Leader))
    }

    pub(super) fn refresh_shared_recovery_backup_on_interval(&self) {
        let Some(tx) = &self.shared_recovery_backup_tx else {
            return;
        };
        match tx.try_send(()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
            Err(mpsc::error::TrySendError::Closed(())) => {
                tracing_event!(
                    Level::WARN,
                    "Scheduled shared config recovery backup worker is unavailable"
                );
            }
        }
    }

    pub fn is_current_shared_follower(&self) -> bool {
        self.is_shared_mode_enabled()
            && matches!(self.current_cluster_role, Some(AppClusterRole::Follower))
    }

    pub(super) fn cluster_capabilities(&self) -> ClusterCapabilities {
        capabilities_for_cluster_role(
            self.is_shared_mode_enabled(),
            self.current_cluster_role,
            self.app_state.capabilities,
        )
    }

    pub(super) fn can_run_leader_services(&self) -> bool {
        self.cluster_capabilities().can_consume_shared_inbox
    }

    pub(super) fn can_write_shared_state(&self) -> bool {
        self.cluster_capabilities().can_write_shared_state
    }

    pub(super) fn ensure_leader_services_running(&mut self) {
        if !self.can_run_leader_services() {
            return;
        }

        #[cfg(test)]
        let persistence_writer_enabled = test_persistence_writer_enabled();
        #[cfg(not(test))]
        let persistence_writer_enabled = true;
        if self.persistence_tx.is_none() && persistence_writer_enabled {
            let (tx, task) =
                spawn_persistence_writer(self.app_command_tx.clone(), self.app_persistence.clone());
            self.persistence_tx = Some(tx);
            self.persistence_task = Some(task);
        }

        if self.rss_service_task.is_none() {
            let Some(sync_now_rx) = self.rss_sync_rx.take() else {
                return;
            };
            let Some(downloaded_entry_rx) = self.rss_downloaded_entry_rx.take() else {
                return;
            };
            let Some(settings_rx) = self.rss_settings_rx.take() else {
                return;
            };
            self.rss_service_task = Some(rss_service::spawn_rss_service(
                self.network_activation.clone(),
                self.client_configs.clone(),
                self.app_state.rss_runtime.history.clone(),
                self.app_command_tx.clone(),
                sync_now_rx,
                downloaded_entry_rx,
                settings_rx,
                self.shutdown_tx.clone(),
            ));
        }
    }

    pub(super) fn current_shared_lock_path() -> io::Result<PathBuf> {
        shared_root_path()
            .map(|root| root.join("superseedr.lock"))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Shared lock path unavailable"))
    }

    pub(super) fn try_acquire_shared_runtime_lock() -> io::Result<Option<File>> {
        let lock_path = Self::current_shared_lock_path()?;
        let file = File::create(lock_path)?;
        if file.try_lock().is_ok() {
            Ok(Some(file))
        } else {
            Ok(None)
        }
    }

    pub(super) async fn maybe_promote_to_shared_leader(&mut self) {
        if !self.is_current_shared_follower() {
            return;
        }

        let Ok(Some(lock_handle)) = Self::try_acquire_shared_runtime_lock() else {
            return;
        };

        tracing_event!(
            Level::INFO,
            "Acquired shared lock; promoting node to cluster leader."
        );
        self.app_lock_handle = Some(lock_handle);
        self.current_cluster_role = Some(AppClusterRole::Leader);
        self.runtime_mode = AppRuntimeMode::SharedLeader;
        self.leader_status_snapshot = None;
        self.sync_cluster_role_label();

        if let Some(shared_inbox) = shared_inbox_path() {
            if let Err(error) = self.watch_path_if_needed(shared_inbox) {
                tracing_event!(
                    Level::WARN,
                    "Failed to watch shared inbox after promotion: {}",
                    error
                );
            }
        }

        self.ensure_leader_services_running();

        match self.app_persistence.load_settings() {
            Ok(new_settings) => {
                self.apply_reloaded_settings(new_settings).await;
                self.start_missing_runtime_torrents_for_current_role().await;
            }
            Err(error) => {
                tracing_event!(
                    Level::ERROR,
                    "Failed to reload shared config after promotion: {}",
                    error
                );
                self.app_state.system_error = Some(format!(
                    "Failed to reload shared config after promotion: {}",
                    error
                ));
            }
        }

        self.process_pending_commands().await;
    }
}
