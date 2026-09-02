// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native execution hooks for effects emitted by the shared TUI reducers.

use super::{apply_browser_transition, enqueue_commands, priority_overrides, App, AppCommand};
use crate::app::{BrowserSearchState, DownloadSelectionTarget};
use crate::integrations::control::ControlRequest;
use crate::theme::{Theme, ThemeName};
use crate::torrent_manager::ManagerCommand;
use crate::tui::interaction_effects::{
    BrowserDialogEffect, BrowserTransition, ConfirmDecision, DownloadConfirmPayload,
};
use std::collections::HashMap;
use std::path::Path;
use tracing::{event as tracing_event, Level};

pub(crate) async fn execute_browser_dialog_effects(
    app: &mut App,
    effects: Vec<BrowserDialogEffect>,
) {
    for effect in effects {
        match effect {
            BrowserDialogEffect::ExecuteConfirmDecision(decision) => {
                if let Some(transition) = execute_native_confirm_decision(app, decision).await {
                    apply_browser_transition(app, transition);
                }
            }
            BrowserDialogEffect::ToConfig(config_ui) => {
                app.app_state.ui.config = config_ui;
                apply_browser_transition(app, BrowserTransition::ToConfig);
            }
            BrowserDialogEffect::CleanupPendingLink => app.cleanup_pending_magnet_preview_runtime(),
            BrowserDialogEffect::ToNormalAndClearPending => {
                apply_browser_transition(app, BrowserTransition::Close);
                let clear_preview = !app.app_state.pending_torrent_link.is_empty();
                app.app_state.pending_torrent_path = None;
                app.app_state.pending_torrent_link.clear();
                if clear_preview {
                    app.app_state.pending_magnet_preview_info_hash = None;
                }
                app.app_state.pending_manual_ingest = None;
            }
            BrowserDialogEffect::ClearSearch => {
                app.app_state.ui.file_browser.search_state = BrowserSearchState::Closed;
                app.app_state.ui.file_browser.search_query.clear();
            }
        }
    }
}

pub(crate) async fn execute_native_confirm_decision(
    app: &mut App,
    decision: ConfirmDecision,
) -> Option<BrowserTransition> {
    match decision {
        ConfirmDecision::ToConfig(mut config_ui) => {
            tracing::info!(target: "superseedr", "Confirming Config Path Selection");
            if let Some(item) = config_ui.items.get(config_ui.selected_index).copied() {
                let update = crate::tui::screens::config::merge_config_item_into_current(
                    &config_ui.settings_edit,
                    &app.client_configs,
                    item,
                    app.is_current_shared_follower(),
                    &config_ui.network_interface_inventory.interfaces,
                );
                if update != app.client_configs {
                    app.apply_config_update_from_ui(update).await;
                }
                config_ui.settings_edit = Box::new(app.client_configs.clone());
            }
            app.app_state.ui.config = config_ui;
            Some(BrowserTransition::ToConfig)
        }
        ConfirmDecision::Download(payload) => execute_download_confirmation(app, payload),
        ConfirmDecision::File(path) => {
            if !path.is_file() {
                return None;
            }
            enqueue_commands(app, vec![AppCommand::AddTorrentFromFile(path)]);
            Some(BrowserTransition::ToNormal)
        }
        ConfirmDecision::None => None,
    }
}

fn execute_download_confirmation(
    app: &mut App,
    payload: DownloadConfirmPayload,
) -> Option<BrowserTransition> {
    match payload.target {
        DownloadSelectionTarget::PendingAdd => {
            if let Some(pending_path) = app.app_state.pending_torrent_path.clone() {
                match app.prepare_add_torrent_file_request(
                    pending_path,
                    Some(payload.base_path),
                    payload.container_name_to_use,
                    payload.file_priorities,
                ) {
                    Ok(request) => {
                        app.app_state.pending_torrent_path = None;
                        let pending_ingest = app.app_state.pending_manual_ingest.take();
                        enqueue_commands(
                            app,
                            vec![AppCommand::SubmitManualAddRequest {
                                request,
                                pending_ingest,
                            }],
                        );
                    }
                    Err(error) => {
                        app.app_state.system_error = Some(error);
                        return None;
                    }
                }
            } else if !app.app_state.pending_torrent_link.is_empty() {
                let pending_ingest = app.app_state.pending_manual_ingest.take();
                let request = app.prepare_add_magnet_request(
                    app.app_state.pending_torrent_link.clone(),
                    Some(payload.base_path),
                    payload.container_name_to_use,
                    payload.file_priorities,
                );
                enqueue_commands(
                    app,
                    vec![AppCommand::SubmitManualAddRequest {
                        request,
                        pending_ingest,
                    }],
                );
                app.app_state.pending_torrent_link.clear();
            } else {
                tracing::warn!(target: "superseedr", "SHIFT+Y pressed but no pending content was found");
            }
            Some(BrowserTransition::ToNormal)
        }
        DownloadSelectionTarget::ExistingTorrent { info_hash } => {
            let file_priorities = if payload.has_preview_files {
                payload.file_priorities
            } else {
                app.app_state
                    .torrents
                    .get(&info_hash)
                    .map(|torrent| torrent.latest_state.file_priorities.clone())
                    .unwrap_or_default()
            };
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
                        file_priorities: priority_overrides(file_priorities),
                    },
                )],
            );
            Some(BrowserTransition::Close)
        }
    }
}

