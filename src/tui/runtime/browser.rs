// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser runtime execution for effects emitted by the shared TUI reducers.

use crate::app::DownloadSelectionTarget;
use crate::integrations::control::ControlRequest;
use crate::tui::effects::{
    priority_overrides, BrowserTransition, ConfigNetworkInterfaceRefresh, ConfirmDecision,
    DownloadConfirmPayload, RuntimeEffect, RuntimeOutcome,
};
use crate::web_integration::{BrowserCommand, BrowserRssPreview, BrowserSession};

pub(crate) async fn execute_runtime_effect(
    app: &mut BrowserSession,
    effect: RuntimeEffect,
) -> Option<RuntimeOutcome> {
    match effect {
        RuntimeEffect::OpenConfigPathBrowser {
            browser_generation,
            preferred_path,
            browser_mode,
        } => app.request_file_tree(
            browser_generation,
            preferred_path.unwrap_or_else(|| app.default_download_path()),
            browser_mode,
            false,
            None,
        ),
        RuntimeEffect::FetchFileTree {
            browser_generation,
            path,
            browser_mode,
            preserve_browser_mode,
            highlight_path,
        } => app.request_file_tree(
            browser_generation,
            path,
            browser_mode,
            preserve_browser_mode,
            highlight_path,
        ),
        RuntimeEffect::ConfirmBrowserSelection(decision) => {
            return execute_browser_confirm_decision(app, decision).await;
        }
        RuntimeEffect::CleanupPendingPreview(_) => {}
        RuntimeEffect::SyncTorrentFilePreview => app.sync_torrent_file_preview(),
        RuntimeEffect::ReplayJournalSource(path) => {
            match path.extension().and_then(|extension| extension.to_str()) {
                Some(extension) if extension.eq_ignore_ascii_case("torrent") => {
                    app.enqueue_command(BrowserCommand::AddTorrentFromFile {
                        path,
                        download_path: app.client_configs.default_download_folder.clone(),
                        container_name: None,
                        validation_status: false,
                        file_priorities: Vec::new(),
                        replace_existing_config: false,
                    });
                }
                _ => {}
            }
        }
        RuntimeEffect::OpenAddTorrentFileBrowser => app.open_add_torrent_file_browser(),
        RuntimeEffect::OpenExistingTorrentFileBrowser(info_hash) => {
            app.open_existing_torrent_file_browser(info_hash);
        }
        RuntimeEffect::RefreshPeerManagement => app.refresh_peer_management_screen(),
        RuntimeEffect::ApplyConfig(settings) => {
            app.apply_browser_config_update(*settings);
            return Some(RuntimeOutcome::ConfigApplied(app.client_configs.clone()));
        }
        RuntimeEffect::RefreshConfigNetworkInterfaces(reason) => {
            if reason == ConfigNetworkInterfaceRefresh::Explicit {
                app.refresh_browser_network_interfaces();
            }
        }
        RuntimeEffect::BroadcastManagerDataRate(rate_ms) => {
            app.broadcast_manager_data_rate(rate_ms);
        }
        RuntimeEffect::ApplyThemePrevious => app.apply_adjacent_theme(false),
        RuntimeEffect::ApplyThemeNext => app.apply_adjacent_theme(true),
        RuntimeEffect::PersistVisualizationSelections => {}
        RuntimeEffect::SubmitControlRequest(request) => enqueue_control_request(app, request),
        RuntimeEffect::HandlePastedText(text) => handle_pasted_text(app, &text).await,
        RuntimeEffect::UpdateRssConfig(settings) => app.apply_browser_config_update(*settings),
        RuntimeEffect::SyncRss => app.enqueue_command(BrowserCommand::RssSyncNow),
        RuntimeEffect::DownloadRssPreview(item) => {
            app.enqueue_command(BrowserCommand::RssDownloadPreview {
                item: BrowserRssPreview {
                    dedupe_key: item.dedupe_key,
                    title: item.title,
                    link: item.link,
                    guid: item.guid,
                    source: item.source,
                    date_iso: item.date_iso,
                },
            });
        }
    }
    None
}

