// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Narrow WASM-only bridge from browser-owned behavior to production reducers and rendering.

use super::types::*;

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use ratatui::Frame;
use tokio::sync::{mpsc, watch};

use crate::app::torrent_manager_protocol::{DiskIoOperation, ManagerCommand, ManagerEvent};
use crate::app::{
    advance_ui_effects_for_elapsed, align_unpinned_sort_with_visible_activity,
    build_torrent_preview_tree, refresh_autosort_after_stats, AppMode, AppState, BrowserPane,
    BrowserSearchState, ConfigItem, DataRate, DownloadSelectionTarget, FileBrowserMode,
    FileMetadata, FilePriority, RssPreviewItem, TorrentControlState, TorrentFilePreview,
    TorrentFilePreviewState, TorrentMetrics,
};
use crate::config::{
    RssAddedVia, RssFeed, RssFilter, RssFilterMode, RssHistoryEntry, Settings, SortDirection,
    TorrentSortColumn,
};
use crate::dht_service::{DhtSizeEstimate, DhtStatus, DhtWaveTelemetry};
use crate::networking::NetworkInterfaceInfo;
use crate::peer_manager::{
    parse_peer_client, PeerManagerEndpointView, PeerManagerTrackedPeer, PeerManagerView,
};
use crate::persistence::activity_history::{
    ActivityHistoryPersistedState, ActivityHistoryRollupState,
};
use crate::persistence::event_journal::{
    append_event_journal_entry, EventCategory, EventJournalEntry, EventScope, EventType,
};
use crate::persistence::network_history::{
    NetworkHistoryPersistedState, NetworkHistoryRollupState,
};
use crate::presentation::{PresentationFixture, PresentationState};
use crate::storage::AppStorage;
use crate::telemetry::activity_history_telemetry::ActivityHistoryTelemetry;
use crate::telemetry::network_history_telemetry::NetworkHistoryTelemetry;
use crate::telemetry::ui_telemetry::UiTelemetry;
use crate::terminal_event::Event;
use crate::theme::{Theme, ThemeName};
use crate::torrent_file::{Info, InfoFile, Torrent};
use crate::tui::screens::{peers, rss};
use crate::tui::tree::RawNode;
use strum::IntoEnumIterator;

pub struct BrowserSession {
    pub(crate) app_state: AppState,
    pub(crate) client_configs: Settings,
    app_storage: AppStorage,
    dht_status: DhtStatus,
    dht_wave_telemetry: DhtWaveTelemetry,
    pending_browser_commands: VecDeque<BrowserCommand>,
    manager_data_rate_ms: u64,
    torrent_manager_command_txs: HashMap<Vec<u8>, mpsc::Sender<ManagerCommand>>,
    torrent_metric_watch_rxs: HashMap<Vec<u8>, watch::Receiver<TorrentMetrics>>,
    manager_event_tx: mpsc::Sender<ManagerEvent>,
    manager_event_rx: mpsc::Receiver<ManagerEvent>,
    browser_tracked_peers: HashMap<(Vec<u8>, String), PeerManagerTrackedPeer>,
    browser_peer_metrics_updates: u64,
    browser_selected_peer_rate_frame_updates: u64,
    browser_selected_peer_rate_frame_changes: u64,
    browser_network_interface_refreshes: u64,
    fps_sample_elapsed: f64,
    fps_sample_frames: u32,
}

/// Manager-side endpoint used by the browser-owned torrent simulation.
///
/// Its contract deliberately matches the production torrent manager: commands
/// arrive over an mpsc receiver, metrics are published through a watch sender,
/// and discrete lifecycle/telemetry events use the shared manager event queue.
pub struct BrowserTorrentManagerEndpoint {
    command_rx: mpsc::Receiver<ManagerCommand>,
    metrics_tx: watch::Sender<TorrentMetrics>,
    manager_event_tx: mpsc::Sender<ManagerEvent>,
}

impl BrowserTorrentManagerEndpoint {
    pub fn drain_commands(&mut self) -> Vec<ManagerCommand> {
        let mut commands = Vec::new();
        while let Ok(command) = self.command_rx.try_recv() {
            commands.push(command);
        }
        commands
    }

    pub fn publish_metrics(&self, metrics: TorrentMetrics) {
        let _ = self.metrics_tx.send(metrics);
    }

    pub fn publish_update(&self, update: BrowserTorrentUpdate) {
        self.publish_metrics(update.into_torrent_metrics());
    }

    pub fn publish_frame(&self, update: BrowserTorrentFrameUpdate) {
        let mut metrics = self.metrics_tx.borrow().clone();
        update.apply_to_torrent_metrics(&mut metrics);
        self.publish_metrics(metrics);
    }

    pub fn publish_event(&self, event: ManagerEvent) {
        let _ = self.manager_event_tx.try_send(event);
    }

    pub fn publish_metadata(
        &self,
        info_hash: Vec<u8>,
        torrent_name: String,
        files: &[BrowserFileUpdate],
    ) {
        let torrent = Torrent {
            info: Info {
                piece_length: 16_384,
                name: torrent_name,
                files: files
                    .iter()
                    .map(|file| InfoFile {
                        length: i64::try_from(file.size).unwrap_or(i64::MAX),
                        path: file
                            .relative_path
                            .split('/')
                            .filter(|segment| !segment.is_empty())
                            .map(str::to_string)
                            .collect(),
                        ..InfoFile::default()
                    })
                    .collect(),
                ..Info::default()
            },
            ..Torrent::default()
        };
        self.publish_event(ManagerEvent::MetadataLoaded {
            info_hash,
            torrent: Box::new(torrent),
        });
    }
}

fn preview_file_count(node: &RawNode<crate::app::TorrentPreviewPayload>) -> usize {
    usize::from(node.payload.file_index.is_some())
        + node.children.iter().map(preview_file_count).sum::<usize>()
}

fn simulated_browser_network_interfaces() -> Vec<NetworkInterfaceInfo> {
    vec![NetworkInterfaceInfo {
        identity: "browser-demo0".to_string(),
        display_name: "Browser Demo Interface".to_string(),
        ipv4_index: Some(1),
        ipv6_index: None,
        is_up: true,
        is_loopback: false,
        ipv4_addresses: vec![Ipv4Addr::new(192, 0, 2, 10)],
        ipv6_addresses: Vec::new(),
    }]
}

impl BrowserSession {
    pub fn from_fixture(width: u16, height: u16, fixture: PresentationFixture) -> Self {
        let presentation = PresentationState::from_fixture(width, height, fixture);
        let (mut app_state, dht_status, dht_wave_telemetry, mut settings) =
            presentation.into_parts();
        settings.ui_refresh_rate = DataRate::Rate60s;
        app_state.data_rate = DataRate::Rate60s;
        app_state.ui.config.network_interface_inventory.interfaces =
            simulated_browser_network_interfaces();
        let app_storage = AppStorage::memory(settings.clone());
        let (manager_event_tx, manager_event_rx) = mpsc::channel(1_000);
        let manager_data_rate_ms = settings.ui_refresh_rate.as_ms();
        Self {
            app_state,
            client_configs: settings,
            app_storage,
            dht_status,
            dht_wave_telemetry,
            pending_browser_commands: VecDeque::new(),
            manager_data_rate_ms,
            torrent_manager_command_txs: HashMap::new(),
            torrent_metric_watch_rxs: HashMap::new(),
            manager_event_tx,
            manager_event_rx,
            browser_tracked_peers: HashMap::new(),
            browser_peer_metrics_updates: 0,
            browser_selected_peer_rate_frame_updates: 0,
            browser_selected_peer_rate_frame_changes: 0,
            browser_network_interface_refreshes: 0,
            fps_sample_elapsed: 0.0,
            fps_sample_frames: 0,
        }
    }

    pub async fn dispatch_event(&mut self, event: Event) {
        crate::tui::runtime::handle_event(self, event).await;
        self.sync_mock_torrent_preview_request();
    }

    pub async fn flush_pending_paste_burst(&mut self) {
        crate::tui::runtime::flush_pending_paste_burst(self).await;
        self.sync_mock_torrent_preview_request();
    }

    pub fn draw(&self, frame: &mut Frame) {
        crate::tui::render::draw(
            frame,
            &self.app_state,
            &self.dht_status,
            &self.dht_wave_telemetry,
            &self.client_configs,
        );
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.app_state.screen_area = ratatui::layout::Rect::new(0, 0, width.max(1), height.max(1));
        self.app_state.ui.needs_redraw = true;
    }

    pub fn screen_size(&self) -> (u16, u16) {
        (
            self.app_state.screen_area.width,
            self.app_state.screen_area.height,
        )
    }

    pub fn theme_name(&self) -> ThemeName {
        self.client_configs.ui_theme
    }

    pub fn rendered_theme_name(&self) -> ThemeName {
        self.app_state.theme.name
    }

    pub fn target_fps(&self) -> f64 {
        self.app_state.data_rate.target_fps()
    }

    pub fn browser_download_limit_bps(&self) -> Option<u64> {
        let limit = self.client_configs.global_download_limit_bps;
        (!crate::config::is_unlimited_rate_limit_bps(limit)).then_some(limit)
    }

