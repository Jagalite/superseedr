// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser host managers; shared app transitions retain application policy.

use super::*;

impl BrowserSession {
    /// Physical service work is exposed to the real browser composition root.
    /// Demo composition deliberately uses ephemeral/simulated services instead.
    pub fn drain_effects(&mut self) -> Vec<crate::app::AppEffect> {
        let mut effects: Vec<_> = self.pending_app_effects.drain(..).collect();
        if std::mem::take(&mut self.checkpoint_requested) {
            effects.push(crate::app::AppEffect::CheckpointRequested);
        }
        effects
    }

    pub(super) fn execute_app_effect(&mut self, effect: crate::app::AppEffect) {
        match effect {
            crate::app::AppEffect::RefreshRss => {
                rss::recompute_rss_derived(&mut self.app_state, &self.client_configs)
            }
            crate::app::AppEffect::CheckpointRequested => self.checkpoint_requested = true,
            other => {
                if !self.app_state.capabilities.demo {
                    self.pending_app_effects.push_back(other);
                }
            }
        }
    }

    pub(super) fn observe_service(&mut self, observation: crate::app::reducer::ServiceObservation) {
        for effect in crate::app::reduce_app_action(
            &mut self.app_state,
            crate::app::AppAction::ServiceObserved(observation),
        ) {
            self.execute_app_effect(effect);
        }
    }

    pub fn drain_commands(&mut self) -> Vec<BrowserCommand> {
        self.pending_browser_commands.drain(..).collect()
    }

    pub(crate) fn enqueue_command(&mut self, command: BrowserCommand) {
        if self.pending_browser_commands.len() >= 1000 {
            self.set_browser_error(
                "Browser operation queue is full; retry after pending work finishes.",
            );
            return;
        }
        if !self.app_state.capabilities.demo
            && !self.app_state.capabilities.rss
            && matches!(
                command,
                BrowserCommand::RssSyncNow | BrowserCommand::RssDownloadPreview { .. }
            )
        {
            self.set_browser_error("RSS is unavailable in this browser host.");
            return;
        }
        self.pending_browser_commands.push_back(command);
    }

