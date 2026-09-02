// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser execution hooks for effects emitted by the shared TUI reducers.

use super::{apply_browser_transition, enqueue_commands, priority_overrides, App, AppCommand};
use crate::app::{BrowserSearchState, DownloadSelectionTarget};
use crate::integrations::control::ControlRequest;
use crate::tui::interaction_effects::{
    BrowserDialogEffect, BrowserTransition, ConfirmDecision, DownloadConfirmPayload,
};
use std::path::Path;

pub(super) fn replay_source_exists(_path: &Path) -> bool {
    true
}

pub(crate) async fn execute_browser_dialog_effects(
    app: &mut App,
    effects: Vec<BrowserDialogEffect>,
) {
    for effect in effects {
        match effect {
            BrowserDialogEffect::ExecuteConfirmDecision(decision) => match decision {
                ConfirmDecision::ToConfig(mut config) => {
                    if let Some(item) = config.items.get(config.selected_index).copied() {
                        let update = crate::tui::screens::config::merge_config_item_into_current(
                            &config.settings_edit,
                            &app.client_configs,
                            item,
                            app.is_current_shared_follower(),
                            &config.network_interface_inventory.interfaces,
                        );
                        if update != app.client_configs {
                            app.apply_config_update_from_ui(update).await;
                        }
                        config.settings_edit = Box::new(app.client_configs.clone());
                    }
                    app.app_state.ui.config = config;
                    apply_browser_transition(app, BrowserTransition::ToConfig);
                }
                ConfirmDecision::Download(payload) => execute_download_confirmation(app, payload),
                ConfirmDecision::File(path) => {
                    if app.client_configs.always_show_add_location_prompt {
                        if !app.open_manual_torrent_file_browser(path) {
                            app.app_state.system_error = Some(
                                "The simulated torrent preview is not ready for configuration."
                                    .to_string(),
                            );
                        }
                    } else {
                        enqueue_commands(app, vec![AppCommand::AddTorrentFromFile(path)]);
                        apply_browser_transition(app, BrowserTransition::Close);
                    }
                }
                ConfirmDecision::None => {}
            },
            BrowserDialogEffect::ToConfig(config) => {
                app.app_state.ui.config = config;
                apply_browser_transition(app, BrowserTransition::ToConfig);
            }
            BrowserDialogEffect::CleanupPendingLink => {
                app.app_state.pending_magnet_preview_info_hash = None;
            }
            BrowserDialogEffect::ToNormalAndClearPending => {
                apply_browser_transition(app, BrowserTransition::Close);
                app.app_state.pending_torrent_path = None;
                app.app_state.pending_torrent_link.clear();
            }
            BrowserDialogEffect::ClearSearch => {
                app.app_state.ui.file_browser.search_state = BrowserSearchState::Closed;
                app.app_state.ui.file_browser.search_query.clear();
            }
        }
    }
}

fn execute_download_confirmation(app: &mut App, payload: DownloadConfirmPayload) {
    match payload.target {
        DownloadSelectionTarget::PendingAdd => {
            if let Some(path) = app.app_state.pending_torrent_path.take() {
                enqueue_commands(
                    app,
                    vec![AppCommand::SubmitControlRequest(
                        ControlRequest::AddTorrentFile {
                            source_path: path,
                            download_path: Some(payload.base_path),
                            container_name: payload.container_name_to_use,
                            validation_status: false,
                            file_priorities: priority_overrides(payload.file_priorities),
                        },
                    )],
                );
                apply_browser_transition(app, BrowserTransition::Close);
            } else if !app.app_state.pending_torrent_link.is_empty() {
                enqueue_commands(
                    app,
                    vec![AppCommand::SubmitControlRequest(
                        ControlRequest::AddMagnet {
                            magnet_link: app.app_state.pending_torrent_link.clone(),
                            download_path: Some(payload.base_path),
                            container_name: payload.container_name_to_use,
                            validation_status: false,
                            file_priorities: priority_overrides(payload.file_priorities),
                        },
                    )],
                );
                app.app_state.pending_torrent_link.clear();
                apply_browser_transition(app, BrowserTransition::Close);
            }
        }
        DownloadSelectionTarget::ExistingTorrent { info_hash } => {
            let existing = app.app_state.torrents.get(&info_hash).map(|torrent| {
                (
                    torrent.latest_state.download_path.clone(),
                    torrent.latest_state.container_name.clone(),
                )
            });
            let (download_path, container_name) = existing.unwrap_or_default();
            enqueue_commands(
                app,
                vec![AppCommand::SubmitControlRequest(
                    ControlRequest::SetTorrentConfig {
                        info_hash_hex: hex::encode(info_hash),
                        download_path,
                        container_name,
                        file_priorities: priority_overrides(payload.file_priorities),
                    },
                )],
            );
            apply_browser_transition(app, BrowserTransition::Close);
        }
    }
}

pub(super) fn refresh_config_network_interfaces_on_open(_app: &mut App) {}

pub(super) fn broadcast_manager_data_rate(_app: &mut App, _new_rate: u64) {}

pub(super) fn apply_adjacent_theme(app: &mut App, next: bool) {
    app.apply_adjacent_theme(next);
}

pub(super) fn persist_visualization_selections(_app: &mut App) {}

pub(super) async fn handle_pasted_text(app: &mut App, pasted_text: &str) {
    let pasted_text = pasted_text.trim();
    if !pasted_text.starts_with("magnet:") {
        return;
    }
    if app.client_configs.always_show_add_location_prompt {
        app.open_manual_magnet_browser(pasted_text.to_string());
    } else {
        enqueue_commands(
            app,
            vec![AppCommand::SubmitControlRequest(
                ControlRequest::AddMagnet {
                    magnet_link: pasted_text.to_string(),
                    download_path: app.client_configs.default_download_folder.clone(),
                    container_name: None,
                    validation_status: false,
                    file_priorities: Vec::new(),
                },
            )],
        );
    }
}
