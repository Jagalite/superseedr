// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native ingest execution. Runtime resources remain owned by `App`.

use super::*;

impl App {
    pub(super) fn control_priority_overrides(
        file_priorities: &HashMap<usize, FilePriority>,
    ) -> Vec<ControlFilePriorityOverride> {
        let mut overrides: Vec<_> = file_priorities
            .iter()
            .map(|(file_index, priority)| ControlFilePriorityOverride {
                file_index: *file_index,
                priority: *priority,
            })
            .collect();
        overrides.sort_by_key(|entry| entry.file_index);
        overrides
    }

    pub(super) fn shared_add_staging_dir() -> Result<PathBuf, String> {
        shared_root_path()
            .map(|root| root.join("staged-adds"))
            .ok_or_else(|| "Shared add staging directory is unavailable".to_string())
    }

    pub(super) fn is_shared_staged_add_path(path: &Path) -> bool {
        Self::shared_add_staging_dir()
            .map(|dir| path.starts_with(&dir))
            .unwrap_or(false)
    }

    pub(super) fn cleanup_staged_add_file(path: &Path) {
        if !Self::is_shared_staged_add_path(path) {
            return;
        }

        if let Err(error) = fs::remove_file(path) {
            if error.kind() != ErrorKind::NotFound {
                tracing_event!(
                    Level::WARN,
                    "Failed to remove staged add file {:?}: {}",
                    path,
                    error
                );
            }
        }
    }

    pub(crate) fn prepare_add_torrent_file_request(
        &self,
        source_path: PathBuf,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        file_priorities: HashMap<usize, FilePriority>,
    ) -> Result<ControlRequest, String> {
        let request_source_path = if self.is_current_shared_follower() {
            let staging_dir = Self::shared_add_staging_dir()?;
            fs::create_dir_all(&staging_dir)
                .map_err(|error| format!("Failed to create shared staging directory: {}", error))?;
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let hash = hex::encode(sha1::Sha1::digest(
                format!(
                    "{}:{}:{}",
                    source_path.display(),
                    std::process::id(),
                    now_ms
                )
                .as_bytes(),
            ));
            let staged_path =
                staging_dir.join(format!("staged-{}-{}.torrent", now_ms, &hash[..12]));
            fs::copy(&source_path, &staged_path).map_err(|error| {
                format_filesystem_path_error(
                    "Failed to stage torrent file for leader processing",
                    &source_path,
                    &error,
                )
            })?;
            staged_path
        } else {
            source_path
        };

        Ok(ControlRequest::AddTorrentFile {
            source_path: request_source_path,
            download_path,
            container_name,
            validation_status: false,
            file_priorities: Self::control_priority_overrides(&file_priorities),
        })
    }

    pub(crate) fn prepare_add_magnet_request(
        &self,
        magnet_link: String,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        file_priorities: HashMap<usize, FilePriority>,
    ) -> ControlRequest {
        ControlRequest::AddMagnet {
            magnet_link,
            download_path,
            container_name,
            validation_status: false,
            file_priorities: Self::control_priority_overrides(&file_priorities),
        }
    }

    pub(super) fn resolve_add_payload(
        &self,
        source: IngestSource,
        path: &Path,
    ) -> Result<ResolvedAddPayload, String> {
        match source {
            IngestSource::TorrentFile => Ok(ResolvedAddPayload::TorrentFile {
                source_path: path.to_path_buf(),
            }),
            IngestSource::TorrentPathFile => {
                let payload = fs::read_to_string(path).map_err(|error| {
                    format_filesystem_path_error("Failed to read torrent path file", path, &error)
                })?;
                let source_path =
                    crate::config::resolve_shared_cli_torrent_path(Path::new(payload.trim()))
                        .map_err(|error| {
                            format!(
                                "Failed to resolve shared torrent path from file {:?}: {}",
                                path, error
                            )
                        })?;
                Ok(ResolvedAddPayload::TorrentFile { source_path })
            }
            IngestSource::MagnetFile => {
                let payload = fs::read_to_string(path)
                    .map_err(|error| format!("Failed to read magnet file {:?}: {}", path, error))?;
                Ok(ResolvedAddPayload::MagnetLink {
                    magnet_link: payload.trim().to_string(),
                })
            }
        }
    }