    /// Registers a browser-owned torrent manager behind the same command,
    /// metrics, and event channels used by the native runtime.
    pub fn register_torrent_manager(
        &mut self,
        info_hash: Vec<u8>,
    ) -> Result<BrowserTorrentManagerEndpoint, &'static str> {
        self.register_torrent_manager_with_metrics(TorrentMetrics {
            info_hash,
            ..TorrentMetrics::default()
        })
    }

    pub fn register_torrent_manager_with_metrics(
        &mut self,
        initial_metrics: TorrentMetrics,
    ) -> Result<BrowserTorrentManagerEndpoint, &'static str> {
        if self.app_state.lifecycle.phase != crate::app::AppPhase::Running {
            return Err("The application is shutting down; manager registration is closed.");
        }
        let info_hash = initial_metrics.info_hash.clone();
        let (command_tx, command_rx) = mpsc::channel(100);
        let (metrics_tx, metrics_rx) = watch::channel(initial_metrics);
        if self.manager_data_rate_ms != DataRate::Rate60s.as_ms() {
            let _ = command_tx.try_send(ManagerCommand::SetDataRate(self.manager_data_rate_ms));
        }
        self.torrent_manager_command_txs
            .insert(info_hash.clone(), command_tx);
        let lifetime = ManagerLifetime::new();
        let source = lifetime.source();
        self.manager_lifetimes.insert(info_hash.clone(), lifetime);
        self.torrent_metric_watch_rxs.insert(info_hash, metrics_rx);
        Ok(BrowserTorrentManagerEndpoint {
            source,
            command_rx,
            metrics_tx,
            manager_event_tx: self.manager_event_tx.clone(),
            telemetry_batch_tx: self.telemetry_batch_tx.clone(),
        })
    }

    pub(crate) fn send_manager_command(
        &mut self,
        info_hash: &[u8],
        command: ManagerCommand,
    ) -> bool {
        let result = self
            .torrent_manager_command_txs
            .get(info_hash)
            .ok_or("Torrent manager is unavailable.")
            .and_then(|sender| {
                sender.try_send(command).map_err(|error| match error {
                    mpsc::error::TrySendError::Full(_) => {
                        "Torrent manager command queue is full; retry the operation."
                    }
                    mpsc::error::TrySendError::Closed(_) => {
                        "Torrent manager command channel is closed."
                    }
                })
            });
        if let Err(message) = result {
            self.set_browser_error(message);
            false
        } else {
            true
        }
    }

    pub(crate) fn broadcast_manager_data_rate(&mut self, rate_ms: u64) {
        self.manager_data_rate_ms = rate_ms;
        for sender in self.torrent_manager_command_txs.values() {
            let _ = sender.try_send(ManagerCommand::SetDataRate(rate_ms));
        }
    }

    /// Drains manager output through the production telemetry reducer.
    pub fn drain_manager_messages(&mut self) {
        let mut changed = false;
        while let Ok((source, batch)) = self.telemetry_batch_rx.try_recv() {
            if !source.is_current() {
                continue;
            }
            changed = true;
            self.apply_telemetry_batch(&batch);
        }
        while self.pending_app_effects.len() < 1000 {
            let Ok(observation) = self.manager_event_rx.try_recv() else {
                break;
            };
            if !observation.source.is_current() {
                continue;
            }
            let event = observation.event;
            if self.app_state.lifecycle.phase == crate::app::AppPhase::Stopping {
                if let ManagerEvent::DeletionComplete(info_hash, result) = event {
                    self.app_state.lifecycle.manager_stopped(&info_hash, result);
                    self.release_torrent_runtime(&info_hash, false);
                    self.finish_shutdown_if_ready();
                    continue;
                }
            }
            changed = true;
            let effects = crate::app::reduce_app_action(
                &mut self.app_state,
                crate::app::AppAction::ManagerEvent(event),
            );
            for effect in effects {
                match effect {
                    crate::app::AppEffect::TorrentRemoved {
                        info_hash,
                        result,
                        was_present,
                        recovery,
                    } => {
                        crate::app::reducer::reconcile_removed_catalog(
                            &mut self.client_configs,
                            &info_hash,
                            &result,
                            true,
                            recovery,
                        );
                        self.pending_catalog_restores.remove(&info_hash);
                        if result.is_ok() {
                            self.forget_catalog_entry(&info_hash);
                        }
                        self.checkpoint_requested = true;
                        self.release_torrent_runtime(&info_hash, was_present);
                    }
                    other => self.execute_app_effect(other),
                }
            }
        }

        let selected_hash = self
            .selected_torrent_hash_hex()
            .and_then(|hash| hex::decode(hash).ok());
        let batch_started_with_incomplete_torrents =
            crate::app::has_effectively_incomplete_torrents(&self.app_state);
        let mut closed = Vec::new();
        let mut completion_events = Vec::new();
        for (info_hash, receiver) in &mut self.torrent_metric_watch_rxs {
            match receiver.has_changed() {
                Ok(false) => {}
                Ok(true) => {
                    let metrics = receiver.borrow_and_update().clone();
                    self.pending_catalog_restores.remove(info_hash);
                    let selected = selected_hash.as_ref() == Some(info_hash);
                    let previous_peer_rates = selected.then(|| {
                        self.app_state
                            .torrents
                            .get(info_hash)
                            .map(|torrent| {
                                torrent
                                    .latest_state
                                    .peers
                                    .iter()
                                    .map(|peer| {
                                        (
                                            peer.address.clone(),
                                            peer.download_speed_bps,
                                            peer.upload_speed_bps,
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default()
                    });
                    let control_state = metrics.torrent_control_state.clone();
                    let delete_files = metrics.delete_files;
                    let torrent_or_magnet = metrics.torrent_or_magnet.clone();
                    let is_multi_file = metrics.is_multi_file;
                    let file_priorities = metrics.file_priorities.clone();
                    let effects = crate::app::reduce_app_action(
                        &mut self.app_state,
                        crate::app::AppAction::ManagerMetrics(Box::new(metrics)),
                    );
                    for effect in effects {
                        if let crate::app::AppEffect::TorrentCompleted {
                            info_hash,
                            torrent_name,
                        } = effect
                        {
                            completion_events.push((info_hash, torrent_name));
                        }
                    }
                    if let Some(display) = self.app_state.torrents.get_mut(info_hash) {
                        display.latest_state.torrent_control_state = control_state;
                        display.latest_state.delete_files = delete_files;
                        if !torrent_or_magnet.is_empty() {
                            display.latest_state.torrent_or_magnet = torrent_or_magnet;
                        }
                        display.latest_state.is_multi_file = is_multi_file;
                        display.latest_state.file_priorities = file_priorities.clone();
                        if !display.file_preview_tree.is_empty() {
                            crate::app::apply_torrent_preview_file_priorities(
                                &mut display.file_preview_tree,
                                &file_priorities,
                            );
                        }
                        if let Some(previous) = previous_peer_rates {
                            let peer_rate_changed = display.latest_state.peers.iter().any(|peer| {
                                previous.iter().any(|(address, download, upload)| {
                                    address == &peer.address
                                        && (*download != peer.download_speed_bps
                                            || *upload != peer.upload_speed_bps)
                                })
                            });
                            self.browser_selected_peer_rate_frame_updates = self
                                .browser_selected_peer_rate_frame_updates
                                .saturating_add(1);
                            self.browser_selected_peer_rate_frame_changes = self
                                .browser_selected_peer_rate_frame_changes
                                .saturating_add(u64::from(peer_rate_changed));
                        }
                    }
                    changed = true;
                }
                Err(_) => closed.push(info_hash.clone()),
            }
        }
        for info_hash in closed {
            self.torrent_metric_watch_rxs.remove(&info_hash);
        }
        let select_upload_priority_if_all_complete =
            batch_started_with_incomplete_torrents && !completion_events.is_empty();
        for (info_hash, torrent_name) in completion_events {
            self.record_torrent_completed_event(&info_hash, torrent_name);
        }
        if changed {
            crate::app::finalize_manager_metrics_batch(
                &mut self.app_state,
                select_upload_priority_if_all_complete,
            );
        }
        self.retry_pending_shutdowns();
    }

    pub fn apply_browser_torrent_config(
        &mut self,
        info_hash_hex: &str,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        file_priorities: &[BrowserFilePriorityOverride],
    ) -> bool {
        let Ok(info_hash) = hex::decode(info_hash_hex) else {
            return false;
        };
        let priorities = file_priorities
            .iter()
            .map(|value| {
                let priority = match value.priority {
                    BrowserFilePriority::High => FilePriority::High,
                    BrowserFilePriority::Skip => FilePriority::Skip,
                };
                (value.file_index, priority)
            })
            .collect();
        crate::app::apply_torrent_configuration(
            &mut self.app_state,
            &info_hash,
            download_path,
            container_name,
            priorities,
        )
    }

    pub fn upsert_browser_torrent(&mut self, update: BrowserTorrentUpdate) {
        let batch_started_with_incomplete_torrents =
            crate::app::has_effectively_incomplete_torrents(&self.app_state);
        let info_hash = update.info_hash.clone();
        self.pending_catalog_restores.remove(&info_hash);
        let effects = crate::app::reduce_app_action(
            &mut self.app_state,
            crate::app::AppAction::ManagerMetrics(Box::new(update.clone().into_torrent_metrics())),
        );

        let display = self
            .app_state
            .torrents
            .get_mut(&info_hash)
            .expect("production telemetry inserted the browser torrent");
        display.latest_state.torrent_or_magnet = update.torrent_or_magnet;
        display.latest_state.torrent_control_state = match update.control_state {
            BrowserTorrentControlState::Running => TorrentControlState::Running,
            BrowserTorrentControlState::Paused => TorrentControlState::Paused,
            BrowserTorrentControlState::Deleting => TorrentControlState::Deleting,
        };
        display.latest_state.bytes_downloaded_this_tick = update.bytes_downloaded_this_tick;
        display.latest_state.bytes_uploaded_this_tick = update.bytes_uploaded_this_tick;
        display.latest_state.is_multi_file = update.files.len() > 1;
        display.file_preview_tree = build_torrent_preview_tree(
            update
                .files
                .iter()
                .map(|file| {
                    (
                        file.relative_path
                            .split('/')
                            .filter(|segment| !segment.is_empty())
                            .map(str::to_string)
                            .collect(),
                        file.size,
                    )
                })
                .collect(),
            &display.latest_state.file_priorities,
        );
        if update.download_history.len() > display.download_history.len() {
            display.download_history = update.download_history;
            display.upload_history = update.upload_history;
        }
        if display.latest_state.blocks_in_history.is_empty() {
            display.latest_state.blocks_in_history = update.blocks_in_history;
            display.latest_state.blocks_out_history = update.blocks_out_history;
        }
        if display.peer_discovery_history.is_empty() {
            display.peer_discovery_history = update.peer_discovery_history;
            display.peer_connection_history = update.peer_connection_history;
            display.peer_disconnect_history = update.peer_disconnect_history;
        }
        let select_upload_priority_if_all_complete = batch_started_with_incomplete_torrents
            && effects
                .iter()
                .any(|effect| matches!(effect, crate::app::AppEffect::TorrentCompleted { .. }));
        crate::app::finalize_manager_metrics_batch(
            &mut self.app_state,
            select_upload_priority_if_all_complete,
        );
        for effect in effects {
            if let crate::app::AppEffect::TorrentCompleted {
                info_hash,
                torrent_name,
            } = effect
            {
                self.record_torrent_completed_event(&info_hash, torrent_name);
            }
        }
    }

    pub fn note_torrent_added(&mut self) {
        self.checkpoint_requested = true;
        let _ =
            crate::app::reduce_app_action(&mut self.app_state, crate::app::AppAction::TorrentAdded);
    }

    pub fn initialize_torrent_sort_for_current_lifecycle(&mut self) {
        crate::app::reset_torrent_sort_for_current_lifecycle(&mut self.app_state);
    }

    pub fn set_torrent_paused_hex(&mut self, info_hash_hex: &str, paused: bool) -> bool {
        let Ok(info_hash) = hex::decode(info_hash_hex) else {
            return false;
        };
        let Some(torrent) = self.app_state.torrents.get_mut(&info_hash) else {
            return false;
        };
        torrent.latest_state.torrent_control_state = if paused {
            TorrentControlState::Paused
        } else {
            TorrentControlState::Running
        };
        self.app_state.ui.needs_redraw = true;
        true
    }

    pub fn remove_torrent_hex(&mut self, info_hash_hex: &str) -> bool {
        let Ok(info_hash) = hex::decode(info_hash_hex) else {
            return false;
        };
        self.remove_torrent(&info_hash)
    }

    pub(super) fn remove_torrent(&mut self, info_hash: &[u8]) -> bool {
        let removed = remove_torrent_from_state(&mut self.app_state, info_hash);
        self.forget_catalog_entry(info_hash);
        self.release_torrent_runtime(info_hash, removed);
        removed
    }

    fn forget_catalog_entry(&mut self, info_hash: &[u8]) {
        self.pending_catalog_restores.remove(info_hash);
        self.app_state.cleanup_failures.remove(info_hash);
        self.client_configs.torrents.retain(|torrent| {
            crate::torrent_identity::info_hash_from_torrent_source(&torrent.torrent_or_magnet)
                .as_deref()
                != Some(info_hash)
        });
        self.checkpoint_requested = true;
    }

    pub(super) fn release_torrent_runtime(&mut self, info_hash: &[u8], removed: bool) {
        self.unsent_shutdowns.remove(info_hash);
        self.manager_lifetimes.remove(info_hash);
        if removed {
            self.checkpoint_requested = true;
        }
        self.torrent_manager_command_txs.remove(info_hash);
        self.torrent_metric_watch_rxs.remove(info_hash);
        self.browser_tracked_peers
            .retain(|(torrent_hash, _), _| torrent_hash.as_slice() != info_hash);
        if removed {
            let mut peer_view = (*self.app_state.peer_manager_view).clone();
            peer_view
                .tracked_peers
                .retain(|peer| peer.torrent_info_hash.as_slice() != info_hash);
            peer_view.registered_torrents = self.app_state.torrents.len();
            self.app_state.peer_manager_view = Arc::new(peer_view);
            peers::recompute_peer_management_derived(
                &mut self.app_state,
                web_time::SystemTime::now(),
            );
        }
    }
}
