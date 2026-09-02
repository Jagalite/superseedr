// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native runtime execution for effects emitted by the shared TUI reducers.

use super::{App, AppCommand};
use crate::app::{AppMode, BrowserSearchState, DownloadSelectionTarget};
use crate::integrations::control::ControlRequest;
use crate::theme::{Theme, ThemeName};
use crate::torrent_manager::ManagerCommand;
use crate::tui::effects::{
    priority_overrides, BrowserDialogEffect, BrowserFsEffect, BrowserTransition, ConfigEffect,
    ConfirmDecision, DeleteConfirmEffect, DownloadConfirmPayload, JournalEffect, RssRuntimeEffect,
    TorrentManagementEffect, TuiEffect, UiEffect,
};
use crate::tui::events;
use std::collections::HashMap;
use std::path::Path;
use tokio::sync::{broadcast, mpsc};
use tracing::{event as tracing_event, Level};

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

fn execute_journal_effects(app: &mut App, effects: Vec<JournalEffect>) {
    let mut commands = Vec::new();
    for effect in effects {
        match effect {
            JournalEffect::ReplaySource(path) => {
                if !path.exists() {
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
                events::open_config_screen_state(&mut app.app_state, &app.client_configs);
                app.refresh_config_network_interfaces();
            }
            UiEffect::BroadcastManagerDataRate(new_rate) => {
                broadcast_manager_data_rate(app, new_rate);
            }
            UiEffect::ApplyThemePrev => apply_adjacent_theme(app, false),
            UiEffect::ApplyThemeNext => apply_adjacent_theme(app, true),
            UiEffect::PersistVisualizationSelections => persist_visualization_selections(app),
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

fn refresh_config_network_interfaces(app: &mut App) {
    enqueue_commands(app, vec![AppCommand::RefreshConfigNetworkInterfaces]);
}

fn apply_browser_transition(app: &mut App, transition: BrowserTransition) {
    events::apply_browser_transition_state(&mut app.app_state, transition);
    if transition == BrowserTransition::ToConfig {
        app.refresh_config_network_interfaces();
    }
}

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

fn broadcast_manager_data_rate(app: &mut App, new_rate: u64) {
    for manager_tx in app.torrent_manager_command_txs.values() {
        let _ = manager_tx.try_send(ManagerCommand::SetDataRate(new_rate));
    }
}

fn apply_adjacent_theme(app: &mut App, next: bool) {
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

fn persist_visualization_selections(app: &mut App) {
    app.persist_visualization_selections();
}

async fn handle_pasted_text(app: &mut App, pasted_text: &str) {
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

pub(crate) fn spawn_app_command_batch_sender(
    app_command_tx: mpsc::Sender<AppCommand>,
    mut shutdown_rx: broadcast::Receiver<()>,
    commands: Vec<AppCommand>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        send_app_command_batch_until_shutdown(&app_command_tx, &mut shutdown_rx, commands).await;
    })
}

async fn send_app_command_batch_until_shutdown(
    app_command_tx: &mpsc::Sender<AppCommand>,
    shutdown_rx: &mut broadcast::Receiver<()>,
    commands: Vec<AppCommand>,
) {
    for command in commands {
        tokio::select! {
            result = app_command_tx.send(command) => {
                if result.is_err() {
                    break;
                }
            }
            shutdown = shutdown_rx.recv() => {
                match shutdown {
                    Ok(())
                    | Err(broadcast::error::RecvError::Closed)
                    | Err(broadcast::error::RecvError::Lagged(_)) => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod command_sender_tests {
    use super::*;
    use std::time::Duration;

    fn pause_command(byte: u8) -> AppCommand {
        AppCommand::SubmitControlRequest(ControlRequest::Pause {
            info_hash_hex: hex::encode(vec![byte; 20]),
        })
    }

    #[tokio::test]
    async fn app_command_batch_sender_sends_batch_larger_than_channel_capacity() {
        let (app_command_tx, mut app_command_rx) = mpsc::channel(1);
        let (shutdown_tx, _) = broadcast::channel(1);

        let handle = spawn_app_command_batch_sender(
            app_command_tx,
            shutdown_tx.subscribe(),
            vec![pause_command(1), pause_command(2), pause_command(3)],
        );

        let mut received = 0;
        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(1), app_command_rx.recv())
                .await
                .expect("timed out waiting for submitted app command")
                .expect("app command channel closed before batch completed");
            received += 1;
        }

        handle.await.expect("app command sender task panicked");
        assert_eq!(received, 3);
    }

    #[tokio::test]
    async fn app_command_batch_sender_stops_when_shutdown_is_signaled() {
        let (app_command_tx, mut app_command_rx) = mpsc::channel(1);
        let (shutdown_tx, _) = broadcast::channel(1);

        let handle = spawn_app_command_batch_sender(
            app_command_tx,
            shutdown_tx.subscribe(),
            vec![pause_command(1), pause_command(2), pause_command(3)],
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while app_command_rx.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("timed out waiting for first queued app command");

        shutdown_tx.send(()).expect("broadcast shutdown");
        handle.await.expect("app command sender task panicked");

        let mut received = 0;
        while app_command_rx.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, 1);
    }
}
