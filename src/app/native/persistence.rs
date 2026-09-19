// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native persistence execution. Runtime resources remain owned by `App`.

use super::*;

impl App {
    pub(super) async fn flush_persistence_writer(&mut self) {
        self.persist_event_journal();
        self.persistence_tx = None;
        if let Some(mut task) = self.persistence_task.take() {
            loop {
                tokio::select! {
                    result = &mut task => {
                        if let Err(error) = result {
                            self.app_state.system_error = Some(format!("Persistence writer join failed: {error}"));
                        }
                        break;
                    }
                    Some(command) = self.app_command_rx.recv() => self.accept_checkpoint_command(command),
                }
            }
            // A final acknowledgement can already be queued when the writer completes.
            while let Ok(command) = self.app_command_rx.try_recv() {
                self.accept_checkpoint_command(command);
            }
        }
        self.event_journal_persistence_tx = None;
        if let Some(handle) = self.event_journal_persistence_task.take() {
            if let Err(error) = handle.await {
                tracing_event!(Level::ERROR, %error, "Error joining event journal persistence task");
            }
        }
    }

    pub(super) fn accept_checkpoint_command(&mut self, command: AppCommand) {
        match command {
            AppCommand::CheckpointPersisted { revision, result } => {
                reduce_app_action(
                    &mut self.app_state,
                    AppAction::CheckpointCompleted { revision, result },
                );
            }
            AppCommand::NetworkHistoryPersisted {
                request_id,
                success,
            } => {
                apply_network_history_persist_result(&mut self.app_state, request_id, success);
            }
            AppCommand::ActivityHistoryPersisted {
                request_id,
                success,
            } => {
                apply_activity_history_persist_result(&mut self.app_state, request_id, success);
            }
            other => self.app_state.pending_watch_commands.push_back(other),
        }
    }

    pub(super) async fn flush_shared_recovery_backup_worker(&mut self) {
        self.shared_recovery_backup_tx = None;
        if let Some(handle) = self.shared_recovery_backup_task.take() {
            if let Err(error) = handle.await {
                tracing_event!(Level::ERROR, %error, "Error joining recovery backup worker");
            }
        }
    }

    pub(super) fn startup_network_history_restore(&mut self) {
        self.app_state.network_history_restore_pending = true;
        let tx = self.app_command_tx.clone();
        let app_persistence = self.app_persistence.clone();
        self.background_tasks.spawn(async move {
            let load_result =
                tokio::task::spawn_blocking(move || app_persistence.load_network_history_state())
                    .await;
            match load_result {
                Ok(state) => {
                    let _ = tx.send(AppCommand::NetworkHistoryLoaded(state)).await;
                }
                Err(e) => {
                    tracing_event!(
                        Level::ERROR,
                        "Network history restore task failed to join: {}",
                        e
                    );
                    let _ = tx
                        .send(AppCommand::NetworkHistoryLoaded(
                            NetworkHistoryPersistedState::default(),
                        ))
                        .await;
                }
            }
        });
    }

    pub(super) fn startup_activity_history_restore(&mut self) {
        self.app_state.activity_history_restore_pending = true;
        let tx = self.app_command_tx.clone();
        let app_persistence = self.app_persistence.clone();
        self.background_tasks.spawn(async move {
            let load_result =
                tokio::task::spawn_blocking(move || app_persistence.load_activity_history_state())
                    .await;
            match load_result {
                Ok(state) => {
                    let _ = tx
                        .send(AppCommand::ActivityHistoryLoaded(Box::new(state)))
                        .await;
                }
                Err(e) => {
                    tracing_event!(
                        Level::ERROR,
                        "Activity history restore task failed to join: {}",
                        e
                    );
                    let _ = tx
                        .send(AppCommand::ActivityHistoryLoaded(Box::default()))
                        .await;
                }
            }
        });
    }

