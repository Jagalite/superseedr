// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native torrent runtime execution. Runtime resources remain owned by `App`.

use super::*;

impl App {
    pub(super) fn register_manager_event_source(
        &mut self,
        info_hash: &[u8],
    ) -> mpsc::Sender<ManagerEvent> {
        let lifetime = ManagerLifetime::new();
        let source = lifetime.source();
        self.manager_lifetimes.insert(info_hash.to_vec(), lifetime);
        let (sender, mut receiver) = mpsc::channel(100);
        let app_sender = self.manager_event_tx.clone();
        self.background_tasks.spawn(async move {
            while let Some(event) = receiver.recv().await {
                if source.is_current()
                    && app_sender
                        .send(ManagerObservation {
                            source: source.clone(),
                            event,
                        })
                        .await
                        .is_err()
                {
                    break;
                }
            }
        });
        sender
    }

    pub(super) fn remove_torrent_runtime(&mut self, info_hash: &[u8]) {
        self.manager_lifetimes.remove(info_hash);
        remove_torrent_from_state(&mut self.app_state, info_hash);
        self.startup_completion_suppressed_hashes.remove(info_hash);
        self.torrent_manager_command_txs.remove(info_hash);
        self.torrent_manager_incoming_peer_txs.remove(info_hash);
        self.torrent_metric_watch_rxs.remove(info_hash);
        let _ = self
            .peer_manager
            .handle()
            .unregister_torrent(info_hash.to_vec());
        self.integrity_scheduler.remove_torrent(info_hash);
        self.refresh_rss_derived();
        self.dispatch_integrity_probe_batches();
    }

    pub(crate) fn cleanup_pending_magnet_preview_runtime(&mut self) {
        let Some(info_hash) = self.app_state.pending_magnet_preview_info_hash.take() else {
            return;
        };

        self.cleanup_pending_magnet_preview_runtime_for(info_hash);
    }

    pub(crate) fn cleanup_pending_magnet_preview_runtime_for(&mut self, info_hash: Vec<u8>) {
        if let Some(manager_tx) = self.torrent_manager_command_txs.get(&info_hash).cloned() {
            let mut shutdown_rx = self.shutdown_tx.subscribe();
            self.background_tasks.spawn(async move {
                tokio::select! {
                    result = manager_tx.send(ManagerCommand::Shutdown) => {
                        if let Err(error) = result {
                            tracing::error!("Failed to send Shutdown to cancelled preview manager: {}", error);
                        }
                    }
                    shutdown = shutdown_rx.recv() => {
                        match shutdown {
                            Ok(())
                            | Err(broadcast::error::RecvError::Closed)
                            | Err(broadcast::error::RecvError::Lagged(_)) => {}
                        }
                    }
                }
            });
        }

        self.remove_torrent_runtime(&info_hash);
        self.save_state_to_disk();
        self.app_state.ui.needs_redraw = true;
    }

    pub(super) async fn load_runtime_torrent_from_settings(
        &mut self,
        torrent_config: TorrentSettings,
    ) -> bool {
        if !should_load_persisted_torrent(&torrent_config) {
            tracing_event!(
                Level::WARN,
                torrent = %torrent_config.torrent_or_magnet,
                "Skipping persisted torrent left in transient Deleting state during startup or convergence"
            );
            return false;
        }

        tracing_event!(
            Level::DEBUG,
            torrent = %torrent_config.torrent_or_magnet,
            torrent_name = %torrent_config.name,
            validation_status = torrent_config.validation_status,
            "Restoring persisted torrent into runtime"
        );
        if torrent_config.validation_status {
            if let Some(info_hash) =
                info_hash_from_torrent_source(&torrent_config.torrent_or_magnet)
            {
                self.startup_completion_suppressed_hashes.insert(info_hash);
            }
        }

        if self.should_suppress_follower_runtime_for_torrent(&torrent_config) {
            self.ensure_display_only_torrent_from_settings(&torrent_config);
            return true;
        }

        let restored_torrent_sort = self.app_state.torrent_sort;
        let restored_torrent_sort_pinned = self.app_state.torrent_sort_pinned;
        let ingest_result = if torrent_config.torrent_or_magnet.starts_with("magnet:") {
            self.add_magnet_torrent(
                torrent_config.name.clone(),
                torrent_config.torrent_or_magnet.clone(),
                torrent_config.download_path.clone(),
                torrent_config.validation_status,
                torrent_config.torrent_control_state.clone(),
                torrent_config.file_priorities.clone(),
                torrent_config.container_name.clone(),
            )
            .await
        } else {
            self.add_torrent_from_file(
                PathBuf::from(&torrent_config.torrent_or_magnet),
                torrent_config.download_path.clone(),
                torrent_config.validation_status,
                torrent_config.torrent_control_state.clone(),
                torrent_config.file_priorities.clone(),
                torrent_config.container_name.clone(),
            )
            .await
        };

        let restored = matches!(
            ingest_result,
            CommandIngestResult::Added { .. } | CommandIngestResult::Duplicate { .. }
        );
        if restored {
            self.app_state.torrent_sort = restored_torrent_sort;
            self.app_state.torrent_sort_pinned = restored_torrent_sort_pinned;
            sort_and_filter_torrent_list_state(&mut self.app_state);
            preserve_restored_added_at(&mut self.app_state, &torrent_config);
        }
        restored
    }