    pub fn browser_upload_limit_bps(&self) -> Option<u64> {
        let limit = self.client_configs.global_upload_limit_bps;
        (!crate::config::is_unlimited_rate_limit_bps(limit)).then_some(limit)
    }

    pub fn effective_download_limit_bps(&self) -> u64 {
        self.browser_download_limit_bps().unwrap_or_default()
    }

    pub fn configured_upload_limit_bps(&self) -> u64 {
        self.client_configs.global_upload_limit_bps
    }

    pub fn fps_label(&self) -> String {
        crate::tui::screens::normal::footer_fps_label(&self.app_state)
    }

    pub fn set_screen(&mut self, screen: BrowserScreen) {
        match screen {
            BrowserScreen::Config => {
                *self.app_state.ui.config.settings_edit = self.client_configs.clone();
                self.app_state.ui.config.selected_index = 0;
                self.app_state.ui.config.items = ConfigItem::iter().collect();
                self.refresh_browser_network_interfaces();
            }
            BrowserScreen::DeleteConfirm => {
                if let Some(info_hash) = self.app_state.torrent_list_order.first() {
                    self.app_state.ui.delete_confirm.info_hash = info_hash.clone();
                    self.app_state.ui.delete_confirm.with_files = false;
                }
            }
            BrowserScreen::TorrentManagement => {
                crate::tui::screens::torrents::initialize_torrent_management_cursor(
                    &mut self.app_state,
                );
            }
            BrowserScreen::FileBrowser => {
                self.app_state.ui.file_browser.browser_mode = FileBrowserMode::Directory;
            }
            _ => {}
        }
        self.app_state.mode = match screen {
            BrowserScreen::Welcome => AppMode::Welcome,
            BrowserScreen::Normal => AppMode::Normal,
            BrowserScreen::Help => AppMode::Help,
            BrowserScreen::Journal => AppMode::Journal,
            BrowserScreen::PeerManagement => AppMode::PeerManagement,
            BrowserScreen::TorrentManagement => AppMode::TorrentManagement,
            BrowserScreen::PowerSaving => AppMode::PowerSaving,
            BrowserScreen::DeleteConfirm => AppMode::DeleteConfirm,
            BrowserScreen::Config => AppMode::Config,
            BrowserScreen::FileBrowser => AppMode::FileBrowser,
            BrowserScreen::Rss => AppMode::Rss,
        };
        self.app_state.ui.needs_redraw = true;
    }

    pub fn screen(&self) -> BrowserScreen {
        match self.app_state.mode {
            AppMode::Welcome => BrowserScreen::Welcome,
            AppMode::Normal => BrowserScreen::Normal,
            AppMode::Help => BrowserScreen::Help,
            AppMode::Journal => BrowserScreen::Journal,
            AppMode::PeerManagement => BrowserScreen::PeerManagement,
            AppMode::TorrentManagement => BrowserScreen::TorrentManagement,
            AppMode::PowerSaving => BrowserScreen::PowerSaving,
            AppMode::DeleteConfirm => BrowserScreen::DeleteConfirm,
            AppMode::Config => BrowserScreen::Config,
            AppMode::FileBrowser => BrowserScreen::FileBrowser,
            AppMode::Rss => BrowserScreen::Rss,
        }
    }

    pub fn key_text_input_active(&self) -> bool {
        let state = &self.app_state;
        match state.mode {
            AppMode::Normal => state.ui.is_searching,
            AppMode::Help => state.ui.help.is_searching,
            AppMode::Journal => state.ui.journal.is_searching,
            AppMode::PeerManagement => {
                state.ui.peer_management.is_searching
                    || state.ui.peer_management.details_is_searching
            }
            AppMode::TorrentManagement => state.ui.torrent_management.is_searching,
            AppMode::FileBrowser => {
                state.ui.file_browser.search_state.is_editing()
                    || matches!(
                        &state.ui.file_browser.browser_mode,
                        FileBrowserMode::DownloadLocSelection {
                            is_editing_name: true,
                            ..
                        }
                    )
            }
            AppMode::Welcome
            | AppMode::PowerSaving
            | AppMode::DeleteConfirm
            | AppMode::Config
            | AppMode::Rss => false,
        }
    }

    pub fn normal_search_query(&self) -> &str {
        &self.app_state.ui.search_query
    }

    pub fn torrent_management_search_query(&self) -> &str {
        &self.app_state.ui.torrent_management.search_query
    }

    pub fn file_browser_search_query(&self) -> &str {
        &self.app_state.ui.file_browser.search_query
    }

    pub fn web_quit_key_enabled(&self) -> bool {
        matches!(self.app_state.mode, AppMode::Normal)
            && !self.app_state.ui.is_searching
            && !self.app_state.ui.visualization_focus.active
    }

    pub fn should_quit(&self) -> bool {
        self.app_state.should_quit
    }

    pub(crate) fn is_current_shared_follower(&self) -> bool {
        false
    }

    pub(crate) fn accepts_pasted_text(&self, pasted_text: &str) -> bool {
        pasted_text.trim().starts_with("magnet:")
    }

    pub(crate) fn begin_file_browser_fetch(
        &mut self,
        browser_generation: u64,
        path: PathBuf,
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
        browser.state = crate::tui::tree::TreeViewState {
            current_path: path,
            ..crate::tui::tree::TreeViewState::default()
        };
        browser.data.clear();
        browser.browser_mode = if preserve_browser_mode && was_file_browser {
            crate::app::merge_file_browser_mode_for_fetch(&browser.browser_mode, browser_mode)
        } else {
            browser_mode
        };
        self.app_state.mode = AppMode::FileBrowser;
        self.app_state.ui.needs_redraw = true;
        true
    }

    pub(crate) fn request_file_tree(
        &mut self,
        browser_generation: u64,
        path: PathBuf,
        browser_mode: FileBrowserMode,
        preserve_browser_mode: bool,
        highlight_path: Option<PathBuf>,
    ) {
        if self.begin_file_browser_fetch(
            browser_generation,
            path.clone(),
            browser_mode,
            preserve_browser_mode,
        ) {
            self.enqueue_command(BrowserCommand::FetchFileTree {
                browser_generation,
                path,
                highlight_path,
            });
        }
    }

    pub(crate) fn open_add_torrent_file_browser(&mut self) {
        let initial_path = self
            .client_configs
            .default_download_folder
            .clone()
            .unwrap_or_else(|| "/simulated".into());
        let browser = &mut self.app_state.ui.file_browser;
        let browser_generation = browser.next_browser_generation();
        browser.return_to_torrent_management_on_close = false;
        if browser.state.current_path != initial_path || browser.data.is_empty() {
            self.request_file_tree(
                browser_generation,
                initial_path,
                FileBrowserMode::File(vec![".torrent".to_string()]),
                false,
                None,
            );
            return;
        }

        let browser = &mut self.app_state.ui.file_browser;
        browser.search_state = BrowserSearchState::Closed;
        browser.search_query.clear();
        browser.fetch_pending = false;
        browser.fetch_error = None;
        browser.browser_mode = FileBrowserMode::File(vec![".torrent".to_string()]);
        self.app_state.mode = AppMode::FileBrowser;
    }

    pub(crate) fn open_manual_magnet_browser(
        &mut self,
        magnet_link: String,
        container_name: String,
    ) {
        let preview_tree = canonical_browser_magnet_info_hash(&magnet_link)
            .and_then(|info_hash| self.app_state.torrents.get(&info_hash))
            .map(|display| display.file_preview_tree.clone())
            .unwrap_or_default();
        let mut preview_state = crate::tui::tree::TreeViewState::new();
        for node in &preview_tree {
            node.expand_all(&mut preview_state);
        }
        preview_state.cursor_path = preview_tree.first().map(|node| node.full_path.clone());
        self.app_state.pending_torrent_path = None;
        self.app_state.pending_torrent_link = magnet_link;
        let initial_path = self
            .client_configs
            .default_download_folder
            .clone()
            .unwrap_or_else(|| "/simulated/downloads".into());
        let focused_pane = if self.client_configs.default_download_folder.is_some() {
            BrowserPane::TorrentPreview
        } else {
            BrowserPane::FileSystem
        };
        let browser_generation = self.app_state.ui.file_browser.next_browser_generation();
        self.request_file_tree(
            browser_generation,
            initial_path,
            FileBrowserMode::DownloadLocSelection {
                target: DownloadSelectionTarget::PendingAdd,
                torrent_files: Vec::new(),
                container_name: container_name.clone(),
                use_container: true,
                is_editing_name: false,
                focused_pane,
                preview_tree,
                preview_state,
                cursor_pos: 0,
                original_name_backup: container_name,
            },
            false,
            None,
        );
    }