    pub(super) fn control_request_for_add_payload(
        &self,
        payload: &ResolvedAddPayload,
        download_path: Option<PathBuf>,
    ) -> Result<ControlRequest, String> {
        match payload {
            ResolvedAddPayload::TorrentFile { source_path } => self
                .prepare_add_torrent_file_request(
                    source_path.clone(),
                    download_path,
                    None,
                    HashMap::new(),
                ),
            ResolvedAddPayload::MagnetLink { magnet_link } => Ok(self.prepare_add_magnet_request(
                magnet_link.clone(),
                download_path,
                None,
                HashMap::new(),
            )),
        }
    }

    pub(super) fn resolve_add_ingress_action(
        &self,
        source: IngestSource,
        path: &Path,
    ) -> AddIngressAction {
        let is_host_watch_path = self.is_host_watch_path(path);
        let is_shared_inbox_path = self.is_shared_inbox_path(path);

        if self.is_current_shared_follower()
            && is_host_watch_path
            && !matches!(source, IngestSource::TorrentPathFile)
        {
            return AddIngressAction::RelayRawWatchFile;
        }

        let payload = match self.resolve_add_payload(source, path) {
            Ok(payload) => payload,
            Err(message) => {
                if is_shared_inbox_path && matches!(path.try_exists(), Ok(false)) {
                    return AddIngressAction::IgnoreMissingSharedInboxItem { message };
                }
                return AddIngressAction::Fail { message };
            }
        };

        match choose_add_destination(
            &self.client_configs,
            self.is_current_shared_follower(),
            is_host_watch_path,
            is_shared_inbox_path,
            matches!(self.runtime_mode, AppRuntimeMode::SharedLeader),
        ) {
            AddDestination::Manual => AddIngressAction::OpenManualBrowser { payload },
            AddDestination::Direct(download_path) => AddIngressAction::ApplyDirectly {
                payload,
                download_path,
            },
            AddDestination::Forward(download_path) => {
                match self.control_request_for_add_payload(&payload, Some(download_path)) {
                    Ok(request) => AddIngressAction::QueueControlRequest(request),
                    Err(message) => AddIngressAction::Fail { message },
                }
            }
            AddDestination::Reject(message) => AddIngressAction::Fail { message },
        }
    }

    pub(super) fn should_archive_processed_ingest(
        &self,
        source: IngestSource,
        path: &Path,
    ) -> bool {
        match source {
            IngestSource::TorrentFile => {
                self.is_host_watch_path(path) || self.is_shared_inbox_path(path)
            }
            IngestSource::TorrentPathFile | IngestSource::MagnetFile => true,
        }
    }

    pub(super) fn update_pending_ingest_source_path(&mut self, path: &Path, final_path: PathBuf) {
        let correlation_id = self
            .app_state
            .pending_ingest_by_path
            .get_mut(path)
            .map(|record| {
                record.source_path = final_path.clone();
                record.correlation_id.clone()
            });

        let Some(correlation_id) = correlation_id else {
            return;
        };

        let mut updated = false;
        for entry in self.app_state.event_journal_state.entries.iter_mut().rev() {
            if entry.category != EventCategory::Ingest {
                continue;
            }
            if entry.correlation_id.as_deref() != Some(correlation_id.as_str()) {
                continue;
            }
            entry.source_path = Some(final_path.clone());
            updated = true;
            if entry.event_type == EventType::IngestQueued {
                break;
            }
        }
        if updated {
            self.persist_event_journal();
        }
    }

    pub(super) fn archive_processed_ingest(
        &mut self,
        source: IngestSource,
        path: &Path,
    ) -> Option<PathBuf> {
        if !self.should_archive_processed_ingest(source, path) {
            return None;
        }

        match archive_watch_file(path, source.processed_archive_extension()) {
            Ok(destination) => {
                self.update_pending_ingest_source_path(path, destination.clone());
                Some(destination)
            }
            Err(error) => {
                tracing_event!(
                    Level::WARN,
                    "Failed to archive processed ingest file {:?}: {}",
                    path,
                    error
                );
                None
            }
        }
    }

