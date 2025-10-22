// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{
    App, AppMode, ConfigItem, SelectedHeader, TorrentControlState, PEER_HEADERS, TORRENT_HEADERS,
};
use crate::config::SortDirection;
use ratatui::crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEventKind};
use ratatui::style::{Color, Style};
use ratatui_explorer::{FileExplorer, Theme};
use std::path::Path;
use tracing::{event as tracing_event, Level};

use clipboard::{ClipboardContext, ClipboardProvider};

/// Handles all TUI events and updates the application state accordingly.
pub async fn handle_event(event: CrosstermEvent, app: &mut App) {
    if let CrosstermEvent::Key(key) = event {
        // --- WINDOWS LOGIC ---
        #[cfg(windows)]
        {
            let mut help_key_handled = false;
            // On Windows, we only get Press, so we just toggle
            if key.code == KeyCode::Char('m') && key.kind == KeyEventKind::Press {
                app.app_state.show_help = !app.app_state.show_help;
                help_key_handled = true;
            }

            if help_key_handled {
                app.app_state.ui_needs_redraw = true;
                return;
            }

            // If help is shown, consume all other key presses
            if app.app_state.show_help {
                return;
            }
        }

        // --- LINUX / MACOS LOGIC ---
        #[cfg(not(windows))]
        {
            // This is your original logic, which works on Linux
            let mut help_key_handled = false;
            if app.app_state.show_help {
                if key.code == KeyCode::Char('m') && key.kind == KeyEventKind::Release {
                    app.app_state.show_help = false;
                    help_key_handled = true;
                }
            } else if key.code == KeyCode::Char('m') && key.kind == KeyEventKind::Press {
                app.app_state.show_help = true;
                help_key_handled = true;
            }

            if help_key_handled {
                app.app_state.ui_needs_redraw = true;
                return;
            }
        }
    }

    match &mut app.app_state.mode {
        AppMode::Welcome => {
            if let CrosstermEvent::Key(key) = event {
                if key.kind == KeyEventKind::Press && key.code == KeyCode::Esc {
                    app.app_state.mode = AppMode::Normal;
                }
            }
        }
        AppMode::Normal => match event {
            CrosstermEvent::Key(key) => {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Esc => {
                            app.app_state.system_error = None;
                        }
                        KeyCode::Char('x') => {
                            app.app_state.anonymize_torrent_names =
                                !app.app_state.anonymize_torrent_names;
                        }
                        KeyCode::Char('z') => {
                            app.app_state.mode = AppMode::PowerSaving;
                            return;
                        }
                        KeyCode::Char('q') => {
                            app.app_state.should_quit = true;
                        }
                        KeyCode::Char('c') => {
                            let items = vec![
                                ConfigItem::ClientPort,
                                ConfigItem::DefaultDownloadFolder,
                                ConfigItem::WatchFolder,
                                ConfigItem::GlobalDownloadLimit,
                                ConfigItem::GlobalUploadLimit,
                            ];
                            app.app_state.mode = AppMode::Config {
                                settings_edit: Box::new(app.client_configs.clone()),
                                selected_index: 0,
                                items,
                                editing: None,
                            };
                        }
                        KeyCode::Char('t') => {
                            app.app_state.graph_mode = app.app_state.graph_mode.next();
                        }
                        KeyCode::Char('T') => {
                            app.app_state.graph_mode = app.app_state.graph_mode.prev();
                        }
                        KeyCode::Char('p') => {
                            if let Some(info_hash) = app
                                .app_state
                                .torrent_list_order
                                .get(app.app_state.selected_torrent_index)
                            {
                                if let (Some(torrent_display), Some(torrent_manager_command_tx)) = (
                                    app.app_state.torrents.get_mut(info_hash),
                                    app.torrent_manager_command_txs.get(info_hash),
                                ) {
                                    let (new_state, command) =
                                        match torrent_display.latest_state.torrent_control_state {
                                            TorrentControlState::Running => (
                                                TorrentControlState::Paused,
                                                crate::torrent_manager::ManagerCommand::Pause,
                                            ),
                                            TorrentControlState::Paused => (
                                                TorrentControlState::Running,
                                                crate::torrent_manager::ManagerCommand::Resume,
                                            ),
                                            TorrentControlState::Deleting => return,
                                        };
                                    torrent_display.latest_state.torrent_control_state = new_state;
                                    let torrent_manager_command_tx_clone =
                                        torrent_manager_command_tx.clone();
                                    tokio::spawn(async move {
                                        let _ =
                                            torrent_manager_command_tx_clone.send(command).await;
                                    });
                                }
                            }
                        }
                        KeyCode::Char('d') => {
                            if let Some(info_hash) = app
                                .app_state
                                .torrent_list_order
                                .get(app.app_state.selected_torrent_index)
                                .cloned()
                            {
                                app.app_state.mode = AppMode::DeleteConfirm {
                                    info_hash,
                                    with_files: false,
                                };
                            }
                        }
                        KeyCode::Char('D') => {
                            if let Some(info_hash) = app
                                .app_state
                                .torrent_list_order
                                .get(app.app_state.selected_torrent_index)
                                .cloned()
                            {
                                app.app_state.mode = AppMode::DeleteConfirm {
                                    info_hash,
                                    with_files: true,
                                };
                            }
                        }
                        KeyCode::Char('s') => {
                            match app.app_state.selected_header {
                                SelectedHeader::Torrent(i) => {
                                    let column = TORRENT_HEADERS[i];
                                    if app.app_state.torrent_sort.0 == column {
                                        app.app_state.torrent_sort.1 =
                                            if app.app_state.torrent_sort.1
                                                == SortDirection::Ascending
                                            {
                                                SortDirection::Descending
                                            } else {
                                                SortDirection::Ascending
                                            };
                                    } else {
                                        app.app_state.torrent_sort.0 = column;
                                        app.app_state.torrent_sort.1 = SortDirection::Descending;
                                    }
                                    app.sort_torrent_list();
                                }
                                SelectedHeader::Peer(i) => {
                                    let column = PEER_HEADERS[i];
                                    if app.app_state.peer_sort.0 == column {
                                        app.app_state.peer_sort.1 = if app.app_state.peer_sort.1
                                            == SortDirection::Ascending
                                        {
                                            SortDirection::Descending
                                        } else {
                                            SortDirection::Ascending
                                        };
                                    } else {
                                        app.app_state.peer_sort.0 = column;
                                        app.app_state.peer_sort.1 = SortDirection::Descending;
                                    }
                                }
                            };
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            app.app_state.selected_torrent_index =
                                app.app_state.selected_torrent_index.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if !app.app_state.torrent_list_order.is_empty() {
                                let new_index =
                                    app.app_state.selected_torrent_index.saturating_add(1);
                                if new_index < app.app_state.torrent_list_order.len() {
                                    app.app_state.selected_torrent_index = new_index;
                                }
                            }
                        }
                        KeyCode::Left | KeyCode::Char('h') => {
                            app.app_state.selected_header = match app.app_state.selected_header {
                                SelectedHeader::Torrent(0) => {
                                    if !app.app_state.torrent_list_order.is_empty() {
                                        SelectedHeader::Peer(PEER_HEADERS.len() - 1)
                                    } else {
                                        SelectedHeader::Torrent(0)
                                    }
                                }
                                SelectedHeader::Torrent(i) => SelectedHeader::Torrent(i - 1),
                                SelectedHeader::Peer(0) => {
                                    SelectedHeader::Torrent(TORRENT_HEADERS.len() - 1)
                                }
                                SelectedHeader::Peer(i) => SelectedHeader::Peer(i - 1),
                            };
                        }
                        KeyCode::Right | KeyCode::Char('l') => {
                            app.app_state.selected_header = match app.app_state.selected_header {
                                SelectedHeader::Torrent(i) if i < TORRENT_HEADERS.len() - 1 => {
                                    SelectedHeader::Torrent(i + 1)
                                }
                                SelectedHeader::Torrent(i) => {
                                    if !app.app_state.torrent_list_order.is_empty() {
                                        SelectedHeader::Peer(0)
                                    } else {
                                        SelectedHeader::Torrent(i)
                                    }
                                }
                                SelectedHeader::Peer(i) if i < PEER_HEADERS.len() - 1 => {
                                    SelectedHeader::Peer(i + 1)
                                }
                                SelectedHeader::Peer(_) => SelectedHeader::Torrent(0),
                            };
                        }
                        KeyCode::Char('v') => match ClipboardContext::new() {
                            Ok(mut ctx) => match ctx.get_contents() {
                                Ok(text) => {
                                    handle_pasted_text(app, text.trim()).await;
                                }
                                Err(e) => {
                                    tracing_event!(Level::ERROR, "Clipboard read error: {}", e);
                                    app.app_state.system_error =
                                        Some(format!("Clipboard read error: {}", e));
                                }
                            },
                            Err(e) => {
                                tracing_event!(Level::ERROR, "Clipboard context error: {}", e);
                                app.app_state.system_error =
                                    Some(format!("Clipboard initialization error: {}", e));
                            }
                        },
                        _ => {}
                    }
                }
            }
            #[cfg(not(windows))]
            CrosstermEvent::Paste(pasted_text) => {
                handle_pasted_text(app, pasted_text.trim()).await;
            }
            _ => {}
        },
        AppMode::PowerSaving => {
            if let CrosstermEvent::Key(key) = event {
                // Handle events on key press for consistency
                if key.kind == KeyEventKind::Press {
                    if let KeyCode::Char('z') = key.code {
                        app.app_state.mode = AppMode::Normal;
                    }
                }
            }
        }
        AppMode::Config {
            settings_edit,
            selected_index,
            items,
            editing,
        } => {
            if let Some((item, buffer)) = editing {
                if let CrosstermEvent::Key(key) = event {
                    if key.kind == KeyEventKind::Press {
                        match key.code {
                            KeyCode::Char(c) => {
                                if c.is_ascii_digit() {
                                    buffer.push(c);
                                }
                            }
                            KeyCode::Backspace => {
                                buffer.pop();
                            }
                            KeyCode::Esc => *editing = None,
                            KeyCode::Enter => {
                                match item {
                                    ConfigItem::ClientPort => {
                                        if let Ok(new_port) = buffer.parse::<u16>() {
                                            if new_port > 0 {
                                                settings_edit.client_port = new_port;
                                            }
                                        }
                                    }
                                    ConfigItem::GlobalDownloadLimit => {
                                        if let Ok(new_rate) = buffer.parse::<u64>() {
                                            settings_edit.global_download_limit_bps = new_rate;
                                            let bucket = app.global_dl_bucket.clone();
                                            tokio::spawn(async move {
                                                bucket.lock().await.set_rate(new_rate as f64);
                                            });
                                        }
                                    }
                                    ConfigItem::GlobalUploadLimit => {
                                        if let Ok(new_rate) = buffer.parse::<u64>() {
                                            settings_edit.global_upload_limit_bps = new_rate;
                                            let bucket = app.global_ul_bucket.clone();
                                            tokio::spawn(async move {
                                                bucket.lock().await.set_rate(new_rate as f64);
                                            });
                                        }
                                    }
                                    _ => {}
                                }
                                *editing = None;
                            }
                            _ => {}
                        }
                    }
                }
            } else if let CrosstermEvent::Key(key) = event {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => {
                            app.client_configs = *settings_edit.clone();
                            app.app_state.mode = AppMode::Normal;
                        }
                        KeyCode::Enter => {
                            let selected_item = items[*selected_index];
                            match selected_item {
                                ConfigItem::GlobalDownloadLimit
                                | ConfigItem::GlobalUploadLimit
                                | ConfigItem::ClientPort => {
                                    *editing = Some((selected_item, String::new()));
                                }
                                ConfigItem::DefaultDownloadFolder | ConfigItem::WatchFolder => {
                                    let theme = Theme::default().add_default_title();
                                    match FileExplorer::with_theme(theme) {
                                        Ok(file_explorer) => {
                                            app.app_state.mode = AppMode::ConfigPathPicker {
                                                settings_edit: settings_edit.clone(),
                                                for_item: selected_item,
                                                file_explorer,
                                            };
                                        }
                                        Err(e) => tracing_event!(
                                            Level::ERROR,
                                            "Failed to create FileExplorer for config: {}",
                                            e
                                        ),
                                    }
                                }
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            *selected_index = selected_index.saturating_sub(1)
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if *selected_index < items.len() - 1 {
                                *selected_index += 1;
                            }
                        }
                        KeyCode::Right | KeyCode::Char('l') => {
                            let item = items[*selected_index];
                            let increment = 10_000 * 8;
                            match item {
                                ConfigItem::GlobalDownloadLimit => {
                                    let new_rate = settings_edit
                                        .global_download_limit_bps
                                        .saturating_add(increment);
                                    settings_edit.global_download_limit_bps = new_rate;
                                    let bucket = app.global_dl_bucket.clone();
                                    tokio::spawn(async move {
                                        bucket.lock().await.set_rate(new_rate as f64);
                                    });
                                }
                                ConfigItem::GlobalUploadLimit => {
                                    let new_rate = settings_edit
                                        .global_upload_limit_bps
                                        .saturating_add(increment);
                                    settings_edit.global_upload_limit_bps = new_rate;
                                    let bucket = app.global_ul_bucket.clone();
                                    tokio::spawn(async move {
                                        bucket.lock().await.set_rate(new_rate as f64);
                                    });
                                }
                                _ => {}
                            }
                        }
                        KeyCode::Left | KeyCode::Char('h') => {
                            let item = items[*selected_index];
                            let decrement = 10_000 * 8;
                            match item {
                                ConfigItem::ClientPort => {}
                                ConfigItem::GlobalDownloadLimit => {
                                    let new_rate = settings_edit
                                        .global_download_limit_bps
                                        .saturating_sub(decrement);
                                    settings_edit.global_download_limit_bps = new_rate;
                                    let bucket = app.global_dl_bucket.clone();
                                    tokio::spawn(async move {
                                        bucket.lock().await.set_rate(new_rate as f64);
                                    });
                                }
                                ConfigItem::GlobalUploadLimit => {
                                    let new_rate = settings_edit
                                        .global_upload_limit_bps
                                        .saturating_sub(decrement);
                                    settings_edit.global_upload_limit_bps = new_rate;
                                    let bucket = app.global_ul_bucket.clone();
                                    tokio::spawn(async move {
                                        bucket.lock().await.set_rate(new_rate as f64);
                                    });
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        AppMode::ConfigPathPicker {
            settings_edit,
            for_item,
            file_explorer,
        } => {
            if let CrosstermEvent::Key(key) = event {
                let items = || {
                    vec![
                        ConfigItem::ClientPort,
                        ConfigItem::DefaultDownloadFolder,
                        ConfigItem::WatchFolder,
                    ]
                };
                let return_to_config = |settings_edit, for_item| -> AppMode {
                    let config_items = items();
                    let selected_index = config_items
                        .iter()
                        .position(|&item| item == for_item)
                        .unwrap_or(0);
                    AppMode::Config {
                        settings_edit,
                        selected_index,
                        items: config_items,
                        editing: None,
                    }
                };
                match key.code {
                    KeyCode::Tab => {
                        let path = file_explorer.current().path().clone();
                        let dir_path = if path.is_dir() {
                            path
                        } else {
                            path.parent().unwrap_or(&path).to_path_buf()
                        };
                        match for_item {
                            ConfigItem::DefaultDownloadFolder => {
                                settings_edit.default_download_folder = Some(dir_path)
                            }
                            ConfigItem::WatchFolder => settings_edit.watch_folder = Some(dir_path),
                            _ => {}
                        }
                        app.app_state.mode = return_to_config(settings_edit.clone(), *for_item);
                    }
                    KeyCode::Esc => {
                        app.app_state.mode = return_to_config(settings_edit.clone(), *for_item)
                    }
                    _ => {
                        if file_explorer.handle(&event).is_err() { /* Log error */ }
                    }
                }
            }
        }
        AppMode::FilePicker(file_explorer) => {
            if let CrosstermEvent::Key(key) = event {
                match key.code {
                    KeyCode::Tab => {
                        let mut download_path = file_explorer.current().path().clone();
                        if !download_path.is_dir() {
                            if let Some(parent) = download_path.parent() {
                                download_path = parent.to_path_buf();
                            }
                        }
                        app.add_magnet_torrent(
                            "Fetching name...".to_string(),
                            app.app_state.pending_torrent_link.clone(),
                            download_path,
                            false,
                            TorrentControlState::Running,
                        )
                        .await;
                        app.app_state.mode = AppMode::Normal;
                    }
                    KeyCode::Esc => app.app_state.mode = AppMode::Normal,
                    _ => {
                        if let Err(e) = file_explorer.handle(&event) {
                            tracing_event!(Level::ERROR, "File explorer error: {}", e);
                        }
                    }
                }
            } else if let Err(e) = file_explorer.handle(&event) {
                tracing_event!(Level::ERROR, "File explorer error: {}", e);
            }
        }
        AppMode::DeleteConfirm {
            info_hash,
            with_files,
        } => {
            if let CrosstermEvent::Key(key) = event {
                match key.code {
                    KeyCode::Enter => {
                        let command = if *with_files {
                            crate::torrent_manager::ManagerCommand::DeleteFile
                        } else {
                            crate::torrent_manager::ManagerCommand::Shutdown
                        };
                        if let Some(manager_tx) = app.torrent_manager_command_txs.get(info_hash) {
                            let manager_tx_clone = manager_tx.clone();
                            tokio::spawn(async move {
                                let _ = manager_tx_clone.send(command).await;
                            });
                        }
                        if let Some(torrent) = app.app_state.torrents.get_mut(info_hash) {
                            torrent.latest_state.torrent_control_state =
                                TorrentControlState::Deleting;
                        }
                        app.app_state.mode = AppMode::Normal;
                    }
                    KeyCode::Esc => app.app_state.mode = AppMode::Normal,
                    _ => {}
                }
            }
        }
    }
    app.app_state.ui_needs_redraw = true;
}

async fn handle_pasted_text(app: &mut App, pasted_text: &str) {
    if pasted_text.starts_with("magnet:") {
        app.app_state.pending_torrent_link = pasted_text.to_string();
        let theme = Theme::default()
            .add_default_title()
            .with_item_style(Style::default().fg(Color::DarkGray))
            .with_dir_style(Style::default());
        match FileExplorer::with_theme(theme) {
            Ok(mut file_explorer) => {
                let initial_path = app
                    .client_configs
                    .default_download_folder
                    .as_deref()
                    .and_then(|path| path.parent())
                    .map(|path| path.to_path_buf())
                    .or_else(|| app.find_most_common_download_path());
                if let Some(common_path) = initial_path {
                    file_explorer.set_cwd(common_path).ok();
                }
                app.app_state.mode = AppMode::FilePicker(file_explorer);
            }
            Err(e) => {
                tracing_event!(Level::ERROR, "Failed to create FileExplorer: {}", e);
            }
        }
    } else {
        let path = Path::new(pasted_text.trim());
        if path.is_file() && path.extension().is_some_and(|ext| ext == "torrent") {
            if let Some(download_path) = app.client_configs.default_download_folder.clone() {
                app.add_torrent_from_file(
                    path.to_path_buf(),
                    download_path,
                    false,
                    TorrentControlState::Running,
                )
                .await;
            } else {
                tracing_event!(
                    Level::WARN,
                    "Could not add pasted torrent file: Default download folder is not set."
                );
            }
        } else {
            // Only report error for the clipboard paste (KeyCode::Char('v')),
            // as CrosstermEvent::Paste is native and might be triggered unintentionally.
            // This error reporting comes from the original KeyCode::Char('v') logic.
            tracing_event!(
                Level::WARN,
                "Clipboard content not recognized as magnet link or torrent file: {}",
                pasted_text
            );
            app.app_state.system_error = Some(
                "Clipboard content not recognized as magnet link or torrent file.".to_string(),
            );
        }
    }
}
