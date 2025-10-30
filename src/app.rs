// SPDX-License-Identifier: GPL-3.0-or-later

use crate::tui;

use std::fs;
use std::io::Stdout;

use std::collections::VecDeque;

use magnet_url::Magnet;

use crate::torrent_manager::DiskIoOperation;

use crate::config::{PeerSortColumn, Settings, SortDirection, TorrentSettings, TorrentSortColumn};
use crate::token_bucket::TokenBucket;

use crate::tui_events;

use crate::config::get_watch_path;

use crate::resource_manager::ResourceType;

use crate::torrent_file::parser::from_bytes;
use crate::torrent_manager::ManagerCommand;
use crate::torrent_manager::ManagerEvent;
use crate::torrent_manager::TorrentManager;
use crate::torrent_manager::TorrentParameters;

use crate::config::get_app_paths;
use crate::config::save_settings;

use std::collections::HashMap;
use tokio::io::AsyncReadExt;
use tokio::signal;
use tokio::sync::broadcast;
use tokio::sync::mpsc::Sender;
use tokio::sync::Mutex;

use std::sync::Arc;
use std::time::Instant;

#[cfg(feature = "dht")]
use mainline::{async_dht::AsyncDht, Dht};
#[cfg(not(feature = "dht"))]
type AsyncDht = ();

use sha1::Digest;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use std::time::Duration;

use notify::{Config, Error as NotifyError, Event, RecommendedWatcher, RecursiveMode, Watcher};

use ratatui::{backend::CrosstermBackend, Terminal};

use ratatui_explorer::FileExplorer;

use sysinfo::System;

use data_encoding::BASE32;

use tracing::{event as tracing_event, Level};

use crate::resource_manager::{ResourceManager, ResourceManagerClient};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use tokio::time;

use ratatui::crossterm::event::{self, Event as CrosstermEvent};

use rand::seq::SliceRandom;
use rand::Rng;

#[cfg(unix)]
use rlimit::Resource;

const SECONDS_HISTORY_MAX: usize = 3600; // 1 hour of per-second data
const MINUTES_HISTORY_MAX: usize = 48 * 60; // 48 hours of per-minute data

const FILE_HANDLE_MINIMUM: usize = 64;
const SAFE_BUDGET_PERCENTAGE: f64 = 0.85;

#[derive(Default, Clone)]
pub struct CalculatedLimits {
    pub reserve_permits: usize,
    pub max_connected_peers: usize,
    pub disk_read_permits: usize,
    pub disk_write_permits: usize,
}
impl CalculatedLimits {
    pub fn into_map(self) -> HashMap<ResourceType, usize> {
        let mut map = HashMap::new();
        map.insert(ResourceType::Reserve, self.reserve_permits);
        map.insert(ResourceType::PeerConnection, self.max_connected_peers);
        map.insert(ResourceType::DiskRead, self.disk_read_permits);
        map.insert(ResourceType::DiskWrite, self.disk_write_permits);
        map
    }
}

#[derive(Default, Clone, Copy, PartialEq, Debug)]
pub enum GraphDisplayMode {
    OneMinute,
    FiveMinutes,
    #[default]
    TenMinutes,
    ThirtyMinutes,
    OneHour,
    ThreeHours,
    TwelveHours,
    TwentyFourHours,
}

impl GraphDisplayMode {
    pub fn as_seconds(&self) -> usize {
        match self {
            Self::OneMinute => 60,
            Self::FiveMinutes => 300,
            Self::TenMinutes => 600,
            Self::ThirtyMinutes => 1800,
            Self::OneHour => 3600,
            Self::ThreeHours => 3 * 3600,
            Self::TwelveHours => 12 * 3600,
            Self::TwentyFourHours => 86_400,
        }
    }