    pub(super) fn save_state_to_disk(&mut self) {
        if !self.cluster_capabilities().can_persist_local_runtime_state {
            return;
        }

        let mut payload = build_persist_payload(
            &mut self.client_configs,
            &mut self.app_state,
            &self.startup_deferred_load_queue,
        );
        if let Some(network_binding) = &self.persisted_network_binding_override {
            payload.settings.network_binding = network_binding.clone();
        }
        let network_history_request_id = payload
            .network_history
            .as_ref()
            .map(|request| request.request_id);
        let activity_history_request_id = payload
            .activity_history
            .as_ref()
            .map(|request| request.request_id);

        if queue_persistence_payload(self.persistence_tx.as_ref(), payload).is_ok() {
            self.app_state.pending_network_history_persist_request_id = network_history_request_id;
            self.app_state.pending_activity_history_persist_request_id =
                activity_history_request_id;
        } else {
            let revision = self.app_state.checkpoint.requested_revision;
            reduce_app_action(
                &mut self.app_state,
                AppAction::CheckpointCompleted {
                    revision,
                    result: Err(
                        "Failed to queue persistence payload: persistence task unavailable".into(),
                    ),
                },
            );
        }
    }

    pub(crate) fn persist_visualization_selections(&mut self) {
        self.client_configs.peer_stream_visualization =
            self.app_state.ui.visualization_focus.peer_stream;
        self.client_configs.disk_health_visualization =
            self.app_state.ui.visualization_focus.disk_health;
        self.client_configs.dht_visualization = self.app_state.ui.visualization_focus.dht;

        if self.is_current_shared_follower() {
            if let Err(error) = self.app_persistence.save_settings(&self.client_configs) {
                self.app_state.system_error = Some(format!(
                    "Failed to save follower visualization settings: {}",
                    error
                ));
                self.app_state.ui.needs_redraw = true;
            }
        } else {
            self.save_state_to_disk();
        }
    }

    pub(super) fn append_event_journal_entry(&mut self, entry: EventJournalEntry) {
        append_event_journal_entry(&mut self.app_state.event_journal_state, entry);
        self.persist_event_journal();
    }

    pub(super) fn persist_event_journal(&self) {
        let Some(tx) = self.event_journal_persistence_tx.as_ref() else {
            return;
        };
        tx.send_replace(Some(EventJournalPersistRequest {
            state: self.app_state.event_journal_state.clone(),
            can_write_shared_state: self.can_write_shared_state(),
        }));
        if tx.is_closed() {
            tracing_event!(
                Level::ERROR,
                "Failed to queue event journal persistence: writer unavailable"
            );
        }
    }

    pub(super) fn control_event_scope(&self) -> EventScope {
        if crate::config::is_shared_config_mode() {
            EventScope::Shared
        } else {
            EventScope::Host
        }
    }

    pub(super) fn persist_torrent_metadata_snapshot(
        &mut self,
        info_hash: &[u8],
        torrent: &crate::torrent_file::Torrent,
        file_priorities: &HashMap<usize, FilePriority>,
    ) {
        if !self.cluster_capabilities().can_write_shared_state {
            return;
        }

        let entry = TorrentMetadataEntry {
            info_hash_hex: hex::encode(info_hash),
            torrent_name: torrent.info.name.clone(),
            total_size: torrent.info.total_length().max(0) as u64,
            is_multi_file: !torrent.info.files.is_empty(),
            files: torrent
                .file_list()
                .into_iter()
                .map(|(parts, length)| TorrentMetadataFileEntry {
                    relative_path: parts.join("/"),
                    length,
                })
                .collect(),
            file_priorities: file_priorities.clone(),
        };

        if self
            .persisted_torrent_metadata_cache
            .get(info_hash)
            .is_some_and(|persisted| persisted == &entry)
        {
            return;
        }

        if let Err(error) = self.app_persistence.upsert_torrent_metadata(entry.clone()) {
            tracing_event!(
                Level::WARN,
                "Failed to persist torrent metadata snapshot: {}",
                error
            );
            return;
        }

        self.persisted_torrent_metadata_cache
            .insert(info_hash.to_vec(), entry);
    }