    pub(super) async fn sync_runtime_torrents_from_settings(
        &mut self,
        old_settings: &Settings,
        new_settings: &Settings,
    ) -> bool {
        let mut all_delivered = true;
        let follower = self.is_current_shared_follower();
        let effects = reconcile_catalog(&mut self.app_state, old_settings, new_settings, follower);
        for effect in effects {
            match effect {
                CatalogEffect::Configure {
                    info_hash,
                    commands,
                } => {
                    if let Some(manager_tx) = self.torrent_manager_command_txs.get(&info_hash) {
                        for command in commands {
                            all_delivered &= self
                                .send_manager_command_until_shutdown(manager_tx, command)
                                .await;
                        }
                    }
                }
                CatalogEffect::ReaderOnly(torrent) => {
                    if let Some(info_hash) =
                        info_hash_from_torrent_source(&torrent.torrent_or_magnet)
                    {
                        if let Some(manager_tx) = self.torrent_manager_command_txs.get(&info_hash) {
                            all_delivered &= self
                                .send_manager_command_until_shutdown(
                                    manager_tx,
                                    ManagerCommand::Shutdown,
                                )
                                .await;
                        }
                    }
                    self.ensure_display_only_torrent_from_settings(&torrent);
                }
                CatalogEffect::Stop(info_hash) => {
                    if let Some(manager_tx) = self.torrent_manager_command_txs.get(&info_hash) {
                        all_delivered &= self
                            .send_manager_command_until_shutdown(
                                manager_tx,
                                ManagerCommand::Shutdown,
                            )
                            .await;
                        if let Some(runtime) = self.app_state.torrents.get_mut(&info_hash) {
                            runtime.latest_state.torrent_control_state =
                                TorrentControlState::Deleting;
                            runtime.latest_state.delete_files = false;
                        }
                    } else {
                        self.remove_torrent_runtime(&info_hash);
                    }
                }
                CatalogEffect::Restore(torrent) => {
                    self.load_runtime_torrent_from_settings(torrent).await;
                }
            }
        }

        if self.is_current_shared_follower() {
            self.refresh_follower_read_model();
        }
        all_delivered
    }

    pub(super) async fn send_manager_command_until_shutdown(
        &self,
        manager_tx: &mpsc::Sender<ManagerCommand>,
        command: ManagerCommand,
    ) -> bool {
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        tokio::select! {
            result = manager_tx.send(command) => {
                if result.is_err() {
                    tracing_event!(Level::WARN, "Torrent manager command channel closed");
                    return false;
                }
                true
            }
            _ = shutdown_rx.recv() => false,
        }
    }