pub(super) fn refresh_config_network_interfaces_on_open(app: &mut App) {
    app.refresh_config_network_interfaces();
}

pub(super) fn replay_source_exists(path: &Path) -> bool {
    path.exists()
}

pub(super) fn broadcast_manager_data_rate(app: &mut App, new_rate: u64) {
    for manager_tx in app.torrent_manager_command_txs.values() {
        let _ = manager_tx.try_send(ManagerCommand::SetDataRate(new_rate));
    }
}

pub(super) fn apply_adjacent_theme(app: &mut App, next: bool) {
    if app.is_current_shared_follower() {
        app.app_state.system_error =
            Some("Shared theme changes are leader-only while this node is a follower.".to_string());
        return;
    }
    let themes = ThemeName::sorted_for_ui();
    let current_idx = themes
        .iter()
        .position(|&theme| theme == app.client_configs.ui_theme)
        .unwrap_or(0);
    let new_idx = if next {
        (current_idx + 1) % themes.len()
    } else if current_idx == 0 {
        themes.len() - 1
    } else {
        current_idx - 1
    };
    app.client_configs.ui_theme = themes[new_idx];
    app.app_state.theme = Theme::builtin(themes[new_idx]);
    enqueue_commands(
        app,
        vec![AppCommand::UpdateConfig(app.client_configs.clone())],
    );
}

pub(super) fn persist_visualization_selections(app: &mut App) {
    app.persist_visualization_selections();
}

pub(super) async fn handle_pasted_text(app: &mut App, pasted_text: &str) {
    let pasted_text = pasted_text.trim();
    if pasted_text.starts_with("magnet:") {
        let download_path = app.client_configs.default_download_folder.clone();
        if app.is_current_shared_follower() {
            let Some(download_path) = download_path else {
                app.app_state.system_error = Some(
                    "Follower pasted magnet adds require a default download folder so the leader can apply the torrent without local manual UI.".to_string(),
                );
                return;
            };
            let request = app.prepare_add_magnet_request(
                pasted_text.to_string(),
                Some(download_path),
                None,
                HashMap::new(),
            );
            enqueue_commands(app, vec![AppCommand::SubmitControlRequest(request)]);
            return;
        }
        if let Some(download_path) =
            download_path.filter(|_| !app.client_configs.always_show_add_location_prompt)
        {
            let request = app.prepare_add_magnet_request(
                pasted_text.to_string(),
                Some(download_path),
                None,
                HashMap::new(),
            );
            enqueue_commands(app, vec![AppCommand::SubmitControlRequest(request)]);
        } else if let Err(message) = app
            .open_manual_magnet_browser(pasted_text.to_string())
            .await
        {
            app.app_state.system_error = Some(message);
        }
        return;
    }

    let path = Path::new(pasted_text);
    if path.is_file()
        && path
            .extension()
            .is_some_and(|extension| extension == "torrent")
    {
        if let Some(download_path) = app
            .client_configs
            .default_download_folder
            .clone()
            .filter(|_| !app.client_configs.always_show_add_location_prompt)
        {
            match app.prepare_add_torrent_file_request(
                path.to_path_buf(),
                Some(download_path),
                None,
                HashMap::new(),
            ) {
                Ok(request) => {
                    enqueue_commands(app, vec![AppCommand::SubmitControlRequest(request)])
                }
                Err(error) => app.app_state.system_error = Some(error),
            }
        } else {
            enqueue_commands(
                app,
                vec![AppCommand::AddTorrentFromFile(path.to_path_buf())],
            );
        }
        return;
    }

    tracing_event!(
        Level::WARN,
        "Pasted content not recognized as magnet link or torrent file: {}",
        pasted_text
    );
    app.app_state.system_error =
        Some("Pasted content not recognized as magnet link or torrent file.".to_string());
}

pub(crate) fn native_pasted_text_supported(pasted_text: &str) -> bool {
    let pasted_text = pasted_text.trim();
    if pasted_text.starts_with("magnet:") {
        return true;
    }
    let path = Path::new(pasted_text);
    path.is_file()
        && path
            .extension()
            .is_some_and(|extension| extension == "torrent")
}
