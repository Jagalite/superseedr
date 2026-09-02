// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser runtime execution for effects emitted by the shared TUI reducers.

use super::{App, AppCommand};
use crate::app::{AppMode, BrowserSearchState, DownloadSelectionTarget};
use crate::integrations::control::ControlRequest;
use crate::tui::effects::{
    priority_overrides, BrowserDialogEffect, BrowserFsEffect, BrowserTransition, ConfigEffect,
    ConfirmDecision, DeleteConfirmEffect, DownloadConfirmPayload, JournalEffect, RssRuntimeEffect,
    TorrentManagementEffect, TuiEffect, UiEffect,
};
use crate::tui::events;

pub(crate) async fn execute_tui_effects(app: &mut App, effects: Vec<TuiEffect>) {
    for effect in effects {
        match effect {
            TuiEffect::BrowserFs {
                browser_generation,
                effects,
            } => execute_browser_fs_effects(app, browser_generation, effects),
            TuiEffect::BrowserDialog(effects) => {
                execute_browser_dialog_effects(app, effects).await;
            }
            TuiEffect::SyncTorrentFilePreview => app.sync_torrent_file_preview(),
            TuiEffect::Journal(effects) => execute_journal_effects(app, effects),
            TuiEffect::TorrentManagement(effects) => {
                execute_torrent_management_effects(app, effects);
            }
            TuiEffect::Normal(effects) => execute_normal_effects(app, effects).await,
            TuiEffect::ApplyConfig(settings) => {
                app.apply_config_update_from_ui(*settings).await;
                *app.app_state.ui.config.settings_edit = app.client_configs.clone();
                app.app_state.ui.config.network_interface_selection_pending = false;
            }
            TuiEffect::Config(effects) => execute_config_effects(app, effects),
            TuiEffect::DeleteConfirm(effects) => execute_delete_confirm_effects(app, effects),
            TuiEffect::Rss(effects) => execute_rss_effects(app, effects),
        }
    }
}

fn execute_browser_fs_effects(
    app: &mut App,
    browser_generation: u64,
    effects: Vec<BrowserFsEffect>,
) {
    let commands = effects
        .into_iter()
        .map(|effect| match effect {
            BrowserFsEffect::FetchFileTree {
                path,
                browser_mode,
                highlight_path,
            } => AppCommand::FetchFileTree {
                browser_generation,
                path,
                browser_mode,
                preserve_browser_mode: true,
                highlight_path,
            },
        })
        .collect();
    enqueue_commands(app, commands);
}

fn execute_config_effects(app: &mut App, effects: Vec<ConfigEffect>) {
    let mut commands = Vec::new();
    for effect in effects {
        match effect {
            ConfigEffect::OpenPathBrowser { path, browser_mode } => {
                app.app_state.ui.file_browser.browser_generation = app
                    .app_state
                    .ui
                    .file_browser
                    .browser_generation
                    .wrapping_add(1);
                commands.push(AppCommand::FetchFileTree {
                    browser_generation: app.app_state.ui.file_browser.browser_generation,
                    path,
                    browser_mode: *browser_mode,
                    preserve_browser_mode: false,
                    highlight_path: None,
                });
            }
            ConfigEffect::RefreshNetworkInterfaces => {}
            ConfigEffect::ApplySettings => {
                debug_assert!(
                    false,
                    "ApplySettings must be reduced before effect execution"
                );
            }
        }
    }
    enqueue_commands(app, commands);
}

fn execute_journal_effects(app: &mut App, effects: Vec<JournalEffect>) {
    let commands = effects
        .into_iter()
        .filter_map(|effect| match effect {
            JournalEffect::ReplaySource(path) => {
                match path.extension().and_then(|extension| extension.to_str()) {
                    Some(extension) if extension.eq_ignore_ascii_case("torrent") => {
                        Some(AppCommand::AddTorrentFromFile(path))
                    }
                    Some(extension) if extension.eq_ignore_ascii_case("magnet") => {
                        Some(AppCommand::AddMagnetFromFile(path))
                    }
                    Some(extension) if extension.eq_ignore_ascii_case("path") => {
                        Some(AppCommand::AddTorrentFromPathFile(path))
                    }
                    _ => None,
                }
            }
        })
        .collect();
    enqueue_commands(app, commands);
}

fn execute_rss_effects(app: &mut App, effects: Vec<RssRuntimeEffect>) {
    let commands = effects
        .into_iter()
        .map(|effect| match effect {
            RssRuntimeEffect::UpdateConfig(settings) => AppCommand::UpdateConfig(*settings),
            RssRuntimeEffect::SyncNow => AppCommand::RssSyncNow,
            RssRuntimeEffect::DownloadPreview(item) => AppCommand::RssDownloadPreview(item),
        })
        .collect();
    enqueue_commands(app, commands);
}