    pub(super) fn drain_latest_torrent_metrics(&mut self) {
        let mut changed = false;
        let batch_started_with_incomplete_torrents =
            has_effectively_incomplete_torrents(&self.app_state);
        let mut closed_info_hashes = Vec::new();
        let mut completion_events: Vec<(Vec<u8>, String)> = Vec::new();

        for (info_hash, rx) in self.torrent_metric_watch_rxs.iter_mut() {
            match rx.has_changed() {
                Ok(false) => {}
                Ok(true) => {
                    let message = rx.borrow_and_update().clone();
                    for effect in reduce_app_action(
                        &mut self.app_state,
                        AppAction::ManagerMetrics(Box::new(message)),
                    ) {
                        if let AppEffect::TorrentCompleted {
                            info_hash,
                            torrent_name,
                        } = effect
                        {
                            completion_events.push((info_hash, torrent_name));
                        }
                    }
                    changed = true;
                }
                Err(_) => {
                    closed_info_hashes.push(info_hash.clone());
                }
            }
        }

        for info_hash in closed_info_hashes {
            self.torrent_metric_watch_rxs.remove(&info_hash);
            let _ = self.peer_manager.handle().unregister_torrent(info_hash);
        }

        let select_upload_priority_if_all_complete = batch_started_with_incomplete_torrents
            && completion_events.iter().any(|(info_hash, _)| {
                !self
                    .startup_completion_suppressed_hashes
                    .contains(info_hash)
            });
        if changed {
            // Keep RSS derived recomputation off the hot metrics path.
            // Full recompute is done on structural RSS changes (preview/filter/history/add/remove/search/edit).
            finalize_manager_metrics_batch(
                &mut self.app_state,
                select_upload_priority_if_all_complete,
            );
        }

        if !completion_events.is_empty() {
            for (info_hash, torrent_name) in completion_events {
                self.record_torrent_completed_event(&info_hash, Some(torrent_name));
            }
            self.save_state_to_disk();
        }
    }

