// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native control execution. Runtime resources remain owned by `App`.

use super::*;

impl App {
    pub(super) fn queue_control_request_for_leader(
        &mut self,
        request: ControlRequest,
        origin: ControlOrigin,
    ) -> Result<String, String> {
        if !self.cluster_capabilities().can_queue_shared_commands {
            return Err("Shared command queue is unavailable in this mode".to_string());
        }
        let watch_path = resolve_command_watch_path(&self.client_configs)
            .ok_or_else(|| "Could not resolve the shared command inbox".to_string())?;
        let queued_path = write_control_request(&request, &watch_path)
            .map_err(|error| format!("Failed to queue shared control request: {}", error))?;
        self.record_control_queued(queued_path, request.clone(), origin);
        self.save_state_to_disk();
        Ok(format!(
            "Queued for leader processing. {}",
            online_control_success_message(&request)
        ))
    }

    pub async fn dispatch_cluster_control_request(
        &mut self,
        request: ControlRequest,
        origin: ControlOrigin,
    ) -> Result<String, String> {
        self.dispatch_cluster_control_request_with_ingest_result(request, origin)
            .await
            .map(|(message, _)| message)
    }

    pub(super) async fn dispatch_cluster_control_request_with_ingest_result(
        &mut self,
        request: ControlRequest,
        origin: ControlOrigin,
    ) -> Result<(String, Option<CommandIngestResult>), String> {
        if self.is_current_shared_follower() {
            self.queue_control_request_for_leader(request, origin)
                .map(|message| (message, None))
        } else {
            self.apply_control_request_with_ingest_result(&request)
                .await
        }
    }

    pub(super) fn map_add_result_to_control_response(
        result: CommandIngestResult,
    ) -> Result<String, String> {
        match result {
            CommandIngestResult::Added { torrent_name, .. } => Ok(format!(
                "Added torrent '{}'",
                torrent_name.unwrap_or_else(|| "unknown".to_string())
            )),
            CommandIngestResult::Duplicate { torrent_name, .. } => Ok(format!(
                "Torrent '{}' was already present",
                torrent_name.unwrap_or_else(|| "unknown".to_string())
            )),
            CommandIngestResult::Invalid { message, .. }
            | CommandIngestResult::Failed { message, .. } => Err(message),
        }
    }

    pub(super) fn clear_pending_magnet_preview_if_applied(&mut self, result: &CommandIngestResult) {
        let applied_info_hash = match result {
            CommandIngestResult::Added {
                info_hash: Some(info_hash),
                ..
            }
            | CommandIngestResult::Duplicate {
                info_hash: Some(info_hash),
                ..
            } => info_hash,
            _ => return,
        };

        if self.app_state.pending_magnet_preview_info_hash.as_deref()
            == Some(applied_info_hash.as_slice())
        {
            self.app_state.pending_magnet_preview_info_hash = None;
        }
    }

    pub(super) async fn handle_submit_control_request(
        &mut self,
        request: ControlRequest,
        pending_manual_ingest: Option<PendingManualIngest>,
    ) {
        let pending_manual_ingest = pending_manual_ingest.filter(|_| {
            matches!(
                &request,
                ControlRequest::AddTorrentFile { .. } | ControlRequest::AddMagnet { .. }
            )
        });

        match self
            .dispatch_cluster_control_request_with_ingest_result(request, ControlOrigin::CliOnline)
            .await
        {
            Ok((_message, ingest_result)) => {
                if let (Some(pending), Some(ingest_result)) = (pending_manual_ingest, ingest_result)
                {
                    self.archive_processed_ingest(pending.source, &pending.path);
                    self.record_ingest_result(&pending.path, &ingest_result);
                    self.save_state_to_disk();
                }
            }
            Err(error) => {
                self.app_state.system_error = Some(error);
                self.app_state.ui.needs_redraw = true;
            }
        }
    }

    pub(super) async fn apply_control_request(
        &mut self,
        request: &ControlRequest,
    ) -> Result<String, String> {
        self.apply_control_request_with_ingest_result(request)
            .await
            .map(|(message, _)| message)
    }

    pub(super) async fn apply_control_request_with_ingest_result(
        &mut self,
        request: &ControlRequest,
    ) -> Result<(String, Option<CommandIngestResult>), String> {
        validate_runtime_control_request(request)?;

        match plan_control_request(&self.client_configs, request)? {
            ControlExecutionPlan::StatusNow => {
                self.trigger_status_dump_now();
                Ok(("Wrote fresh status snapshot".to_string(), None))
            }
            ControlExecutionPlan::StatusFollowStart { interval_secs } => {
                self.set_runtime_status_dump_interval_override(Some(interval_secs));
                self.trigger_status_dump_now();
                Ok((
                    format!(
                        "Enabled runtime status dumps every {} seconds",
                        interval_secs
                    ),
                    None,
                ))
            }
            ControlExecutionPlan::StatusFollowStop => {
                self.set_runtime_status_dump_interval_override(Some(0));
                Ok(("Stopped runtime status dumps".to_string(), None))
            }
            ControlExecutionPlan::ApplySettings {
                next_settings,
                success_message,
            } => {
                self.apply_settings_update(next_settings, true).await;
                if let Some(error) = &self.app_state.settings_application.last_error {
                    return Err(error.clone());
                }
                self.trigger_status_dump_after_successful_cluster_mutation();
                Ok((success_message, None))
            }
            ControlExecutionPlan::AddTorrentFile {
                source_path,
                download_path,
                container_name,
                validation_status,
                file_priorities,
            } => {
                let has_applied_download_path = download_path.is_some();
                let ingest_result = self
                    .add_torrent_from_file(
                        source_path.clone(),
                        download_path,
                        validation_status,
                        TorrentControlState::Running,
                        file_priorities,
                        container_name,
                    )
                    .await;
                Self::cleanup_staged_add_file(&source_path);
                if matches!(
                    ingest_result,
                    CommandIngestResult::Added { .. } | CommandIngestResult::Duplicate { .. }
                ) {
                    if has_applied_download_path {
                        self.clear_pending_magnet_preview_if_applied(&ingest_result);
                    }
                    self.save_state_to_disk();
                    self.trigger_status_dump_after_successful_cluster_mutation();
                }
                let response = Self::map_add_result_to_control_response(ingest_result.clone())?;
                Ok((response, Some(ingest_result)))
            }
            ControlExecutionPlan::AddMagnet {
                magnet_link,
                download_path,
                container_name,
                validation_status,
                file_priorities,
            } => {
                let has_applied_download_path = download_path.is_some();
                let ingest_result = self
                    .add_magnet_torrent(
                        "Fetching name...".to_string(),
                        magnet_link,
                        download_path,
                        validation_status,
                        TorrentControlState::Running,
                        file_priorities,
                        container_name,
                    )
                    .await;
                if matches!(
                    ingest_result,
                    CommandIngestResult::Added { .. } | CommandIngestResult::Duplicate { .. }
                ) {
                    if has_applied_download_path {
                        self.clear_pending_magnet_preview_if_applied(&ingest_result);
                    }
                    self.save_state_to_disk();
                    self.trigger_status_dump_after_successful_cluster_mutation();
                }
                let response = Self::map_add_result_to_control_response(ingest_result.clone())?;
                Ok((response, Some(ingest_result)))
            }
        }
    }
}
