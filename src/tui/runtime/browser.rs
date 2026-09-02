// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser runtime execution for effects emitted by the shared TUI reducers.

use crate::app::{DownloadSelectionTarget, FilePriority};
use crate::integrations::control::ControlRequest;
use crate::tui::effects::{
    priority_overrides, BrowserTransition, ConfigNetworkInterfaceRefresh, ConfirmDecision,
    DownloadConfirmPayload, RuntimeEffect, RuntimeOutcome,
};
use crate::web_integration::{
    BrowserCommand, BrowserFilePriority, BrowserFilePriorityOverride, BrowserRssPreview,
    BrowserSession,
};

pub(crate) async fn execute_runtime_effect(
    app: &mut BrowserSession,
    effect: RuntimeEffect,
) -> Option<RuntimeOutcome> {
    match effect {
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
        RuntimeEffect::BroadcastManagerDataRate(_) => {}
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
    let command = match request {
        ControlRequest::AddMagnet {
            magnet_link,
            download_path,
            container_name,
            validation_status,
            ..
        } => BrowserCommand::AddMagnet {
            magnet_link,
            download_path,
            container_name,
            validation_status,
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
        ControlRequest::Pause { info_hash_hex } => BrowserCommand::Pause { info_hash_hex },
        ControlRequest::Resume { info_hash_hex } => BrowserCommand::Resume { info_hash_hex },
        ControlRequest::Delete {
            info_hash_hex,
            delete_files,
        } => BrowserCommand::Delete {
            info_hash_hex,
            delete_files,
        },
        ControlRequest::SetTorrentConfig {
            info_hash_hex,
            download_path,
            container_name,
            file_priorities,
        } => BrowserCommand::SetTorrentConfig {
            info_hash_hex,
            download_path,
            container_name,
            file_priorities: browser_priority_overrides(file_priorities),
        },
        _ => return,
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
                enqueue_control_request(
                    app,
                    ControlRequest::AddMagnet {
                        magnet_link: app.app_state.pending_torrent_link.clone(),
                        download_path: Some(payload.base_path),
                        container_name: payload.container_name_to_use,
                        validation_status: false,
                        file_priorities: priority_overrides(payload.file_priorities),
                    },
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
    let pasted_text = pasted_text.trim();
    if !pasted_text.starts_with("magnet:") {
        return;
    }
    if app.client_configs.always_show_add_location_prompt {
        let Some(info_hash) =
            crate::web_integration::canonical_browser_magnet_info_hash(pasted_text)
        else {
            app.app_state.system_error = Some(
                "Pasted content is not a valid magnet with a supported info hash.".to_string(),
            );
            return;
        };
        let id = info_hash.first().copied().unwrap_or_default();
        app.open_manual_magnet_browser(pasted_text.to_string(), format!("Orbit Archive {id:02x}"));
    } else {
        enqueue_control_request(
            app,
            ControlRequest::AddMagnet {
                magnet_link: pasted_text.to_string(),
                download_path: app.client_configs.default_download_folder.clone(),
                container_name: None,
                validation_status: false,
                file_priorities: Vec::new(),
            },
        );
    }
}