    pub async fn add_torrent_from_file(
        &mut self,
        path: PathBuf,
        download_path: Option<PathBuf>,
        is_validated: bool,
        torrent_control_state: TorrentControlState,
        file_priorities: HashMap<usize, FilePriority>,
        container_name: Option<String>,
    ) -> CommandIngestResult {
        let buffer = match fs::read(&path) {
            Ok(buf) => buf,
            Err(e) => {
                let message =
                    format_filesystem_path_error("Failed to read torrent file", &path, &e);
                tracing_event!(Level::ERROR, "{}", message);
                return CommandIngestResult::Failed {
                    info_hash: None,
                    torrent_name: None,
                    message,
                };
            }
        };

        let torrent = match from_bytes(&buffer) {
            Ok(t) => t,
            Err(e) => {
                let file_size = buffer.len();
                let head_len = file_size.min(24);
                let tail_len = file_size.min(24);
                let head_hex = hex::encode(&buffer[..head_len]);
                let tail_hex = hex::encode(&buffer[file_size.saturating_sub(tail_len)..]);
                let likely_cause = if e.to_string().contains("End of stream") {
                    "likely truncated/incomplete .torrent file"
                } else {
                    "malformed or unsupported bencode payload"
                };
                let message = format!(
                    "Failed to parse torrent file {:?}: {} | size={} bytes | head={} | tail={} | hint={}",
                    &path, e, file_size, head_hex, tail_hex, likely_cause
                );
                tracing_event!(Level::ERROR, "{}", message);
                return CommandIngestResult::Invalid {
                    info_hash: None,
                    torrent_name: None,
                    message,
                };
            }
        };

        #[cfg(all(feature = "dht", feature = "pex"))]
        {
            if torrent.info.private == Some(1) {
                let message = format!(
                    "Rejected private torrent '{}' in normal build.",
                    torrent.info.name
                );
                tracing_event!(Level::ERROR, "{}", message);
                self.app_state.system_error = Some(format!(
                    "Private Torrent Rejected:'{}' This build (with DHT/PEX) is not safe for private trackers. Please use private builds for this torrent.",
                    torrent.info.name
                ));
                return CommandIngestResult::Failed {
                    info_hash: None,
                    torrent_name: Some(torrent.info.name.clone()),
                    message,
                };
            }
        }

        let info_hash = if torrent.info.meta_version == Some(2) {
            if !torrent.info.pieces.is_empty() {
                let mut hasher = sha1::Sha1::new();
                hasher.update(&torrent.info_dict_bencode);
                hasher.finalize().to_vec()
            } else {
                // Pure V2 -> Primary is V2 (SHA-256 Truncated)
                let mut hasher = Sha256::new();
                hasher.update(&torrent.info_dict_bencode);
                hasher.finalize()[0..20].to_vec()
            }
        } else {
            // V1 -> SHA-1
            let mut hasher = sha1::Sha1::new();
            hasher.update(&torrent.info_dict_bencode);
            hasher.finalize().to_vec()
        };

        if self.app_state.torrents.contains_key(&info_hash) {
            if !self.has_live_runtime_for_torrent(&info_hash) {
                self.clear_display_only_torrent(&info_hash);
            } else {
                let should_apply_duplicate_config =
                    self.app_state.pending_magnet_preview_info_hash.as_deref()
                        == Some(info_hash.as_slice());
                let mut applied_duplicate_config = false;
                if should_apply_duplicate_config {
                    if let Some(path) = download_path {
                        applied_duplicate_config = true;
                        if let Some(display) = self.app_state.torrents.get_mut(&info_hash) {
                            display.latest_state.download_path = Some(path.clone());
                            display.latest_state.container_name = container_name.clone();
                            display.latest_state.file_priorities = file_priorities.clone();
                            apply_torrent_preview_file_priorities(
                                &mut display.file_preview_tree,
                                &file_priorities,
                            );
                        }
                        if let Some(manager_tx) = self.torrent_manager_command_txs.get(&info_hash) {
                            self.send_manager_command_until_shutdown(
                                manager_tx,
                                ManagerCommand::SetUserTorrentConfig {
                                    torrent_data_path: path,
                                    file_priorities: file_priorities.clone(),
                                    container_name,
                                },
                            )
                            .await;
                        }
                    }
                }
                let message = if applied_duplicate_config {
                    format!(
                        "Updated path for existing torrent from file: {}",
                        torrent.info.name
                    )
                } else {
                    format!("Ignoring already present torrent: {}", torrent.info.name)
                };
                tracing_event!(Level::INFO, "{}", message);
                return CommandIngestResult::Duplicate {
                    info_hash: Some(info_hash),
                    torrent_name: Some(torrent.info.name),
                };
            }
        }

        let torrent_files_dir = match crate::config::runtime_data_dir() {
            Some(data_dir) => data_dir.join("torrents"),
            None => {
                let message = "Could not determine application data directory.".to_string();
                tracing_event!(Level::ERROR, "{}", message);
                return CommandIngestResult::Failed {
                    info_hash: Some(info_hash),
                    torrent_name: Some(torrent.info.name.clone()),
                    message,
                };
            }
        };
        if let Err(e) = fs::create_dir_all(&torrent_files_dir) {
            let message = format!("Could not create torrents data directory: {}", e);
            tracing_event!(Level::ERROR, "{}", message);
            return CommandIngestResult::Failed {
                info_hash: Some(info_hash),
                torrent_name: Some(torrent.info.name.clone()),
                message,
            };
        }
        let permanent_torrent_path =
            torrent_files_dir.join(format!("{}.torrent", hex::encode(&info_hash)));
        let shared_torrent_path = crate::config::shared_torrent_file_path(&info_hash);

        let persist_torrent_copy = |destination: &PathBuf, label: &str| -> std::io::Result<()> {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }

            let temp_torrent_path =
                destination.with_extension(format!("torrent.{}.tmp", std::process::id()));
            fs::write(&temp_torrent_path, &buffer)?;
            if let Err(e) = fs::rename(&temp_torrent_path, destination) {
                if e.kind() == ErrorKind::AlreadyExists {
                    if let Err(remove_err) = fs::remove_file(destination) {
                        if remove_err.kind() != ErrorKind::NotFound {
                            let _ = fs::remove_file(&temp_torrent_path);
                            return Err(remove_err);
                        }
                    }
                    if let Err(retry_err) = fs::rename(&temp_torrent_path, destination) {
                        let _ = fs::remove_file(&temp_torrent_path);
                        return Err(retry_err);
                    }
                } else {
                    let _ = fs::remove_file(&temp_torrent_path);
                    return Err(e);
                }
            }

            tracing_event!(
                Level::DEBUG,
                "Persisted torrent file copy in {}: {:?}",
                label,
                destination
            );
            Ok(())
        };

        if let Err(e) = persist_torrent_copy(&permanent_torrent_path, "data directory") {
            let message = format!("Failed to persist torrent copy in data directory: {}", e);
            tracing_event!(Level::ERROR, "{}", message);
            return CommandIngestResult::Failed {
                info_hash: Some(info_hash),
                torrent_name: Some(torrent.info.name.clone()),
                message,
            };
        }

        if self.can_write_shared_state() {
            if let Some(shared_path) = &shared_torrent_path {
                if let Err(e) = persist_torrent_copy(shared_path, "shared config directory") {
                    let message = format!(
                        "Failed to persist torrent copy in shared config directory: {}",
                        e
                    );
                    tracing_event!(Level::ERROR, "{}", message);
                    return CommandIngestResult::Failed {
                        info_hash: Some(info_hash),
                        torrent_name: Some(torrent.info.name.clone()),
                        message,
                    };
                }
            }
        }