    pub fn to_string(self) -> &'static str {
        match self {
            Self::OneMinute => "1m",
            Self::FiveMinutes => "5m",
            Self::TenMinutes => "10m",
            Self::ThirtyMinutes => "30m",
            Self::OneHour => "1h",
            Self::ThreeHours => "3h",
            Self::TwelveHours => "12h",
            Self::TwentyFourHours => "24h",
        }
    }

    pub fn next(&self) -> Self {
        match self {
            Self::OneMinute => Self::FiveMinutes,
            Self::FiveMinutes => Self::TenMinutes,
            Self::TenMinutes => Self::ThirtyMinutes,
            Self::ThirtyMinutes => Self::OneHour,
            Self::OneHour => Self::ThreeHours,
            Self::ThreeHours => Self::TwelveHours, // New cycle step
            Self::TwelveHours => Self::TwentyFourHours, // New cycle step
            Self::TwentyFourHours => Self::OneMinute, // Cycle back from 24h
        }
    }

    pub fn prev(&self) -> Self {
        match self {
            Self::OneMinute => Self::TwentyFourHours, // Cycle back to 24h
            Self::FiveMinutes => Self::OneMinute,
            Self::TenMinutes => Self::FiveMinutes,
            Self::ThirtyMinutes => Self::TenMinutes,
            Self::OneHour => Self::ThirtyMinutes,
            Self::ThreeHours => Self::OneHour, // New cycle step
            Self::TwelveHours => Self::ThreeHours, // New cycle step
            Self::TwentyFourHours => Self::TwelveHours, // Updated cycle
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SelectedHeader {
    Torrent(usize), // index within torrent headers
    Peer(usize),    // index within peer headers
}
impl Default for SelectedHeader {
    fn default() -> Self {
        // Default to selecting the first header (index 0) of the Torrent table.
        SelectedHeader::Torrent(0)
    }
}

pub const TORRENT_HEADERS: &[TorrentSortColumn] = &[
    TorrentSortColumn::Name,
    TorrentSortColumn::Down,
    TorrentSortColumn::Up,
];

pub enum AppCommand {
    AddTorrentFromFile(PathBuf),
    AddTorrentFromPathFile(PathBuf),
    AddMagnetFromFile(PathBuf),
    ClientShutdown(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ConfigItem {
    ClientPort,
    DefaultDownloadFolder,
    WatchFolder,
    GlobalDownloadLimit,
    GlobalUploadLimit,
}

#[derive(Default)]
pub enum AppMode {
    Welcome,
    #[default]
    Normal,
    PowerSaving,
    FilePicker(FileExplorer),
    DeleteConfirm {
        info_hash: Vec<u8>,
        with_files: bool,
    },
    Config {
        settings_edit: Box<Settings>,
        selected_index: usize,
        items: Vec<ConfigItem>,
        editing: Option<(ConfigItem, String)>,
    },
    ConfigPathPicker {
        settings_edit: Box<Settings>,
        for_item: ConfigItem,
        file_explorer: FileExplorer,
    },
}

#[derive(Default, Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum TorrentControlState {
    #[default]
    Running,
    Paused,
    Deleting,
}

pub const PEER_HEADERS: &[PeerSortColumn] = &[
    PeerSortColumn::Flags,
    PeerSortColumn::Address,
    PeerSortColumn::Client,
    PeerSortColumn::Action,
    PeerSortColumn::Completed,
    PeerSortColumn::DL,
    PeerSortColumn::UL,
    PeerSortColumn::TotalDL,
    PeerSortColumn::TotalUL,
];
#[derive(Debug, Clone, Default)]
pub struct PeerInfo {
    pub address: String,
    pub peer_id: Vec<u8>,
    pub am_choking: bool,
    pub peer_choking: bool,
    pub am_interested: bool,
    pub peer_interested: bool,
    pub bitfield: Vec<bool>,
    pub download_speed_bps: u64,
    pub upload_speed_bps: u64,
    pub total_downloaded: u64,
    pub total_uploaded: u64,
    pub last_action: String,
}

#[derive(Debug, Default, Clone)]
pub struct TorrentState {
    pub torrent_control_state: TorrentControlState,
    pub info_hash: Vec<u8>,
    pub torrent_or_magnet: String,
    pub torrent_name: String,
    pub download_path: PathBuf,
    pub number_of_successfully_connected_peers: usize,
    pub number_of_pieces_total: u32,
    pub number_of_pieces_completed: u32,
    pub download_speed_bps: u64,
    pub upload_speed_bps: u64,
    pub bytes_downloaded_this_tick: u64,
    pub bytes_uploaded_this_tick: u64,
    pub eta: Duration,
    pub peers: Vec<PeerInfo>,
    pub activity_message: String,
    pub next_announce_in: Duration,
}

#[derive(Default, Debug)]
pub struct TorrentDisplayState {
    pub latest_state: TorrentState,
    pub download_history: Vec<u64>,
    pub upload_history: Vec<u64>,

    pub bytes_read_this_tick: u64,
    pub bytes_written_this_tick: u64,
    pub disk_read_speed_bps: u64,
    pub disk_write_speed_bps: u64,
    pub disk_read_history_log: VecDeque<DiskIoOperation>,
    pub disk_write_history_log: VecDeque<DiskIoOperation>,
    pub disk_read_thrash_score: u64,
    pub disk_write_thrash_score: u64,

    pub smoothed_download_speed_bps: u64,
    pub smoothed_upload_speed_bps: u64,
}

#[derive(Default)]
pub struct AppState {
    pub should_quit: bool,
    pub shutdown_progress: f64,
    pub system_warning: Option<String>,
    pub system_error: Option<String>,
    pub limits: CalculatedLimits,

    pub mode: AppMode,
    pub show_help: bool,
    pub externally_accessable_port: bool,
    pub anonymize_torrent_names: bool,

    pub pending_torrent_link: String,
    pub torrents: HashMap<Vec<u8>, TorrentDisplayState>,

    pub torrent_list_order: Vec<Vec<u8>>,

    pub total_download_history: Vec<u64>,
    pub total_upload_history: Vec<u64>,
    pub avg_download_history: Vec<u64>,
    pub avg_upload_history: Vec<u64>,
    pub disk_backoff_history_ms: VecDeque<u64>,
    pub minute_disk_backoff_history_ms: VecDeque<u64>,
    pub max_disk_backoff_this_tick_ms: u64,

    pub lifetime_downloaded_from_config: u64,
    pub lifetime_uploaded_from_config: u64,

    pub session_total_downloaded: u64,
    pub session_total_uploaded: u64,

    pub cpu_usage: f32,
    pub ram_usage_percent: f32,
    pub avg_disk_read_bps: u64,
    pub avg_disk_write_bps: u64,

    pub disk_read_history: Vec<u64>,
    pub disk_write_history: Vec<u64>,
    pub app_ram_usage: u64,

    pub run_time: u64,

    pub global_disk_read_history_log: VecDeque<DiskIoOperation>,
    pub global_disk_write_history_log: VecDeque<DiskIoOperation>,
    pub global_disk_read_thrash_score: u64,
    pub global_disk_write_thrash_score: u64,

    pub read_op_start_times: VecDeque<Instant>,
    pub write_op_start_times: VecDeque<Instant>,
    pub read_latency_ema: f64,
    pub write_latency_ema: f64,
    pub avg_disk_read_latency: Duration,
    pub avg_disk_write_latency: Duration,
    pub reads_completed_this_tick: u32,
    pub writes_completed_this_tick: u32,
    pub read_iops: u32,
    pub write_iops: u32,

    pub ui_needs_redraw: bool,

    pub selected_header: SelectedHeader,
    pub torrent_sort: (TorrentSortColumn, SortDirection),
    pub peer_sort: (PeerSortColumn, SortDirection),
    pub selected_torrent_index: usize,

    pub graph_mode: GraphDisplayMode,
    pub minute_avg_dl_history: Vec<u64>,
    pub minute_avg_ul_history: Vec<u64>,

    pub last_tuning_score: u64,
    pub current_tuning_score: u64,
    pub tuning_countdown: u64,
    pub last_tuning_limits: CalculatedLimits,
    pub is_seeding: bool,
    pub baseline_speed_ema: f64,
    pub global_disk_thrash_score: f64,
    pub adaptive_max_scpb: f64,
    pub global_seek_cost_per_byte_history: Vec<f64>,

    pub recently_processed_files: HashMap<PathBuf, Instant>,
}

pub struct App {
    // State that changes and is drawn to the screen
    pub app_state: AppState,
    pub client_configs: Settings,

    // Static or shared resources
    pub torrent_manager_incoming_peer_txs: HashMap<Vec<u8>, Sender<(TcpStream, Vec<u8>)>>,
    pub torrent_manager_command_txs: HashMap<Vec<u8>, Sender<ManagerCommand>>,
    pub distributed_hash_table: AsyncDht,
    pub resource_manager: ResourceManagerClient,
    pub global_dl_bucket: Arc<Mutex<TokenBucket>>,
    pub global_ul_bucket: Arc<Mutex<TokenBucket>>,

    // Communication Channels
    pub torrent_tx: mpsc::Sender<TorrentState>,
    pub torrent_rx: mpsc::Receiver<TorrentState>,
    pub manager_event_tx: mpsc::Sender<ManagerEvent>,
    pub manager_event_rx: mpsc::Receiver<ManagerEvent>,
    pub app_command_tx: mpsc::Sender<AppCommand>,
    pub app_command_rx: mpsc::Receiver<AppCommand>,
    pub tui_event_tx: mpsc::Sender<CrosstermEvent>,
    pub tui_event_rx: mpsc::Receiver<CrosstermEvent>,
    pub shutdown_tx: broadcast::Sender<()>,
}
impl App {
    pub async fn new(client_configs: Settings) -> Result<Self, Box<dyn std::error::Error>> {
        let (manager_event_tx, manager_event_rx) = mpsc::channel::<ManagerEvent>(100);
        let (app_command_tx, app_command_rx) = mpsc::channel::<AppCommand>(10);
        let (tui_event_tx, tui_event_rx) = mpsc::channel::<CrosstermEvent>(100);
        let (torrent_tx, torrent_rx) = mpsc::channel::<TorrentState>(100);
        let (shutdown_tx, _) = broadcast::channel(1);

        let (limits, system_warning) = calculate_adaptive_limits(&client_configs);
        tracing_event!(
            Level::DEBUG,
            "Adaptive limits calculated: max_peers={}, disk_reads={}, disk_writes={}",
            limits.max_connected_peers,
            limits.disk_read_permits,
            limits.disk_write_permits
        );
        let mut rm_limits = HashMap::new();
        rm_limits.insert(ResourceType::Reserve, (limits.reserve_permits, 0));
        rm_limits.insert(
            ResourceType::PeerConnection,
            (limits.max_connected_peers, 0),
        );
        // For disk I/O, we can allow a small queue to buffer requests.
        rm_limits.insert(
            ResourceType::DiskRead,
            (limits.disk_read_permits, limits.disk_read_permits * 2),
        );
        rm_limits.insert(
            ResourceType::DiskWrite,
            (limits.disk_write_permits, limits.disk_read_permits * 2),
        );
        let (resource_manager, resource_manager_client) =
            ResourceManager::new(rm_limits, shutdown_tx.clone());
        tokio::spawn(resource_manager.run());

        #[cfg(feature = "dht")]
        let bootstrap_nodes: Vec<&str> = client_configs
            .bootstrap_nodes
            .iter()
            .map(AsRef::as_ref)
            .collect();

        #[cfg(feature = "dht")]
        let distributed_hash_table = Dht::builder()
            .bootstrap(&bootstrap_nodes)
            .port(client_configs.client_port)
            .server_mode()
            .build()?
            .as_async();

        #[cfg(not(feature = "dht"))]
        let distributed_hash_table = ();

        let dl_limit = client_configs.global_download_limit_bps as f64;
        let ul_limit = client_configs.global_upload_limit_bps as f64;
        let global_dl_bucket = Arc::new(Mutex::new(TokenBucket::new(dl_limit, dl_limit)));
        let global_ul_bucket = Arc::new(Mutex::new(TokenBucket::new(ul_limit, ul_limit)));

        let app_state = AppState {
            system_warning,
            system_error: None,
            limits: limits.clone(),
            ui_needs_redraw: true,
            torrent_sort: (
                client_configs.torrent_sort_column,
                client_configs.torrent_sort_direction,
            ),
            peer_sort: (
                client_configs.peer_sort_column,
                client_configs.peer_sort_direction,
            ),
            lifetime_downloaded_from_config: client_configs.lifetime_downloaded,
            lifetime_uploaded_from_config: client_configs.lifetime_uploaded,
            minute_disk_backoff_history_ms: VecDeque::with_capacity(24 * 60),
            max_disk_backoff_this_tick_ms: 0,
            last_tuning_score: 0,
            current_tuning_score: 0,
            tuning_countdown: 90,
            last_tuning_limits: limits.clone(),
            adaptive_max_scpb: 10.0,
            ..Default::default()
        };

        let mut app = Self {
            app_state,
            client_configs: client_configs.clone(),
            torrent_manager_incoming_peer_txs: HashMap::new(),
            torrent_manager_command_txs: HashMap::new(),
            distributed_hash_table,
            resource_manager: resource_manager_client,
            global_dl_bucket,
            global_ul_bucket,
            torrent_tx,
            torrent_rx,
            manager_event_tx,
            manager_event_rx,
            app_command_tx,
            app_command_rx,
            tui_event_tx,
            tui_event_rx,
            shutdown_tx,
        };

        let mut torrents_to_load = app.client_configs.torrents.clone();
        torrents_to_load.sort_by_key(|t| !t.validation_status);
        for torrent_config in torrents_to_load {
            if torrent_config.torrent_or_magnet.starts_with("magnet:") {
                app.add_magnet_torrent(
                    torrent_config.name.clone(),
                    torrent_config.torrent_or_magnet.clone(),
                    torrent_config.download_path.clone(),
                    torrent_config.validation_status,
                    torrent_config.torrent_control_state,
                )
                .await;
            } else {
                app.add_torrent_from_file(
                    PathBuf::from(&torrent_config.torrent_or_magnet),
                    torrent_config.download_path.clone(),
                    torrent_config.validation_status,
                    torrent_config.torrent_control_state,
                )
                .await;
            }
        }

        if app.app_state.torrents.is_empty() {
            app.app_state.mode = AppMode::Welcome;
        }

        Ok(app)
    }

    pub async fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.process_pending_commands().await;

        // --- Setup network listener ---
        let listener =
            tokio::net::TcpListener::bind(format!("0.0.0.0:{}", self.client_configs.client_port))
                .await?;

        // --- Spawn TUI event handler task ---
        let tui_event_tx_clone = self.tui_event_tx.clone();
        let mut tui_shutdown_rx = self.shutdown_tx.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tui_shutdown_rx.recv() => break,

                    result = tokio::task::spawn_blocking(event::read) => {
                        let event = match result {
                            Ok(Ok(e)) => e,
                            Ok(Err(e)) => {
                                tracing_event!(Level::ERROR, "Crossterm event read error: {}", e);
                                break;
                            }
                            Err(e) => {
                                tracing_event!(Level::ERROR, "Blocking TUI read task panicked: {}", e);
                                break;
                            }
                        };

                        if tui_event_tx_clone.send(event).await.is_err() {
                            break;
                        }
                    }

                }
            }
        });

        let (notify_tx, mut notify_rx) = mpsc::channel::<Result<Event, NotifyError>>(100);
        let mut watcher = RecommendedWatcher::new(
            move |res: Result<Event, NotifyError>| {
                if let Err(e) = notify_tx.blocking_send(res) {
                    tracing_event!(
                        Level::ERROR,
                        "Failed to send file event to main loop: {}",
                        e
                    );
                }
            },
            Config::default(),
        )?;
        if let Some(path) = &self.client_configs.watch_folder {
            if let Err(e) = watcher.watch(path, RecursiveMode::NonRecursive) {
                tracing_event!(Level::ERROR, "Failed to watch user path {:?}: {}", path, e);
            } else {
                tracing_event!(Level::INFO, "Watching user path: {:?}", path);
            }
        }
        if let Some((watch_path, _)) = get_watch_path() {
            if let Err(e) = watcher.watch(&watch_path, RecursiveMode::NonRecursive) {
                tracing_event!(
                    Level::ERROR,
                    "Failed to watch system path {:?}: {}",
                    watch_path,
                    e
                );
            } else {
                tracing_event!(Level::INFO, "Watching system path: {:?}", watch_path);
            }
        }

        // --- System Stats Setup ---
        let mut stats_interval = time::interval(Duration::from_secs(1));
        let mut sys = System::new();

        // Self tuning torrent limits
        let mut tuning_interval = time::interval(Duration::from_secs(90));

        // Main application loop
        let mut draw_interval = time::interval(Duration::from_millis(17));
        while !self.app_state.should_quit {
            tokio::select! {
                _ = signal::ctrl_c() => {
                    self.app_state.should_quit = true;
                }
                Ok(Ok((mut stream, _addr))) = tokio::time::timeout(Duration::from_secs(1), listener.accept()) => {
                    if !self.app_state.externally_accessable_port {
                        self.app_state.externally_accessable_port = true;
                    }

                    let torrent_manager_incoming_peer_txs_clone = self.torrent_manager_incoming_peer_txs.clone();
                    let resource_manager_clone = self.resource_manager.clone();
                    let mut permit_shutdown_rx = self.shutdown_tx.subscribe();
                    tokio::spawn(async move {
                        let _session_permit = tokio::select! {
                            permit_result = resource_manager_clone.acquire_peer_connection() => {
                                match permit_result {
                                    Ok(permit) => Some(permit),
                                    Err(_) => {
                                        tracing_event!(Level::DEBUG, "Failed to acquire permit. Manager shut down?");
                                        None
                                    }
                                }
                            }
                            _ = permit_shutdown_rx.recv() => {
                                None
                            }
                        };
                        let mut buffer = vec![0u8; 68];
                        if (stream.read_exact(&mut buffer).await).is_ok() {
                            let peer_info_hash = &buffer[28..48];
                            if let Some(torrent_manager_tx) = torrent_manager_incoming_peer_txs_clone.get(peer_info_hash) {
                                let torrent_manager_tx_clone = torrent_manager_tx.clone();
                                let _ = torrent_manager_tx_clone.send((stream, buffer)).await;
                            }
                        }
                    });
                }
                Some(event) = self.manager_event_rx.recv() => {
                    match event {
                        ManagerEvent::DeletionComplete(info_hash, result) => {
                            if let Err(e) = result {
                                tracing_event!(Level::ERROR, "Deletion failed for torrent: {}", e);
                            }
                            // Now we can safely clean up the UI state
                            self.app_state.torrents.remove(&info_hash);
                            self.torrent_manager_command_txs.remove(&info_hash);
                            self.torrent_manager_incoming_peer_txs.remove(&info_hash);
                            self.app_state.torrent_list_order.retain(|ih| *ih != info_hash);

                            if self.app_state.selected_torrent_index >= self.app_state.torrent_list_order.len() && !self.app_state.torrent_list_order.is_empty() {
                                self.app_state.selected_torrent_index = self.app_state.torrent_list_order.len() - 1;
                            }

                            self.app_state.ui_needs_redraw = true;
                        }
                       ManagerEvent::DiskReadStarted { info_hash, op } => {
                            self.app_state.read_op_start_times.push_front(Instant::now());
                            self.app_state.global_disk_read_history_log.push_front(op);
                            self.app_state.global_disk_read_history_log.truncate(100);
                            if let Some(torrent) = self.app_state.torrents.get_mut(&info_hash) {
                                torrent.bytes_read_this_tick += op.length as u64; // Keep this one
                                torrent.disk_read_history_log.push_front(op);
                                torrent.disk_read_history_log.truncate(50);
                            }
                        }
                        ManagerEvent::DiskReadFinished => {
                            if let Some(start_time) = self.app_state.read_op_start_times.pop_front() {
                                let duration = start_time.elapsed();
                                const LATENCY_EMA_PERIOD: f64 = 10.0; // Smooth over the last 10 operations
                                let alpha = 2.0 / (LATENCY_EMA_PERIOD + 1.0);
                                let current_micros = duration.as_micros() as f64;

                                let new_ema = if self.app_state.read_latency_ema == 0.0 {
                                    // Seed the EMA with the first value to avoid it starting too low.
                                    current_micros
                                } else {
                                    (current_micros * alpha) + (self.app_state.read_latency_ema * (1.0 - alpha))
                                };

                                self.app_state.read_latency_ema = new_ema;
                                self.app_state.avg_disk_read_latency = Duration::from_micros(new_ema as u64);
                            }
                            self.app_state.reads_completed_this_tick += 1;
                        }
                        ManagerEvent::DiskWriteStarted { info_hash, op } => {
                            self.app_state.write_op_start_times.push_front(Instant::now());
                            self.app_state.global_disk_write_history_log.push_front(op);
                            self.app_state.global_disk_write_history_log.truncate(100);
                            if let Some(torrent) = self.app_state.torrents.get_mut(&info_hash) {
                                torrent.bytes_written_this_tick += op.length as u64; // Keep this one
                                torrent.disk_write_history_log.push_front(op);
                                torrent.disk_write_history_log.truncate(50);
                            }
                        }
                        ManagerEvent::DiskWriteFinished => {
                            if let Some(start_time) = self.app_state.write_op_start_times.pop_front() {
                                let duration = start_time.elapsed();
                                const LATENCY_EMA_PERIOD: f64 = 10.0;
                                let alpha = 2.0 / (LATENCY_EMA_PERIOD + 1.0);
                                let current_micros = duration.as_micros() as f64;

                                let new_ema = if self.app_state.write_latency_ema == 0.0 {
                                    current_micros
                                } else {
                                    (current_micros * alpha) + (self.app_state.write_latency_ema * (1.0 - alpha))
                                };

                                self.app_state.write_latency_ema = new_ema;
                                self.app_state.avg_disk_write_latency = Duration::from_micros(new_ema as u64);
                            }
                            self.app_state.writes_completed_this_tick += 1;
                        }
                        ManagerEvent::DiskIoBackoff { duration } => {
                            let duration_ms = duration.as_millis() as u64;
                            self.app_state.max_disk_backoff_this_tick_ms =
                                self.app_state.max_disk_backoff_this_tick_ms.max(duration_ms);

                            if self.app_state.system_warning.is_none() {
                                let warning_msg = "System Warning: Potential FD limit hit (detected via Disk I/O backoff). Increase 'ulimit -n' if issues persist.".to_string();
                                self.app_state.system_warning = Some(warning_msg);
                            }
                        }
                    }
                }

                Some(message) = self.torrent_rx.recv() => {


                    self.app_state.session_total_downloaded += message.bytes_downloaded_this_tick;
                    self.app_state.session_total_uploaded += message.bytes_uploaded_this_tick;

                    let display_state = self.app_state.torrents.entry(message.info_hash).or_default();

                    display_state.latest_state.number_of_successfully_connected_peers = message.number_of_successfully_connected_peers;
                    display_state.latest_state.number_of_pieces_total = message.number_of_pieces_total;
                    display_state.latest_state.number_of_pieces_completed = message.number_of_pieces_completed;
                    display_state.latest_state.download_speed_bps = message.download_speed_bps;
                    display_state.latest_state.upload_speed_bps = message.upload_speed_bps;
                    display_state.latest_state.eta = message.eta;
                    display_state.latest_state.next_announce_in = message.next_announce_in;

                    // Also update the name if the manager discovered it from metadata
                    if !message.torrent_name.is_empty() {
                        display_state.latest_state.torrent_name = message.torrent_name;
                    }

                    // Update the individual history for the details pane charts
                    display_state.download_history.push(display_state.latest_state.download_speed_bps);
                    display_state.upload_history.push(display_state.latest_state.upload_speed_bps);

                    // Keep the individual history capped
                    if display_state.download_history.len() > 200 {
                        display_state.download_history.remove(0);
                        display_state.upload_history.remove(0);
                    }

                    // Keep the total history capped
                    if self.app_state.total_download_history.len() > 200 {
                        self.app_state.total_download_history.remove(0);
                        self.app_state.total_upload_history.remove(0);
                    }

                    display_state.smoothed_download_speed_bps = display_state.latest_state.download_speed_bps;
                    display_state.smoothed_upload_speed_bps = display_state.latest_state.upload_speed_bps;
                    display_state.latest_state.peers = message.peers;

                    display_state.latest_state.activity_message = message.activity_message;

                    self.sort_torrent_list();
                    self.app_state.ui_needs_redraw = true;
                }

                Some(command) = self.app_command_rx.recv() => {
                    match command {
                        AppCommand::AddTorrentFromFile(path) => {
                            // All state mutation happens here, in the main task.
                            if let Some(download_path) = &self.client_configs.default_download_folder {

                                self.add_torrent_from_file(
                                    path.to_path_buf(),
                                    download_path.to_path_buf(),
                                    false,
                                    TorrentControlState::Running
                                ).await;

                                // Move or rename file for it not to reprocess.
                                let move_successful = if let Some(watch_folder) = &self.client_configs.watch_folder {
                                    (|| {
                                        let parent_dir = watch_folder.parent()?;
                                        let processed_folder = parent_dir.join("processed_torrents");
                                        fs::create_dir_all(&processed_folder).ok()?;

                                        let file_name = path.file_name()?;
                                        let new_path = processed_folder.join(file_name);
                                        fs::rename(&path, &new_path).ok()?;

                                        Some(()) // Return Some(()) to indicate success
                                    })().is_some()
                                } else {
                                    false // Watch folder is not set, so we can't move.
                                };

                                // If the move operation failed for any reason, fall back to renaming.
                                if !move_successful {
                                    tracing_event!(Level::WARN, "Could not move torrent file. Defaulting to renaming in place.");
                                    let mut new_path = path.clone();
                                    new_path.set_extension("torrent.added");
                                    if let Err(e) = fs::rename(&path, &new_path) {
                                        tracing_event!(Level::ERROR, "Fallback rename failed for {:?}: {}", path, e);
                                    }
                                }

                            } else {
                                tracing_event!(Level::ERROR, "Watch folder cannot add torrent: default download folder is not set.");
                                self.app_state.system_error = Some("Failed to add torrent: Default download folder is not set. Press [c] to configure.".to_string());
                            }
                        }
                        AppCommand::AddTorrentFromPathFile(path) => {
                            if let Some((_, processed_path)) = get_watch_path() {
                                match fs::read_to_string(&path) {
                                    Ok(torrent_file_path_str) => {
                                        let torrent_file_path = PathBuf::from(torrent_file_path_str.trim());
                                        if let Some(download_path) = self.client_configs.default_download_folder.clone() {
                                            self.add_torrent_from_file(torrent_file_path, download_path, false, TorrentControlState::Running).await;
                                        } else {
                                            tracing_event!(Level::ERROR, "Cannot add torrent from path file: default download folder not set.");
                                        }
                                    }
                                    Err(e) => {
                                        tracing_event!(Level::ERROR, "Failed to read torrent path from file {:?}: {}", &path, e);
                                    }
                                }

                                // Move the .path file to the processed directory to prevent re-processing
                                if let Some(file_name) = path.file_name() {
                                    let new_path = processed_path.join(file_name);
                                    if let Err(e) = fs::rename(&path, &new_path) {
                                        tracing_event!(Level::WARN, "Failed to move processed path file {:?}: {}", &path, e);
                                    }
                                }
                            }
                        }
                        AppCommand::AddMagnetFromFile(path) => {
                            // This now uses the consolidated processed_path
                            if let Some((_, processed_path)) = get_watch_path() {
                                match fs::read_to_string(&path) {
                                    Ok(magnet_link) => {
                                        if let Some(download_path) = self.client_configs.default_download_folder.clone() {
                                            self.add_magnet_torrent("Fetching name...".to_string(), magnet_link.trim().to_string(), download_path, false, TorrentControlState::Running).await;
                                        } else {
                                            tracing_event!(Level::ERROR, "Watch folder cannot add magnet: default download folder is not set.");
                                            self.app_state.system_error = Some("Failed to add torrent: Default download folder not set. Press [c] to configure.".to_string());
                                        }
                                    }
                                    Err(e) => {
                                        tracing_event!(Level::ERROR, "Failed to read magnet file {:?}: {}", &path, e);
                                    }
                                }

                                if let Err(e) = fs::create_dir_all(&processed_path) {
                                    tracing_event!(Level::ERROR, "Could not create processed files directory: {}", e);
                                } else if let Some(file_name) = path.file_name() {
                                    let new_path = processed_path.join(file_name);
                                    if let Err(e) = fs::rename(&path, &new_path) {
                                        tracing_event!(Level::ERROR, "Failed to move processed magnet file {:?}: {}", &path, e);
                                    }
                                }
                            } else {
                                tracing_event!(Level::ERROR, "Could not get system watch paths for magnet processing.");
                            }
                        }
                        AppCommand::ClientShutdown(path) => {
                            tracing_event!(Level::INFO, "Shutdown command received via command file.");
                            self.app_state.should_quit = true;
                            if let Err(e) = fs::remove_file(&path) {
                                tracing_event!(Level::WARN, "Failed to remove command file {:?}: {}", &path, e);
                            }
                        }
                    }
                },

                Some(event) = self.tui_event_rx.recv() => {
                    tui_events::handle_event(event, self).await;
                }

                Some(result) = notify_rx.recv() => {
                    match result {
                        Ok(event) => {

                            if event.kind.is_create() || event.kind.is_modify() {
                                const DEBOUNCE_DURATION: Duration = Duration::from_millis(500);
                                for path in &event.paths {
                                    if path.to_string_lossy().ends_with(".tmp") {
                                        tracing_event!(Level::DEBUG, "Skipping temporary file: {:?}", path);
                                        continue;
                                    }
                                    let now = Instant::now();
                                    if let Some(last_time) = self.app_state.recently_processed_files.get(path) { 
                                        if now.duration_since(*last_time) < DEBOUNCE_DURATION {
                                            tracing_event!(Level::DEBUG, "Skipping file {:?} due to debounce. (Accessing via app_state)", path);
                                            continue; 
                                        }
                                    }

                                    self.app_state.recently_processed_files.insert(path.clone(), now);
                                    tracing_event!(Level::INFO, "Processing file event: {:?} for path: {:?}", event.kind, path);

                                    if path.extension().is_some_and(|ext| ext == "torrent") {
                                        let _ = self.app_command_tx
                                            .send(AppCommand::AddTorrentFromFile(path.clone()))
                                            .await;
                                    }
                                    if path.extension().is_some_and(|ext| ext == "path") {
                                        let _ = self.app_command_tx
                                            .send(AppCommand::AddTorrentFromPathFile(path.clone()))
                                            .await;
                                    }
                                    if path.extension().is_some_and(|ext| ext == "magnet") {
                                        let _ = self.app_command_tx
                                            .send(AppCommand::AddMagnetFromFile(path.clone()))
                                            .await;
                                    }

                                    if path.file_name().is_some_and(|name| name == "shutdown.cmd") {
                                        tracing_event!(Level::INFO, "Shutdown command detected: {:?}", path);
                                        let _ = self.app_command_tx
                                            .send(AppCommand::ClientShutdown(path.clone()))
                                            .await;
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            tracing_event!(Level::ERROR, "File watcher error: {:?}", error);
                        }
                    }
                }

                _ = stats_interval.tick() => {

                    if matches!(self.app_state.mode, AppMode::PowerSaving) && !self.app_state.run_time.is_multiple_of(5) {
                        self.app_state.run_time += 1;
                        continue;
                    }

                    let pid = match sysinfo::get_current_pid() {
                        Ok(pid) => pid,
                        Err(e) => {
                            tracing_event!(Level::ERROR, "Could not get current PID: {}", e);
                            continue; // Skip this tick of stats collection
                        }
                    };

                    sys.refresh_cpu_usage();
                    sys.refresh_memory();
                    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);


                    if let Some(process) = sys.process(pid) {
                        self.app_state.cpu_usage = process.cpu_usage() / sys.cpus().len() as f32;
                        self.app_state.app_ram_usage = process.memory();
                        self.app_state.ram_usage_percent = (process.memory() as f32 / sys.total_memory() as f32) * 100.0;
                        self.app_state.run_time = process.run_time();
                    }


                    // --- Calculate all thrash scores ---
                    self.app_state.global_disk_read_thrash_score = calculate_thrash_score(&self.app_state.global_disk_read_history_log);
                    self.app_state.global_disk_write_thrash_score = calculate_thrash_score(&self.app_state.global_disk_write_history_log);

                    let global_read_thrash_f64 = calculate_thrash_score_seek_cost_f64(&self.app_state.global_disk_read_history_log);
                    let global_write_thrash_f64 = calculate_thrash_score_seek_cost_f64(&self.app_state.global_disk_write_history_log);
                    self.app_state.global_disk_thrash_score = global_read_thrash_f64 + global_write_thrash_f64;

                    if self.app_state.global_disk_thrash_score > 0.01 {
                         self.app_state.global_seek_cost_per_byte_history.push(self.app_state.global_disk_thrash_score);
                    }
                    if self.app_state.global_seek_cost_per_byte_history.len() > 1000 {
                        self.app_state.global_seek_cost_per_byte_history.remove(0);
                    }
                    const MIN_SAMPLES_TO_LEARN: usize = 50;
                    if self.app_state.global_seek_cost_per_byte_history.len() > MIN_SAMPLES_TO_LEARN {
                        let mut sorted_history = self.app_state.global_seek_cost_per_byte_history.clone();
                        sorted_history.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                        let percentile_index = (sorted_history.len() as f64 * 0.95) as usize;
                        let new_scpb_max = sorted_history[percentile_index];
                        self.app_state.adaptive_max_scpb = new_scpb_max.max(1.0);
                    }


                    let mut global_disk_read_bps = 0;
                    let mut global_disk_write_bps = 0;

                    for torrent in self.app_state.torrents.values_mut() {
                        // Calculate and store per-torrent speed
                        torrent.disk_read_speed_bps = torrent.bytes_read_this_tick * 8;
                        torrent.disk_write_speed_bps = torrent.bytes_written_this_tick * 8;

                        // Sum for global total
                        global_disk_read_bps += torrent.disk_read_speed_bps;
                        global_disk_write_bps += torrent.disk_write_speed_bps;

                        // Reset per-torrent counters for the next tick
                        torrent.bytes_read_this_tick = 0;
                        torrent.bytes_written_this_tick = 0;

                        // Calculate per-torrent thrash scores
                        torrent.disk_read_thrash_score = calculate_thrash_score(&torrent.disk_read_history_log);
                        torrent.disk_write_thrash_score = calculate_thrash_score(&torrent.disk_write_history_log);
                    }

                    // Update the global history with the new, accurate totals
                    self.app_state.disk_read_history.push(global_disk_read_bps);
                    self.app_state.disk_write_history.push(global_disk_write_bps);
                    if self.app_state.disk_read_history.len() > 60 {
                        self.app_state.disk_read_history.remove(0);
                        self.app_state.disk_write_history.remove(0);
                    }

                    // Update the global average display value
                    self.app_state.avg_disk_read_bps = if self.app_state.disk_read_history.is_empty() {
                        0
                    } else {
                        self.app_state.disk_read_history.iter().sum::<u64>() / self.app_state.disk_read_history.len() as u64
                    };
                    self.app_state.avg_disk_write_bps = if self.app_state.disk_write_history.is_empty() {
                        0
                    } else {
                        self.app_state.disk_write_history.iter().sum::<u64>() / self.app_state.disk_write_history.len() as u64
                    };

                    let mut total_dl = 0;
                    let mut total_ul = 0;
                    for torrent in self.app_state.torrents.values() {
                        total_dl += torrent.smoothed_download_speed_bps;
                        total_ul += torrent.smoothed_upload_speed_bps;
                    }

                    self.app_state.total_download_history.push(total_dl);
                    self.app_state.total_upload_history.push(total_ul);
                    self.app_state.avg_download_history.push(total_dl);
                    self.app_state.avg_upload_history.push(total_ul);

                     // --- IOPS Calculations---
                    self.app_state.read_iops = self.app_state.reads_completed_this_tick;
                    self.app_state.write_iops = self.app_state.writes_completed_this_tick;
                    self.app_state.reads_completed_this_tick = 0;
                    self.app_state.writes_completed_this_tick = 0;

                    // Record the maximum backoff duration seen during the tick that just ended
                    self.app_state.disk_backoff_history_ms.push_back(self.app_state.max_disk_backoff_this_tick_ms);
                    if self.app_state.disk_backoff_history_ms.len() > SECONDS_HISTORY_MAX {
                        self.app_state.disk_backoff_history_ms.pop_front();
                    }

                    // System Runtime calculations ==================================
                    let run_time = self.app_state.run_time;
                    if run_time > 0 && run_time.is_multiple_of(60) {
                        let history_len = self.app_state.disk_backoff_history_ms.len();
                        let start_index = history_len.saturating_sub(60);

                        // 2. Now get the mutable borrow and slice
                        let backoff_slice_ms = &self.app_state.disk_backoff_history_ms.make_contiguous()[start_index..];
                        // Find the *maximum* backoff duration within that minute
                        let max_backoff_in_minute_ms = backoff_slice_ms.iter().max().copied().unwrap_or(0);
                        self.app_state.minute_disk_backoff_history_ms.push_back(max_backoff_in_minute_ms);
                        // Prune the minute-resolution history
                        if self.app_state.minute_disk_backoff_history_ms.len() > MINUTES_HISTORY_MAX {
                           self.app_state.minute_disk_backoff_history_ms.pop_front();
                        }


                        let seconds_dl = &self.app_state.avg_download_history;
                        // Get the last 60 seconds of data for an accurate average
                        let minute_slice_dl = &seconds_dl[seconds_dl.len().saturating_sub(60)..];
                        if !minute_slice_dl.is_empty() {
                            let minute_avg_dl = minute_slice_dl.iter().sum::<u64>() / minute_slice_dl.len() as u64;
                            self.app_state.minute_avg_dl_history.push(minute_avg_dl);
                        }

                        let seconds_ul = &self.app_state.avg_upload_history;
                        let minute_slice_ul = &seconds_ul[seconds_ul.len().saturating_sub(60)..];
                        if !minute_slice_ul.is_empty() {
                            let minute_avg_ul = minute_slice_ul.iter().sum::<u64>() / minute_slice_ul.len() as u64;
                            self.app_state.minute_avg_ul_history.push(minute_avg_ul);
                        }
                    }
                    self.app_state.max_disk_backoff_this_tick_ms = 0;

                    if self.app_state.avg_download_history.len() > SECONDS_HISTORY_MAX {
                        self.app_state.avg_download_history.remove(0);
                        self.app_state.avg_upload_history.remove(0);
                    }
                    if self.app_state.minute_avg_dl_history.len() > MINUTES_HISTORY_MAX {
                        self.app_state.minute_avg_dl_history.remove(0);
                        self.app_state.minute_avg_ul_history.remove(0);
                    }

                    // Check if the primary objective (seeding vs. leeching) has changed.
                    let is_leeching = self.app_state.torrents.values().any(|t| {
                        t.latest_state.number_of_pieces_completed < t.latest_state.number_of_pieces_total
                    });
                    let is_seeding = !is_leeching;

                    // If the objective has changed, reset the tuning baseline immediately.
                    if is_seeding != self.app_state.is_seeding {
                        tracing_event!(Level::DEBUG, "Self-Tune: Objective changed to {}. Resetting score.", if is_seeding { "Seeding" } else { "Leeching" });
                        self.app_state.last_tuning_score = 0;
                        self.app_state.current_tuning_score = 0;
                        self.app_state.last_tuning_limits = self.app_state.limits.clone();
                    }
                    self.app_state.is_seeding = is_seeding;

                    self.app_state.tuning_countdown = self.app_state.tuning_countdown.saturating_sub(1);
                    self.app_state.ui_needs_redraw = true;
                }

                _ = tuning_interval.tick() => {
                    self.app_state.tuning_countdown = 90;
                    let history = if !self.app_state.is_seeding { // if leeching
                        &self.app_state.avg_download_history
                    } else {
                        &self.app_state.avg_upload_history
                    };

                    let relevant_history = &history[history.len().saturating_sub(60)..];
                    let new_raw_score = if relevant_history.is_empty() {
                        0
                    } else {
                        relevant_history.iter().sum::<u64>() / relevant_history.len() as u64
                    };
                    let current_scpb = self.app_state.global_disk_thrash_score;
                    let scpb_max = self.app_state.adaptive_max_scpb;
                    let penalty_factor = (current_scpb / scpb_max - 1.0).max(0.0);
                    let new_score = (new_raw_score as f64 / (1.0 + penalty_factor)) as u64;
                    self.app_state.current_tuning_score = new_score;

                    const BASELINE_ALPHA: f64 = 0.1; // Slower-moving average
                    let new_score_f64 = new_score as f64;
                    if self.app_state.baseline_speed_ema == 0.0 {
                        self.app_state.baseline_speed_ema = new_score_f64;
                    } else {
                        self.app_state.baseline_speed_ema = (new_score_f64 * BASELINE_ALPHA)
                            + (self.app_state.baseline_speed_ema * (1.0 - BASELINE_ALPHA));
                    }

                    let best_score = self.app_state.last_tuning_score;
                    if new_score > best_score {
                        self.app_state.last_tuning_score = new_score;
                        self.app_state.last_tuning_limits = self.app_state.limits.clone();
                        tracing_event!(Level::DEBUG, "Self-Tune: SUCCESS. New best score: {} (raw: {}, penalty: {:.2}x)", new_score, new_raw_score, penalty_factor);
                    } else {
                        self.app_state.limits = self.app_state.last_tuning_limits.clone();

                        let baseline_u64 = self.app_state.baseline_speed_ema as u64;

                        const REALITY_CHECK_FACTOR: f64 = 2.0;
                        if best_score > 10_000
                            && best_score
                                > (self.app_state.baseline_speed_ema * REALITY_CHECK_FACTOR) as u64
                        {
                            self.app_state.last_tuning_score = baseline_u64;
                            tracing_event!(Level::DEBUG, "Self-Tune: REALITY CHECK. Score {} (raw: {}) failed. Old best {} is stale vs. baseline {}. Resetting best to baseline.", new_score, new_raw_score, best_score, baseline_u64);
                        } else {
                            tracing_event!(Level::DEBUG, "Self-Tune: REVERTING. Score {} (raw: {}, penalty: {:.2}x) was not better than {}. (Baseline is {})", new_score, new_raw_score, penalty_factor, best_score, baseline_u64);
                        }

                        let _ = self.resource_manager
                            .update_limits(self.app_state.limits.clone().into_map())
                            .await;
                    }

                    let (next_limits, desc) = make_random_adjustment(self.app_state.limits.clone());
                    self.app_state.limits = next_limits; // Optimistically set the new limits

                    tracing_event!(Level::DEBUG, "Self-Tune: Trying next change... {}", desc);
                    let _ = self.resource_manager
                        .update_limits(self.app_state.limits.clone().into_map())
                        .await;
                }

                _ = draw_interval.tick() => {
                    if self.app_state.ui_needs_redraw {
                        terminal.draw(|f| {
                            tui::draw(f, &self.app_state, &self.client_configs);
                        })?;
                        self.app_state.ui_needs_redraw = false;
                    }
                }
            }
        }

        let _ = self.shutdown_tx.send(());

        self.client_configs.lifetime_downloaded += self.app_state.session_total_downloaded;
        self.client_configs.lifetime_uploaded += self.app_state.session_total_uploaded;
        self.client_configs.torrent_sort_column = self.app_state.torrent_sort.0;
        self.client_configs.torrent_sort_direction = self.app_state.torrent_sort.1;
        self.client_configs.peer_sort_column = self.app_state.peer_sort.0;
        self.client_configs.peer_sort_direction = self.app_state.peer_sort.1;

        self.client_configs.torrents = self
            .app_state
            .torrents
            .values()
            .map(|torrent| TorrentSettings {
                torrent_or_magnet: torrent.latest_state.torrent_or_magnet.clone(),
                name: torrent.latest_state.torrent_name.clone(),
                validation_status: torrent.latest_state.number_of_pieces_total
                    == torrent.latest_state.number_of_pieces_completed,
                download_path: torrent.latest_state.download_path.clone(),
                torrent_control_state: torrent.latest_state.torrent_control_state.clone(),
            })
            .collect();
        save_settings(&self.client_configs)?;

        for manager_tx in self.torrent_manager_command_txs.values() {
            let _ = manager_tx.try_send(ManagerCommand::Shutdown);
        }

        let hard_limit_timeout = Duration::from_secs(2);
        match self.run_shutdown_ui(terminal, hard_limit_timeout).await {
            Ok(_) => {
                tracing_event!(Level::INFO, "Shutdown UI finished gracefully.");
            }
            Err(e) => {
                tracing_event!(Level::ERROR, "Shutdown UI loop failed: {}", e);
            }
        }

        Ok(())
    }

    pub fn sort_torrent_list(&mut self) {
        let torrents_map = &self.app_state.torrents;
        let (sort_by, sort_direction) = self.app_state.torrent_sort;

        self.app_state
            .torrent_list_order
            .sort_by(|a_info_hash, b_info_hash| {
                let Some(a_torrent) = torrents_map.get(a_info_hash) else {
                    return std::cmp::Ordering::Equal;
                };
                let Some(b_torrent) = torrents_map.get(b_info_hash) else {
                    return std::cmp::Ordering::Equal;
                };

                // Determine the natural ordering for the selected column.
                let ordering = match sort_by {
                    // For Name, natural order is Ascending (A -> Z).
                    TorrentSortColumn::Name => a_torrent
                        .latest_state
                        .torrent_name
                        .cmp(&b_torrent.latest_state.torrent_name),

                    // For speeds and progress, natural order is Descending (High -> Low).
                    TorrentSortColumn::Down => b_torrent
                        .smoothed_download_speed_bps
                        .cmp(&a_torrent.smoothed_download_speed_bps),
                    TorrentSortColumn::Up => b_torrent
                        .smoothed_upload_speed_bps
                        .cmp(&a_torrent.smoothed_upload_speed_bps),
                };

                // Determine the default direction for the column.
                let default_direction = match sort_by {
                    TorrentSortColumn::Name => SortDirection::Ascending,
                    _ => SortDirection::Descending,
                };

                // If the user's chosen direction is NOT the default, reverse the ordering.
                if sort_direction != default_direction {
                    ordering.reverse()
                } else {
                    ordering
                }
            });
    }

    pub fn find_most_common_download_path(&mut self) -> Option<PathBuf> {
        let mut counts: HashMap<PathBuf, usize> = HashMap::new();

        for state in self.app_state.torrents.values() {
            if let Some(parent_path) = state.latest_state.download_path.parent() {
                *counts.entry(parent_path.to_path_buf()).or_insert(0) += 1;
            }
        }

        counts
            .into_iter()
            .max_by_key(|&(_, count)| count)
            .map(|(path, _)| path)
    }

    pub fn decode_info_hash(&mut self, hash_string: &str) -> Result<Vec<u8>, String> {
        if hash_string.len() == 40 {
            // It's Hex encoded
            hex::decode(hash_string).map_err(|e| e.to_string())
        } else if hash_string.len() == 32 {
            // It's Base32 encoded
            BASE32
                .decode(hash_string.to_uppercase().as_bytes())
                .map_err(|e| e.to_string())
        } else {
            Err(format!("Invalid info_hash length: {}", hash_string.len()))
        }
    }

    pub async fn add_torrent_from_file(
        &mut self,
        path: PathBuf,
        download_path: PathBuf,
        is_validated: bool,
        torrent_control_state: TorrentControlState,
    ) {
        let buffer = match fs::read(&path) {
            Ok(buf) => buf,
            Err(e) => {
                tracing_event!(
                    Level::ERROR,
                    "Failed to read torrent file {:?}: {}",
                    &path,
                    e
                );
                return;
            }
        };

        let torrent = match from_bytes(&buffer) {
            Ok(t) => t,
            Err(e) => {
                tracing_event!(
                    Level::ERROR,
                    "Failed to parse torrent file {:?}: {}",
                    &path,
                    e
                );
                return;
            }
        };

        #[cfg(all(feature = "dht", feature = "pex"))]
        {
            if torrent.info.private == Some(1) {
                tracing_event!(
                    Level::ERROR,
                    "Rejected private torrent '{}' in normal build.",
                    torrent.info.name
                );
                self.app_state.system_error = Some(format!(
                    "Private Torrent Rejected:'{}' This build (with DHT/PEX) is not safe for private trackers. Please use private builds for this torrent.",
                    torrent.info.name
                ));
                return;
            }
        }

        let mut hasher = sha1::Sha1::new();
        hasher.update(&torrent.info_dict_bencode);
        let info_hash = hasher.finalize().to_vec();

        if self.app_state.torrents.contains_key(&info_hash) {
            tracing_event!(
                Level::INFO,
                "Ignoring already present torrent: {}",
                torrent.info.name
            );
            return;
        }

        // Create a permanent copy of the torrent file
        let torrent_files_dir = match get_app_paths() {
            Some((_, data_dir)) => data_dir.join("torrents"),
            None => {
                tracing_event!(
                    Level::ERROR,
                    "Could not determine application data directory."
                );
                return;
            }
        };
        if let Err(e) = fs::create_dir_all(&torrent_files_dir) {
            tracing_event!(
                Level::ERROR,
                "Could not create torrents data directory: {}",
                e
            );
            return;
        }
        let permanent_torrent_path =
            torrent_files_dir.join(format!("{}.torrent", hex::encode(&info_hash)));
        if let Err(e) = fs::copy(&path, &permanent_torrent_path) {
            tracing_event!(
                Level::ERROR,
                "Failed to copy torrent to data directory: {}",
                e
            );
            return;
        }

        let placeholder_state = TorrentDisplayState {
            latest_state: TorrentState {
                torrent_control_state: torrent_control_state.clone(),
                info_hash: info_hash.clone(),
                torrent_or_magnet: permanent_torrent_path.to_string_lossy().to_string(),
                torrent_name: torrent.info.name.clone(),
                download_path: download_path.clone(),
                number_of_pieces_total: (torrent.info.pieces.len() / 20) as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        self.app_state
            .torrents
            .insert(info_hash.clone(), placeholder_state);
        self.app_state.torrent_list_order.push(info_hash.clone());

        let (incoming_peer_tx, incoming_peer_rx) = mpsc::channel::<(TcpStream, Vec<u8>)>(100);
        self.torrent_manager_incoming_peer_txs
            .insert(info_hash.clone(), incoming_peer_tx);
        let (manager_command_tx, manager_command_rx) = mpsc::channel::<ManagerCommand>(100);
        self.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_command_tx);

        let torrent_tx_clone = self.torrent_tx.clone();
        let manager_event_tx_clone = self.manager_event_tx.clone();
        let resource_manager_clone = self.resource_manager.clone();
        let global_dl_bucket_clone = self.global_dl_bucket.clone();
        let global_ul_bucket_clone = self.global_ul_bucket.clone();

        #[cfg(feature = "dht")]
        let dht_clone = self.distributed_hash_table.clone();
        #[cfg(not(feature = "dht"))]
        let dht_clone = ();

        let torrent_params = TorrentParameters {
            dht_handle: dht_clone,
            incoming_peer_rx,
            metrics_tx: torrent_tx_clone,
            torrent_validation_status: is_validated,
            download_dir: download_path,
            manager_command_rx,
            manager_event_tx: manager_event_tx_clone,
            settings: Arc::clone(&Arc::new(self.client_configs.clone())),
            resource_manager: resource_manager_clone,
            global_dl_bucket: global_dl_bucket_clone,
            global_ul_bucket: global_ul_bucket_clone,
        };

        match TorrentManager::from_torrent(torrent_params, torrent) {
            Ok(torrent_manager) => {
                tokio::spawn(async move {
                    let _ = torrent_manager
                        .run(torrent_control_state == TorrentControlState::Paused)
                        .await;
                });
            }
            Err(e) => {
                tracing_event!(
                    Level::ERROR,
                    "Failed to create torrent manager from file: {:?}",
                    e
                );
                self.app_state.torrents.remove(&info_hash);
                self.app_state
                    .torrent_list_order
                    .retain(|ih| *ih != info_hash);
            }
        }
    }

    pub async fn add_magnet_torrent(
        &mut self,
        torrent_name: String,
        magnet_link: String,
        download_path: PathBuf,
        is_validated: bool,
        torrent_control_state: TorrentControlState,
    ) {
        let magnet = match Magnet::new(&magnet_link) {
            Ok(m) => m,
            Err(e) => {
                tracing_event!(Level::ERROR, "Could not parse invalid magnet: {:?}", e);
                return;
            }
        };

        let hash_string = match magnet.hash() {
            Some(hash) => hash,
            None => {
                tracing_event!(Level::ERROR, "Magnet link is missing info_hash");
                return;
            }
        };

        let info_hash = match self.decode_info_hash(hash_string) {
            Ok(hash) => hash,
            Err(e) => {
                tracing_event!(Level::ERROR, "Failed to decode info_hash: {}", e);
                return;
            }
        };

        if self.app_state.torrents.contains_key(&info_hash) {
            tracing_event!(Level::INFO, "Ignoring already present torrent from magnet");
            return;
        }

        let placeholder_state = TorrentDisplayState {
            latest_state: TorrentState {
                torrent_control_state: torrent_control_state.clone(),
                info_hash: info_hash.clone(),
                torrent_or_magnet: magnet_link.clone(),
                torrent_name,
                download_path: download_path.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        self.app_state
            .torrents
            .insert(info_hash.clone(), placeholder_state);
        self.app_state.torrent_list_order.push(info_hash.clone());

        let (incoming_peer_tx, incoming_peer_rx) = mpsc::channel::<(TcpStream, Vec<u8>)>(100);
        self.torrent_manager_incoming_peer_txs
            .insert(info_hash.clone(), incoming_peer_tx);
        let (manager_command_tx, manager_command_rx) = mpsc::channel::<ManagerCommand>(100);
        self.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_command_tx);

        let dht_clone = self.distributed_hash_table.clone();
        let torrent_tx_clone = self.torrent_tx.clone();
        let manager_event_tx_clone = self.manager_event_tx.clone();
        let resource_manager_clone = self.resource_manager.clone();
        let global_dl_bucket_clone = self.global_dl_bucket.clone();
        let global_ul_bucket_clone = self.global_ul_bucket.clone();
        let torrent_params = TorrentParameters {
            dht_handle: dht_clone,
            incoming_peer_rx,
            metrics_tx: torrent_tx_clone,
            torrent_validation_status: is_validated,
            download_dir: download_path,
            manager_command_rx,
            manager_event_tx: manager_event_tx_clone,
            settings: Arc::clone(&Arc::new(self.client_configs.clone())),
            resource_manager: resource_manager_clone,
            global_dl_bucket: global_dl_bucket_clone,
            global_ul_bucket: global_ul_bucket_clone,
        };

        match TorrentManager::from_magnet(torrent_params, magnet) {
            Ok(torrent_manager) => {
                tokio::spawn(async move {
                    let _ = torrent_manager
                        .run(torrent_control_state == TorrentControlState::Paused)
                        .await;
                });
            }
            Err(e) => {
                tracing_event!(
                    Level::ERROR,
                    "Failed to create new torrent manager from magnet: {:?}",
                    e
                );
                self.app_state.torrents.remove(&info_hash);
                self.app_state
                    .torrent_list_order
                    .retain(|ih| *ih != info_hash);
            }
        }
    }

    async fn run_shutdown_ui(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
        display_duration: Duration,
    ) -> Result<(), std::io::Error> {
        let shutdown_start = Instant::now();

        loop {
            let elapsed = shutdown_start.elapsed();
            let progress_ratio = (elapsed.as_secs_f64() / display_duration.as_secs_f64()).min(1.0);
            self.app_state.shutdown_progress = progress_ratio;

            terminal.draw(|f| {
                tui::draw(f, &self.app_state, &self.client_configs);
            })?;

            if shutdown_start.elapsed() >= display_duration {
                break;
            }

            time::sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    }

    async fn process_pending_commands(&mut self) {
        if let Some((watch_path, _)) = get_watch_path() {
            let Ok(entries) = fs::read_dir(watch_path) else {
                return;
            };

            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }

                // Re-use the existing AppCommand logic to process the files
                if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                    let command = match ext {
                        "torrent" => Some(AppCommand::AddTorrentFromFile(path.clone())),
                        "path" => Some(AppCommand::AddTorrentFromPathFile(path.clone())),
                        "magnet" => Some(AppCommand::AddMagnetFromFile(path.clone())),
                        "cmd" if path.file_name().is_some_and(|name| name == "shutdown.cmd") => {
                            Some(AppCommand::ClientShutdown(path.clone()))
                        }
                        _ => None,
                    };

                    if let Some(cmd) = command {
                        // Send the command to our own channel to be processed by the main loop.
                        // This avoids duplicating the processing logic.
                        let _ = self.app_command_tx.send(cmd).await;
                    }
                }
            }
        }
    }
}

