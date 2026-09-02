// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Runtime-side execution for effects emitted by the platform-neutral TUI reducers.

use super::command_sender::spawn_app_command_batch_sender;
use super::{App, AppCommand, AppMode, ConfigItem, FilePriority, RssScreen, TorrentControlState};
use crate::integrations::control::{ControlFilePriorityOverride, ControlRequest};
use crate::terminal_event::Event;
use crate::tui::kernel::effects::{
    BrowserFsEffect, BrowserTransition, ConfigEffect, DeleteConfirmEffect, JournalEffect,
    RssRuntimeEffect, TorrentManagementEffect, TuiEffect, UiEffect,
};
use crate::tui::screens::torrents;
use std::collections::HashMap;
use strum::IntoEnumIterator;
use web_time::Instant;

#[cfg(target_arch = "wasm32")]
#[path = "tui_effect_executor/browser.rs"]
mod browser;
#[cfg(not(target_arch = "wasm32"))]
#[path = "tui_effect_executor/native.rs"]
mod native;

#[cfg(target_arch = "wasm32")]
use browser as platform;
#[cfg(target_arch = "wasm32")]
pub(crate) use browser::execute_browser_dialog_effects;
#[cfg(not(target_arch = "wasm32"))]
use native as platform;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::{execute_browser_dialog_effects, native_pasted_text_supported};

#[cfg(all(not(target_arch = "wasm32"), test))]
pub(crate) async fn execute_native_confirm_decision(
    app: &mut App,
    decision: crate::tui::kernel::effects::ConfirmDecision,
) -> Option<BrowserTransition> {
    native::execute_native_confirm_decision(app, decision).await
}

pub(crate) async fn handle_event(app: &mut App, event: Event) {
    handle_event_at(event, app, Instant::now()).await;
}

pub(crate) async fn flush_pending_paste_burst(app: &mut App) {
    flush_pending_paste_burst_at(app, Instant::now()).await;
}

pub(crate) async fn handle_event_at(event: Event, app: &mut App, now: Instant) {
    let pending_text =
        crate::tui::kernel::reducer::pending_paste_text_before_event(&event, &app.app_state)
            .map(str::to_owned);
    let pending_is_paste = pending_text
        .as_deref()
        .is_some_and(|text| app.accepts_pasted_text(text));
    let translated = crate::tui::kernel::reducer::translate_event(
        event,
        &mut app.app_state,
        now,
        pending_is_paste,
    );
    reduce_and_execute_events(app, translated).await;
}

pub(crate) async fn flush_pending_paste_burst_at(app: &mut App, now: Instant) {
    let pending_text =
        crate::tui::kernel::reducer::due_paste_text(&app.app_state, now).map(str::to_owned);
    let pending_is_paste = pending_text
        .as_deref()
        .is_some_and(|text| app.accepts_pasted_text(text));
    let translated =
        crate::tui::kernel::reducer::flush_due_events(&mut app.app_state, now, pending_is_paste);
    reduce_and_execute_events(app, translated).await;
}

async fn reduce_and_execute_events(app: &mut App, events: Vec<Event>) {
    if events.is_empty() {
        return;
    }

    for event in events {
        let settings = app.client_configs.clone();
        let shared_follower = app.is_current_shared_follower();
        let effects = crate::tui::kernel::reducer::reduce_event(
            event,
            &mut app.app_state,
            &settings,
            shared_follower,
        );
        execute_tui_effects(app, effects).await;
    }
    app.app_state.ui.needs_redraw = true;
}

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

fn apply_browser_transition(app: &mut App, transition: BrowserTransition) {
    match transition {
        BrowserTransition::ToNormal => {
            app.app_state
                .ui
                .file_browser
                .invalidate_browser_generation();
            app.app_state
                .ui
                .file_browser
                .return_to_torrent_management_on_close = false;
            app.app_state.mode = AppMode::Normal;
        }
        BrowserTransition::ToConfig => {
            app.app_state
                .ui
                .file_browser
                .invalidate_browser_generation();
            app.app_state
                .ui
                .file_browser
                .return_to_torrent_management_on_close = false;
            app.app_state.mode = AppMode::Config;
            platform::refresh_config_network_interfaces_on_open(app);
        }
        BrowserTransition::Close => {
            let return_to_management = app
                .app_state
                .ui
                .file_browser
                .return_to_torrent_management_on_close;
            app.app_state
                .ui
                .file_browser
                .invalidate_browser_generation();
            app.app_state
                .ui
                .file_browser
                .return_to_torrent_management_on_close = false;
            app.app_state.mode = if return_to_management {
                AppMode::TorrentManagement
            } else {
                AppMode::Normal
            };
        }
    }
}

fn priority_overrides(
    priorities: HashMap<usize, FilePriority>,
) -> Vec<ControlFilePriorityOverride> {
    let mut overrides: Vec<_> = priorities
        .into_iter()
        .filter(|(_, priority)| !matches!(priority, FilePriority::Normal))
        .map(|(file_index, priority)| ControlFilePriorityOverride {
            file_index,
            priority,
        })
        .collect();
    overrides.sort_by_key(|value| value.file_index);
    overrides
}