    pub(super) fn persist_magnet_metadata_snapshot(
        &mut self,
        info_hash: &[u8],
        magnet_link: &str,
        torrent_name: &str,
        file_priorities: &HashMap<usize, FilePriority>,
    ) {
        let Some(length) = extract_magnet_exact_length(magnet_link) else {
            return;
        };
        let Some(file_name) = extract_magnet_display_name(magnet_link) else {
            return;
        };
        let entry = TorrentMetadataEntry {
            info_hash_hex: hex::encode(info_hash),
            torrent_name: torrent_name.to_string(),
            total_size: length,
            is_multi_file: false,
            files: vec![TorrentMetadataFileEntry {
                relative_path: normalize_magnet_metadata_path(&file_name),
                length,
            }],
            file_priorities: file_priorities.clone(),
        };

        if self
            .persisted_torrent_metadata_cache
            .get(info_hash)
            .is_some_and(|persisted| persisted == &entry)
        {
            return;
        }

        if let Err(error) = self.app_persistence.upsert_torrent_metadata(entry.clone()) {
            tracing_event!(
                Level::WARN,
                "Failed to persist magnet metadata snapshot: {}",
                error
            );
            return;
        }

        self.persisted_torrent_metadata_cache
            .insert(info_hash.to_vec(), entry);
    }

    pub(super) fn record_ingest_queued(
        &mut self,
        path: PathBuf,
        origin: IngestOrigin,
        ingest_kind: IngestKind,
        source_watch_folder: Option<PathBuf>,
    ) -> bool {
        if self.app_state.pending_ingest_by_path.contains_key(&path) {
            return false;
        }

        let correlation_id = event_correlation_id_for_path(&path);
        self.app_state.pending_ingest_by_path.insert(
            path.clone(),
            PendingIngestRecord {
                correlation_id: correlation_id.clone(),
                origin,
                ingest_kind,
                source_watch_folder: source_watch_folder.clone(),
                source_path: path.clone(),
            },
        );
        self.append_event_journal_entry(EventJournalEntry {
            host_id: self.event_journal_host_id.clone(),
            ts_iso: chrono::Utc::now().to_rfc3339(),
            category: EventCategory::Ingest,
            event_type: EventType::IngestQueued,
            source_watch_folder,
            source_path: Some(path),
            correlation_id: Some(correlation_id),
            message: Some("Queued ingest item".to_string()),
            details: EventDetails::Ingest {
                origin,
                ingest_kind,
                download_path: None,
                container_name: None,
                payload_path: None,
            },
            ..Default::default()
        });
        true
    }

    pub(super) fn record_watch_path_discovered(&mut self, path: &Path) {
        if let Some(ingest_kind) = ingest_kind_from_path(path) {
            if self.record_ingest_queued(
                path.to_path_buf(),
                IngestOrigin::WatchFolder,
                ingest_kind,
                self.source_watch_folder_for_path(path),
            ) {
                self.save_state_to_disk();
            }
        }
    }

    pub(super) fn record_rss_queued(
        &mut self,
        path: PathBuf,
        origin: IngestOrigin,
        ingest_kind: IngestKind,
    ) {
        if self.record_ingest_queued(path, origin, ingest_kind, shared_inbox_path()) {
            self.save_state_to_disk();
        }
    }

    pub(super) fn control_origin_for_command_path(&self, path: &Path) -> ControlOrigin {
        if self.is_shared_inbox_path(path) {
            ControlOrigin::SharedRelay
        } else if self.is_host_watch_path(path) {
            ControlOrigin::WatchFolder
        } else {
            ControlOrigin::CliOnline
        }
    }