    pub(crate) fn open_manual_torrent_file_browser(&mut self, path: PathBuf) -> bool {
        let (container_name, preview_tree) =
            match &self.app_state.ui.file_browser.torrent_file_preview {
                TorrentFilePreviewState::Ready {
                    path: preview_path,
                    preview,
                } if preview_path == &path => (preview.name.clone(), preview.tree.clone()),
                _ => return false,
            };
        let file_count = preview_tree.iter().map(preview_file_count).sum::<usize>();
        let mut preview_state = crate::tui::tree::TreeViewState::new();
        for node in &preview_tree {
            node.expand_all(&mut preview_state);
        }
        preview_state.cursor_path = preview_tree.first().map(|node| node.full_path.clone());

        self.app_state.pending_torrent_link.clear();
        self.app_state.pending_torrent_path = Some(path);
        let initial_path = self
            .client_configs
            .default_download_folder
            .clone()
            .unwrap_or_else(|| "/simulated/downloads".into());
        let focused_pane = if self.client_configs.default_download_folder.is_some() {
            BrowserPane::TorrentPreview
        } else {
            BrowserPane::FileSystem
        };
        let browser_generation = self.app_state.ui.file_browser.next_browser_generation();
        self.request_file_tree(
            browser_generation,
            initial_path,
            FileBrowserMode::DownloadLocSelection {
                target: DownloadSelectionTarget::PendingAdd,
                torrent_files: Vec::new(),
                container_name: container_name.clone(),
                use_container: file_count > 1,
                is_editing_name: false,
                focused_pane,
                preview_tree,
                preview_state,
                cursor_pos: 0,
                original_name_backup: container_name,
            },
            false,
            None,
        );
        true
    }

    pub(crate) fn open_existing_torrent_file_browser(&mut self, info_hash: Vec<u8>) {
        let Some(display) = self.app_state.torrents.get(&info_hash) else {
            return;
        };
        let return_to_torrent_management =
            matches!(self.app_state.mode, AppMode::TorrentManagement);
        let mut preview_state = crate::tui::tree::TreeViewState::new();
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
        let fetch_browser_mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::ExistingTorrent {
                info_hash: info_hash.clone(),
            },
            torrent_files: Vec::new(),
            container_name: String::new(),
            use_container: false,
            is_editing_name: false,
            focused_pane: BrowserPane::TorrentPreview,
            preview_tree: Vec::new(),
            preview_state: crate::tui::tree::TreeViewState::default(),
            cursor_pos: 0,
            original_name_backup: String::new(),
        };