pub(crate) fn execute_browser_fs_effects(
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

pub(crate) fn execute_config_effects(app: &mut App, effects: Vec<ConfigEffect>) {
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
            ConfigEffect::RefreshNetworkInterfaces => refresh_config_network_interfaces(app),
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

pub(crate) fn execute_journal_effects(app: &mut App, effects: Vec<JournalEffect>) {
    let mut commands = Vec::new();
    for effect in effects {
        match effect {
            JournalEffect::ReplaySource(path) => {
                if !platform::replay_source_exists(&path) {
                    app.app_state.ui.journal.status_message =
                        Some("Replay source file is no longer available".to_string());
                    continue;
                }

                let command = match path.extension().and_then(|extension| extension.to_str()) {
                    Some(extension) if extension.eq_ignore_ascii_case("torrent") => {
                        AppCommand::AddTorrentFromFile(path)
                    }
                    Some(extension) if extension.eq_ignore_ascii_case("magnet") => {
                        AppCommand::AddMagnetFromFile(path)
                    }
                    Some(extension) if extension.eq_ignore_ascii_case("path") => {
                        AppCommand::AddTorrentFromPathFile(path)
                    }
                    _ => continue,
                };
                commands.push(command);
            }
        }
    }
    enqueue_commands(app, commands);
}

pub(crate) fn execute_rss_effects(app: &mut App, effects: Vec<RssRuntimeEffect>) {
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

pub(crate) async fn execute_normal_effects(app: &mut App, effects: Vec<UiEffect>) {
    for effect in effects {
        match effect {
            UiEffect::ToPowerSaving => app.app_state.mode = AppMode::PowerSaving,
            UiEffect::ToDeleteConfirm => app.app_state.mode = AppMode::DeleteConfirm,
            UiEffect::OpenAddTorrentFileBrowser => app.open_add_torrent_file_browser(),
            UiEffect::OpenExistingTorrentFileBrowser(info_hash) => {
                app.open_existing_torrent_file_browser(info_hash);
            }
            UiEffect::OpenConfigScreen => {
                *app.app_state.ui.config.settings_edit = app.client_configs.clone();
                app.app_state.ui.config.selected_index = 0;
                app.app_state.ui.config.items = ConfigItem::iter().collect();
                app.app_state.ui.config.active_pane = super::ConfigPane::Settings;
                app.app_state.ui.config.editing = None;
                app.app_state.ui.config.network_interface_selection_pending = false;
                app.app_state.mode = AppMode::Config;
                platform::refresh_config_network_interfaces_on_open(app);
            }
            UiEffect::BroadcastManagerDataRate(new_rate) => {
                platform::broadcast_manager_data_rate(app, new_rate);
            }
            UiEffect::ApplyThemePrev => platform::apply_adjacent_theme(app, false),
            UiEffect::ApplyThemeNext => platform::apply_adjacent_theme(app, true),
            UiEffect::PersistVisualizationSelections => {
                platform::persist_visualization_selections(app);
            }
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
            UiEffect::OpenRssScreen => {
                app.app_state.ui.rss.active_screen = RssScreen::Unified;
                app.app_state.mode = AppMode::Rss;
            }
            UiEffect::OpenJournalScreen => {
                app.app_state.ui.journal.selected_index = 0;
                app.app_state.ui.journal.scroll_offset = 0;
                app.app_state.mode = AppMode::Journal;
            }
            UiEffect::OpenPeerManagementScreen => {
                app.refresh_peer_management_screen();
                app.app_state.ui.peer_management.selected_index = 0;
                app.app_state.ui.peer_management.show_details = false;
                app.app_state.ui.peer_management.status_message = None;
                app.app_state.mode = AppMode::PeerManagement;
            }
            UiEffect::OpenTorrentManagementScreen => {
                app.app_state.ui.torrent_management.status_message = None;
                app.app_state.ui.torrent_management.review_scroll_offset = 0;
                app.app_state.mode = AppMode::TorrentManagement;
                torrents::initialize_torrent_management_cursor(&mut app.app_state);
            }
            UiEffect::HandlePastedText(text) => platform::handle_pasted_text(app, &text).await,
        }
    }
}

fn refresh_config_network_interfaces(app: &mut App) {
    enqueue_commands(app, vec![AppCommand::RefreshConfigNetworkInterfaces]);
}

pub(crate) fn execute_delete_confirm_effects(app: &mut App, effects: Vec<DeleteConfirmEffect>) {
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
                if !app.is_current_shared_follower() {
                    if let Some(torrent) = app.app_state.torrents.get_mut(&info_hash) {
                        torrent.latest_state.torrent_control_state = TorrentControlState::Deleting;
                        torrent.latest_state.delete_files =
                            app.app_state.ui.delete_confirm.with_files;
                    }
                }
            }
            DeleteConfirmEffect::ToNormal => app.app_state.mode = AppMode::Normal,
        }
    }
    enqueue_commands(app, commands);
}

pub(crate) fn execute_torrent_management_effects(
    app: &mut App,
    effects: Vec<TorrentManagementEffect>,
) {
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
                if !app.is_current_shared_follower() {
                    if let Some(torrent) = app.app_state.torrents.get_mut(&info_hash) {
                        torrent.latest_state.torrent_control_state = state;
                        torrent.latest_state.delete_files = delete_files;
                    }
                }
            }
            TorrentManagementEffect::OpenExistingTorrentFileBrowser(info_hash) => {
                app.open_existing_torrent_file_browser(info_hash);
            }
        }
    }
    enqueue_commands(app, commands);
}

fn enqueue_commands(app: &App, commands: Vec<AppCommand>) {
    if commands.is_empty() {
        return;
    }
    spawn_app_command_batch_sender(
        app.app_command_tx.clone(),
        app.shutdown_tx.subscribe(),
        commands,
    );
}