        self.persist_torrent_metadata_snapshot(&info_hash, &torrent, &file_priorities);
        let number_of_pieces_total = torrent_piece_count(&torrent);
        let added_at_unix_secs = current_unix_secs();

        let resolved_torrent_name = torrent.info.name.clone();
        let placeholder_state = TorrentDisplayState {
            latest_state: TorrentMetrics {
                torrent_control_state: torrent_control_state.clone(),
                delete_files: false,
                info_hash: info_hash.clone(),
                torrent_or_magnet: shared_torrent_path
                    .clone()
                    .unwrap_or_else(|| permanent_torrent_path.clone())
                    .to_string_lossy()
                    .to_string(),
                torrent_name: resolved_torrent_name.clone(),
                download_path: download_path.clone(),
                container_name: container_name.clone(),
                is_complete: is_validated,
                is_multi_file: !torrent.info.files.is_empty(),
                file_count: Some(torrent_file_count(&torrent)),
                number_of_pieces_total,
                file_priorities: file_priorities.clone(),
                ..Default::default()
            },
            added_at_unix_secs: Some(added_at_unix_secs),
            file_preview_tree: build_torrent_preview_tree(torrent.file_list(), &file_priorities),
            ..Default::default()
        };
        self.app_state
            .torrents
            .insert(info_hash.clone(), placeholder_state);
        self.app_state.torrent_list_order.push(info_hash.clone());
        self.refresh_rss_derived();

        if matches!(self.app_state.mode, AppMode::Welcome) {
            self.app_state.mode = AppMode::Normal;
        }