fn calculate_thrash_score(history_log: &VecDeque<DiskIoOperation>) -> u64 {
    if history_log.len() < 2 {
        return 0; // Not enough data to calculate a seek distance
    }

    let mut total_seek_distance = 0;
    let mut last_offset_end: Option<u64> = None;

    // Iterate in reverse to process operations in chronological order (oldest to newest)
    for op in history_log.iter().rev() {
        if let Some(prev_offset_end) = last_offset_end {
            total_seek_distance += op.offset.abs_diff(prev_offset_end);
        }
        last_offset_end = Some(op.offset + op.length as u64);
    }

    // The number of "seeks" is one less than the number of operations
    let seek_count = history_log.len() - 1;
    total_seek_distance / seek_count as u64
}

fn calculate_thrash_score_seek_cost_f64(history_log: &VecDeque<DiskIoOperation>) -> f64 {
    if history_log.len() < 2 {
        return 0.0; // Not enough data to calculate a seek distance
    }

    let mut total_seek_distance = 0;
    let mut total_bytes_transferred = 0;
    let mut last_offset_end: Option<u64> = None;

    // Iterate in reverse to process operations in chronological order (oldest to newest)
    for op in history_log.iter().rev() {
        if let Some(prev_offset_end) = last_offset_end {
            total_seek_distance += op.offset.abs_diff(prev_offset_end);
        }
        last_offset_end = Some(op.offset + op.length as u64);
        total_bytes_transferred += op.length as u64;
    }

    if total_bytes_transferred == 0 {
        return 0.0; // Avoid division by zero
    }

    // Return the "seek cost per byte"
    total_seek_distance as f64 / total_bytes_transferred as f64
}