    pub(super) fn control_origin_for_ingest_path(&self, path: &Path) -> ControlOrigin {
        match self
            .app_state
            .pending_ingest_by_path
            .get(path)
            .map(|record| record.origin)
        {
            Some(IngestOrigin::RssAuto) => ControlOrigin::RssAuto,
            Some(IngestOrigin::RssManual) => ControlOrigin::RssManual,
            Some(IngestOrigin::WatchFolder) | None => ControlOrigin::WatchFolder,
        }
    }

    pub(super) fn record_control_queued(
        &mut self,
        path: PathBuf,
        request: ControlRequest,
        origin: ControlOrigin,
    ) -> bool {
        if self.app_state.pending_control_by_path.contains_key(&path) {
            return false;
        }

        let correlation_id = event_correlation_id_for_path(&path);
        let source_watch_folder = self.source_watch_folder_for_path(&path);
        self.app_state.pending_control_by_path.insert(
            path.clone(),
            PendingControlRecord {
                correlation_id: correlation_id.clone(),
                request: request.clone(),
                origin,
                source_watch_folder: source_watch_folder.clone(),
                source_path: path.clone(),
            },
        );
        self.append_event_journal_entry(EventJournalEntry {
            scope: self.control_event_scope(),
            host_id: self.event_journal_host_id.clone(),
            ts_iso: chrono::Utc::now().to_rfc3339(),
            category: EventCategory::Control,
            event_type: EventType::ControlQueued,
            source_watch_folder,
            source_path: Some(path),
            correlation_id: Some(correlation_id),
            message: Some(format!("Queued control action '{}'", request.action_name())),
            details: control_event_details(&request, origin),
            ..Default::default()
        });
        true
    }

    pub(super) fn record_control_result(
        &mut self,
        path: &PathBuf,
        request: &ControlRequest,
        result: Result<String, String>,
    ) {
        let pending = self.app_state.pending_control_by_path.remove(path);
        let correlation_id = pending
            .as_ref()
            .map(|record| record.correlation_id.clone())
            .unwrap_or_else(|| event_correlation_id_for_path(path));
        let (source_watch_folder, source_path, request, origin) = pending
            .map(|record| {
                (
                    record.source_watch_folder,
                    Some(record.source_path),
                    record.request,
                    record.origin,
                )
            })
            .unwrap_or_else(|| {
                (
                    self.source_watch_folder_for_path(path),
                    Some(path.clone()),
                    request.clone(),
                    self.control_origin_for_command_path(path),
                )
            });
        let (event_type, message) = match result {
            Ok(message) => (EventType::ControlApplied, Some(message)),
            Err(message) => (EventType::ControlFailed, Some(message)),
        };
        self.append_event_journal_entry(EventJournalEntry {
            scope: self.control_event_scope(),
            host_id: self.event_journal_host_id.clone(),
            ts_iso: chrono::Utc::now().to_rfc3339(),
            category: EventCategory::Control,
            event_type,
            source_watch_folder,
            source_path,
            correlation_id: Some(correlation_id),
            message,
            details: control_event_details(&request, origin),
            ..Default::default()
        });
    }