        let (incoming_peer_tx, incoming_peer_rx) =
            mpsc::channel::<crate::torrent_manager::IncomingPeerSession>(100);
        self.torrent_manager_incoming_peer_txs
            .insert(info_hash.clone(), incoming_peer_tx);
        let (manager_command_tx, manager_command_rx) = mpsc::channel::<ManagerCommand>(100);
        self.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_command_tx);

        let (torrent_metrics_tx, torrent_metrics_rx) = watch::channel(TorrentMetrics::default());
        self.torrent_metric_watch_rxs
            .insert(info_hash.clone(), torrent_metrics_rx.clone());
        let manager_event_tx_clone = self.register_manager_event_source(&info_hash);
        let resource_manager_clone = self.resource_manager.clone();
        let global_dl_bucket_clone = self.global_dl_bucket.clone();
        let global_ul_bucket_clone = self.global_ul_bucket.clone();

        let dht_handle = self.dht_service.handle();

        let torrent_params = TorrentParameters {
            network_activation: self.network_activation.clone(),
            dht_handle,
            incoming_peer_rx,
            metrics_tx: torrent_metrics_tx,
            peer_policy_rx: self.peer_manager.handle().subscribe_policy(),
            torrent_validation_status: is_validated,
            torrent_data_path: download_path,
            container_name: container_name.clone(),
            manager_command_rx,
            manager_event_tx: manager_event_tx_clone,
            settings: Arc::clone(&Arc::new(self.client_configs.clone())),
            resource_manager: resource_manager_clone,
            global_dl_bucket: global_dl_bucket_clone,
            global_ul_bucket: global_ul_bucket_clone,
            file_priorities: file_priorities.clone(),
        };
        let start_paused = torrent_control_state == TorrentControlState::Paused;
        let should_announce_on_add = torrent_control_state == TorrentControlState::Running
            && (self.app_state.externally_accessable_port_v4
                || self.app_state.externally_accessable_port_v6);

        match TorrentManager::from_torrent(
            torrent_params.with_payload(crate::persistence::Payload::native()),
            torrent,
        ) {
            Ok(torrent_manager) => {
                if !self
                    .peer_manager
                    .handle()
                    .register_torrent(info_hash.clone(), torrent_metrics_rx)
                {
                    tracing_event!(
                        Level::WARN,
                        info_hash = %hex::encode(&info_hash),
                        "Peer manager was unavailable while registering torrent metrics"
                    );
                }
                self.manager_tasks.spawn(async move {
                    let _ = torrent_manager.run(start_paused).await;
                });
                if should_announce_on_add {
                    self.announce_torrents_to_dht(std::iter::once(info_hash.clone()));
                }
                let _ = reduce_app_action(&mut self.app_state, AppAction::TorrentAdded);
                tracing_event!(
                    Level::INFO,
                    info_hash = %hex::encode(&info_hash),
                    torrent_name = %resolved_torrent_name,
                    torrent_count = self.app_state.torrents.len(),
                    has_runtime_entry = self.app_state.torrents.contains_key(&info_hash),
                    "Magnet torrent manager created successfully"
                );
                self.dispatch_integrity_probe_batches();
                CommandIngestResult::Added {
                    info_hash: Some(info_hash),
                    torrent_name: Some(resolved_torrent_name),
                }
            }
            Err(e) => {
                let message = format!("Failed to create torrent manager from file: {:?}", e);
                tracing_event!(Level::ERROR, "{}", message);
                self.app_state.torrents.remove(&info_hash);
                self.app_state
                    .torrent_list_order
                    .retain(|ih| *ih != info_hash);
                self.remove_torrent_runtime(&info_hash);
                self.refresh_rss_derived();
                CommandIngestResult::Failed {
                    info_hash: Some(info_hash),
                    torrent_name: Some(resolved_torrent_name),
                    message,
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn add_magnet_torrent(
        &mut self,
        torrent_name: String,
        magnet_link: String,
        download_path: Option<PathBuf>,
        is_validated: bool,
        torrent_control_state: TorrentControlState,
        file_priorities: HashMap<usize, FilePriority>,
        container_name: Option<String>,
    ) -> CommandIngestResult {
        let magnet = match Magnet::new(&magnet_link) {
            Ok(m) => m,
            Err(e) => {
                let message = format!("Could not parse invalid magnet: {:?}", e);
                tracing_event!(Level::ERROR, "Could not parse invalid magnet: {:?}", e);
                return CommandIngestResult::Invalid {
                    info_hash: None,
                    torrent_name: None,
                    message,
                };
            }
        };

        let (v1_hash, v2_hash) = parse_hybrid_hashes(&magnet_link);
        let Some(info_hash) = v1_hash.clone().or_else(|| v2_hash.clone()) else {
            let message = "Magnet link is missing both btih and btmh hashes".to_string();
            tracing_event!(Level::ERROR, "{}", message);
            return CommandIngestResult::Invalid {
                info_hash: None,
                torrent_name: None,
                message,
            };
        };
        let resolved_name = resolve_magnet_torrent_name(&torrent_name, &magnet_link, &info_hash);
        let resolved_torrent_name = resolved_name.clone();
        self.persist_magnet_metadata_snapshot(
            &info_hash,
            &magnet_link,
            &resolved_torrent_name,
            &file_priorities,
        );

        if self.app_state.torrents.contains_key(&info_hash) {
            if !self.has_live_runtime_for_torrent(&info_hash) {
                self.clear_display_only_torrent(&info_hash);
            } else {
                if let Some(path) = download_path {
                    if let Some(display) = self.app_state.torrents.get_mut(&info_hash) {
                        display.latest_state.download_path = Some(path.clone());
                        display.latest_state.container_name = container_name.clone();
                        display.latest_state.file_priorities = file_priorities.clone();
                        apply_torrent_preview_file_priorities(
                            &mut display.file_preview_tree,
                            &file_priorities,
                        );
                    }
                    if let Some(manager_tx) = self.torrent_manager_command_txs.get(&info_hash) {
                        self.send_manager_command_until_shutdown(
                            manager_tx,
                            ManagerCommand::SetUserTorrentConfig {
                                torrent_data_path: path,
                                file_priorities: file_priorities.clone(),
                                container_name,
                            },
                        )
                        .await;
                    }
                }
                tracing_event!(Level::INFO, "Updated path for existing torrent from magnet");
                return CommandIngestResult::Duplicate {
                    info_hash: Some(info_hash),
                    torrent_name: Some(resolved_name),
                };
            }
        }

        let placeholder_state = TorrentDisplayState {
            latest_state: TorrentMetrics {
                torrent_control_state: torrent_control_state.clone(),
                delete_files: false,
                info_hash: info_hash.clone(),
                torrent_or_magnet: magnet_link.clone(),
                torrent_name: resolved_name.clone(),
                download_path: download_path.clone(),
                container_name: container_name.clone(),
                is_complete: is_validated,
                is_multi_file: false,
                file_count: None,
                ..Default::default()
            },
            added_at_unix_secs: Some(current_unix_secs()),
            ..Default::default()
        };
        self.app_state
            .torrents
            .insert(info_hash.clone(), placeholder_state);
        self.app_state.torrent_list_order.push(info_hash.clone());
        self.refresh_rss_derived();

        if matches!(self.app_state.mode, AppMode::Welcome) {
            self.app_state.mode = AppMode::Normal;
        }

        let (incoming_peer_tx, incoming_peer_rx) =
            mpsc::channel::<crate::torrent_manager::IncomingPeerSession>(100);
        self.torrent_manager_incoming_peer_txs
            .insert(info_hash.clone(), incoming_peer_tx);
        let (manager_command_tx, manager_command_rx) = mpsc::channel::<ManagerCommand>(100);
        self.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_command_tx);

        let dht_handle = self.dht_service.handle();
        let (torrent_metrics_tx, torrent_metrics_rx) = watch::channel(TorrentMetrics::default());
        self.torrent_metric_watch_rxs
            .insert(info_hash.clone(), torrent_metrics_rx.clone());
        let manager_event_tx_clone = self.register_manager_event_source(&info_hash);
        let resource_manager_clone = self.resource_manager.clone();
        let global_dl_bucket_clone = self.global_dl_bucket.clone();
        let global_ul_bucket_clone = self.global_ul_bucket.clone();
        let torrent_params = TorrentParameters {
            network_activation: self.network_activation.clone(),
            dht_handle,
            incoming_peer_rx,
            metrics_tx: torrent_metrics_tx,
            peer_policy_rx: self.peer_manager.handle().subscribe_policy(),
            torrent_validation_status: is_validated,
            torrent_data_path: download_path.clone(),
            container_name: container_name.clone(),
            manager_command_rx,
            manager_event_tx: manager_event_tx_clone,
            settings: Arc::clone(&Arc::new(self.client_configs.clone())),
            resource_manager: resource_manager_clone,
            global_dl_bucket: global_dl_bucket_clone,
            global_ul_bucket: global_ul_bucket_clone,
            file_priorities: file_priorities.clone(),
        };
        let start_paused = torrent_control_state == TorrentControlState::Paused;
        let should_announce_on_add = torrent_control_state == TorrentControlState::Running
            && (self.app_state.externally_accessable_port_v4
                || self.app_state.externally_accessable_port_v6);

        match TorrentManager::from_magnet(
            torrent_params.with_payload(crate::persistence::Payload::native()),
            magnet,
            &magnet_link,
        ) {
            Ok(torrent_manager) => {
                if !self
                    .peer_manager
                    .handle()
                    .register_torrent(info_hash.clone(), torrent_metrics_rx)
                {
                    tracing_event!(
                        Level::WARN,
                        info_hash = %hex::encode(&info_hash),
                        "Peer manager was unavailable while registering torrent metrics"
                    );
                }
                self.manager_tasks.spawn(async move {
                    let _ = torrent_manager.run(start_paused).await;
                });
                if should_announce_on_add {
                    self.announce_torrents_to_dht(std::iter::once(info_hash.clone()));
                }
                let _ = reduce_app_action(&mut self.app_state, AppAction::TorrentAdded);
                self.dispatch_integrity_probe_batches();
                CommandIngestResult::Added {
                    info_hash: Some(info_hash),
                    torrent_name: Some(resolved_torrent_name),
                }
            }
            Err(e) => {
                let message = format!("Failed to create new torrent manager from magnet: {:?}", e);
                tracing_event!(Level::ERROR, "{}", message);
                self.app_state.torrents.remove(&info_hash);
                self.app_state
                    .torrent_list_order
                    .retain(|ih| *ih != info_hash);
                self.remove_torrent_runtime(&info_hash);
                self.refresh_rss_derived();
                CommandIngestResult::Failed {
                    info_hash: Some(info_hash),
                    torrent_name: Some(resolved_name),
                    message,
                }
            }
        }
    }

    pub(super) fn has_live_runtime_for_torrent(&self, info_hash: &[u8]) -> bool {
        self.torrent_manager_command_txs.contains_key(info_hash)
    }

    pub(super) fn clear_display_only_torrent(&mut self, info_hash: &[u8]) {
        if self.has_live_runtime_for_torrent(info_hash) {
            return;
        }

        self.app_state.torrents.remove(info_hash);
        self.app_state
            .torrent_list_order
            .retain(|existing| existing.as_slice() != info_hash);
    }
}
