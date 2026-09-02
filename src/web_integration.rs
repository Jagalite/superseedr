// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Narrow WASM-only bridge from browser-owned behavior to production reducers and rendering.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use ratatui::Frame;

use crate::app::{
    advance_ui_effects_for_elapsed, align_unpinned_sort_with_visible_activity,
    build_torrent_preview_tree, refresh_autosort_after_stats, App, AppCommand, AppMode, ConfigItem,
    DataRate, FileBrowserMode, FileMetadata, FilePriority, RssPreviewItem, TorrentControlState,
    TorrentFilePreview, TorrentFilePreviewState, TorrentMetrics,
};
use crate::config::{
    RssAddedVia, RssFeed, RssFilter, RssFilterMode, RssHistoryEntry, Settings, SortDirection,
    TorrentSortColumn,
};
use crate::dht_service::{DhtSizeEstimate, DhtStatus, DhtWaveTelemetry};
use crate::integrations::control::ControlRequest;
use crate::networking::NetworkInterfaceInfo;
use crate::peer_manager::{PeerManagerEndpointView, PeerManagerTrackedPeer, PeerManagerView};
use crate::persistence::activity_history::{
    ActivityHistoryPersistedState, ActivityHistoryRollupState,
};
use crate::persistence::event_journal::{EventCategory, EventJournalEntry, EventScope, EventType};
use crate::persistence::network_history::{
    NetworkHistoryPersistedState, NetworkHistoryRollupState,
};
use crate::presentation::{PresentationFixture, PresentationState};
use crate::telemetry::activity_history_telemetry::ActivityHistoryTelemetry;
use crate::telemetry::network_history_telemetry::NetworkHistoryTelemetry;
use crate::telemetry::ui_telemetry::UiTelemetry;
use crate::terminal_event::Event;
use crate::theme::{Theme, ThemeName};
use crate::torrent_manager::{
    DiskIoOperation, FileActivityDirection, FileActivityUpdate, ManagerEvent,
};
use crate::tui::screens::{peers, rss};
use crate::tui::tree::RawNode;
use strum::IntoEnumIterator;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrowserCommand {
    AddMagnet {
        magnet_link: String,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        validation_status: bool,
    },
    Pause {
        info_hash_hex: String,
    },
    Resume {
        info_hash_hex: String,
    },
    Delete {
        info_hash_hex: String,
        delete_files: bool,
    },
    FetchFileTree {
        browser_generation: u64,
        path: PathBuf,
        highlight_path: Option<PathBuf>,
    },
    FetchTorrentPreview {
        browser_generation: u64,
        request_id: u64,
        path: PathBuf,
    },
    AddTorrentFromFile {
        path: PathBuf,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        validation_status: bool,
        file_priorities: Vec<BrowserFilePriorityOverride>,
    },
    SetTorrentConfig {
        info_hash_hex: String,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        file_priorities: Vec<BrowserFilePriorityOverride>,
    },
    RssSyncNow,
    RssDownloadPreview {
        item: BrowserRssPreview,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserRssPreview {
    pub dedupe_key: String,
    pub title: String,
    pub link: Option<String>,
    pub guid: Option<String>,
    pub source: Option<String>,
    pub date_iso: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserFilePriority {
    High,
    Skip,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserFilePriorityOverride {
    pub file_index: usize,
    pub priority: BrowserFilePriority,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BrowserTorrentControlState {
    #[default]
    Running,
    Paused,
    Deleting,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BrowserPeerTransport {
    #[default]
    Tcp,
    Utp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserFileActivityDirection {
    Download,
    Upload,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserFileActivityUpdate {
    pub touched_relative_paths: Vec<String>,
    pub direction: BrowserFileActivityDirection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserScreen {
    Welcome,
    Normal,
    Help,
    Journal,
    PeerManagement,
    TorrentManagement,
    PowerSaving,
    DeleteConfirm,
    Config,
    FileBrowser,
    Rss,
}

#[derive(Clone, Debug, Default)]
pub struct BrowserTorrentUpdate {
    pub info_hash: Vec<u8>,
    pub torrent_name: String,
    pub torrent_or_magnet: String,
    pub pieces_total: u32,
    pub pieces_completed: u32,
    pub download_speed_bps: u64,
    pub upload_speed_bps: u64,
    pub bytes_downloaded_this_tick: u64,
    pub bytes_uploaded_this_tick: u64,
    pub eta: Duration,
    pub next_announce_in: Duration,
    pub activity_message: String,
    pub download_path: Option<PathBuf>,
    pub container_name: Option<String>,
    pub control_state: BrowserTorrentControlState,
    pub data_available: bool,
    pub is_complete: bool,
    pub total_size: u64,
    pub bytes_written: u64,
    pub session_downloaded: u64,
    pub session_uploaded: u64,
    pub peers: Vec<BrowserPeerUpdate>,
    pub files: Vec<BrowserFileUpdate>,
    pub file_activity_updates: Vec<BrowserFileActivityUpdate>,
    pub download_history: Vec<u64>,
    pub upload_history: Vec<u64>,
    pub blocks_in_history: Vec<u64>,
    pub blocks_out_history: Vec<u64>,
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,
    pub peer_discovery_history: Vec<u64>,
    pub peer_connection_history: Vec<u64>,
    pub peer_disconnect_history: Vec<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct BrowserTorrentFrameUpdate {
    pub info_hash: Vec<u8>,
    pub control_state: BrowserTorrentControlState,
    pub pieces_total: u32,
    pub pieces_completed: u32,
    pub download_speed_bps: u64,
    pub upload_speed_bps: u64,
    pub bytes_downloaded_this_tick: u64,
    pub bytes_uploaded_this_tick: u64,
    pub session_downloaded: u64,
    pub session_uploaded: u64,
    pub eta: Duration,
    pub next_announce_in: Duration,
    pub activity_message: String,
    pub data_available: bool,
    pub is_complete: bool,
    pub total_size: u64,
    pub bytes_written: u64,
    pub peer_rates: Vec<BrowserPeerRateFrameUpdate>,
}

#[derive(Clone, Debug, Default)]
pub struct BrowserPeerRateFrameUpdate {
    pub address: String,
    pub download_speed_bps: u64,
    pub upload_speed_bps: u64,
}

#[derive(Clone, Debug, Default)]
pub struct BrowserPeerUpdate {
    pub address: String,
    pub client: String,
    pub download_speed_bps: u64,
    pub upload_speed_bps: u64,
    pub total_downloaded: u64,
    pub total_uploaded: u64,
    pub bitfield: Vec<bool>,
    pub transport: BrowserPeerTransport,
    pub am_choking: bool,
    pub peer_choking: bool,
    pub am_interested: bool,
    pub peer_interested: bool,
    pub connection_count: u64,
    pub disconnect_count: u64,
    pub last_action: String,
}

#[derive(Clone, Debug, Default)]
pub struct BrowserFileUpdate {
    pub relative_path: String,
    pub size: u64,
}

#[derive(Clone, Debug, Default)]
pub struct BrowserFileTreeEntry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BrowserTorrentPreviewFile {
    pub relative_path: String,
    pub size: u64,
}

#[derive(Clone, Debug, Default)]
pub struct BrowserJournalUpdate {
    pub timestamp: String,
    pub torrent_name: Option<String>,
    pub message: String,
    pub kind: BrowserJournalKind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BrowserJournalKind {
    #[default]
    Lifecycle,
    DataUnavailable,
    DataRecovered,
}

#[derive(Clone, Debug, Default)]
pub struct BrowserRssUpdate {
    pub feed_url: String,
    pub filter_query: String,
    pub item_title: String,
    pub item_link: String,
    pub timestamp: String,
}

#[derive(Clone, Debug, Default)]
pub struct BrowserTelemetryUpdate {
    pub cpu_usage: f32,
    pub ram_usage_percent: f32,
    pub app_ram_usage: u64,
    pub run_time: u64,
    pub total_download_history: Vec<u64>,
    pub total_upload_history: Vec<u64>,
    pub disk_read_history: Vec<u64>,
    pub disk_write_history: Vec<u64>,
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,
    pub disk_backoff_history_ms: Vec<u64>,
    pub dht_nodes: usize,
    pub dht_active_lookups: usize,
    pub dht_peers_found: usize,
    pub filesystem: Vec<BrowserFileUpdate>,
    pub journal: Vec<BrowserJournalUpdate>,
    pub rss: Vec<BrowserRssUpdate>,
}

#[derive(Clone, Debug, Default)]
pub struct BrowserRuntimeTelemetryUpdate {
    pub cpu_usage: f32,
    pub ram_usage_percent: f32,
    pub app_ram_usage: u64,
    pub run_time: u64,
    pub total_download_history: Vec<u64>,
    pub total_upload_history: Vec<u64>,
    pub disk_read_history: Vec<u64>,
    pub disk_write_history: Vec<u64>,
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,
    pub disk_backoff_history_ms: Vec<u64>,
    pub dht_nodes: usize,
    pub dht_active_lookups: usize,
    pub dht_peers_found: usize,
}

#[derive(Clone, Debug, Default)]
pub struct BrowserManagerEventUpdate {
    pub info_hash: Vec<u8>,
    pub peers_discovered: usize,
    pub peers_connected: usize,
    pub peers_disconnected: usize,
    pub blocks_received: usize,
    pub blocks_sent: usize,
    pub disk_read_bytes: u64,
    pub disk_write_bytes: u64,
    pub disk_read_operations: usize,
    pub disk_write_operations: usize,
    pub disk_operation_sequence: u64,
    pub disk_seek_chaos: bool,
    pub disk_read_latency_micros: u64,
    pub disk_write_latency_micros: u64,
    pub recv_to_write_latency_micros: u64,
    pub disk_backoff_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserTorrentSnapshot {
    pub info_hash_hex: String,
    pub name: String,
    pub control_state: BrowserTorrentControlState,
    pub activity: String,
    pub pieces_total: u32,
    pub pieces_completed: u32,
    pub total_size: u64,
    pub bytes_written: u64,
    pub download_speed_bps: u64,
    pub upload_speed_bps: u64,
    pub bytes_downloaded_this_tick: u64,
    pub bytes_uploaded_this_tick: u64,
    pub eta: Duration,
    pub next_announce_in: Duration,
    pub connected_peers: usize,
    pub tcp_peers: usize,
    pub utp_peers: usize,
    pub beneficial_tcp_peers: usize,
    pub beneficial_utp_peers: usize,
    pub session_downloaded: u64,
    pub session_uploaded: u64,
    pub data_available: bool,
    pub is_complete: bool,
    pub download_history_len: usize,
    pub upload_history_len: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BrowserVisualizationSnapshot {
    pub total_download_bps: u64,
    pub total_upload_bps: u64,
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,
    pub effects_phase_time: f64,
    pub file_download_phase: f64,
    pub file_upload_phase: f64,
    pub disk_health_phase: f64,
    pub disk_health_state_level: u8,
    pub tracked_peers: usize,
    pub network_history_samples: usize,
    pub activity_history_samples: usize,
    pub peer_connected_events: u64,
    pub peer_discovered_events: u64,
    pub peer_disconnected_events: u64,
    pub blocks_received_events: u64,
    pub blocks_sent_events: u64,
    pub read_iops: u32,
    pub write_iops: u32,
    pub disk_read_latency_micros: u64,
    pub disk_write_latency_micros: u64,
    pub recv_to_write_latency_micros: u64,
    pub recent_file_activity: usize,
    pub recent_file_download_activity: usize,
    pub recent_file_upload_activity: usize,
    pub swarm_availability_samples: usize,
    pub dht_wave_initialized: bool,
    pub dht_active_queries: usize,
    pub dht_peers_found: usize,
    pub dht_query_load: f64,
}

pub struct BrowserSession {
    app: App,
    dht_status: DhtStatus,
    dht_wave_telemetry: DhtWaveTelemetry,
    pending_browser_commands: VecDeque<BrowserCommand>,
    browser_tracked_peers: HashMap<(Vec<u8>, String), PeerManagerTrackedPeer>,
    browser_peer_metrics_updates: u64,
    browser_selected_peer_rate_frame_updates: u64,
    browser_selected_peer_rate_frame_changes: u64,
    browser_disk_operation_sequence: u64,
    browser_network_interface_refreshes: u64,
    fps_sample_elapsed: f64,
    fps_sample_frames: u32,
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
        let mut app = App::new(app_state, settings);
        app.app_state
            .ui
            .config
            .network_interface_inventory
            .interfaces = simulated_browser_network_interfaces();
        Self {
            app,
            dht_status,
            dht_wave_telemetry,
            pending_browser_commands: VecDeque::new(),
            browser_tracked_peers: HashMap::new(),
            browser_peer_metrics_updates: 0,
            browser_selected_peer_rate_frame_updates: 0,
            browser_selected_peer_rate_frame_changes: 0,
            browser_disk_operation_sequence: 0,
            browser_network_interface_refreshes: 0,
            fps_sample_elapsed: 0.0,
            fps_sample_frames: 0,
        }
    }

    pub async fn dispatch_event(&mut self, event: Event) {
        crate::app::tui_effect_executor::handle_event(&mut self.app, event).await;
        self.sync_mock_torrent_preview_request();
    }

    pub async fn flush_pending_paste_burst(&mut self) {
        crate::app::tui_effect_executor::flush_pending_paste_burst(&mut self.app).await;
        self.sync_mock_torrent_preview_request();
    }

    pub fn draw(&self, frame: &mut Frame) {
        crate::tui::render::draw(
            frame,
            &self.app.app_state,
            &self.dht_status,
            &self.dht_wave_telemetry,
            &self.app.client_configs,
        );
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.app.app_state.screen_area =
            ratatui::layout::Rect::new(0, 0, width.max(1), height.max(1));
        self.app.app_state.ui.needs_redraw = true;
    }

    pub fn screen_size(&self) -> (u16, u16) {
        (
            self.app.app_state.screen_area.width,
            self.app.app_state.screen_area.height,
        )
    }

    pub fn theme_name(&self) -> ThemeName {
        self.app.client_configs.ui_theme
    }

    pub fn rendered_theme_name(&self) -> ThemeName {
        self.app.app_state.theme.name
    }

    pub fn target_fps(&self) -> f64 {
        self.app.app_state.data_rate.target_fps()
    }

    pub fn browser_download_limit_bps(&self) -> Option<u64> {
        let limit = self.app.client_configs.global_download_limit_bps;
        (!crate::config::is_unlimited_rate_limit_bps(limit)).then_some(limit)
    }

    pub fn browser_upload_limit_bps(&self) -> Option<u64> {
        let limit = self.app.client_configs.global_upload_limit_bps;
        (!crate::config::is_unlimited_rate_limit_bps(limit)).then_some(limit)
    }

    pub fn effective_download_limit_bps(&self) -> u64 {
        self.browser_download_limit_bps().unwrap_or_default()
    }

    pub fn configured_upload_limit_bps(&self) -> u64 {
        self.app.client_configs.global_upload_limit_bps
    }

    pub fn fps_label(&self) -> String {
        crate::tui::screens::normal::footer_fps_label(&self.app.app_state)
    }

    pub fn set_screen(&mut self, screen: BrowserScreen) {
        match screen {
            BrowserScreen::Config => {
                *self.app.app_state.ui.config.settings_edit = self.app.client_configs.clone();
                self.app.app_state.ui.config.selected_index = 0;
                self.app.app_state.ui.config.items = ConfigItem::iter().collect();
                self.refresh_browser_network_interfaces();
            }
            BrowserScreen::DeleteConfirm => {
                if let Some(info_hash) = self.app.app_state.torrent_list_order.first() {
                    self.app.app_state.ui.delete_confirm.info_hash = info_hash.clone();
                    self.app.app_state.ui.delete_confirm.with_files = false;
                }
            }
            BrowserScreen::TorrentManagement => {
                crate::tui::screens::torrents::initialize_torrent_management_cursor(
                    &mut self.app.app_state,
                );
            }
            BrowserScreen::FileBrowser => {
                self.app.app_state.ui.file_browser.browser_mode = FileBrowserMode::Directory;
            }
            _ => {}
        }
        self.app.app_state.mode = match screen {
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
        self.app.app_state.ui.needs_redraw = true;
    }

    pub fn screen(&self) -> BrowserScreen {
        match self.app.app_state.mode {
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

    pub fn normal_text_input_active(&self) -> bool {
        matches!(self.app.app_state.mode, AppMode::Normal) && self.app.app_state.ui.is_searching
    }

    pub fn normal_search_query(&self) -> &str {
        &self.app.app_state.ui.search_query
    }

    pub fn web_quit_key_enabled(&self) -> bool {
        matches!(self.app.app_state.mode, AppMode::Normal)
            && !self.app.app_state.ui.is_searching
            && !self.app.app_state.ui.visualization_focus.active
    }

    pub fn should_quit(&self) -> bool {
        self.app.app_state.should_quit
    }

    pub fn drain_commands(&mut self) -> Vec<BrowserCommand> {
        let mut commands = Vec::new();
        let mut queued_app_commands = VecDeque::new();
        while let Ok(command) = self.app.app_command_rx.try_recv() {
            queued_app_commands.push_back(command);
        }
        while let Some(command) = queued_app_commands.pop_front() {
            let command = match command {
                AppCommand::BrowserBatch(batch) => {
                    for command in batch.into_iter().rev() {
                        queued_app_commands.push_front(command);
                    }
                    continue;
                }
                command => command,
            };
            let command = match command {
                AppCommand::AddTorrentFromFile(path) => BrowserCommand::AddTorrentFromFile {
                    path,
                    download_path: self.app.client_configs.default_download_folder.clone(),
                    container_name: None,
                    validation_status: false,
                    file_priorities: Vec::new(),
                },
                AppCommand::FetchFileTree {
                    browser_generation,
                    path,
                    browser_mode,
                    preserve_browser_mode,
                    highlight_path,
                } => {
                    if !self.app.begin_file_browser_fetch(
                        browser_generation,
                        path.clone(),
                        browser_mode,
                        preserve_browser_mode,
                    ) {
                        continue;
                    }
                    BrowserCommand::FetchFileTree {
                        browser_generation,
                        path,
                        highlight_path,
                    }
                }
                AppCommand::SubmitControlRequest(ControlRequest::AddMagnet {
                    magnet_link,
                    download_path,
                    container_name,
                    validation_status,
                    ..
                }) => BrowserCommand::AddMagnet {
                    magnet_link,
                    download_path,
                    container_name,
                    validation_status,
                },
                AppCommand::SubmitControlRequest(ControlRequest::AddTorrentFile {
                    source_path,
                    download_path,
                    container_name,
                    validation_status,
                    file_priorities,
                }) => BrowserCommand::AddTorrentFromFile {
                    path: source_path,
                    download_path,
                    container_name,
                    validation_status,
                    file_priorities: file_priorities
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
                        .collect(),
                },
                AppCommand::SubmitControlRequest(ControlRequest::Pause { info_hash_hex }) => {
                    BrowserCommand::Pause { info_hash_hex }
                }
                AppCommand::SubmitControlRequest(ControlRequest::Resume { info_hash_hex }) => {
                    BrowserCommand::Resume { info_hash_hex }
                }
                AppCommand::SubmitControlRequest(ControlRequest::Delete {
                    info_hash_hex,
                    delete_files,
                }) => BrowserCommand::Delete {
                    info_hash_hex,
                    delete_files,
                },
                AppCommand::SubmitControlRequest(ControlRequest::SetTorrentConfig {
                    info_hash_hex,
                    download_path,
                    container_name,
                    file_priorities,
                }) => BrowserCommand::SetTorrentConfig {
                    info_hash_hex,
                    download_path,
                    container_name,
                    file_priorities: file_priorities
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
                        .collect(),
                },
                AppCommand::UpdateConfig(settings) => {
                    self.apply_browser_config_update(settings);
                    continue;
                }
                AppCommand::RssSyncNow => BrowserCommand::RssSyncNow,
                AppCommand::RssDownloadPreview(item) => BrowserCommand::RssDownloadPreview {
                    item: BrowserRssPreview {
                        dedupe_key: item.dedupe_key,
                        title: item.title,
                        link: item.link,
                        guid: item.guid,
                        source: item.source,
                        date_iso: item.date_iso,
                    },
                },
                AppCommand::RefreshConfigNetworkInterfaces => {
                    self.refresh_browser_network_interfaces();
                    continue;
                }
                _ => continue,
            };
            commands.push(command);
        }
        commands.extend(self.pending_browser_commands.drain(..));
        commands
    }

    fn apply_browser_config_update(&mut self, settings: Settings) {
        self.app.app_state.effective_download_limit_bps = settings.global_download_limit_bps;
        self.app.app_state.theme = Theme::builtin(settings.ui_theme);
        self.app.client_configs = settings;
        rss::recompute_rss_derived(&mut self.app.app_state, &self.app.client_configs);
        self.app.app_state.ui.needs_redraw = true;
    }

    fn refresh_browser_network_interfaces(&mut self) {
        let inventory = &mut self.app.app_state.ui.config.network_interface_inventory;
        inventory.interfaces = simulated_browser_network_interfaces();
        inventory.loading = false;
        inventory.error = None;
        self.browser_network_interface_refreshes =
            self.browser_network_interface_refreshes.saturating_add(1);
        self.app.app_state.ui.needs_redraw = true;
    }

    pub fn apply_mock_file_tree(
        &mut self,
        browser_generation: u64,
        path: PathBuf,
        entries: Vec<BrowserFileTreeEntry>,
        highlight_path: Option<PathBuf>,
    ) -> bool {
        let browser = &mut self.app.app_state.ui.file_browser;
        if browser_generation != browser.browser_generation
            || !matches!(self.app.app_state.mode, AppMode::FileBrowser)
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
        self.app.app_state.ui.needs_redraw = true;
        self.sync_mock_torrent_preview_request();
        true
    }

    fn sync_mock_torrent_preview_request(&mut self) {
        let selected_path = if matches!(self.app.app_state.mode, AppMode::FileBrowser)
            && matches!(
                self.app.app_state.ui.file_browser.browser_mode,
                FileBrowserMode::File(_)
            )
            && !self.app.app_state.ui.file_browser.fetch_pending
        {
            let browser = &self.app.app_state.ui.file_browser;
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
            let browser = &mut self.app.app_state.ui.file_browser;
            if !matches!(browser.torrent_file_preview, TorrentFilePreviewState::Idle) {
                browser.torrent_preview_request_id =
                    browser.torrent_preview_request_id.wrapping_add(1);
                browser.torrent_file_preview = TorrentFilePreviewState::Idle;
                self.app.app_state.ui.needs_redraw = true;
            }
            return;
        };

        if self
            .app
            .app_state
            .ui
            .file_browser
            .torrent_file_preview
            .path()
            == Some(path.as_path())
        {
            return;
        }

        let browser = &mut self.app.app_state.ui.file_browser;
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
        self.app.app_state.ui.needs_redraw = true;
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
        let browser = &mut self.app.app_state.ui.file_browser;
        if !matches!(self.app.app_state.mode, AppMode::FileBrowser)
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
        self.app.app_state.ui.needs_redraw = true;
        true
    }

    pub fn torrent_preview_state(&self) -> &'static str {
        match self.app.app_state.ui.file_browser.torrent_file_preview {
            TorrentFilePreviewState::Idle => "idle",
            TorrentFilePreviewState::Loading { .. } => "loading",
            TorrentFilePreviewState::Ready { .. } => "ready",
            TorrentFilePreviewState::Error { .. } => "error",
        }
    }

    pub fn torrent_preview_name(&self) -> &str {
        match &self.app.app_state.ui.file_browser.torrent_file_preview {
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

        match &self.app.app_state.ui.file_browser.torrent_file_preview {
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
        let Some(torrent) = self.app.app_state.torrents.get_mut(&info_hash) else {
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
        self.app.app_state.ui.needs_redraw = true;
        true
    }

    pub fn apply_mock_rss_sync(&mut self, last_sync_at: String, next_sync_at: String) {
        self.app.app_state.rss_runtime.last_sync_at = Some(last_sync_at);
        self.app.app_state.rss_runtime.next_sync_at = Some(next_sync_at);
        rss::recompute_rss_derived(&mut self.app.app_state, &self.app.client_configs);
        self.app.app_state.ui.needs_redraw = true;
    }

    pub fn apply_mock_rss_download(&mut self, item: &BrowserRssPreview, info_hash: &[u8]) {
        for preview in &mut self.app.app_state.rss_runtime.preview_items {
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
            .app
            .app_state
            .rss_runtime
            .history
            .iter_mut()
            .find(|existing| existing.dedupe_key == entry.dedupe_key)
        {
            *existing = entry;
        } else {
            self.app.app_state.rss_runtime.history.push(entry);
        }
        rss::recompute_rss_derived(&mut self.app.app_state, &self.app.client_configs);
        self.app.app_state.ui.needs_redraw = true;
    }

    pub fn set_browser_error(&mut self, message: impl Into<String>) {
        self.app.app_state.system_error = Some(message.into());
        self.app.app_state.ui.needs_redraw = true;
    }

    pub fn clear_browser_error(&mut self) {
        self.app.app_state.system_error = None;
        self.app.app_state.ui.needs_redraw = true;
    }

    pub fn upsert_mock_torrent(&mut self, update: BrowserTorrentUpdate) {
        let info_hash = update.info_hash.clone();
        let tcp_peer_count = update
            .peers
            .iter()
            .filter(|peer| peer.transport == BrowserPeerTransport::Tcp)
            .count();
        let utp_peer_count = update.peers.len().saturating_sub(tcp_peer_count);
        let beneficial_tcp_peer_count = update
            .peers
            .iter()
            .filter(|peer| {
                peer.transport == BrowserPeerTransport::Tcp
                    && (peer.total_downloaded > 0 || peer.total_uploaded > 0)
            })
            .count();
        let beneficial_utp_peer_count = update
            .peers
            .iter()
            .filter(|peer| {
                peer.transport == BrowserPeerTransport::Utp
                    && (peer.total_downloaded > 0 || peer.total_uploaded > 0)
            })
            .count();
        let peers = update
            .peers
            .iter()
            .map(|peer| crate::app::PeerInfo {
                address: peer.address.clone(),
                peer_id: peer.client.as_bytes().to_vec(),
                am_choking: peer.am_choking,
                peer_choking: peer.peer_choking,
                am_interested: peer.am_interested,
                peer_interested: peer.peer_interested,
                bitfield: peer.bitfield.clone(),
                download_speed_bps: peer.download_speed_bps,
                upload_speed_bps: peer.upload_speed_bps,
                total_downloaded: peer.total_downloaded,
                total_uploaded: peer.total_uploaded,
                connection_count: peer.connection_count,
                disconnect_count: peer.disconnect_count,
                last_action: peer.last_action.clone(),
            })
            .collect::<Vec<_>>();
        let file_activity_updates = update
            .file_activity_updates
            .iter()
            .map(|activity| FileActivityUpdate {
                touched_relative_paths: activity.touched_relative_paths.clone(),
                direction: match activity.direction {
                    BrowserFileActivityDirection::Download => FileActivityDirection::Download,
                    BrowserFileActivityDirection::Upload => FileActivityDirection::Upload,
                },
            })
            .collect();

        UiTelemetry::on_metrics(
            &mut self.app.app_state,
            TorrentMetrics {
                torrent_control_state: match update.control_state {
                    BrowserTorrentControlState::Running => TorrentControlState::Running,
                    BrowserTorrentControlState::Paused => TorrentControlState::Paused,
                    BrowserTorrentControlState::Deleting => TorrentControlState::Deleting,
                },
                info_hash: info_hash.clone(),
                torrent_or_magnet: update.torrent_or_magnet.clone(),
                torrent_name: update.torrent_name,
                download_path: update.download_path,
                container_name: update.container_name,
                is_multi_file: update.files.len() > 1,
                file_count: Some(update.files.len()),
                data_available: update.data_available,
                is_complete: update.is_complete,
                number_of_successfully_connected_peers: peers.len(),
                tcp_peer_count,
                utp_peer_count,
                beneficial_tcp_peer_count,
                beneficial_utp_peer_count,
                number_of_pieces_total: update.pieces_total,
                number_of_pieces_completed: update.pieces_completed,
                download_speed_bps: update.download_speed_bps,
                upload_speed_bps: update.upload_speed_bps,
                bytes_downloaded_this_tick: update.bytes_downloaded_this_tick,
                bytes_uploaded_this_tick: update.bytes_uploaded_this_tick,
                session_total_downloaded: update.session_downloaded,
                session_total_uploaded: update.session_uploaded,
                eta: update.eta,
                peers,
                activity_message: update.activity_message,
                next_announce_in: update.next_announce_in,
                total_size: update.total_size,
                bytes_written: update.bytes_written,
                file_activity_updates,
                ..TorrentMetrics::default()
            },
        );

        let display = self
            .app
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
        if !self.app.app_state.torrent_list_order.contains(&info_hash) {
            self.app
                .app_state
                .torrent_list_order
                .push(info_hash.clone());
        }
        self.app.app_state.ui.needs_redraw = true;
    }

    pub fn apply_mock_torrent_frame(&mut self, update: BrowserTorrentFrameUpdate) {
        let updates_selected_torrent = self
            .app
            .app_state
            .torrent_list_order
            .get(self.app.app_state.ui.selected_torrent_index)
            == Some(&update.info_hash);
        let Some(display) = self.app.app_state.torrents.get_mut(&update.info_hash) else {
            return;
        };
        display.latest_state.torrent_control_state = match update.control_state {
            BrowserTorrentControlState::Running => TorrentControlState::Running,
            BrowserTorrentControlState::Paused => TorrentControlState::Paused,
            BrowserTorrentControlState::Deleting => TorrentControlState::Deleting,
        };
        display.latest_state.number_of_pieces_total = update.pieces_total;
        display.latest_state.number_of_pieces_completed = update.pieces_completed;
        display.latest_state.download_speed_bps = update.download_speed_bps;
        display.latest_state.upload_speed_bps = update.upload_speed_bps;
        display.smoothed_download_speed_bps = update.download_speed_bps;
        display.smoothed_upload_speed_bps = update.upload_speed_bps;
        display.latest_state.bytes_downloaded_this_tick = update.bytes_downloaded_this_tick;
        display.latest_state.bytes_uploaded_this_tick = update.bytes_uploaded_this_tick;
        display.latest_state.session_total_downloaded = update.session_downloaded;
        display.latest_state.session_total_uploaded = update.session_uploaded;
        display.latest_state.eta = update.eta;
        display.latest_state.next_announce_in = update.next_announce_in;
        display.latest_state.activity_message = update.activity_message;
        display.latest_state.data_available = update.data_available;
        display.latest_state.is_complete = update.is_complete;
        display.latest_state.total_size = update.total_size;
        display.latest_state.bytes_written = update.bytes_written;
        let mut peer_rate_changed = false;
        for peer_rate in update.peer_rates {
            let Some(peer) = display
                .latest_state
                .peers
                .iter_mut()
                .find(|peer| peer.address == peer_rate.address)
            else {
                continue;
            };
            peer_rate_changed |= peer.download_speed_bps != peer_rate.download_speed_bps
                || peer.upload_speed_bps != peer_rate.upload_speed_bps;
            peer.download_speed_bps = peer_rate.download_speed_bps;
            peer.upload_speed_bps = peer_rate.upload_speed_bps;
        }
        if updates_selected_torrent {
            self.browser_selected_peer_rate_frame_updates = self
                .browser_selected_peer_rate_frame_updates
                .saturating_add(1);
            self.browser_selected_peer_rate_frame_changes = self
                .browser_selected_peer_rate_frame_changes
                .saturating_add(u64::from(peer_rate_changed));
        }
        self.app.app_state.ui.needs_redraw = true;
    }

    pub fn refresh_mock_peer_manager(&mut self) {
        let snapshots = self
            .app
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
                        last_seen: Some(
                            web_time::SystemTime::UNIX_EPOCH
                                + Duration::from_secs(1_700_000_000 + self.app.app_state.run_time),
                        ),
                        clients: vec![String::from_utf8_lossy(&peer.peer_id).into_owned()],
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
        self.app.app_state.peer_manager_view = Arc::new(PeerManagerView {
            registered_torrents: self.app.app_state.torrents.len(),
            metrics_updates: self.browser_peer_metrics_updates,
            tracked_peers,
        });
        peers::recompute_peer_management_derived(
            &mut self.app.app_state,
            web_time::SystemTime::now(),
        );
    }

    pub fn apply_mock_manager_events(&mut self, update: BrowserManagerEventUpdate) {
        let disk_operation_sequence = if update.disk_seek_chaos {
            update.disk_operation_sequence
        } else {
            let sequence = self.browser_disk_operation_sequence;
            self.browser_disk_operation_sequence =
                self.browser_disk_operation_sequence.saturating_add(
                    update
                        .disk_read_operations
                        .max(update.disk_write_operations)
                        .min(4) as u64,
                );
            sequence
        };
        let state = &mut self.app.app_state;
        let info_hash = update.info_hash;
        for _ in 0..update.peers_discovered {
            UiTelemetry::on_manager_event_metrics(
                state,
                &ManagerEvent::PeerDiscovered {
                    info_hash: info_hash.clone(),
                },
            );
        }
        for _ in 0..update.peers_connected {
            UiTelemetry::on_manager_event_metrics(
                state,
                &ManagerEvent::PeerConnected {
                    info_hash: info_hash.clone(),
                },
            );
        }
        for _ in 0..update.peers_disconnected {
            UiTelemetry::on_manager_event_metrics(
                state,
                &ManagerEvent::PeerDisconnected {
                    info_hash: info_hash.clone(),
                },
            );
        }
        if let Some(torrent) = state.torrents.get_mut(&info_hash) {
            torrent.latest_state.blocks_in_this_tick = torrent
                .latest_state
                .blocks_in_this_tick
                .saturating_add(update.blocks_received as u64);
            torrent.latest_state.blocks_out_this_tick = torrent
                .latest_state
                .blocks_out_this_tick
                .saturating_add(update.blocks_sent as u64);
        }

        let piece_index = u32::from(info_hash.first().copied().unwrap_or_default());
        let disk_read_operations = update.disk_read_operations.max(usize::from(
            update.disk_read_bytes > 0 && update.disk_read_operations == 0,
        ));
        let disk_read_bytes = update.disk_read_bytes;
        let represented_disk_reads = disk_read_operations.min(4);
        let represented_disk_read_bytes = disk_read_bytes.min(
            u64::try_from(represented_disk_reads)
                .unwrap_or(u64::MAX)
                .saturating_mul(16_384),
        );
        for operation in 0..represented_disk_reads {
            let operation_count = represented_disk_reads.max(1) as u64;
            let base_length = represented_disk_read_bytes / operation_count;
            let extra_operations = represented_disk_read_bytes % operation_count;
            let op = DiskIoOperation {
                piece_index: piece_index
                    .wrapping_add(disk_operation_sequence as u32)
                    .wrapping_add(operation as u32),
                offset: disk_operation_sequence
                    .saturating_add(operation as u64)
                    .saturating_mul(16_384),
                length: base_length
                    .saturating_add(u64::from((operation as u64) < extra_operations))
                    .max(1) as usize,
            };
            UiTelemetry::on_manager_event_metrics(
                state,
                &ManagerEvent::DiskReadStarted {
                    info_hash: info_hash.clone(),
                    op,
                },
            );
            UiTelemetry::on_manager_event_metrics(state, &ManagerEvent::DiskReadFinished);
        }
        if let Some(torrent) = state.torrents.get_mut(&info_hash) {
            torrent.bytes_read_this_tick = torrent
                .bytes_read_this_tick
                .saturating_add(disk_read_bytes.saturating_sub(represented_disk_read_bytes));
        }
        state.reads_completed_this_tick = state
            .reads_completed_this_tick
            .saturating_add(disk_read_operations.saturating_sub(represented_disk_reads) as u32);
        let disk_write_operations = update.disk_write_operations.max(usize::from(
            update.disk_write_bytes > 0 && update.disk_write_operations == 0,
        ));
        let disk_write_bytes = update.disk_write_bytes;
        let represented_disk_writes = disk_write_operations.min(4);
        let represented_disk_write_bytes = disk_write_bytes.min(
            u64::try_from(represented_disk_writes)
                .unwrap_or(u64::MAX)
                .saturating_mul(16_384),
        );
        for operation in 0..represented_disk_writes {
            let operation_count = represented_disk_writes.max(1) as u64;
            let base_length = represented_disk_write_bytes / operation_count;
            let extra_operations = represented_disk_write_bytes % operation_count;
            let op = DiskIoOperation {
                piece_index: piece_index
                    .wrapping_add(disk_operation_sequence as u32)
                    .wrapping_add(operation as u32),
                offset: disk_operation_sequence
                    .saturating_add(operation as u64)
                    .saturating_mul(16_384),
                length: base_length
                    .saturating_add(u64::from((operation as u64) < extra_operations))
                    .max(1) as usize,
            };
            for event in [
                ManagerEvent::DiskWriteStarted {
                    info_hash: info_hash.clone(),
                    op,
                },
                ManagerEvent::DiskWriteCompleted {
                    info_hash: info_hash.clone(),
                    op,
                },
                ManagerEvent::DiskWriteFinished {
                    info_hash: info_hash.clone(),
                    piece_index: op.piece_index,
                },
            ] {
                UiTelemetry::on_manager_event_metrics(state, &event);
            }
        }
        let unrepresented_disk_write_bytes =
            disk_write_bytes.saturating_sub(represented_disk_write_bytes);
        state.bytes_written_completed_this_tick = state
            .bytes_written_completed_this_tick
            .saturating_add(unrepresented_disk_write_bytes);
        if let Some(torrent) = state.torrents.get_mut(&info_hash) {
            torrent.bytes_written_this_tick = torrent
                .bytes_written_this_tick
                .saturating_add(unrepresented_disk_write_bytes);
        }
        state.writes_completed_this_tick = state
            .writes_completed_this_tick
            .saturating_add(disk_write_operations.saturating_sub(represented_disk_writes) as u32);
        if update.disk_read_operations > 0 {
            state.read_latency_ema = update.disk_read_latency_micros as f64;
            state.avg_disk_read_latency = Duration::from_micros(update.disk_read_latency_micros);
        }
        if update.disk_write_operations > 0 {
            state.write_latency_ema = update.disk_write_latency_micros as f64;
            state.avg_disk_write_latency = Duration::from_micros(update.disk_write_latency_micros);
            state
                .recv_to_write_latency_samples
                .retain(|duration| !duration.is_zero());
            state
                .recv_to_write_latency_samples
                .push_back(Duration::from_micros(update.recv_to_write_latency_micros));
            while state.recv_to_write_latency_samples.len() > 1024 {
                state.recv_to_write_latency_samples.pop_front();
            }
        }
        if update.disk_backoff_ms > 0 {
            UiTelemetry::on_manager_event_metrics(
                state,
                &ManagerEvent::DiskIoBackoff {
                    duration: Duration::from_millis(update.disk_backoff_ms),
                },
            );
        }
    }

    pub fn run_mock_second_tick(
        &mut self,
        cpu_usage: f32,
        ram_usage_percent: f32,
        app_ram_usage: u64,
        run_time: u64,
    ) {
        let previous_torrent_sort = self.app.app_state.torrent_sort;
        let previous_peer_sort = self.app.app_state.peer_sort;
        UiTelemetry::on_second_tick_with_system_snapshot(
            &mut self.app.app_state,
            cpu_usage,
            ram_usage_percent,
            app_ram_usage,
            run_time,
        );
        align_unpinned_sort_with_visible_activity(&mut self.app.app_state);
        refresh_autosort_after_stats(
            &mut self.app.app_state,
            previous_torrent_sort,
            previous_peer_sort,
        );
        NetworkHistoryTelemetry::on_second_tick(&mut self.app.app_state);
        ActivityHistoryTelemetry::on_second_tick(&mut self.app.app_state);
        self.app.app_state.ui.needs_redraw = true;
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

        let state = &mut self.app.app_state;

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

        self.app.client_configs.rss.feeds = rss
            .iter()
            .map(|item| RssFeed {
                url: item.feed_url.clone(),
                enabled: true,
            })
            .collect();
        self.app.client_configs.rss.filters = rss
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
        rss::recompute_rss_derived(state, &self.app.client_configs);

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
        if self
            .app
            .app_state
            .network_history_state
            .tiers
            .second_1s
            .len()
            >= history_len
            && self
                .app
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
            .app
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
        let state = &mut self.app.app_state;

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
        let state = &mut self.app.app_state;
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
            &mut self.app.app_state,
            &self.app.client_configs,
            &self.dht_status,
            &self.dht_wave_telemetry,
            delta_seconds,
        );
        self.fps_sample_elapsed += delta_seconds;
        self.fps_sample_frames = self.fps_sample_frames.saturating_add(1);
        if self.fps_sample_elapsed >= 1.0 {
            let measured = f64::from(self.fps_sample_frames) / self.fps_sample_elapsed;
            let target = self.app.app_state.data_rate.target_fps();
            // requestAnimationFrame commonly reports one frame below its nominal refresh rate
            // because the sampling window straddles a callback boundary. Keep genuine misses
            // visible while avoiding a false 59/60 oscillation in the unchanged production footer.
            self.app.app_state.ui.measured_fps = Some(if measured >= target * 0.98 {
                target
            } else {
                measured
            });
            self.fps_sample_elapsed = 0.0;
            self.fps_sample_frames = 0;
        }
        self.app.app_state.ui.needs_redraw = true;
    }

    pub fn set_torrent_paused_hex(&mut self, info_hash_hex: &str, paused: bool) -> bool {
        let Ok(info_hash) = hex::decode(info_hash_hex) else {
            return false;
        };
        let Some(torrent) = self.app.app_state.torrents.get_mut(&info_hash) else {
            return false;
        };
        torrent.latest_state.torrent_control_state = if paused {
            TorrentControlState::Paused
        } else {
            TorrentControlState::Running
        };
        self.app.app_state.ui.needs_redraw = true;
        true
    }

    pub fn remove_torrent_hex(&mut self, info_hash_hex: &str) -> bool {
        let Ok(info_hash) = hex::decode(info_hash_hex) else {
            return false;
        };
        let removed = self.app.app_state.torrents.remove(&info_hash).is_some();
        self.browser_tracked_peers
            .retain(|(torrent_hash, _), _| torrent_hash != &info_hash);
        if removed {
            let mut peer_view = (*self.app.app_state.peer_manager_view).clone();
            peer_view
                .tracked_peers
                .retain(|peer| peer.torrent_info_hash != info_hash);
            peer_view.registered_torrents = self.app.app_state.torrents.len();
            self.app.app_state.peer_manager_view = Arc::new(peer_view);
            peers::recompute_peer_management_derived(
                &mut self.app.app_state,
                web_time::SystemTime::now(),
            );
        }
        self.app
            .app_state
            .torrent_list_order
            .retain(|candidate| candidate != &info_hash);
        if self.app.app_state.torrent_list_order.is_empty() {
            self.app.app_state.ui.selected_torrent_index = 0;
        } else {
            self.app.app_state.ui.selected_torrent_index = self
                .app
                .app_state
                .ui
                .selected_torrent_index
                .min(self.app.app_state.torrent_list_order.len() - 1);
        }
        self.app.app_state.ui.needs_redraw = true;
        removed
    }

    pub fn torrent_control_state_hex(
        &self,
        info_hash_hex: &str,
    ) -> Option<BrowserTorrentControlState> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        self.app.app_state.torrents.get(&info_hash).map(|torrent| {
            match torrent.latest_state.torrent_control_state {
                TorrentControlState::Running => BrowserTorrentControlState::Running,
                TorrentControlState::Paused => BrowserTorrentControlState::Paused,
                TorrentControlState::Deleting => BrowserTorrentControlState::Deleting,
            }
        })
    }

    pub fn torrent_delete_files_hex(&self, info_hash_hex: &str) -> Option<bool> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        self.app
            .app_state
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
        self.app
            .app_state
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
        self.app.client_configs.default_download_folder.as_ref()
    }

    pub fn set_browser_add_location_prompt(&mut self, enabled: bool) {
        self.app.client_configs.always_show_add_location_prompt = enabled;
    }

    pub fn set_browser_default_download_folder(&mut self, path: PathBuf) {
        self.app.client_configs.default_download_folder = Some(path);
    }

    pub fn apply_next_browser_theme_setting(&mut self) {
        let themes = ThemeName::sorted_for_ui();
        let current = themes
            .iter()
            .position(|theme| *theme == self.app.client_configs.ui_theme)
            .unwrap_or_default();
        let mut settings = self.app.client_configs.clone();
        settings.ui_theme = themes[(current + 1) % themes.len()];
        self.apply_browser_config_update(settings);
    }

    pub fn browser_network_interface_count(&self) -> usize {
        self.app
            .app_state
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
        &self.app.app_state.ui.file_browser.state.current_path
    }

    pub fn file_browser_cursor_path(&self) -> Option<&PathBuf> {
        self.app
            .app_state
            .ui
            .file_browser
            .state
            .cursor_path
            .as_ref()
    }

    pub fn delete_confirmation(&self) -> Option<(&[u8], bool)> {
        matches!(self.app.app_state.mode, AppMode::DeleteConfirm).then_some((
            self.app.app_state.ui.delete_confirm.info_hash.as_slice(),
            self.app.app_state.ui.delete_confirm.with_files,
        ))
    }

    pub fn torrent_count(&self) -> usize {
        self.app.app_state.torrents.len()
    }

    pub fn rss_feed_count(&self) -> usize {
        self.app.client_configs.rss.feeds.len()
    }

    pub fn rss_enabled_feed_count(&self) -> usize {
        self.app
            .client_configs
            .rss
            .feeds
            .iter()
            .filter(|feed| feed.enabled)
            .count()
    }

    pub fn rss_history_count(&self) -> usize {
        self.app.app_state.rss_runtime.history.len()
    }

    pub fn rss_downloaded_preview_count(&self) -> usize {
        self.app
            .app_state
            .rss_runtime
            .preview_items
            .iter()
            .filter(|item| item.is_downloaded)
            .count()
    }

    pub fn rss_last_sync_at(&self) -> Option<&str> {
        self.app.app_state.rss_runtime.last_sync_at.as_deref()
    }

    pub fn system_error(&self) -> Option<&str> {
        self.app.app_state.system_error.as_deref()
    }

    pub fn torrent_sort_column(&self) -> &'static str {
        match self.app.app_state.torrent_sort.0 {
            TorrentSortColumn::Name => "name",
            TorrentSortColumn::Up => "up",
            TorrentSortColumn::Down => "down",
            TorrentSortColumn::Progress => "progress",
        }
    }

    pub fn torrent_sort_pinned(&self) -> bool {
        self.app.app_state.torrent_sort_pinned
    }

    pub fn torrent_sort_direction(&self) -> &'static str {
        match self.app.app_state.torrent_sort.1 {
            SortDirection::Ascending => "ascending",
            SortDirection::Descending => "descending",
        }
    }

    pub fn ordered_torrent_rates(&self) -> Vec<(u64, u64)> {
        self.app
            .app_state
            .torrent_list_order
            .iter()
            .filter_map(|info_hash| self.app.app_state.torrents.get(info_hash))
            .map(|torrent| {
                (
                    torrent.smoothed_download_speed_bps,
                    torrent.smoothed_upload_speed_bps,
                )
            })
            .collect()
    }

    pub fn anonymize_names(&self) -> bool {
        self.app.app_state.anonymize_torrent_names
    }

    pub fn selected_torrent_hash_hex(&self) -> Option<String> {
        self.app
            .app_state
            .torrent_list_order
            .get(self.app.app_state.ui.selected_torrent_index)
            .map(hex::encode)
    }

    pub fn selected_peer_rates(&self) -> Vec<(String, u64, u64)> {
        self.app
            .app_state
            .torrent_list_order
            .get(self.app.app_state.ui.selected_torrent_index)
            .and_then(|info_hash| self.app.app_state.torrents.get(info_hash))
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

    pub fn select_torrent_hex(&mut self, info_hash_hex: &str) -> bool {
        let Ok(info_hash) = hex::decode(info_hash_hex) else {
            return false;
        };
        let Some(index) = self
            .app
            .app_state
            .torrent_list_order
            .iter()
            .position(|candidate| candidate == &info_hash)
        else {
            return false;
        };
        self.app.app_state.ui.selected_torrent_index = index;
        self.app.app_state.ui.needs_redraw = true;
        true
    }

    pub fn torrent_snapshot_hex(&self, info_hash_hex: &str) -> Option<BrowserTorrentSnapshot> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        let torrent = self.app.app_state.torrents.get(&info_hash)?;
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
        self.app
            .app_state
            .torrents
            .get(&info_hash)?
            .latest_state
            .download_path
            .as_ref()
    }

    pub fn selected_torrent_snapshot(&self) -> Option<BrowserTorrentSnapshot> {
        self.selected_torrent_hash_hex()
            .and_then(|info_hash| self.torrent_snapshot_hex(&info_hash))
    }

    pub fn visualization_snapshot(&self) -> BrowserVisualizationSnapshot {
        let selected = self
            .app
            .app_state
            .torrent_list_order
            .get(self.app.app_state.ui.selected_torrent_index)
            .and_then(|info_hash| self.app.app_state.torrents.get(info_hash));
        BrowserVisualizationSnapshot {
            total_download_bps: self
                .app
                .app_state
                .torrents
                .values()
                .map(|torrent| torrent.latest_state.download_speed_bps)
                .sum(),
            total_upload_bps: self
                .app
                .app_state
                .torrents
                .values()
                .map(|torrent| torrent.latest_state.upload_speed_bps)
                .sum(),
            disk_read_bps: self.app.app_state.avg_disk_read_bps,
            disk_write_bps: self.app.app_state.avg_disk_write_bps,
            effects_phase_time: self.app.app_state.ui.effects_phase_time,
            file_download_phase: self.app.app_state.ui.file_activity_download_phase,
            file_upload_phase: self.app.app_state.ui.file_activity_upload_phase,
            disk_health_phase: self.app.app_state.disk_health_phase,
            disk_health_state_level: self.app.app_state.disk_health_state_level,
            tracked_peers: self.app.app_state.peer_manager_view.tracked_peers.len(),
            network_history_samples: self
                .app
                .app_state
                .network_history_state
                .tiers
                .second_1s
                .len(),
            activity_history_samples: self
                .app
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
            read_iops: self.app.app_state.read_iops,
            write_iops: self.app.app_state.write_iops,
            disk_read_latency_micros: self.app.app_state.avg_disk_read_latency.as_micros() as u64,
            disk_write_latency_micros: self.app.app_state.avg_disk_write_latency.as_micros() as u64,
            recv_to_write_latency_micros: self.app.app_state.recv_to_write_p95.as_micros() as u64,
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
            dht_wave_initialized: self.app.app_state.ui.dht_wave.initialized,
            dht_active_queries: self.dht_wave_telemetry.inflight_ipv4_queries
                + self.dht_wave_telemetry.inflight_ipv6_queries,
            dht_peers_found: self.dht_wave_telemetry.unique_peers_found_last_10s,
            dht_query_load: self.app.app_state.ui.dht_wave.query_load,
        }
    }

    pub fn torrent_management_cursor_hash_hex(&self) -> Option<String> {
        self.app
            .app_state
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