    pub(super) fn record_ingest_result(&mut self, path: &PathBuf, result: &CommandIngestResult) {
        let pending = self.app_state.pending_ingest_by_path.remove(path);
        let fallback_kind = ingest_kind_from_path(path).unwrap_or_default();
        let correlation_id = pending
            .as_ref()
            .map(|record| record.correlation_id.clone())
            .unwrap_or_else(|| event_correlation_id_for_path(path));
        let (origin, ingest_kind, source_watch_folder, source_path) = pending
            .map(|record| {
                (
                    record.origin,
                    record.ingest_kind,
                    record.source_watch_folder,
                    Some(record.source_path),
                )
            })
            .unwrap_or_else(|| {
                (
                    IngestOrigin::WatchFolder,
                    fallback_kind,
                    self.source_watch_folder_for_path(path),
                    Some(path.clone()),
                )
            });

        let (event_type, torrent_name, info_hash_hex, message) = match result {
            CommandIngestResult::Added {
                info_hash,
                torrent_name,
            } => (
                EventType::IngestAdded,
                torrent_name.clone(),
                info_hash.as_ref().map(hex::encode),
                Some("Added torrent from ingest item".to_string()),
            ),
            CommandIngestResult::Duplicate {
                info_hash,
                torrent_name,
            } => (
                EventType::IngestDuplicate,
                torrent_name.clone(),
                info_hash.as_ref().map(hex::encode),
                Some("Ignored duplicate ingest item".to_string()),
            ),
            CommandIngestResult::Invalid {
                info_hash,
                torrent_name,
                message,
            } => (
                EventType::IngestInvalid,
                torrent_name.clone(),
                info_hash.as_ref().map(hex::encode),
                Some(message.clone()),
            ),
            CommandIngestResult::Failed {
                info_hash,
                torrent_name,
                message,
            } => (
                EventType::IngestFailed,
                torrent_name.clone(),
                info_hash.as_ref().map(hex::encode),
                Some(message.clone()),
            ),
        };
        let (download_path, container_name, payload_path) = info_hash_hex
            .as_deref()
            .and_then(|hash| hex::decode(hash).ok())
            .and_then(|info_hash| self.app_state.torrents.get(&info_hash))
            .map(|torrent| {
                (
                    torrent.latest_state.download_path.clone(),
                    torrent.latest_state.container_name.clone(),
                    Self::torrent_saved_location(&torrent.latest_state),
                )
            })
            .unwrap_or_default();

        self.append_event_journal_entry(EventJournalEntry {
            host_id: self.event_journal_host_id.clone(),
            ts_iso: chrono::Utc::now().to_rfc3339(),
            category: EventCategory::Ingest,
            event_type,
            torrent_name,
            info_hash_hex,
            source_watch_folder,
            source_path,
            correlation_id: Some(correlation_id),
            message,
            details: EventDetails::Ingest {
                origin,
                ingest_kind,
                download_path,
                container_name,
                payload_path,
            },
            ..Default::default()
        });
    }

    pub(super) fn record_data_health_event(
        &mut self,
        info_hash: &[u8],
        torrent_name: Option<String>,
        event_type: EventType,
        issue_files: Vec<String>,
        message: String,
    ) {
        self.append_event_journal_entry(EventJournalEntry {
            host_id: self.event_journal_host_id.clone(),
            ts_iso: chrono::Utc::now().to_rfc3339(),
            category: EventCategory::DataHealth,
            event_type,
            torrent_name,
            info_hash_hex: Some(hex::encode(info_hash)),
            message: Some(message),
            details: EventDetails::DataHealth {
                issue_count: issue_files.len(),
                issue_files,
            },
            ..Default::default()
        });
    }

    pub(super) fn record_torrent_completed_event(
        &mut self,
        info_hash: &[u8],
        torrent_name: Option<String>,
    ) {
        let info_hash_hex = hex::encode(info_hash);
        if self.startup_completion_suppressed_hashes.remove(info_hash) {
            tracing_event!(
                Level::INFO,
                info_hash = %info_hash_hex,
                torrent_name = %torrent_name.clone().unwrap_or_default(),
                "Skipping startup TorrentCompleted journal entry for restored complete torrent"
            );
            return;
        }
        if self
            .app_state
            .event_journal_state
            .entries
            .iter()
            .any(|entry| {
                entry.event_type == EventType::TorrentCompleted
                    && entry.info_hash_hex.as_deref() == Some(info_hash_hex.as_str())
            })
        {
            tracing_event!(
                Level::INFO,
                info_hash = %info_hash_hex,
                torrent_name = %torrent_name.clone().unwrap_or_default(),
                "Skipping duplicate TorrentCompleted journal entry"
            );
            return;
        }

        tracing_event!(
            Level::INFO,
            info_hash = %info_hash_hex,
            torrent_name = %torrent_name.clone().unwrap_or_default(),
            "Recording TorrentCompleted journal entry"
        );
        self.append_event_journal_entry(EventJournalEntry {
            host_id: self.event_journal_host_id.clone(),
            ts_iso: chrono::Utc::now().to_rfc3339(),
            category: EventCategory::TorrentLifecycle,
            event_type: EventType::TorrentCompleted,
            torrent_name,
            info_hash_hex: Some(info_hash_hex),
            message: Some("Torrent completed".to_string()),
            ..Default::default()
        });
    }
}

