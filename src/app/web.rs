// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Minimal WASM-selected application surface for production TUI reducers.
//!
//! This type owns shared state, settings, and the existing `AppCommand` channel only. Browser
//! fixtures, command fulfillment, timers, rendering cadence, and browser integration remain under
//! `web`.

use tokio::sync::{broadcast, mpsc};

use super::{
    AppCommand, AppMode, AppState, BrowserPane, BrowserSearchState, DownloadSelectionTarget,
    FileBrowserMode, TorrentFilePreviewState,
};
use crate::config::Settings;
use crate::theme::{Theme, ThemeName};
use crate::tui::tree::TreeViewState;
use web_time::SystemTime;

pub(crate) struct WebApp {
    pub app_state: AppState,
    pub client_configs: Settings,
    pub app_command_tx: mpsc::Sender<AppCommand>,
    pub app_command_rx: mpsc::Receiver<AppCommand>,
    pub shutdown_tx: broadcast::Sender<()>,
}

impl WebApp {
    pub(crate) fn new(app_state: AppState, client_configs: Settings) -> Self {
        let (app_command_tx, app_command_rx) = mpsc::channel(32);
        let (shutdown_tx, _) = broadcast::channel(1);
        Self {
            app_state,
            client_configs,
            app_command_tx,
            app_command_rx,
            shutdown_tx,
        }
    }

    pub(crate) fn try_send_command(&self, command: AppCommand) {
        let _ = self.app_command_tx.try_send(command);
    }

    pub(crate) fn is_current_shared_follower(&self) -> bool {
        false
    }

    pub(crate) async fn apply_config_update_from_ui(&mut self, settings: Settings) {
        self.client_configs = settings;
    }

    pub(crate) fn sync_torrent_file_preview(&mut self) {}

    pub(crate) fn begin_file_browser_fetch(
        &mut self,
        browser_generation: u64,
        path: std::path::PathBuf,
        browser_mode: FileBrowserMode,
        preserve_browser_mode: bool,
    ) -> bool {
        if browser_generation != self.app_state.ui.file_browser.browser_generation {
            return false;
        }

        let was_file_browser = matches!(self.app_state.mode, AppMode::FileBrowser);
        let browser = &mut self.app_state.ui.file_browser;
        browser.fetch_request_id = browser.fetch_request_id.wrapping_add(1);
        browser.fetch_pending = true;
        browser.fetch_error = None;
        browser.torrent_preview_request_id = browser.torrent_preview_request_id.wrapping_add(1);
        browser.torrent_file_preview = TorrentFilePreviewState::Idle;
        if !preserve_browser_mode {
            browser.search_state = BrowserSearchState::Closed;
            browser.search_query.clear();
        }
        browser.state = TreeViewState {
            current_path: path,
            ..TreeViewState::default()
        };
        browser.data.clear();
        browser.browser_mode = if preserve_browser_mode && was_file_browser {
            super::merge_file_browser_mode_for_fetch(&browser.browser_mode, browser_mode)
        } else {
            browser_mode
        };
        self.app_state.mode = AppMode::FileBrowser;
        self.app_state.ui.needs_redraw = true;
        true
    }

    pub(crate) fn open_add_torrent_file_browser(&mut self) {
        let browser = &mut self.app_state.ui.file_browser;
        browser.next_browser_generation();
        browser.return_to_torrent_management_on_close = false;
        browser.state.current_path = self
            .client_configs
            .default_download_folder
            .clone()
            .unwrap_or_else(|| "/simulated".into());
        browser.search_state = BrowserSearchState::Closed;
        browser.search_query.clear();
        browser.fetch_pending = false;
        browser.fetch_error = None;
        browser.browser_mode = FileBrowserMode::File(vec![".torrent".to_string()]);
        self.app_state.mode = AppMode::FileBrowser;
    }

    pub(crate) fn open_existing_torrent_file_browser(&mut self, info_hash: Vec<u8>) {
        let Some(display) = self.app_state.torrents.get(&info_hash) else {
            return;
        };
        let return_to_torrent_management =
            matches!(self.app_state.mode, AppMode::TorrentManagement);
        let mut preview_state = TreeViewState::new();
        for node in &display.file_preview_tree {
            node.expand_all(&mut preview_state);
        }
        preview_state.cursor_path = display
            .file_preview_tree
            .first()
            .map(|node| node.full_path.clone());
        let initial_path = display
            .latest_state
            .download_path
            .clone()
            .or_else(|| self.client_configs.default_download_folder.clone())
            .unwrap_or_else(|| "/simulated/downloads".into());
        let preview_tree = display.file_preview_tree.clone();

        let browser = &mut self.app_state.ui.file_browser;
        browser.invalidate_browser_generation();
        browser.state = TreeViewState {
            current_path: initial_path,
            ..TreeViewState::default()
        };
        browser.search_state = BrowserSearchState::Closed;
        browser.search_query.clear();
        browser.return_to_torrent_management_on_close = return_to_torrent_management;
        browser.browser_mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::ExistingTorrent { info_hash },
            torrent_files: Vec::new(),
            container_name: String::new(),
            use_container: false,
            is_editing_name: false,
            focused_pane: BrowserPane::TorrentPreview,
            preview_tree,
            preview_state,
            cursor_pos: 0,
            original_name_backup: String::new(),
        };
        self.app_state.mode = AppMode::FileBrowser;
    }

    pub(crate) fn sort_and_filter_torrent_list(&mut self) {
        super::sort_and_filter_torrent_list_state(&mut self.app_state);
    }

    pub(crate) fn refresh_peer_management_screen(&mut self) {
        crate::tui::screens::peers::recompute_peer_management_derived(
            &mut self.app_state,
            SystemTime::now(),
        );
    }

    pub(crate) fn apply_adjacent_theme(&mut self, next: bool) {
        let themes = ThemeName::sorted_for_ui();
        let current = themes
            .iter()
            .position(|theme| *theme == self.client_configs.ui_theme)
            .unwrap_or_default();
        let selected = if next {
            (current + 1) % themes.len()
        } else if current == 0 {
            themes.len() - 1
        } else {
            current - 1
        };
        self.client_configs.ui_theme = themes[selected];
        self.app_state.theme = Theme::builtin(themes[selected]);
    }
}