fn calculate_adaptive_limits(client_configs: &Settings) -> (CalculatedLimits, Option<String>) {
    let effective_limit;
    let mut system_warning = None;
    const RECOMMENDED_MINIMUM: usize = 1024;

    if let Some(override_val) = client_configs.resource_limit_override {
        effective_limit = override_val;
        if effective_limit < RECOMMENDED_MINIMUM {
            system_warning = Some(format!(
                "Warning: Resource limit is set to {}, which is below the recommended minimum of {}. Performance may be degraded.",
                effective_limit, RECOMMENDED_MINIMUM
            ));
        }
    } else {
        #[cfg(unix)]
        {
            if let Ok((soft_limit, _)) = Resource::NOFILE.get() {
                effective_limit = soft_limit as usize;
                if effective_limit < RECOMMENDED_MINIMUM {
                    system_warning = Some(format!(
                        "Warning: System file handle limit is {}, which is below the recommended minimum of {}. Performance may be degraded. Consider increasing with 'ulimit -n'.",
                        effective_limit, RECOMMENDED_MINIMUM
                    ));
                }
            } else {
                effective_limit = RECOMMENDED_MINIMUM;
            }
        }
        #[cfg(windows)]
        {
            effective_limit = 8192;
        }
        #[cfg(not(any(unix, windows)))]
        {
            effective_limit = RECOMMENDED_MINIMUM;
        }
    }

    if let Some(warning) = &system_warning {
        tracing_event!(Level::WARN, "{}", warning);
    }

    let available_budget_after_reservation = effective_limit.saturating_sub(FILE_HANDLE_MINIMUM);
    let safe_budget = available_budget_after_reservation as f64 * SAFE_BUDGET_PERCENTAGE;
    const PEER_PROPORTION: f64 = 0.70;
    const DISK_READ_PROPORTION: f64 = 0.15;
    const DISK_WRITE_PROPORTION: f64 = 0.15;

    let limits = CalculatedLimits {
        reserve_permits: 0,
        max_connected_peers: (safe_budget * PEER_PROPORTION).max(10.0) as usize,
        disk_read_permits: (safe_budget * DISK_READ_PROPORTION).max(4.0) as usize,
        disk_write_permits: (safe_budget * DISK_WRITE_PROPORTION).max(4.0) as usize,
    };

    (limits, system_warning)
}