async fn execute_normal_effects(app: &mut App, effects: Vec<UiEffect>) {
    for effect in effects {
        match effect {
            UiEffect::ToPowerSaving => app.app_state.mode = AppMode::PowerSaving,
            UiEffect::ToDeleteConfirm => app.app_state.mode = AppMode::DeleteConfirm,
            UiEffect::OpenAddTorrentFileBrowser => app.open_add_torrent_file_browser(),
            UiEffect::OpenExistingTorrentFileBrowser(info_hash) => {
                app.open_existing_torrent_file_browser(info_hash);
            }
            UiEffect::OpenConfigScreen => {
                events::open_config_screen_state(&mut app.app_state, &app.client_configs);
            }
            UiEffect::BroadcastManagerDataRate(_) => {}
            UiEffect::ApplyThemePrev => app.apply_adjacent_theme(false),
            UiEffect::ApplyThemeNext => app.apply_adjacent_theme(true),
            UiEffect::PersistVisualizationSelections => {}
            UiEffect::SendPause(info_hash) => enqueue_commands(
                app,
                vec![AppCommand::SubmitControlRequest(ControlRequest::Pause {
                    info_hash_hex: hex::encode(info_hash),
                })],
            ),
            UiEffect::SendResume(info_hash) => enqueue_commands(
                app,
                vec![AppCommand::SubmitControlRequest(ControlRequest::Resume {
                    info_hash_hex: hex::encode(info_hash),
                })],
            ),
            UiEffect::OpenHelpScreen => app.app_state.mode = AppMode::Help,
            UiEffect::OpenRssScreen => events::open_rss_screen_state(&mut app.app_state),
            UiEffect::OpenJournalScreen => events::open_journal_screen_state(&mut app.app_state),
            UiEffect::OpenPeerManagementScreen => {
                app.refresh_peer_management_screen();
                events::open_peer_management_screen_state(&mut app.app_state);
            }
            UiEffect::OpenTorrentManagementScreen => {
                events::open_torrent_management_screen_state(&mut app.app_state);
            }
            UiEffect::HandlePastedText(text) => handle_pasted_text(app, &text).await,
        }
    }
}

fn execute_delete_confirm_effects(app: &mut App, effects: Vec<DeleteConfirmEffect>) {
    let mut commands = Vec::new();
    for effect in effects {
        match effect {
            DeleteConfirmEffect::SendManagerCommand {
                info_hash,
                with_files,
            } => commands.push(AppCommand::SubmitControlRequest(ControlRequest::Delete {
                info_hash_hex: hex::encode(info_hash),
                delete_files: with_files,
            })),
            DeleteConfirmEffect::MarkDeleting { info_hash } => {
                let shared_follower = app.is_current_shared_follower();
                events::mark_torrent_deleting_state(
                    &mut app.app_state,
                    &info_hash,
                    shared_follower,
                );
            }
            DeleteConfirmEffect::ToNormal => app.app_state.mode = AppMode::Normal,
        }
    }
    enqueue_commands(app, commands);
}

fn execute_torrent_management_effects(app: &mut App, effects: Vec<TorrentManagementEffect>) {
    let mut commands = Vec::new();
    for effect in effects {
        match effect {
            TorrentManagementEffect::ToNormal => app.app_state.mode = AppMode::Normal,
            TorrentManagementEffect::SubmitControlRequest(request) => {
                commands.push(AppCommand::SubmitControlRequest(request));
            }
            TorrentManagementEffect::MarkControlState {
                info_hash,
                state,
                delete_files,
            } => {
                let shared_follower = app.is_current_shared_follower();
                events::mark_torrent_control_state(
                    &mut app.app_state,
                    &info_hash,
                    state,
                    delete_files,
                    shared_follower,
                );
            }
            TorrentManagementEffect::OpenExistingTorrentFileBrowser(info_hash) => {
                app.open_existing_torrent_file_browser(info_hash);
            }
        }
    }
    enqueue_commands(app, commands);
}

fn apply_browser_transition(app: &mut App, transition: BrowserTransition) {
    events::apply_browser_transition_state(&mut app.app_state, transition);
}

fn enqueue_commands(app: &App, commands: Vec<AppCommand>) {
    if commands.is_empty() {
        return;
    }
    let _ = app
        .app_command_tx
        .try_send(AppCommand::BrowserBatch(commands));
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

async fn handle_pasted_text(app: &mut App, pasted_text: &str) {
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