pub(super) fn queue_persistence_payload(
    tx: Option<&watch::Sender<Option<PersistPayload>>>,
    payload: PersistPayload,
) -> Result<(), ()> {
    let Some(tx) = tx else {
        return Err(());
    };
    tx.send_replace(Some(payload));
    if tx.is_closed() {
        return Err(());
    }
    Ok(())
}

#[cfg(test)]
pub(super) async fn flush_persistence_writer_parts(
    persistence_tx: &mut Option<watch::Sender<Option<PersistPayload>>>,
    persistence_task: &mut Option<tokio::task::JoinHandle<()>>,
) {
    *persistence_tx = None;
    if let Some(handle) = persistence_task.take() {
        if let Err(e) = handle.await {
            tracing_event!(Level::ERROR, "Error joining persistence task: {}", e);
        }
    }
}

pub(super) fn spawn_persistence_writer(
    app_command_tx: mpsc::Sender<AppCommand>,
    app_persistence: AppPersistence,
) -> (
    watch::Sender<Option<PersistPayload>>,
    tokio::task::JoinHandle<()>,
) {
    let (persistence_tx, mut persistence_rx) = watch::channel::<Option<PersistPayload>>(None);
    let persistence_app_command_tx = app_command_tx.clone();
    let persistence_task = tokio::spawn(async move {
        let mut persistence_error_log_cooldowns: HashMap<String, LogCooldown> = HashMap::new();
        while persistence_rx.changed().await.is_ok() {
            let Some(payload) = persistence_rx.borrow().clone() else {
                continue;
            };
            let network_history_request_id = payload
                .network_history
                .as_ref()
                .map(|request| request.request_id);
            let activity_history_request_id = payload
                .activity_history
                .as_ref()
                .map(|request| request.request_id);
            let app_persistence = app_persistence.clone();
            let revision = payload.revision;
            let write_result = tokio::task::spawn_blocking(move || {
                app_persistence
                    .save_settings(&payload.settings)
                    .map_err(|e| format!("Failed to auto-save settings: {}", e))?;
                app_persistence
                    .save_rss_state(&payload.rss_state)
                    .map_err(|e| format!("Failed to auto-save RSS state: {}", e))?;
                if let Some(network_history) = payload.network_history {
                    app_persistence
                        .save_network_history_state(&network_history.state)
                        .map_err(|e| format!("Failed to auto-save network history state: {}", e))?;
                }
                if let Some(activity_history) = payload.activity_history {
                    app_persistence
                        .save_activity_history_state(&activity_history.state)
                        .map_err(|e| {
                            format!("Failed to auto-save activity history state: {}", e)
                        })?;
                }
                Ok::<(), String>(())
            })
            .await;

            let checkpoint_result = match &write_result {
                Ok(result) => result.clone(),
                Err(error) => Err(format!("Persistence writer join failed: {error}")),
            };
            let _ = persistence_app_command_tx
                .send(AppCommand::CheckpointPersisted {
                    revision,
                    result: checkpoint_result,
                })
                .await;
            match write_result {
                Ok(Ok(())) => {
                    tracing_event!(Level::DEBUG, "Persistence payload auto-saved successfully.");
                    if let Some(request_id) = network_history_request_id {
                        let _ = persistence_app_command_tx
                            .send(AppCommand::NetworkHistoryPersisted {
                                request_id,
                                success: true,
                            })
                            .await;
                    }
                    if let Some(request_id) = activity_history_request_id {
                        let _ = persistence_app_command_tx
                            .send(AppCommand::ActivityHistoryPersisted {
                                request_id,
                                success: true,
                            })
                            .await;
                    }
                }
                Ok(Err(e)) => {
                    if persistence_error_log_cooldowns
                        .entry(e.clone())
                        .or_default()
                        .should_log(Instant::now(), REPEATED_HEALTH_LOG_INTERVAL)
                    {
                        tracing_event!(Level::ERROR, "{}", e);
                    }
                    if let Some(request_id) = network_history_request_id {
                        let _ = persistence_app_command_tx
                            .send(AppCommand::NetworkHistoryPersisted {
                                request_id,
                                success: false,
                            })
                            .await;
                    }
                    if let Some(request_id) = activity_history_request_id {
                        let _ = persistence_app_command_tx
                            .send(AppCommand::ActivityHistoryPersisted {
                                request_id,
                                success: false,
                            })
                            .await;
                    }
                }
                Err(e) => {
                    tracing_event!(Level::ERROR, "Persistence writer join failed: {}", e);
                    if let Some(request_id) = network_history_request_id {
                        let _ = persistence_app_command_tx
                            .send(AppCommand::NetworkHistoryPersisted {
                                request_id,
                                success: false,
                            })
                            .await;
                    }
                    if let Some(request_id) = activity_history_request_id {
                        let _ = persistence_app_command_tx
                            .send(AppCommand::ActivityHistoryPersisted {
                                request_id,
                                success: false,
                            })
                            .await;
                    }
                }
            }
        }
    });

    (persistence_tx, persistence_task)
}