const MIN_STEP_RATE: f64 = 0.01;
const MAX_STEP_RATE: f64 = 0.10;

// --- Define Min/Max bounds for all resource types ---
const MIN_PEERS: usize = 20;
const MIN_DISK: usize = 2;
const MIN_RESERVE: usize = 0;

// --- Maximum attempts to find a valid trade per cycle ---
const MAX_TRADE_ATTEMPTS: usize = 5;

fn get_limit(limits: &CalculatedLimits, resource: ResourceType) -> usize {
    match resource {
        ResourceType::PeerConnection => limits.max_connected_peers,
        ResourceType::DiskRead => limits.disk_read_permits,
        ResourceType::DiskWrite => limits.disk_write_permits,
        ResourceType::Reserve => limits.reserve_permits,
    }
}

fn set_limit(limits: &mut CalculatedLimits, resource: ResourceType, value: usize) {
    match resource {
        ResourceType::PeerConnection => limits.max_connected_peers = value,
        ResourceType::DiskRead => limits.disk_read_permits = value,
        ResourceType::DiskWrite => limits.disk_write_permits = value,
        ResourceType::Reserve => limits.reserve_permits = value,
    }
}

/// Makes a random, proportional trade, retrying a few times if the first is blocked.
/// This version is refactored to support any number of resources, including Reserve.
fn make_random_adjustment(mut limits: CalculatedLimits) -> (CalculatedLimits, String) {
    let mut rng = rand::rng();
    let mut parameters = [
        ResourceType::PeerConnection,
        ResourceType::DiskRead,
        ResourceType::DiskWrite,
        ResourceType::Reserve, // Add Reserve to the trading pool
    ];

    for attempt in 0..MAX_TRADE_ATTEMPTS {
        // 1. Randomly shuffle to pick a Source and Destination
        parameters.shuffle(&mut rng);
        let source_param = parameters[0];
        let dest_param = parameters[1];

        // 2. Get current values and bounds
        let source_val = get_limit(&limits, source_param);
        let dest_val = get_limit(&limits, dest_param);

        let source_min = match source_param {
            ResourceType::PeerConnection => MIN_PEERS,
            ResourceType::DiskRead => MIN_DISK,
            ResourceType::DiskWrite => MIN_DISK,
            ResourceType::Reserve => MIN_RESERVE,
        };

        // 3. Calculate random step rate and amount to trade
        let step_rate = rng.random_range(MIN_STEP_RATE..=MAX_STEP_RATE);
        let amount_to_trade = ((source_val as f64 * step_rate).ceil() as usize).max(1);

        // 4. Check if this specific trade is possible
        let can_give = source_val >= source_min.saturating_add(amount_to_trade);

        if can_give {
            // --- VALID TRADE FOUND ---
            // 5. Perform the 1-for-1 trade
            set_limit(
                &mut limits,
                source_param,
                source_val.saturating_sub(amount_to_trade),
            );
            set_limit(
                &mut limits,
                dest_param,
                dest_val.saturating_add(amount_to_trade),
            );

            let description = format!(
                "Traded {} from {:?} to {:?} (Attempt {})",
                amount_to_trade,
                source_param,
                dest_param,
                attempt + 1
            );
            // Return immediately with the successful trade
            return (limits, description);
        }
        // If trade wasn't possible, the loop continues to the next attempt...
    }

    // --- NO VALID TRADE FOUND after all attempts ---
    // Return the original limits unchanged
    let description = format!(
        "Skipped all trade attempts ({}) this cycle: blocked by bounds",
        MAX_TRADE_ATTEMPTS
    );
    (limits, description)
}