        let browser = &mut self.app_state.ui.file_browser;
        let needs_file_tree_fetch =
            browser.state.current_path != initial_path || browser.data.is_empty();
        browser.invalidate_browser_generation();
        let browser_generation = browser.browser_generation;
        if needs_file_tree_fetch {
            browser.state = crate::tui::tree::TreeViewState {
                current_path: initial_path.clone(),
                ..crate::tui::tree::TreeViewState::default()
            };
            browser.data.clear();
        } else {
            browser.fetch_pending = false;
            browser.fetch_error = None;
        }
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
        self.app_state.ui.needs_redraw = true;
        if needs_file_tree_fetch {
            self.request_file_tree(
                browser_generation,
                initial_path,
                fetch_browser_mode,
                true,
                None,
            );
        }
    }

    pub(crate) fn refresh_peer_management_screen(&mut self) {
        peers::recompute_peer_management_derived(&mut self.app_state, web_time::SystemTime::now());
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

    pub(crate) fn sync_torrent_file_preview(&mut self) {
        self.sync_mock_torrent_preview_request();
    }

    pub fn drain_commands(&mut self) -> Vec<BrowserCommand> {
        self.pending_browser_commands.drain(..).collect()
    }

    pub(crate) fn enqueue_command(&mut self, command: BrowserCommand) {
        self.pending_browser_commands.push_back(command);
    }

    /// Registers a browser-owned torrent manager behind the same command,
    /// metrics, and event channels used by the native runtime.
    pub fn register_torrent_manager(
        &mut self,
        info_hash: Vec<u8>,
    ) -> BrowserTorrentManagerEndpoint {
        self.register_torrent_manager_with_metrics(TorrentMetrics {
            info_hash,
            ..TorrentMetrics::default()
        })
    }

    pub fn register_torrent_manager_with_metrics(
        &mut self,
        initial_metrics: TorrentMetrics,
    ) -> BrowserTorrentManagerEndpoint {
        let info_hash = initial_metrics.info_hash.clone();
        let (command_tx, command_rx) = mpsc::channel(100);
        let (metrics_tx, metrics_rx) = watch::channel(initial_metrics);
        if self.manager_data_rate_ms != DataRate::Rate60s.as_ms() {
            let _ = command_tx.try_send(ManagerCommand::SetDataRate(self.manager_data_rate_ms));
        }
        self.torrent_manager_command_txs
            .insert(info_hash.clone(), command_tx);
        self.torrent_metric_watch_rxs.insert(info_hash, metrics_rx);
        BrowserTorrentManagerEndpoint {
            command_rx,
            metrics_tx,
            manager_event_tx: self.manager_event_tx.clone(),
        }
    }

    pub(crate) fn send_manager_command(&self, info_hash: &[u8], command: ManagerCommand) -> bool {
        self.torrent_manager_command_txs
            .get(info_hash)
            .is_some_and(|sender| sender.try_send(command).is_ok())
    }

    pub(crate) fn broadcast_manager_data_rate(&mut self, rate_ms: u64) {
        self.manager_data_rate_ms = rate_ms;
        for sender in self.torrent_manager_command_txs.values() {
            let _ = sender.try_send(ManagerCommand::SetDataRate(rate_ms));
        }
    }

    /// Drains manager output through the production telemetry reducer.
    pub fn drain_manager_messages(&mut self) {
        let mut changed = false;
        while let Ok(event) = self.manager_event_rx.try_recv() {
            changed = true;
            let effects = crate::app::reduce_app_action(
                &mut self.app_state,
                crate::app::AppAction::ManagerEvent(event),
            );
            for effect in effects {
                let crate::app::AppEffect::HandleManagerEvent(event) = effect else {
                    continue;
                };
                match event {
                    ManagerEvent::DeletionComplete(info_hash, _) => {
                        self.remove_torrent(&info_hash);
                    }
                    ManagerEvent::DataAvailabilityFault { info_hash, .. } => {
                        if let Some(torrent) = self.app_state.torrents.get_mut(&info_hash) {
                            torrent.latest_state.data_available = false;
                        }
                    }
                    ManagerEvent::MetadataLoaded { info_hash, torrent } => {
                        if let Some(display) = self.app_state.torrents.get_mut(&info_hash) {
                            display.latest_state.is_multi_file = !torrent.info.files.is_empty();
                            display.latest_state.file_count = Some(torrent.file_list().len());
                            display.latest_state.total_size =
                                torrent.info.total_length().max(0) as u64;
                            display.file_preview_tree = build_torrent_preview_tree(
                                torrent.file_list(),
                                &display.latest_state.file_priorities,
                            );
                        }
                    }
                    ManagerEvent::TelemetryBatch(_)
                    | ManagerEvent::FileProbeBatchResult { .. }
                    | ManagerEvent::DiskReadStarted { .. }
                    | ManagerEvent::DiskReadFinished
                    | ManagerEvent::DiskWriteStarted { .. }
                    | ManagerEvent::DiskWriteCompleted { .. }
                    | ManagerEvent::DiskWriteFinished { .. }
                    | ManagerEvent::DiskIoBackoff { .. }
                    | ManagerEvent::PeerDiscovered { .. }
                    | ManagerEvent::PeerConnected { .. }
                    | ManagerEvent::PeerDisconnected { .. }
                    | ManagerEvent::BlockReceived { .. }
                    | ManagerEvent::BlockSent { .. } => {}
                    #[cfg(feature = "synthetic-load")]
                    ManagerEvent::PeerConnectAttempted { .. }
                    | ManagerEvent::PeerConnectEstablished { .. }
                    | ManagerEvent::PeerConnectFailed { .. }
                    | ManagerEvent::PeerSessionFailed => {}
                }
            }
        }

        let selected_hash = self
            .selected_torrent_hash_hex()
            .and_then(|hash| hex::decode(hash).ok());
        let mut closed = Vec::new();
        let mut completion_events = Vec::new();
        for (info_hash, receiver) in &mut self.torrent_metric_watch_rxs {
            match receiver.has_changed() {
                Ok(false) => {}
                Ok(true) => {
                    let metrics = receiver.borrow_and_update().clone();
                    let selected = selected_hash.as_ref() == Some(info_hash);
                    let previous_peer_rates = selected.then(|| {
                        self.app_state
                            .torrents
                            .get(info_hash)
                            .map(|torrent| {
                                torrent
                                    .latest_state
                                    .peers
                                    .iter()
                                    .map(|peer| {
                                        (
                                            peer.address.clone(),
                                            peer.download_speed_bps,
                                            peer.upload_speed_bps,
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default()
                    });
                    let control_state = metrics.torrent_control_state.clone();
                    let delete_files = metrics.delete_files;
                    let torrent_or_magnet = metrics.torrent_or_magnet.clone();
                    let is_multi_file = metrics.is_multi_file;
                    let file_priorities = metrics.file_priorities.clone();
                    let effects = crate::app::reduce_app_action(
                        &mut self.app_state,
                        crate::app::AppAction::ManagerMetrics(Box::new(metrics)),
                    );
                    for effect in effects {
                        if let crate::app::AppEffect::TorrentCompleted {
                            info_hash,
                            torrent_name,
                        } = effect
                        {
                            completion_events.push((info_hash, torrent_name));
                        }
                    }
                    if let Some(display) = self.app_state.torrents.get_mut(info_hash) {
                        display.latest_state.torrent_control_state = control_state;
                        display.latest_state.delete_files = delete_files;
                        if !torrent_or_magnet.is_empty() {
                            display.latest_state.torrent_or_magnet = torrent_or_magnet;
                        }
                        display.latest_state.is_multi_file = is_multi_file;
                        display.latest_state.file_priorities = file_priorities.clone();
                        if !display.file_preview_tree.is_empty() {
                            crate::app::apply_torrent_preview_file_priorities(
                                &mut display.file_preview_tree,
                                &file_priorities,
                            );
                        }
                        if let Some(previous) = previous_peer_rates {
                            let peer_rate_changed = display.latest_state.peers.iter().any(|peer| {
                                previous.iter().any(|(address, download, upload)| {
                                    address == &peer.address
                                        && (*download != peer.download_speed_bps
                                            || *upload != peer.upload_speed_bps)
                                })
                            });
                            self.browser_selected_peer_rate_frame_updates = self
                                .browser_selected_peer_rate_frame_updates
                                .saturating_add(1);
                            self.browser_selected_peer_rate_frame_changes = self
                                .browser_selected_peer_rate_frame_changes
                                .saturating_add(u64::from(peer_rate_changed));
                        }
                    }
                    changed = true;
                }
                Err(_) => closed.push(info_hash.clone()),
            }
        }
        for info_hash in closed {
            self.torrent_metric_watch_rxs.remove(&info_hash);
        }
        for (info_hash, torrent_name) in completion_events {
            self.record_torrent_completed_event(&info_hash, torrent_name);
        }
        if changed {
            crate::app::finalize_manager_metrics_batch(&mut self.app_state);
        }
    }

    fn record_torrent_completed_event(&mut self, info_hash: &[u8], torrent_name: String) {
        let info_hash_hex = hex::encode(info_hash);
        if self
            .app_state
            .event_journal_state
            .entries
            .iter()
            .any(|entry| {
                entry.event_type == EventType::TorrentCompleted
                    && entry.info_hash_hex.as_deref() == Some(info_hash_hex.as_str())
            })
        {
            return;
        }

        append_event_journal_entry(
            &mut self.app_state.event_journal_state,
            EventJournalEntry {
                scope: EventScope::Host,
                ts_iso: "2026-08-30T12:06:00Z".to_string(),
                category: EventCategory::TorrentLifecycle,
                event_type: EventType::TorrentCompleted,
                torrent_name: Some(torrent_name),
                info_hash_hex: Some(info_hash_hex),
                message: Some("Torrent completed".to_string()),
                ..Default::default()
            },
        );
    }

    pub fn torrent_completion_journal_count_hex(&self, info_hash_hex: &str) -> usize {
        self.app_state
            .event_journal_state
            .entries
            .iter()
            .filter(|entry| {
                entry.event_type == EventType::TorrentCompleted
                    && entry.info_hash_hex.as_deref() == Some(info_hash_hex)
            })
            .count()
    }

    pub(crate) fn apply_browser_config_update(&mut self, settings: Settings) {
        if let Err(error) = self.app_storage.save_settings(&settings) {
            self.app_state.system_error =
                Some(format!("Failed to save browser configuration: {error}"));
            self.app_state.ui.needs_redraw = true;
            return;
        }
        self.app_state.effective_download_limit_bps = settings.global_download_limit_bps;
        self.app_state.theme = Theme::builtin(settings.ui_theme);
        self.client_configs = settings;
        rss::recompute_rss_derived(&mut self.app_state, &self.client_configs);
        self.app_state.ui.needs_redraw = true;
    }

    pub(crate) fn refresh_browser_network_interfaces(&mut self) {
        let inventory = &mut self.app_state.ui.config.network_interface_inventory;
        inventory.interfaces = simulated_browser_network_interfaces();
        inventory.loading = false;
        inventory.error = None;
        self.browser_network_interface_refreshes =
            self.browser_network_interface_refreshes.saturating_add(1);
        self.app_state.ui.needs_redraw = true;
    }

    pub fn apply_mock_file_tree(
        &mut self,
        browser_generation: u64,
        path: PathBuf,
        entries: Vec<BrowserFileTreeEntry>,
        highlight_path: Option<PathBuf>,
    ) -> bool {
        let browser = &mut self.app_state.ui.file_browser;
        if browser_generation != browser.browser_generation
            || !matches!(self.app_state.mode, AppMode::FileBrowser)
        {
            return false;
        }
        browser.fetch_pending = false;
        browser.fetch_error = None;
        browser.state.current_path = path.clone();
        browser.state.top_most_offset = 0;
        browser.data = entries
            .into_iter()
            .map(|entry| RawNode {
                name: entry.name.clone(),
                full_path: path.join(entry.name),
                children: Vec::new(),
                payload: FileMetadata {
                    size: entry.size,
                    modified: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                },
                is_dir: entry.is_dir,
            })
            .collect();
        browser.state.cursor_path = highlight_path
            .filter(|highlight| browser.data.iter().any(|node| &node.full_path == highlight))
            .or_else(|| browser.data.first().map(|node| node.full_path.clone()));
        self.app_state.ui.needs_redraw = true;
        self.sync_mock_torrent_preview_request();
        true
    }

    fn sync_mock_torrent_preview_request(&mut self) {
        let selected_path = if matches!(self.app_state.mode, AppMode::FileBrowser)
            && matches!(
                self.app_state.ui.file_browser.browser_mode,
                FileBrowserMode::File(_)
            )
            && !self.app_state.ui.file_browser.fetch_pending
        {
            let browser = &self.app_state.ui.file_browser;
            browser
                .state
                .cursor_path
                .as_ref()
                .filter(|path| {
                    browser.data.iter().any(|node| {
                        !node.is_dir
                            && &node.full_path == *path
                            && path
                                .extension()
                                .and_then(|extension| extension.to_str())
                                .is_some_and(|extension| extension.eq_ignore_ascii_case("torrent"))
                    })
                })
                .cloned()
        } else {
            None
        };

        let Some(path) = selected_path else {
            let browser = &mut self.app_state.ui.file_browser;
            if !matches!(browser.torrent_file_preview, TorrentFilePreviewState::Idle) {
                browser.torrent_preview_request_id =
                    browser.torrent_preview_request_id.wrapping_add(1);
                browser.torrent_file_preview = TorrentFilePreviewState::Idle;
                self.app_state.ui.needs_redraw = true;
            }
            return;
        };

        if self.app_state.ui.file_browser.torrent_file_preview.path() == Some(path.as_path()) {
            return;
        }

        let browser = &mut self.app_state.ui.file_browser;
        browser.torrent_preview_request_id = browser.torrent_preview_request_id.wrapping_add(1);
        let request_id = browser.torrent_preview_request_id;
        let browser_generation = browser.browser_generation;
        browser.torrent_file_preview = TorrentFilePreviewState::Loading {
            path: path.clone(),
            request_id,
        };
        self.pending_browser_commands
            .push_back(BrowserCommand::FetchTorrentPreview {
                browser_generation,
                request_id,
                path,
            });
        self.app_state.ui.needs_redraw = true;
    }

    pub fn apply_mock_torrent_preview(
        &mut self,
        browser_generation: u64,
        request_id: u64,
        path: PathBuf,
        name: String,
        protocol_version: String,
        files: Vec<BrowserTorrentPreviewFile>,
    ) -> bool {
        let browser = &mut self.app_state.ui.file_browser;
        if !matches!(self.app_state.mode, AppMode::FileBrowser)
            || browser.browser_generation != browser_generation
            || browser.torrent_preview_request_id != request_id
            || !matches!(
                &browser.torrent_file_preview,
                TorrentFilePreviewState::Loading {
                    path: loading_path,
                    request_id: loading_request_id,
                } if loading_path == &path && *loading_request_id == request_id
            )
        {
            return false;
        }

        let total_size = files.iter().map(|file| file.size).sum();
        let tree = build_torrent_preview_tree(
            files
                .into_iter()
                .map(|file| {
                    (
                        file.relative_path
                            .split('/')
                            .filter(|segment| !segment.is_empty())
                            .map(str::to_string)
                            .collect(),
                        file.size,
                    )
                })
                .collect(),
            &Default::default(),
        );
        browser.torrent_file_preview = TorrentFilePreviewState::Ready {
            path,
            preview: TorrentFilePreview {
                name,
                protocol_version,
                total_size,
                tree,
            },
        };
        self.app_state.ui.needs_redraw = true;
        true
    }

    pub fn torrent_preview_state(&self) -> &'static str {
        match self.app_state.ui.file_browser.torrent_file_preview {
            TorrentFilePreviewState::Idle => "idle",
            TorrentFilePreviewState::Loading { .. } => "loading",
            TorrentFilePreviewState::Ready { .. } => "ready",
            TorrentFilePreviewState::Error { .. } => "error",
        }
    }

    pub fn torrent_preview_name(&self) -> &str {
        match &self.app_state.ui.file_browser.torrent_file_preview {
            TorrentFilePreviewState::Ready { preview, .. } => preview.name.as_str(),
            _ => "",
        }
    }

    pub fn torrent_preview_file_count(&self) -> usize {
        fn count_files(nodes: &[RawNode<crate::app::TorrentPreviewPayload>]) -> usize {
            nodes
                .iter()
                .map(|node| {
                    if node.is_dir {
                        count_files(&node.children)
                    } else {
                        1
                    }
                })
                .sum()
        }

        match &self.app_state.ui.file_browser.torrent_file_preview {
            TorrentFilePreviewState::Ready { preview, .. } => count_files(&preview.tree),
            _ => 0,
        }
    }

    pub fn apply_mock_torrent_config(
        &mut self,
        info_hash_hex: &str,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        file_priorities: &[BrowserFilePriorityOverride],
    ) -> bool {
        let Ok(info_hash) = hex::decode(info_hash_hex) else {
            return false;
        };
        let Some(torrent) = self.app_state.torrents.get_mut(&info_hash) else {
            return false;
        };
        torrent.latest_state.download_path = download_path;
        torrent.latest_state.container_name = container_name;
        torrent.latest_state.file_priorities.clear();
        for override_value in file_priorities {
            let priority = match override_value.priority {
                BrowserFilePriority::High => FilePriority::High,
                BrowserFilePriority::Skip => FilePriority::Skip,
            };
            torrent
                .latest_state
                .file_priorities
                .insert(override_value.file_index, priority);
        }
        crate::app::apply_torrent_preview_file_priorities(
            &mut torrent.file_preview_tree,
            &torrent.latest_state.file_priorities,
        );
        self.app_state.ui.needs_redraw = true;
        true
    }

    pub fn apply_mock_rss_sync(&mut self, last_sync_at: String, next_sync_at: String) {
        self.app_state.rss_runtime.last_sync_at = Some(last_sync_at);
        self.app_state.rss_runtime.next_sync_at = Some(next_sync_at);
        rss::recompute_rss_derived(&mut self.app_state, &self.client_configs);
        self.app_state.ui.needs_redraw = true;
    }

    pub fn apply_mock_rss_download(&mut self, item: &BrowserRssPreview, info_hash: &[u8]) {
        for preview in &mut self.app_state.rss_runtime.preview_items {
            if preview.dedupe_key == item.dedupe_key {
                preview.is_downloaded = true;
            }
        }
        let entry = RssHistoryEntry {
            dedupe_key: item.dedupe_key.clone(),
            info_hash: Some(hex::encode(info_hash)),
            guid: item.guid.clone(),
            link: item.link.clone(),
            title: item.title.clone(),
            source: item.source.clone(),
            date_iso: item
                .date_iso
                .clone()
                .unwrap_or_else(|| "2026-08-30T12:04:00Z".to_string()),
            added_via: RssAddedVia::Manual,
        };
        if let Some(existing) = self
            .app_state
            .rss_runtime
            .history
            .iter_mut()
            .find(|existing| existing.dedupe_key == entry.dedupe_key)
        {
            *existing = entry;
        } else {
            self.app_state.rss_runtime.history.push(entry);
        }
        rss::recompute_rss_derived(&mut self.app_state, &self.client_configs);
        self.app_state.ui.needs_redraw = true;
    }

    pub fn set_browser_error(&mut self, message: impl Into<String>) {
        self.app_state.system_error = Some(message.into());
        self.app_state.ui.needs_redraw = true;
    }

    pub fn clear_browser_error(&mut self) {
        self.app_state.system_error = None;
        self.app_state.ui.needs_redraw = true;
    }

    pub fn upsert_mock_torrent(&mut self, update: BrowserTorrentUpdate) {
        let info_hash = update.info_hash.clone();
        let _ = crate::app::reduce_app_action(
            &mut self.app_state,
            crate::app::AppAction::ManagerMetrics(Box::new(update.clone().into_torrent_metrics())),
        );

        let display = self
            .app_state
            .torrents
            .get_mut(&info_hash)
            .expect("production telemetry inserted the browser torrent");
        display.latest_state.torrent_or_magnet = update.torrent_or_magnet;
        display.latest_state.torrent_control_state = match update.control_state {
            BrowserTorrentControlState::Running => TorrentControlState::Running,
            BrowserTorrentControlState::Paused => TorrentControlState::Paused,
            BrowserTorrentControlState::Deleting => TorrentControlState::Deleting,
        };
        display.latest_state.bytes_downloaded_this_tick = update.bytes_downloaded_this_tick;
        display.latest_state.bytes_uploaded_this_tick = update.bytes_uploaded_this_tick;
        display.latest_state.is_multi_file = update.files.len() > 1;
        display.file_preview_tree = build_torrent_preview_tree(
            update
                .files
                .iter()
                .map(|file| {
                    (
                        file.relative_path
                            .split('/')
                            .filter(|segment| !segment.is_empty())
                            .map(str::to_string)
                            .collect(),
                        file.size,
                    )
                })
                .collect(),
            &display.latest_state.file_priorities,
        );
        if update.download_history.len() > display.download_history.len() {
            display.download_history = update.download_history;
            display.upload_history = update.upload_history;
        }
        if display.latest_state.blocks_in_history.is_empty() {
            display.latest_state.blocks_in_history = update.blocks_in_history;
            display.latest_state.blocks_out_history = update.blocks_out_history;
        }
        if display.peer_discovery_history.is_empty() {
            display.peer_discovery_history = update.peer_discovery_history;
            display.peer_connection_history = update.peer_connection_history;
            display.peer_disconnect_history = update.peer_disconnect_history;
        }
        crate::app::finalize_manager_metrics_batch(&mut self.app_state);
    }

    pub fn refresh_mock_peer_manager(&mut self) {
        let snapshots = self
            .app_state
            .torrents
            .values()
            .map(|torrent| {
                (
                    torrent.latest_state.info_hash.clone(),
                    torrent.latest_state.torrent_name.clone(),
                    torrent.latest_state.peers.clone(),
                )
            })
            .collect::<Vec<_>>();
        let current_keys = snapshots
            .iter()
            .flat_map(|(info_hash, _, peers)| {
                peers
                    .iter()
                    .map(|peer| (info_hash.clone(), peer.address.clone()))
            })
            .collect::<HashSet<_>>();
        for (key, peer) in &mut self.browser_tracked_peers {
            if peer.is_active && !current_keys.contains(key) {
                peer.is_active = false;
                peer.disconnect_count = peer.disconnect_count.saturating_add(1);
            }
        }
        for (info_hash, torrent_name, peers) in snapshots {
            for peer in peers {
                let Ok(endpoint) = peer.address.parse::<std::net::SocketAddr>() else {
                    continue;
                };
                let key = (info_hash.clone(), peer.address.clone());
                let previous = self.browser_tracked_peers.get(&key);
                let inferred_connection_count =
                    previous.map_or(peer.connection_count, |previous| {
                        previous
                            .connection_count
                            .saturating_add(u64::from(!previous.is_active))
                            .max(peer.connection_count)
                    });
                let inferred_disconnect_count = previous
                    .map(|previous| previous.disconnect_count.max(peer.disconnect_count))
                    .unwrap_or(peer.disconnect_count);
                self.browser_tracked_peers.insert(
                    key,
                    PeerManagerTrackedPeer {
                        torrent_info_hash: info_hash.clone(),
                        torrent_name: torrent_name.clone(),
                        ip: endpoint.ip(),
                        is_active: true,
                        endpoints: vec![PeerManagerEndpointView {
                            address: peer.address.clone(),
                            total_downloaded: peer.total_downloaded,
                            total_uploaded: peer.total_uploaded,
                        }],
                        downloaded_evidence_bytes: peer.total_downloaded,
                        uploaded_evidence_bytes: peer.total_uploaded,
                        total_downloaded_bytes: peer.total_downloaded,
                        total_uploaded_bytes: peer.total_uploaded,
                        connection_count: inferred_connection_count,
                        disconnect_count: inferred_disconnect_count,
                        transfer_threshold_bytes: 64 * 1024,
                        reconnect_count: u32::try_from(inferred_connection_count.saturating_sub(1))
                            .unwrap_or(u32::MAX),
                        reconnect_limit: 4,
                        reconnect_window_secs: 300,
                        last_seen: Some(web_time::SystemTime::now()),
                        clients: vec![parse_peer_client(&peer.peer_id)],
                    },
                );
            }
        }
        self.browser_peer_metrics_updates = self.browser_peer_metrics_updates.saturating_add(1);
        let mut tracked_peers = self
            .browser_tracked_peers
            .values()
            .cloned()
            .collect::<Vec<_>>();
        tracked_peers.sort_by(|left, right| {
            left.torrent_info_hash
                .cmp(&right.torrent_info_hash)
                .then_with(|| left.ip.cmp(&right.ip))
                .then_with(|| left.endpoints[0].address.cmp(&right.endpoints[0].address))
        });
        self.app_state.peer_manager_view = Arc::new(PeerManagerView {
            registered_torrents: self.app_state.torrents.len(),
            metrics_updates: self.browser_peer_metrics_updates,
            tracked_peers,
        });
        peers::recompute_peer_management_derived(&mut self.app_state, web_time::SystemTime::now());
    }

    pub fn run_mock_second_tick(
        &mut self,
        cpu_usage: f32,
        ram_usage_percent: f32,
        app_ram_usage: u64,
        run_time: u64,
    ) {
        let previous_torrent_sort = self.app_state.torrent_sort;
        let previous_peer_sort = self.app_state.peer_sort;
        UiTelemetry::on_second_tick_with_system_snapshot(
            &mut self.app_state,
            cpu_usage,
            ram_usage_percent,
            app_ram_usage,
            run_time,
        );
        align_unpinned_sort_with_visible_activity(&mut self.app_state);
        refresh_autosort_after_stats(
            &mut self.app_state,
            previous_torrent_sort,
            previous_peer_sort,
        );
        NetworkHistoryTelemetry::on_second_tick(&mut self.app_state);
        ActivityHistoryTelemetry::on_second_tick(&mut self.app_state);
        self.app_state.ui.needs_redraw = true;
    }

    pub fn apply_mock_telemetry(&mut self, update: BrowserTelemetryUpdate) {
        let BrowserTelemetryUpdate {
            cpu_usage,
            ram_usage_percent,
            app_ram_usage,
            run_time,
            total_download_history,
            total_upload_history,
            disk_read_history,
            disk_write_history,
            disk_read_bps,
            disk_write_bps,
            disk_backoff_history_ms,
            dht_nodes,
            dht_active_lookups,
            dht_peers_found,
            filesystem,
            journal,
            rss,
        } = update;
        self.apply_mock_runtime_telemetry(BrowserRuntimeTelemetryUpdate {
            cpu_usage,
            ram_usage_percent,
            app_ram_usage,
            run_time,
            total_download_history,
            total_upload_history,
            disk_read_history,
            disk_write_history,
            disk_read_bps,
            disk_write_bps,
            disk_backoff_history_ms,
            dht_nodes,
            dht_active_lookups,
            dht_peers_found,
        });

        let state = &mut self.app_state;

        let base_path = PathBuf::from("/simulated");
        state.ui.file_browser.state.current_path = base_path.clone();
        state.ui.file_browser.data = filesystem
            .iter()
            .map(|file| RawNode {
                name: file.relative_path.clone(),
                full_path: base_path.join(&file.relative_path),
                children: Vec::new(),
                payload: FileMetadata {
                    size: file.size,
                    modified: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                },
                is_dir: false,
            })
            .collect();
        state.ui.file_browser.state.cursor_path = state
            .ui
            .file_browser
            .data
            .first()
            .map(|node| node.full_path.clone());

        state.event_journal_state.entries = journal
            .into_iter()
            .enumerate()
            .map(|(index, entry)| {
                let (category, event_type) = match entry.kind {
                    BrowserJournalKind::Lifecycle => {
                        (EventCategory::TorrentLifecycle, EventType::TorrentCompleted)
                    }
                    BrowserJournalKind::DataUnavailable => {
                        (EventCategory::DataHealth, EventType::DataUnavailable)
                    }
                    BrowserJournalKind::DataRecovered => {
                        (EventCategory::DataHealth, EventType::DataRecovered)
                    }
                };
                EventJournalEntry {
                    id: index as u64 + 1,
                    scope: EventScope::Host,
                    ts_iso: entry.timestamp,
                    category,
                    event_type,
                    torrent_name: entry.torrent_name,
                    message: Some(entry.message),
                    ..Default::default()
                }
            })
            .collect();
        state.event_journal_state.next_id = state.event_journal_state.entries.len() as u64 + 1;

        self.client_configs.rss.feeds = rss
            .iter()
            .map(|item| RssFeed {
                url: item.feed_url.clone(),
                enabled: true,
            })
            .collect();
        self.client_configs.rss.filters = rss
            .iter()
            .map(|item| RssFilter {
                query: item.filter_query.clone(),
                mode: RssFilterMode::Fuzzy,
                enabled: true,
            })
            .collect();
        state.rss_runtime.preview_items = rss
            .into_iter()
            .enumerate()
            .map(|(index, item)| RssPreviewItem {
                dedupe_key: format!("simulated-{index}"),
                title: item.item_title,
                link: Some(item.item_link),
                source: Some(item.feed_url),
                date_iso: Some(item.timestamp),
                is_match: true,
                ..Default::default()
            })
            .collect();
        rss::recompute_rss_derived(state, &self.client_configs);

        state.ui.needs_redraw = true;
    }

    fn seed_mock_history_if_short(&mut self, update: &BrowserRuntimeTelemetryUpdate) {
        let history_len = update
            .total_download_history
            .len()
            .max(update.total_upload_history.len())
            .max(update.disk_read_history.len())
            .max(update.disk_write_history.len());
        if history_len == 0 {
            return;
        }
        if self.app_state.network_history_state.tiers.second_1s.len() >= history_len
            && self
                .app_state
                .activity_history_state
                .cpu
                .tiers
                .second_1s
                .len()
                >= history_len
        {
            return;
        }

        let now_unix = web_time::SystemTime::now()
            .duration_since(web_time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let value_at = |history: &[u64], index: usize| {
            let offset = history_len.saturating_sub(history.len());
            index
                .checked_sub(offset)
                .and_then(|local| history.get(local))
                .copied()
                .unwrap_or_default()
        };
        let torrent_histories = self
            .app_state
            .torrents
            .iter()
            .map(|(info_hash, torrent)| {
                (
                    hex::encode(info_hash),
                    torrent.download_history.clone(),
                    torrent.upload_history.clone(),
                )
            })
            .collect::<Vec<_>>();
        let state = &mut self.app_state;

        // Presentation fixtures intentionally carry only a minimal history sample. The browser
        // simulation owns its virtual history, so replace that placeholder before feeding the
        // production rollup reducers and leave an immediately useful graph at startup.
        state.network_history_state = NetworkHistoryPersistedState::default();
        state.network_history_rollups = NetworkHistoryRollupState::default();
        state.activity_history_state = ActivityHistoryPersistedState::default();
        state.activity_history_rollups = ActivityHistoryRollupState::default();

        for index in 0..history_len {
            let ts_unix = now_unix.saturating_sub((history_len - 1 - index) as u64);
            let download_bps = value_at(&update.total_download_history, index);
            let upload_bps = value_at(&update.total_upload_history, index);
            let disk_read_bps = value_at(&update.disk_read_history, index);
            let disk_write_bps = value_at(&update.disk_write_history, index);
            let backoff_ms = value_at(&update.disk_backoff_history_ms, index);
            state.network_history_rollups.ingest_second_sample(
                &mut state.network_history_state,
                ts_unix,
                download_bps,
                upload_bps,
                backoff_ms,
            );
            state.activity_history_rollups.cpu.ingest_second_sample(
                &mut state.activity_history_state.cpu,
                ts_unix,
                (update.cpu_usage.clamp(0.0, 100.0) * 10.0).round() as u64,
                0,
            );
            state.activity_history_rollups.ram.ingest_second_sample(
                &mut state.activity_history_state.ram,
                ts_unix,
                (update.ram_usage_percent.clamp(0.0, 100.0) * 10.0).round() as u64,
                0,
            );
            state.activity_history_rollups.disk.ingest_second_sample(
                &mut state.activity_history_state.disk,
                ts_unix,
                disk_read_bps,
                disk_write_bps,
            );
            state.activity_history_rollups.tuning.ingest_second_sample(
                &mut state.activity_history_state.tuning,
                ts_unix,
                state.current_tuning_score,
                state.last_tuning_score,
            );
            for (key, download_history, upload_history) in &torrent_histories {
                let series = state
                    .activity_history_state
                    .torrents
                    .entry(key.clone())
                    .or_default();
                state
                    .activity_history_rollups
                    .torrents
                    .entry(key.clone())
                    .or_default()
                    .ingest_second_sample(
                        series,
                        ts_unix,
                        value_at(download_history, index),
                        value_at(upload_history, index),
                    );
            }
        }

        state.total_download_history = update.total_download_history.clone();
        state.total_upload_history = update.total_upload_history.clone();
        state.avg_download_history = update.total_download_history.clone();
        state.avg_upload_history = update.total_upload_history.clone();
        state.disk_read_history = update.disk_read_history.clone();
        state.disk_write_history = update.disk_write_history.clone();
        state.avg_disk_read_bps = update.disk_read_bps;
        state.avg_disk_write_bps = update.disk_write_bps;
        state.avg_disk_write_completed_bps = update.disk_write_bps;
        state.disk_backoff_history_ms = update.disk_backoff_history_ms.clone().into();
        if update.disk_read_bps > 0 {
            state.global_disk_read_history_log = VecDeque::from([DiskIoOperation {
                piece_index: 7,
                offset: 0,
                length: 32 * 1024,
            }]);
        }
        if update.disk_write_bps > 0 {
            state.global_disk_write_history_log = VecDeque::from([DiskIoOperation {
                piece_index: 9,
                offset: 64 * 1024,
                length: 32 * 1024,
            }]);
        }
    }

    pub fn apply_mock_runtime_telemetry(&mut self, update: BrowserRuntimeTelemetryUpdate) {
        self.seed_mock_history_if_short(&update);
        let state = &mut self.app_state;
        state.cpu_usage = update.cpu_usage;
        state.ram_usage_percent = update.ram_usage_percent;
        state.app_ram_usage = update.app_ram_usage;
        state.run_time = update.run_time;
        state.session_total_downloaded = state
            .torrents
            .values()
            .map(|torrent| torrent.latest_state.session_total_downloaded)
            .sum();
        state.session_total_uploaded = state
            .torrents
            .values()
            .map(|torrent| torrent.latest_state.session_total_uploaded)
            .sum();
        state.lifetime_downloaded_from_config = state.session_total_downloaded * 3;
        state.lifetime_uploaded_from_config = state.session_total_uploaded * 3;
        self.dht_status.generation = self.dht_status.generation.saturating_add(1);
        self.dht_status.health.enabled = true;
        self.dht_status.health.cached_ipv4_routes = update.dht_nodes;
        self.dht_status.health.active_ipv4_routes = update.dht_nodes / 2;
        self.dht_status.health.dht_size_estimate = Some(DhtSizeEstimate {
            node_count: update.dht_nodes,
            std_dev: Some(update.dht_nodes as f64 * 0.08),
        });
        self.dht_wave_telemetry.active_lookups = update.dht_active_lookups;
        self.dht_wave_telemetry.inflight_ipv4_queries = update.dht_active_lookups;
        self.dht_wave_telemetry.inflight_ipv6_queries = update.dht_active_lookups / 2;
        self.dht_wave_telemetry.unique_peers_found_last_10s = update.dht_peers_found;

        state.ui.needs_redraw = true;
    }

    pub fn advance_mock_visualizations(&mut self, delta_seconds: f64) {
        let delta_seconds = delta_seconds.clamp(0.0, 0.25);
        if delta_seconds == 0.0 {
            return;
        }
        advance_ui_effects_for_elapsed(
            &mut self.app_state,
            &self.client_configs,
            &self.dht_status,
            &self.dht_wave_telemetry,
            delta_seconds,
        );
        self.fps_sample_elapsed += delta_seconds;
        self.fps_sample_frames = self.fps_sample_frames.saturating_add(1);
        if self.fps_sample_elapsed >= 1.0 {
            let measured = f64::from(self.fps_sample_frames) / self.fps_sample_elapsed;
            let target = self.app_state.data_rate.target_fps();
            // requestAnimationFrame commonly reports one frame below its nominal refresh rate
            // because the sampling window straddles a callback boundary. Keep genuine misses
            // visible while avoiding a false 59/60 oscillation in the unchanged production footer.
            self.app_state.ui.measured_fps = Some(if measured >= target * 0.98 {
                target
            } else {
                measured
            });
            self.fps_sample_elapsed = 0.0;
            self.fps_sample_frames = 0;
        }
        self.app_state.ui.needs_redraw = true;
    }

    pub fn set_torrent_paused_hex(&mut self, info_hash_hex: &str, paused: bool) -> bool {
        let Ok(info_hash) = hex::decode(info_hash_hex) else {
            return false;
        };
        let Some(torrent) = self.app_state.torrents.get_mut(&info_hash) else {
            return false;
        };
        torrent.latest_state.torrent_control_state = if paused {
            TorrentControlState::Paused
        } else {
            TorrentControlState::Running
        };
        self.app_state.ui.needs_redraw = true;
        true
    }

    pub fn remove_torrent_hex(&mut self, info_hash_hex: &str) -> bool {
        let Ok(info_hash) = hex::decode(info_hash_hex) else {
            return false;
        };
        self.remove_torrent(&info_hash)
    }

    fn remove_torrent(&mut self, info_hash: &[u8]) -> bool {
        let removed = self.app_state.torrents.remove(info_hash).is_some();
        self.torrent_manager_command_txs.remove(info_hash);
        self.torrent_metric_watch_rxs.remove(info_hash);
        self.browser_tracked_peers
            .retain(|(torrent_hash, _), _| torrent_hash.as_slice() != info_hash);
        if removed {
            let mut peer_view = (*self.app_state.peer_manager_view).clone();
            peer_view
                .tracked_peers
                .retain(|peer| peer.torrent_info_hash.as_slice() != info_hash);
            peer_view.registered_torrents = self.app_state.torrents.len();
            self.app_state.peer_manager_view = Arc::new(peer_view);
            peers::recompute_peer_management_derived(
                &mut self.app_state,
                web_time::SystemTime::now(),
            );
        }
        self.app_state
            .torrent_list_order
            .retain(|candidate| candidate.as_slice() != info_hash);
        if self.app_state.torrent_list_order.is_empty() {
            self.app_state.ui.selected_torrent_index = 0;
        } else {
            self.app_state.ui.selected_torrent_index = self
                .app_state
                .ui
                .selected_torrent_index
                .min(self.app_state.torrent_list_order.len() - 1);
        }
        self.app_state.ui.needs_redraw = true;
        removed
    }

    pub fn torrent_control_state_hex(
        &self,
        info_hash_hex: &str,
    ) -> Option<BrowserTorrentControlState> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        self.app_state.torrents.get(&info_hash).map(|torrent| {
            match torrent.latest_state.torrent_control_state {
                TorrentControlState::Running => BrowserTorrentControlState::Running,
                TorrentControlState::Paused => BrowserTorrentControlState::Paused,
                TorrentControlState::Deleting => BrowserTorrentControlState::Deleting,
            }
        })
    }

    pub fn torrent_delete_files_hex(&self, info_hash_hex: &str) -> Option<bool> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        self.app_state
            .torrents
            .get(&info_hash)
            .map(|torrent| torrent.latest_state.delete_files)
    }

    pub fn torrent_file_priority_hex(
        &self,
        info_hash_hex: &str,
        file_index: usize,
    ) -> Option<BrowserFilePriority> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        self.app_state
            .torrents
            .get(&info_hash)?
            .latest_state
            .file_priorities
            .get(&file_index)
            .and_then(|priority| match priority {
                FilePriority::High => Some(BrowserFilePriority::High),
                FilePriority::Skip => Some(BrowserFilePriority::Skip),
                FilePriority::Normal | FilePriority::Mixed => None,
            })
    }

    pub fn default_download_folder(&self) -> Option<&PathBuf> {
        self.client_configs.default_download_folder.as_ref()
    }

    pub fn set_browser_add_location_prompt(&mut self, enabled: bool) {
        self.client_configs.always_show_add_location_prompt = enabled;
    }

    pub fn set_browser_default_download_folder(&mut self, path: PathBuf) {
        self.client_configs.default_download_folder = Some(path);
    }

    pub fn apply_next_browser_theme_setting(&mut self) {
        let themes = ThemeName::sorted_for_ui();
        let current = themes
            .iter()
            .position(|theme| *theme == self.client_configs.ui_theme)
            .unwrap_or_default();
        let mut settings = self.client_configs.clone();
        settings.ui_theme = themes[(current + 1) % themes.len()];
        self.apply_browser_config_update(settings);
    }

    pub fn browser_network_interface_count(&self) -> usize {
        self.app_state
            .ui
            .config
            .network_interface_inventory
            .interfaces
            .len()
    }

    pub fn browser_network_interface_refreshes(&self) -> u64 {
        self.browser_network_interface_refreshes
    }

    pub fn file_browser_current_path(&self) -> &PathBuf {
        &self.app_state.ui.file_browser.state.current_path
    }

    pub fn file_browser_cursor_path(&self) -> Option<&PathBuf> {
        self.app_state.ui.file_browser.state.cursor_path.as_ref()
    }

    pub fn delete_confirmation(&self) -> Option<(&[u8], bool)> {
        matches!(self.app_state.mode, AppMode::DeleteConfirm).then_some((
            self.app_state.ui.delete_confirm.info_hash.as_slice(),
            self.app_state.ui.delete_confirm.with_files,
        ))
    }

    pub fn torrent_count(&self) -> usize {
        self.app_state.torrents.len()
    }

    pub fn rss_feed_count(&self) -> usize {
        self.client_configs.rss.feeds.len()
    }

    pub fn rss_enabled_feed_count(&self) -> usize {
        self.client_configs
            .rss
            .feeds
            .iter()
            .filter(|feed| feed.enabled)
            .count()
    }

    pub fn rss_history_count(&self) -> usize {
        self.app_state.rss_runtime.history.len()
    }

    pub fn rss_downloaded_preview_count(&self) -> usize {
        self.app_state
            .rss_runtime
            .preview_items
            .iter()
            .filter(|item| item.is_downloaded)
            .count()
    }

    pub fn rss_last_sync_at(&self) -> Option<&str> {
        self.app_state.rss_runtime.last_sync_at.as_deref()
    }

    pub fn system_error(&self) -> Option<&str> {
        self.app_state.system_error.as_deref()
    }

    pub fn torrent_sort_column(&self) -> &'static str {
        match self.app_state.torrent_sort.0 {
            TorrentSortColumn::Name => "name",
            TorrentSortColumn::Up => "up",
            TorrentSortColumn::Down => "down",
            TorrentSortColumn::Progress => "progress",
        }
    }

    pub fn torrent_sort_pinned(&self) -> bool {
        self.app_state.torrent_sort_pinned
    }

    pub fn torrent_sort_direction(&self) -> &'static str {
        match self.app_state.torrent_sort.1 {
            SortDirection::Ascending => "ascending",
            SortDirection::Descending => "descending",
        }
    }

    pub fn ordered_torrent_rates(&self) -> Vec<(u64, u64)> {
        self.app_state
            .torrent_list_order
            .iter()
            .filter_map(|info_hash| self.app_state.torrents.get(info_hash))
            .map(|torrent| {
                (
                    torrent.smoothed_download_speed_bps,
                    torrent.smoothed_upload_speed_bps,
                )
            })
            .collect()
    }

    pub fn anonymize_names(&self) -> bool {
        self.app_state.anonymize_torrent_names
    }

    pub fn selected_torrent_hash_hex(&self) -> Option<String> {
        self.app_state
            .torrent_list_order
            .get(self.app_state.ui.selected_torrent_index)
            .map(hex::encode)
    }

    pub fn selected_peer_rates(&self) -> Vec<(String, u64, u64)> {
        self.app_state
            .torrent_list_order
            .get(self.app_state.ui.selected_torrent_index)
            .and_then(|info_hash| self.app_state.torrents.get(info_hash))
            .map(|torrent| {
                torrent
                    .latest_state
                    .peers
                    .iter()
                    .map(|peer| {
                        (
                            peer.address.clone(),
                            peer.download_speed_bps,
                            peer.upload_speed_bps,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn peer_manager_metrics_updates(&self) -> u64 {
        self.browser_peer_metrics_updates
    }

    pub fn selected_peer_rate_frame_updates(&self) -> u64 {
        self.browser_selected_peer_rate_frame_updates
    }

    pub fn selected_peer_rate_frame_changes(&self) -> u64 {
        self.browser_selected_peer_rate_frame_changes
    }

    pub fn oldest_peer_last_seen_age_secs(&self) -> Option<u64> {
        let now = web_time::SystemTime::now();
        self.app_state
            .peer_manager_view
            .tracked_peers
            .iter()
            .filter_map(|peer| peer.last_seen)
            .map(|last_seen| now.duration_since(last_seen).unwrap_or_default().as_secs())
            .max()
    }

    pub fn select_torrent_hex(&mut self, info_hash_hex: &str) -> bool {
        let Ok(info_hash) = hex::decode(info_hash_hex) else {
            return false;
        };
        let Some(index) = self
            .app_state
            .torrent_list_order
            .iter()
            .position(|candidate| candidate == &info_hash)
        else {
            return false;
        };
        self.app_state.ui.selected_torrent_index = index;
        self.app_state.ui.needs_redraw = true;
        true
    }

    pub fn torrent_snapshot_hex(&self, info_hash_hex: &str) -> Option<BrowserTorrentSnapshot> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        let torrent = self.app_state.torrents.get(&info_hash)?;
        let latest = &torrent.latest_state;
        Some(BrowserTorrentSnapshot {
            info_hash_hex: info_hash_hex.to_string(),
            name: latest.torrent_name.clone(),
            control_state: match latest.torrent_control_state {
                TorrentControlState::Running => BrowserTorrentControlState::Running,
                TorrentControlState::Paused => BrowserTorrentControlState::Paused,
                TorrentControlState::Deleting => BrowserTorrentControlState::Deleting,
            },
            activity: latest.activity_message.clone(),
            pieces_total: latest.number_of_pieces_total,
            pieces_completed: latest.number_of_pieces_completed,
            total_size: latest.total_size,
            bytes_written: latest.bytes_written,
            download_speed_bps: latest.download_speed_bps,
            upload_speed_bps: latest.upload_speed_bps,
            bytes_downloaded_this_tick: latest.bytes_downloaded_this_tick,
            bytes_uploaded_this_tick: latest.bytes_uploaded_this_tick,
            eta: latest.eta,
            next_announce_in: latest.next_announce_in,
            connected_peers: latest.number_of_successfully_connected_peers,
            tcp_peers: latest.tcp_peer_count,
            utp_peers: latest.utp_peer_count,
            beneficial_tcp_peers: latest.beneficial_tcp_peer_count,
            beneficial_utp_peers: latest.beneficial_utp_peer_count,
            session_downloaded: latest.session_total_downloaded,
            session_uploaded: latest.session_total_uploaded,
            data_available: latest.data_available,
            is_complete: latest.is_complete,
            download_history_len: torrent.download_history.len(),
            upload_history_len: torrent.upload_history.len(),
        })
    }

    pub fn torrent_download_path_hex(&self, info_hash_hex: &str) -> Option<&PathBuf> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        self.app_state
            .torrents
            .get(&info_hash)?
            .latest_state
            .download_path
            .as_ref()
    }

    pub fn torrent_container_name_hex(&self, info_hash_hex: &str) -> Option<&str> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        self.app_state
            .torrents
            .get(&info_hash)?
            .latest_state
            .container_name
            .as_deref()
    }

    pub fn selected_torrent_snapshot(&self) -> Option<BrowserTorrentSnapshot> {
        self.selected_torrent_hash_hex()
            .and_then(|info_hash| self.torrent_snapshot_hex(&info_hash))
    }

    pub fn visualization_snapshot(&self) -> BrowserVisualizationSnapshot {
        let selected = self
            .app_state
            .torrent_list_order
            .get(self.app_state.ui.selected_torrent_index)
            .and_then(|info_hash| self.app_state.torrents.get(info_hash));
        BrowserVisualizationSnapshot {
            total_download_bps: self
                .app_state
                .torrents
                .values()
                .map(|torrent| torrent.latest_state.download_speed_bps)
                .sum(),
            total_upload_bps: self
                .app_state
                .torrents
                .values()
                .map(|torrent| torrent.latest_state.upload_speed_bps)
                .sum(),
            disk_read_bps: self.app_state.avg_disk_read_bps,
            disk_write_bps: self.app_state.avg_disk_write_bps,
            effects_phase_time: self.app_state.ui.effects_phase_time,
            file_download_phase: self.app_state.ui.file_activity_download_phase,
            file_upload_phase: self.app_state.ui.file_activity_upload_phase,
            disk_health_phase: self.app_state.disk_health_phase,
            disk_health_state_level: self.app_state.disk_health_state_level,
            tracked_peers: self.app_state.peer_manager_view.tracked_peers.len(),
            network_history_samples: self.app_state.network_history_state.tiers.second_1s.len(),
            activity_history_samples: self
                .app_state
                .activity_history_state
                .cpu
                .tiers
                .second_1s
                .len(),
            peer_connected_events: selected
                .map(|torrent| torrent.peer_connection_history.iter().sum())
                .unwrap_or_default(),
            peer_discovered_events: selected
                .map(|torrent| torrent.peer_discovery_history.iter().sum())
                .unwrap_or_default(),
            peer_disconnected_events: selected
                .map(|torrent| torrent.peer_disconnect_history.iter().sum())
                .unwrap_or_default(),
            blocks_received_events: selected
                .map(|torrent| torrent.latest_state.blocks_in_history.iter().sum())
                .unwrap_or_default(),
            blocks_sent_events: selected
                .map(|torrent| torrent.latest_state.blocks_out_history.iter().sum())
                .unwrap_or_default(),
            read_iops: self.app_state.read_iops,
            write_iops: self.app_state.write_iops,
            disk_read_latency_micros: self.app_state.avg_disk_read_latency.as_micros() as u64,
            disk_write_latency_micros: self.app_state.avg_disk_write_latency.as_micros() as u64,
            recv_to_write_latency_micros: self.app_state.recv_to_write_p95.as_micros() as u64,
            recent_file_activity: selected
                .map(|torrent| torrent.recent_file_activity.len())
                .unwrap_or_default(),
            recent_file_download_activity: selected
                .map(|torrent| {
                    torrent
                        .recent_file_activity
                        .values()
                        .filter(|activity| activity.download_at.is_some())
                        .count()
                })
                .unwrap_or_default(),
            recent_file_upload_activity: selected
                .map(|torrent| {
                    torrent
                        .recent_file_activity
                        .values()
                        .filter(|activity| activity.upload_at.is_some())
                        .count()
                })
                .unwrap_or_default(),
            swarm_availability_samples: selected
                .map(|torrent| torrent.swarm_availability_history.len())
                .unwrap_or_default(),
            dht_wave_initialized: self.app_state.ui.dht_wave.initialized,
            dht_active_queries: self.dht_wave_telemetry.inflight_ipv4_queries
                + self.dht_wave_telemetry.inflight_ipv6_queries,
            dht_peers_found: self.dht_wave_telemetry.unique_peers_found_last_10s,
            dht_query_load: self.app_state.ui.dht_wave.query_load,
        }
    }

    pub fn torrent_management_cursor_hash_hex(&self) -> Option<String> {
        self.app_state
            .ui
            .torrent_management
            .cursor_hash
            .as_deref()
            .map(hex::encode)
    }
}

pub fn canonical_browser_magnet_info_hash(magnet_link: &str) -> Option<Vec<u8>> {
    crate::torrent_identity::canonical_info_hash_from_magnet_link(magnet_link)
}