fn enqueue_control_request(app: &mut BrowserSession, request: ControlRequest) {
    app.submit_control_request(request, false);
}

fn enqueue_control_request_with_config_policy(
    app: &mut BrowserSession,
    request: ControlRequest,
    replace_existing_config: bool,
) {
    app.submit_control_request(request, replace_existing_config);
}

async fn execute_browser_confirm_decision(
    app: &mut BrowserSession,
    decision: ConfirmDecision,
) -> Option<RuntimeOutcome> {
    match decision {
        ConfirmDecision::ToConfig(mut config) => {
            if let Some(item) = config.items.get(config.selected_index).copied() {
                let update = crate::tui::screens::config::merge_config_item_into_current(
                    &config.settings_edit,
                    &app.client_configs,
                    item,
                    app.app_state.runtime_paths.shared_mode,
                    app.is_current_shared_follower(),
                    &config.network_interface_inventory.interfaces,
                );
                if update != app.client_configs {
                    app.apply_browser_config_update(update);
                }
                config.settings_edit = Box::new(app.client_configs.clone());
            }
            Some(RuntimeOutcome::BrowserConfig(config))
        }
        ConfirmDecision::Download(payload) => {
            execute_download_confirmation(app, payload).map(RuntimeOutcome::BrowserTransition)
        }
        ConfirmDecision::File(path) => {
            if app.client_configs.always_show_add_location_prompt {
                if !app.open_manual_torrent_file_browser(path) {
                    app.app_state.system_error = Some(
                        "The simulated torrent preview is not ready for configuration.".to_string(),
                    );
                }
                None
            } else {
                app.enqueue_command(BrowserCommand::AddTorrentFromFile {
                    path,
                    download_path: app.client_configs.default_download_folder.clone(),
                    container_name: None,
                    validation_status: false,
                    file_priorities: Vec::new(),
                    replace_existing_config: false,
                });
                Some(RuntimeOutcome::BrowserTransition(BrowserTransition::Close))
            }
        }
        ConfirmDecision::None => None,
    }
}

fn execute_download_confirmation(
    app: &mut BrowserSession,
    payload: DownloadConfirmPayload,
) -> Option<BrowserTransition> {
    match payload.target {
        DownloadSelectionTarget::PendingAdd => {
            if let Some(path) = app.app_state.pending_torrent_path.take() {
                enqueue_control_request(
                    app,
                    ControlRequest::AddTorrentFile {
                        source_path: path,
                        download_path: Some(payload.base_path),
                        container_name: payload.container_name_to_use,
                        validation_status: false,
                        file_priorities: priority_overrides(payload.file_priorities),
                    },
                );
                return Some(BrowserTransition::Close);
            } else if !app.app_state.pending_torrent_link.is_empty() {
                enqueue_control_request_with_config_policy(
                    app,
                    ControlRequest::AddMagnet {
                        magnet_link: app.app_state.pending_torrent_link.clone(),
                        download_path: Some(payload.base_path),
                        container_name: payload.container_name_to_use,
                        validation_status: false,
                        file_priorities: priority_overrides(payload.file_priorities),
                    },
                    true,
                );
                app.app_state.pending_torrent_link.clear();
                return Some(BrowserTransition::Close);
            }
            None
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
            enqueue_control_request(
                app,
                ControlRequest::SetTorrentConfig {
                    info_hash_hex: hex::encode(info_hash),
                    download_path,
                    container_name,
                    file_priorities: priority_overrides(file_priorities),
                },
            );
            Some(BrowserTransition::Close)
        }
    }
}

async fn handle_pasted_text(app: &mut BrowserSession, pasted_text: &str) {
    app.ingest_pasted_text(pasted_text);
}
