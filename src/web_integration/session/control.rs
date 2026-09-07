// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser control acquisition and manager command execution.

use super::*;
use crate::integrations::control::ControlRequest;

impl BrowserSession {
    pub(crate) fn ingest_pasted_text(&mut self, text: &str) {
        let text = text.trim();
        if !has_browser_magnet_scheme(text) {
            return;
        }
        // The explicitly selected demo backend supplies an implicit virtual destination.
        let mut policy_settings = self.client_configs.clone();
        if self.app_state.capabilities.demo && policy_settings.default_download_folder.is_none() {
            policy_settings.default_download_folder = Some(self.default_download_path());
        }
        match crate::app::choose_add_destination(&policy_settings, false, false, false, false) {
            crate::app::AddDestination::Manual => {
                let Some(info_hash) = canonical_browser_magnet_info_hash(text) else {
                    self.set_browser_error(
                        "Pasted content is not a valid magnet with a supported info hash.",
                    );
                    return;
                };
                let name = if self.app_state.capabilities.demo {
                    format!(
                        "Orbit Archive {:02x}",
                        info_hash.first().copied().unwrap_or_default()
                    )
                } else {
                    crate::app::resolve_magnet_torrent_name("", text, &info_hash)
                };
                self.open_manual_magnet_browser(text.to_string(), name);
            }
            crate::app::AddDestination::Direct(path)
            | crate::app::AddDestination::Forward(path) => {
                self.submit_control_request(
                    ControlRequest::AddMagnet {
                        magnet_link: text.to_string(),
                        download_path: if self.app_state.capabilities.demo
                            && self.client_configs.default_download_folder.is_none()
                        {
                            None
                        } else {
                            Some(path)
                        },
                        container_name: None,
                        validation_status: false,
                        file_priorities: Vec::new(),
                    },
                    false,
                );
            }
            crate::app::AddDestination::Reject(message) => self.set_browser_error(message),
        }
    }

    pub(crate) fn submit_control_request(
        &mut self,
        request: ControlRequest,
        replace_existing_config: bool,
    ) {
        if self.app_state.lifecycle.phase != crate::app::AppPhase::Running {
            self.set_browser_error("The application is shutting down.");
            return;
        }
        enqueue_control_request_with_config_policy(self, request, replace_existing_config);
    }
}

pub(super) fn enqueue_control_request_with_config_policy(
    app: &mut BrowserSession,
    request: ControlRequest,
    replace_existing_config: bool,
) {
    let command = match request {
        ControlRequest::AddMagnet {
            magnet_link,
            download_path,
            container_name,
            validation_status,
            file_priorities,
        } => BrowserCommand::AddMagnet {
            magnet_link,
            download_path,
            container_name,
            validation_status,
            file_priorities: browser_priority_overrides(file_priorities),
            replace_existing_config,
        },
        ControlRequest::AddTorrentFile {
            source_path,
            download_path,
            container_name,
            validation_status,
            file_priorities,
        } => BrowserCommand::AddTorrentFromFile {
            path: source_path,
            download_path,
            container_name,
            validation_status,
            file_priorities: browser_priority_overrides(file_priorities),
            replace_existing_config: true,
        },
        ControlRequest::Pause { info_hash_hex } => {
            let Ok(info_hash) = hex::decode(&info_hash_hex) else {
                return;
            };
            if app.send_manager_command(&info_hash, ManagerCommand::Pause) {
                app.set_torrent_paused_hex(&info_hash_hex, true);
            }
            return;
        }
        ControlRequest::Resume { info_hash_hex } => {
            let Ok(info_hash) = hex::decode(&info_hash_hex) else {
                return;
            };
            if app.send_manager_command(&info_hash, ManagerCommand::Resume) {
                app.set_torrent_paused_hex(&info_hash_hex, false);
            }
            return;
        }
        ControlRequest::Delete {
            info_hash_hex,
            delete_files,
        } => {
            let Ok(info_hash) = hex::decode(&info_hash_hex) else {
                return;
            };
            let command = if delete_files {
                crate::app::torrent_manager_protocol::ManagerCommand::DeleteFile
            } else {
                crate::app::torrent_manager_protocol::ManagerCommand::Shutdown
            };
            if app.send_manager_command(&info_hash, command) {
                if let Some(torrent) = app.app_state.torrents.get_mut(&info_hash) {
                    torrent.latest_state.torrent_control_state =
                        crate::app::TorrentControlState::Deleting;
                    torrent.latest_state.delete_files = delete_files;
                }
            }
            return;
        }
        ControlRequest::SetTorrentConfig {
            info_hash_hex,
            download_path,
            container_name,
            file_priorities,
        } => {
            let Ok(info_hash) = hex::decode(&info_hash_hex) else {
                return;
            };
            let file_priorities = browser_priority_overrides(file_priorities);
            let observed_priorities = file_priorities.clone();
            let production_priorities = file_priorities
                .into_iter()
                .map(|value| {
                    let priority = match value.priority {
                        BrowserFilePriority::High => FilePriority::High,
                        BrowserFilePriority::Skip => FilePriority::Skip,
                    };
                    (value.file_index, priority)
                })
                .collect();
            let torrent_data_path = download_path
                .clone()
                .or_else(|| app.client_configs.default_download_folder.clone())
                .unwrap_or_else(|| app.default_download_path());
            if app.send_manager_command(
                &info_hash,
                crate::app::torrent_manager_protocol::ManagerCommand::SetUserTorrentConfig {
                    torrent_data_path,
                    file_priorities: production_priorities,
                    container_name: container_name.clone(),
                },
            ) {
                let _ = app.apply_browser_torrent_config(
                    &info_hash_hex,
                    download_path.clone(),
                    container_name.clone(),
                    &observed_priorities,
                );
            }
            return;
        }
        _ => {
            app.set_browser_error("This operation is unavailable in this browser host.");
            return;
        }
    };
    app.enqueue_command(command);
}

fn browser_priority_overrides(
    overrides: Vec<crate::integrations::control::ControlFilePriorityOverride>,
) -> Vec<BrowserFilePriorityOverride> {
    overrides
        .into_iter()
        .filter_map(|override_value| {
            let priority = match override_value.priority {
                FilePriority::High => BrowserFilePriority::High,
                FilePriority::Skip => BrowserFilePriority::Skip,
                FilePriority::Normal | FilePriority::Mixed => return None,
            };
            Some(BrowserFilePriorityOverride {
                file_index: override_value.file_index,
                priority,
            })
        })
        .collect()
}