pub(super) fn spawn_event_journal_persistence_writer(
    app_persistence: AppPersistence,
) -> (
    watch::Sender<Option<EventJournalPersistRequest>>,
    tokio::task::JoinHandle<()>,
) {
    let (persistence_tx, mut persistence_rx) =
        watch::channel::<Option<EventJournalPersistRequest>>(None);
    let persistence_task = tokio::spawn(async move {
        while persistence_rx.changed().await.is_ok() {
            let Some(request) = persistence_rx.borrow().clone() else {
                continue;
            };
            let app_persistence = app_persistence.clone();
            let write_result = tokio::task::spawn_blocking(move || {
                app_persistence
                    .save_event_journal_state(&request.state, request.can_write_shared_state)
            })
            .await;

            match write_result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing_event!(Level::ERROR, %error, "Failed to auto-save event journal state");
                }
                Err(error) => {
                    tracing_event!(Level::ERROR, %error, "Event journal writer failed");
                }
            }
        }
    });
    (persistence_tx, persistence_task)
}

pub(super) fn spawn_shared_recovery_backup_worker(
) -> (mpsc::Sender<()>, tokio::task::JoinHandle<()>) {
    let (request_tx, mut request_rx) = mpsc::channel::<()>(1);
    let task = tokio::spawn(async move {
        while request_rx.recv().await.is_some() {
            let result =
                tokio::task::spawn_blocking(refresh_shared_config_recovery_backup_now).await;

            match result {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    tracing_event!(
                        Level::WARN,
                        error = %error,
                        "Failed to refresh scheduled shared config recovery backup"
                    );
                }
                Err(error) => {
                    tracing_event!(
                        Level::ERROR,
                        error = %error,
                        "Scheduled shared config recovery backup worker failed"
                    );
                }
            }
        }
    });
    (request_tx, task)
}

pub(super) fn build_persist_payload(
    client_configs: &mut Settings,
    app_state: &mut AppState,
    startup_deferred_load_queue: &VecDeque<Vec<u8>>,
) -> PersistPayload {
    prepare_checkpoint(
        client_configs,
        app_state,
        startup_deferred_load_queue,
        current_unix_secs(),
    )
}
