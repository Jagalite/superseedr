// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native runtime execution for effects emitted by the shared TUI reducers.

use super::{App, AppCommand};
use crate::app::DownloadSelectionTarget;
use crate::integrations::control::ControlRequest;
use crate::theme::{Theme, ThemeName};
use crate::torrent_manager::ManagerCommand;
use crate::tui::effects::{
    priority_overrides, BrowserTransition, ConfigNetworkInterfaceRefresh, ConfirmDecision,
    DownloadConfirmPayload, RuntimeEffect, RuntimeOutcome,
};
use std::collections::HashMap;
use std::path::Path;
use tokio::sync::{broadcast, mpsc};
use tracing::{event as tracing_event, Level};

pub(crate) async fn execute_runtime_effect(
    app: &mut App,
    effect: RuntimeEffect,
) -> Option<RuntimeOutcome> {
    match effect {
        RuntimeEffect::FetchFileTree {
            browser_generation,
            path,
            browser_mode,
            preserve_browser_mode,
            highlight_path,
        } => enqueue_commands(
            app,
            vec![AppCommand::FetchFileTree {
                browser_generation,
                path,
                browser_mode,
                preserve_browser_mode,
                highlight_path,
            }],
        ),
        RuntimeEffect::ConfirmBrowserSelection(decision) => {
            return execute_native_confirm_decision(app, decision).await;
        }
        RuntimeEffect::CleanupPendingPreview(info_hash) => {
            app.cleanup_pending_magnet_preview_runtime_for(info_hash);
        }
        RuntimeEffect::SyncTorrentFilePreview => app.sync_torrent_file_preview(),
        RuntimeEffect::ReplayJournalSource(path) => {
            if !path.exists() {
                app.app_state.ui.journal.status_message =
                    Some("Replay source file is no longer available".to_string());
                return None;
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
                _ => return None,
            };
            enqueue_commands(app, vec![command]);
        }
        RuntimeEffect::OpenAddTorrentFileBrowser => app.open_add_torrent_file_browser(),
        RuntimeEffect::OpenExistingTorrentFileBrowser(info_hash) => {
            app.open_existing_torrent_file_browser(info_hash);
        }
        RuntimeEffect::RefreshPeerManagement => app.refresh_peer_management_screen(),
        RuntimeEffect::ApplyConfig(settings) => {
            app.apply_config_update_from_ui(*settings).await;
            return Some(RuntimeOutcome::ConfigApplied(app.client_configs.clone()));
        }
        RuntimeEffect::RefreshConfigNetworkInterfaces(
            ConfigNetworkInterfaceRefresh::OnOpen | ConfigNetworkInterfaceRefresh::Explicit,
        ) => {
            enqueue_commands(app, vec![AppCommand::RefreshConfigNetworkInterfaces]);
        }
        RuntimeEffect::BroadcastManagerDataRate(rate) => broadcast_manager_data_rate(app, rate),
        RuntimeEffect::ApplyThemePrevious => apply_adjacent_theme(app, false),
        RuntimeEffect::ApplyThemeNext => apply_adjacent_theme(app, true),
        RuntimeEffect::PersistVisualizationSelections => persist_visualization_selections(app),
        RuntimeEffect::SubmitControlRequest(request) => {
            enqueue_commands(app, vec![AppCommand::SubmitControlRequest(request)]);
        }
        RuntimeEffect::HandlePastedText(text) => handle_pasted_text(app, &text).await,
        RuntimeEffect::UpdateRssConfig(settings) => {
            enqueue_commands(app, vec![AppCommand::UpdateConfig(*settings)]);
        }
        RuntimeEffect::SyncRss => enqueue_commands(app, vec![AppCommand::RssSyncNow]),
        RuntimeEffect::DownloadRssPreview(item) => {
            enqueue_commands(app, vec![AppCommand::RssDownloadPreview(item)]);
        }
    }
    None
}

pub(crate) async fn execute_native_confirm_decision(
    app: &mut App,
    decision: ConfirmDecision,
) -> Option<RuntimeOutcome> {
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
            Some(RuntimeOutcome::BrowserConfig(config_ui))
        }
        ConfirmDecision::Download(payload) => {
            execute_download_confirmation(app, payload).map(RuntimeOutcome::BrowserTransition)
        }
        ConfirmDecision::File(path) => {
            if !path.is_file() {
                return None;
            }
            enqueue_commands(app, vec![AppCommand::AddTorrentFromFile(path)]);
            Some(RuntimeOutcome::BrowserTransition(
                BrowserTransition::ToNormal,
            ))
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
    if app.tui_command_batch_tx.send(commands).is_err() {
        tracing::warn!(target: "superseedr", "TUI command sender is unavailable");
    }
}

pub(crate) fn spawn_serialized_app_command_sender(
    app_command_tx: mpsc::Sender<AppCommand>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> (
    mpsc::UnboundedSender<Vec<AppCommand>>,
    tokio::task::JoinHandle<()>,
) {
    let (batch_tx, mut batch_rx) = mpsc::unbounded_channel::<Vec<AppCommand>>();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                shutdown = shutdown_rx.recv() => {
                    match shutdown {
                        Ok(())
                        | Err(broadcast::error::RecvError::Closed)
                        | Err(broadcast::error::RecvError::Lagged(_)) => break,
                    }
                }
                batch = batch_rx.recv() => {
                    let Some(commands) = batch else {
                        break;
                    };
                    if !send_app_command_batch_until_shutdown(
                        &app_command_tx,
                        &mut shutdown_rx,
                        commands,
                    )
                    .await
                    {
                        break;
                    }
                }
            }
        }
    });
    (batch_tx, task)
}

#[cfg(test)]
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
) -> bool {
    for command in commands {
        tokio::select! {
            result = app_command_tx.send(command) => {
                if result.is_err() {
                    return false;
                }
            }
            shutdown = shutdown_rx.recv() => {
                match shutdown {
                    Ok(())
                    | Err(broadcast::error::RecvError::Closed)
                    | Err(broadcast::error::RecvError::Lagged(_)) => return false,
                }
            }
        }
    }
    true
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

    #[tokio::test]
    async fn serialized_sender_preserves_order_across_successive_input_batches() {
        let (app_command_tx, mut app_command_rx) = mpsc::channel(1);
        let (shutdown_tx, _) = broadcast::channel(1);
        let (batch_tx, task) =
            spawn_serialized_app_command_sender(app_command_tx, shutdown_tx.subscribe());

        batch_tx
            .send(vec![pause_command(1), pause_command(2)])
            .expect("queue first input batch");
        batch_tx
            .send(vec![pause_command(3), pause_command(4)])
            .expect("queue second input batch");

        let mut received = Vec::new();
        for _ in 0..4 {
            let command = tokio::time::timeout(Duration::from_secs(1), app_command_rx.recv())
                .await
                .expect("timed out waiting for serialized command")
                .expect("serialized command channel closed");
            let AppCommand::SubmitControlRequest(ControlRequest::Pause { info_hash_hex }) = command
            else {
                panic!("unexpected command from serialized sender");
            };
            received.push(info_hash_hex);
        }

        assert_eq!(
            received,
            [1_u8, 2, 3, 4]
                .into_iter()
                .map(|byte| hex::encode(vec![byte; 20]))
                .collect::<Vec<_>>()
        );
        shutdown_tx.send(()).expect("broadcast shutdown");
        task.await.expect("serialized sender task panicked");
    }
}