    pub(super) async fn execute_add_ingress_action(
        &mut self,
        source: IngestSource,
        path: PathBuf,
        action: AddIngressAction,
    ) {
        match action {
            AddIngressAction::RelayRawWatchFile => {
                self.app_state.pending_ingest_by_path.remove(&path);
                self.relay_local_watch_file(&path, source.relay_archive_extension());
                self.save_state_to_disk();
            }
            AddIngressAction::QueueControlRequest(request) => {
                let origin = self.control_origin_for_ingest_path(&path);
                if self.is_host_watch_path(&path) {
                    self.app_state.pending_ingest_by_path.remove(&path);
                }
                match self.dispatch_cluster_control_request(request, origin).await {
                    Ok(_message) => {
                        self.archive_processed_ingest(source, &path);
                    }
                    Err(error) => {
                        self.app_state.system_error = Some(error);
                        self.app_state.ui.needs_redraw = true;
                    }
                }
            }
            AddIngressAction::ApplyDirectly {
                payload,
                download_path,
            } => {
                let ingest_result = match payload {
                    ResolvedAddPayload::TorrentFile { source_path } => {
                        self.add_torrent_from_file(
                            source_path,
                            Some(download_path),
                            false,
                            TorrentControlState::Running,
                            HashMap::new(),
                            None,
                        )
                        .await
                    }
                    ResolvedAddPayload::MagnetLink { magnet_link } => {
                        self.add_magnet_torrent(
                            "Fetching name...".to_string(),
                            magnet_link,
                            Some(download_path),
                            false,
                            TorrentControlState::Running,
                            HashMap::new(),
                            None,
                        )
                        .await
                    }
                };
                if let CommandIngestResult::Added {
                    info_hash: Some(info_hash),
                    ..
                } = &ingest_result
                {
                    tracing_event!(
                        Level::INFO,
                        info_hash = %hex::encode(info_hash),
                        torrent_count = self.app_state.torrents.len(),
                        present_in_runtime = self.app_state.torrents.contains_key(info_hash),
                        "Direct ingest added torrent to runtime before persistence"
                    );
                }
                self.clear_pending_magnet_preview_if_applied(&ingest_result);
                self.record_ingest_result(&path, &ingest_result);
                self.save_state_to_disk();
                self.archive_processed_ingest(source, &path);
            }
            AddIngressAction::OpenManualBrowser { payload } => {
                let should_defer_archive = self.is_shared_inbox_path(&path);
                if let Err(message) = self.open_manual_browser_for_payload(source, payload).await {
                    self.app_state.system_error = Some(message.clone());
                    self.record_ingest_result(
                        &path,
                        &CommandIngestResult::Failed {
                            info_hash: None,
                            torrent_name: None,
                            message,
                        },
                    );
                    self.save_state_to_disk();
                } else if should_defer_archive {
                    self.app_state.pending_manual_ingest = Some(PendingManualIngest {
                        source,
                        path: path.clone(),
                    });
                } else {
                    self.app_state.pending_manual_ingest = None;
                }
                if !matches!(source, IngestSource::TorrentFile) && !should_defer_archive {
                    self.archive_processed_ingest(source, &path);
                }
            }
            AddIngressAction::IgnoreMissingSharedInboxItem { message } => {
                tracing_event!(
                    Level::INFO,
                    path = ?path,
                    "{}",
                    message
                );
                self.app_state.pending_ingest_by_path.remove(&path);
                self.save_state_to_disk();
            }
            AddIngressAction::Fail { message } => {
                tracing_event!(Level::ERROR, "{}", message);
                self.app_state.system_error = Some(message.clone());
                self.record_ingest_result(
                    &path,
                    &CommandIngestResult::Failed {
                        info_hash: None,
                        torrent_name: None,
                        message,
                    },
                );
                self.save_state_to_disk();
                self.archive_processed_ingest(source, &path);
            }
        }
    }

    pub(super) fn source_watch_folder_for_path(&self, path: &std::path::Path) -> Option<PathBuf> {
        path.parent().map(Path::to_path_buf)
    }

    pub(super) fn is_host_watch_path(&self, path: &Path) -> bool {
        host_watch_paths(&self.client_configs)
            .iter()
            .any(|host_watch| watched_parent_matches(path, host_watch))
    }

    pub(super) fn is_shared_inbox_path(&self, path: &Path) -> bool {
        let Some(shared_inbox) = shared_inbox_path() else {
            return false;
        };
        watched_parent_matches(path, &shared_inbox)
    }

    pub(super) fn relay_local_watch_file(&mut self, path: &Path, fallback_extension: &str) {
        match relay_watch_file_to_shared_inbox(path) {
            Ok(relayed_path) => {
                tracing_event!(
                    Level::INFO,
                    "Relayed local watch file {:?} to shared inbox {:?}",
                    path,
                    relayed_path
                );
            }
            Err(error) => {
                tracing_event!(
                    Level::WARN,
                    "Failed to relay local watch file {:?}: {}",
                    path,
                    error
                );
                if let Err(archive_error) = archive_watch_file(path, fallback_extension) {
                    tracing_event!(
                        Level::WARN,
                        "Failed to archive local watch file {:?}: {}",
                        path,
                        archive_error
                    );
                }
            }
        }
    }
}
