// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::fs::File;
use std::io::{self, ErrorKind, Stdout};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use std::collections::VecDeque;

use magnet_url::Magnet;

use fuzzy_matcher::FuzzyMatcher;

use strum_macros::EnumIter;

use crate::torrent_manager::DiskIoOperation;

use crate::config::{
    classify_shared_mode_settings_change, resolve_host_watch_path, runtime_watch_paths,
    save_settings, shared_host_id, shared_inbox_path, shared_root_path, upsert_torrent_metadata,
    FeedSyncError, PeerSortColumn, RssFilterMode, RssHistoryEntry, Settings, SettingsChangeScope,
    SortDirection, TorrentMetadataEntry, TorrentMetadataFileEntry, TorrentSettings,
    TorrentSortColumn,
};
use crate::control_service::{
    control_event_details, online_control_success_message, plan_control_request,
    ControlExecutionPlan,
};
use crate::dht::service::{DhtService, DhtServiceConfig, DhtStatus};
use crate::persistence::activity_history::{
    load_activity_history_state, save_activity_history_state, ActivityHistoryPersistedState,
    ActivityHistoryRollupState,
};
use crate::persistence::event_journal::{
    append_event_journal_entry, load_event_journal_state, save_event_journal_state, ControlOrigin,
    EventCategory, EventDetails, EventJournalEntry, EventJournalState, EventScope, EventType,
    IngestKind, IngestOrigin,
};
use crate::persistence::network_history::{
    load_network_history_state, save_network_history_state, NetworkHistoryPersistedState,
    NetworkHistoryRollupState,
};
use crate::persistence::rss::{load_rss_state, save_rss_state, RssPersistedState};

use crate::token_bucket::TokenBucket;

use crate::tui::effects::compute_effects_activity_speed_multiplier;
use crate::tui::events;
use crate::tui::paste_burst::PasteBurst;
use crate::tui::tree;
use crate::tui::tree::RawNode;
use crate::tui::tree::TreeViewState;
use crate::tui::view::draw;

use crate::config::resolve_command_watch_path;
use crate::storage::build_fs_tree;

use crate::resource_manager::ResourceType;
use crate::telemetry::activity_history_telemetry::ActivityHistoryTelemetry;
use crate::telemetry::network_history_telemetry::NetworkHistoryTelemetry;
use crate::telemetry::ui_telemetry::UiTelemetry;
use crate::theme::Theme;
use crate::tuning::{make_random_adjustment, normalize_limits_for_mode, TuningController};

use crate::global_peer_manager::DispatchedPeerCandidate;
use crate::global_peer_manager::GlobalPeerManager;
use crate::integrations::rss_url_safety::is_safe_rss_item_url;
use crate::integrations::status::AppOutputState;
use crate::integrations::{
    control::{write_control_request, ControlFilePriorityOverride, ControlRequest},
    rss_ingest, rss_service, status, watcher,
};
use crate::integrity_scheduler::{
    IntegrityScheduler, ProbeBatchOutcome, TorrentIntegritySnapshot,
    INTEGRITY_SCHEDULER_TICK_INTERVAL,
};
use crate::network_metrics;
use crate::torrent_file::parser::from_bytes;
use crate::torrent_identity::info_hash_from_torrent_source;
use crate::torrent_manager::data_availability_from_file_probe_result;
use crate::torrent_manager::IncomingPeerSession;
use crate::torrent_manager::FileActivityUpdate;
use crate::torrent_manager::ManagerCommand;
use crate::torrent_manager::ManagerEvent;
use crate::torrent_manager::PeerCandidate;
use crate::torrent_manager::TorrentFileProbeStatus;
use crate::torrent_manager::TorrentManager;
use crate::torrent_manager::TorrentParameters;
use crate::watch_inbox::{archive_watch_file, relay_watch_file_to_shared_inbox};

use std::collections::{HashMap, HashSet};
use tokio::io::AsyncReadExt;
use tokio::signal;
use tokio::sync::broadcast;
use tokio::sync::mpsc::Sender;
use tokio::sync::watch;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use sha1::Digest;
use sha2::Sha256;

use notify::{Error as NotifyError, Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use ratatui::prelude::Rect;
use ratatui::{backend::CrosstermBackend, Terminal};

use sysinfo::System;

use tracing::{event as tracing_event, Level};

use crate::resource_manager::{ResourceManager, ResourceManagerClient};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use tokio::time;

fn format_filesystem_path_error(action: &str, path: &Path, error: &io::Error) -> String {
    let detail = match error.kind() {
        ErrorKind::NotFound => "file or directory was not found".to_string(),
        ErrorKind::PermissionDenied => "permission denied".to_string(),
        ErrorKind::IsADirectory => {
            "expected a file here, but the path points to a directory".to_string()
        }
        ErrorKind::NotADirectory => {
            "expected a directory component in the path, but found a file".to_string()
        }
        _ if path.is_dir() => {
            "expected a file here, but the path points to a directory".to_string()
        }
        _ => error.to_string(),
    };

    format!("{} {:?}: {}", action, path, detail)
}
use tokio::time::MissedTickBehavior;

use directories::UserDirs;

use ratatui::crossterm::event::{self, Event as CrosstermEvent};

#[cfg(unix)]
use rlimit::Resource;

const FILE_HANDLE_MINIMUM: usize = 64;
const SAFE_BUDGET_PERCENTAGE: f64 = 0.85;
pub const RSS_MAX_TORRENT_DOWNLOAD_BYTES: usize = 10 * 1024 * 1024;
const RSS_MANUAL_DOWNLOAD_TIMEOUT_SECS: u64 = 20;
const NETWORK_HISTORY_PERSIST_INTERVAL_SECS: u64 = 15 * 60;
const WATCH_FOLDER_RESCAN_INTERVAL_SECS: u64 = 5;
const SHARED_ROLE_RETRY_INTERVAL_SECS: u64 = 2;
const STARTUP_ROLLING_BATCH_SIZE: usize = 1;
const STARTUP_ROLLING_BATCH_INTERVAL_SECS: u64 = 1;
const STARTUP_ROLLING_LOADS_PER_INTERVAL: usize = 1;

const SHUTDOWN_TIMEOUT_SECS: u64 = 20;
const INCOMING_HANDSHAKE_TIMEOUT_SECS: u64 = 10;
const PORT_FAMILY_HIGHLIGHT_DURATION: Duration = Duration::from_secs(2);
const BITTORRENT_PROTOCOL_STR: &[u8] = b"BitTorrent protocol";

pub struct ListenerSet {
    ipv4: Option<TcpListener>,
    ipv6: Option<TcpListener>,
}

impl ListenerSet {
    async fn bind(port: u16) -> io::Result<Self> {
        let ipv6 = match TcpListener::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port))
            .await
        {
            Ok(listener) => Some(listener),
            Err(error) => {
                tracing_event!(
                    Level::WARN,
                    error = %error,
                    "IPv6 listener bind failed; continuing without IPv6 listener."
                );
                None
            }
        };

        let ipv4_port = match (port, ipv6.as_ref()) {
            (0, Some(listener)) => listener.local_addr()?.port(),
            _ => port,
        };

        let ipv4 = match TcpListener::bind(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            ipv4_port,
        ))
        .await
        {
            Ok(listener) => Some(listener),
            Err(error) if ipv6.is_some() && error.kind() == io::ErrorKind::AddrInUse => None,
            Err(error) if ipv6.is_some() => {
                tracing_event!(
                    Level::WARN,
                    error = %error,
                    "IPv4 listener bind failed; continuing with IPv6 listener only."
                );
                None
            }
            Err(error) => return Err(error),
        };

        if ipv4.is_none() && ipv6.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "failed to bind IPv4 or IPv6 listener",
            ));
        }

        Ok(Self { ipv4, ipv6 })
    }

    async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        match (&self.ipv4, &self.ipv6) {
            (Some(ipv4), Some(ipv6)) => {
                tokio::select! {
                    res = ipv4.accept() => res,
                    res = ipv6.accept() => res,
                }
            }
            (Some(ipv4), None) => ipv4.accept().await,
            (None, Some(ipv6)) => ipv6.accept().await,
            (None, None) => Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "no listener is currently bound",
            )),
        }
    }

    fn local_port(&self) -> Option<u16> {
        self.ipv4
            .as_ref()
            .or(self.ipv6.as_ref())
            .and_then(|listener| listener.local_addr().ok())
            .map(|addr| addr.port())
    }
}

#[derive(serde::Deserialize)]
struct CratesResponse {
    #[serde(rename = "crate")]
    krate: CrateInfo,
}

#[derive(serde::Deserialize)]
struct CrateInfo {
    max_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FilePriority {
    #[default]
    Normal,
    High,
    Skip,
    Mixed, // Used for folders that contain children with different priorities
}

impl FilePriority {
    pub fn next(&self) -> Self {
        match self {
            Self::Normal => Self::Skip,
            Self::Skip => Self::High,
            Self::High => Self::Normal,
            Self::Mixed => Self::Normal, // Reset mixed to Normal on toggle
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TorrentPreviewPayload {
    pub file_index: Option<usize>, // None for folders
    pub size: u64,
    pub priority: FilePriority,
}

struct TorrentPreviewFileEntry {
    parts: Vec<String>,
    file_index: usize,
    size: u64,
}

// Implement AddAssign so RawNode::from_path_list can aggregate folder sizes
impl std::ops::AddAssign for TorrentPreviewPayload {
    fn add_assign(&mut self, rhs: Self) {
        self.size += rhs.size;
        // Logic to determine folder priority state (e.g., if children differ -> Mixed)
        if self.priority != rhs.priority {
            self.priority = FilePriority::Mixed;
        }
    }
}

#[derive(Default, Debug, Clone, PartialEq)]
pub enum BrowserPane {
    #[default]
    FileSystem,
    TorrentPreview,
}

#[derive(Default, Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum FileBrowserMode {
    #[default]
    Directory, // User must pick a folder (e.g. Download Location)
    File(Vec<String>), // User must pick a file matching these extensions (e.g. vec!["torrent"])
    // Future proofing: You could add 'AnyFile' or 'FileOrFolder' here later
    DownloadLocSelection {
        torrent_files: Vec<String>, // List of relative file paths in the torrent
        container_name: String,     // Name of the container folder (e.g. hash_name)
        use_container: bool,        // Toggle state
        is_editing_name: bool,      // Whether the user is currently typing the name
        focused_pane: BrowserPane,
        preview_tree: Vec<RawNode<TorrentPreviewPayload>>, // Interactive tree
        preview_state: TreeViewState,                      // Cursor & expansion state for preview
        cursor_pos: usize,
        original_name_backup: String,
    },
    ConfigPathSelection {
        target_item: ConfigItem,
        current_settings: Box<Settings>,
        selected_index: usize,
        items: Vec<ConfigItem>,
    },
}

#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub size: u64,
    pub modified: std::time::SystemTime,
}

#[derive(Debug, Clone, Copy, Default)]
pub enum DataRate {
    RateQuarter,
    RateHalf,
    #[default]
    Rate1s,
    Rate2s,
    Rate4s,
    Rate10s,
    Rate20s,
    Rate30s,
    Rate60s,
}

impl DataRate {
    /// Returns the millisecond value for the data rate.
    pub fn as_ms(&self) -> u64 {
        match self {
            DataRate::RateQuarter => 4000,
            DataRate::RateHalf => 2000,
            DataRate::Rate1s => 1000,
            DataRate::Rate2s => 500,
            DataRate::Rate4s => 250,
            DataRate::Rate10s => 100,
            DataRate::Rate20s => 50,
            DataRate::Rate30s => 33,
            DataRate::Rate60s => 17,
        }
    }

    pub fn to_string(self) -> &'static str {
        match self {
            DataRate::RateQuarter => "0.25 FPS",
            DataRate::RateHalf => "0.5 FPS",
            DataRate::Rate1s => "1 FPS",
            DataRate::Rate2s => "2 FPS",
            DataRate::Rate4s => "4 FPS",
            DataRate::Rate10s => "10 FPS",
            DataRate::Rate20s => "20 FPS",
            DataRate::Rate30s => "30 FPS",
            DataRate::Rate60s => "60 FPS",
        }
    }

    /// Cycles to the next (slower) data rate (lower FPS).
    pub fn next_slower(&self) -> Self {
        match self {
            DataRate::Rate60s => DataRate::Rate30s,
            DataRate::Rate30s => DataRate::Rate20s,
            DataRate::Rate20s => DataRate::Rate10s,
            DataRate::Rate10s => DataRate::Rate4s,
            DataRate::Rate4s => DataRate::Rate2s,
            DataRate::Rate2s => DataRate::Rate1s,
            DataRate::Rate1s => DataRate::RateHalf,
            DataRate::RateHalf => DataRate::RateQuarter,
            DataRate::RateQuarter => DataRate::RateQuarter,
        }
    }

    /// Cycles to the previous (faster) data rate (higher FPS).
    pub fn next_faster(&self) -> Self {
        match self {
            DataRate::RateQuarter => DataRate::RateHalf,
            DataRate::RateHalf => DataRate::Rate1s,
            DataRate::Rate1s => DataRate::Rate2s,
            DataRate::Rate2s => DataRate::Rate4s,
            DataRate::Rate4s => DataRate::Rate10s,
            DataRate::Rate10s => DataRate::Rate20s,
            DataRate::Rate20s => DataRate::Rate30s,
            DataRate::Rate30s => DataRate::Rate60s,
            DataRate::Rate60s => DataRate::Rate60s,
        }
    }
}

#[derive(Default, Clone, Debug)]
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
        map.insert(
            ResourceType::PeerHandshake,
            peer_handshake_limit(self.max_connected_peers),
        );
        map.insert(ResourceType::PeerConnection, self.max_connected_peers);
        map.insert(ResourceType::DiskRead, self.disk_read_permits);
        map.insert(ResourceType::DiskWrite, self.disk_write_permits);
        map
    }
}

fn peer_handshake_limit(max_connected_peers: usize) -> usize {
    (max_connected_peers / 4).clamp(8, 64)
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
    SevenDays,
    ThirtyDays,
    OneYear,
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
            Self::SevenDays => 7 * 86_400,
            Self::ThirtyDays => 30 * 86_400,
            Self::OneYear => 365 * 86_400,
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
            Self::SevenDays => "7d",
            Self::ThirtyDays => "30d",
            Self::OneYear => "1y",
        }
    }

    pub fn next(&self) -> Self {
        match self {
            Self::OneMinute => Self::FiveMinutes,
            Self::FiveMinutes => Self::TenMinutes,
            Self::TenMinutes => Self::ThirtyMinutes,
            Self::ThirtyMinutes => Self::OneHour,
            Self::OneHour => Self::ThreeHours,
            Self::ThreeHours => Self::TwelveHours,
            Self::TwelveHours => Self::TwentyFourHours,
            Self::TwentyFourHours => Self::SevenDays,
            Self::SevenDays => Self::ThirtyDays,
            Self::ThirtyDays => Self::OneYear,
            Self::OneYear => Self::OneYear,
        }
    }

    pub fn prev(&self) -> Self {
        match self {
            Self::OneMinute => Self::OneMinute,
            Self::FiveMinutes => Self::OneMinute,
            Self::TenMinutes => Self::FiveMinutes,
            Self::ThirtyMinutes => Self::TenMinutes,
            Self::OneHour => Self::ThirtyMinutes,
            Self::ThreeHours => Self::OneHour,
            Self::TwelveHours => Self::ThreeHours,
            Self::TwentyFourHours => Self::TwelveHours,
            Self::SevenDays => Self::TwentyFourHours,
            Self::ThirtyDays => Self::SevenDays,
            Self::OneYear => Self::ThirtyDays,
        }
    }
}

#[derive(Default, Clone, Copy, PartialEq, Debug)]
pub enum ChartPanelView {
    #[default]
    Network,
    Cpu,
    Ram,
    Disk,
    Tuning,
    TorrentOverlay,
    MultiTorrentOverlay,
}

impl ChartPanelView {
    pub fn to_string(self) -> &'static str {
        match self {
            Self::Network => "NET",
            Self::Cpu => "CPU",
            Self::Ram => "RAM",
            Self::Disk => "DISK",
            Self::Tuning => "TUNE",
            Self::TorrentOverlay => "TOR",
            Self::MultiTorrentOverlay => "MULTI",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Network => Self::Cpu,
            Self::Cpu => Self::Ram,
            Self::Ram => Self::Disk,
            Self::Disk => Self::Tuning,
            Self::Tuning => Self::TorrentOverlay,
            Self::TorrentOverlay => Self::MultiTorrentOverlay,
            Self::MultiTorrentOverlay => Self::MultiTorrentOverlay,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Network => Self::Network,
            Self::Cpu => Self::Network,
            Self::Ram => Self::Cpu,
            Self::Disk => Self::Ram,
            Self::Tuning => Self::Disk,
            Self::TorrentOverlay => Self::Tuning,
            Self::MultiTorrentOverlay => Self::TorrentOverlay,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SelectedHeader {
    Torrent(usize),
    Peer(usize),
}
impl Default for SelectedHeader {
    fn default() -> Self {
        SelectedHeader::Torrent(0)
    }
}

pub enum AppCommand {
    AddTorrentFromFile(PathBuf),
    AddTorrentFromPathFile(PathBuf),
    AddMagnetFromFile(PathBuf),
    RouteIncomingPeer {
        stream: TcpStream,
        peer_addr: SocketAddr,
        handshake_response: Vec<u8>,
    },
    FinalizeIncomingPeerAdmission {
        info_hash: Vec<u8>,
        peer_addr: SocketAddr,
        session: IncomingPeerSession,
    },
    ReleaseIncomingPeerReservation {
        info_hash: Vec<u8>,
        peer_addr: SocketAddr,
    },
    MarkPortOpen(SocketAddr),
    ReloadClusterState(PathBuf),
    SubmitControlRequest(ControlRequest),
    ControlRequest {
        path: PathBuf,
        request: ControlRequest,
    },
    ClientShutdown(PathBuf),
    PortFileChanged(PathBuf),
    FetchFileTree {
        path: PathBuf,
        browser_mode: FileBrowserMode,
        highlight_path: Option<PathBuf>,
    },
    UpdateFileBrowserData {
        data: Vec<tree::RawNode<FileMetadata>>,
        highlight_path: Option<PathBuf>,
    },
    RssSyncNow,
    RssPreviewUpdated(Vec<RssPreviewItem>),
    RssSyncStatusUpdated {
        last_sync_at: Option<String>,
        next_sync_at: Option<String>,
    },
    RssFeedErrorUpdated {
        feed_url: String,
        error: Option<FeedSyncError>,
    },
    RssDownloadSelected {
        entry: RssHistoryEntry,
        command_path: Option<PathBuf>,
    },
    RssDownloadPreview(RssPreviewItem),
    NetworkHistoryLoaded(NetworkHistoryPersistedState),
    ActivityHistoryLoaded(Box<ActivityHistoryPersistedState>),
    NetworkHistoryPersisted {
        request_id: u64,
        success: bool,
    },
    ActivityHistoryPersisted {
        request_id: u64,
        success: bool,
    },
    UpdateConfig(Settings),
    UpdateVersionAvailable(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppRuntimeMode {
    Normal,
    SharedLeader,
    SharedFollower,
}

impl AppRuntimeMode {
    pub fn is_shared(self) -> bool {
        matches!(self, Self::SharedLeader | Self::SharedFollower)
    }

    pub fn is_shared_follower(self) -> bool {
        matches!(self, Self::SharedFollower)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppClusterRole {
    Leader,
    Follower,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClusterCapabilities {
    can_write_shared_state: bool,
    can_queue_shared_commands: bool,
    can_edit_host_local_config: bool,
    can_persist_local_runtime_state: bool,
    can_consume_shared_inbox: bool,
}

#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IngestSource {
    TorrentFile,
    TorrentPathFile,
    MagnetFile,
}

impl IngestSource {
    fn relay_archive_extension(self) -> &'static str {
        match self {
            Self::TorrentFile => "torrent.forwarded",
            Self::TorrentPathFile => "path.forwarded",
            Self::MagnetFile => "magnet.forwarded",
        }
    }

    fn processed_archive_extension(self) -> &'static str {
        match self {
            Self::TorrentFile => "torrent.added",
            Self::TorrentPathFile => "path.added",
            Self::MagnetFile => "magnet.added",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ResolvedAddPayload {
    TorrentFile { source_path: PathBuf },
    MagnetLink { magnet_link: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AddIngressAction {
    RelayRawWatchFile,
    QueueControlRequest(ControlRequest),
    ApplyDirectly {
        payload: ResolvedAddPayload,
        download_path: PathBuf,
    },
    OpenManualBrowser {
        payload: ResolvedAddPayload,
    },
    IgnoreMissingSharedInboxItem {
        message: String,
    },
    Fail {
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, EnumIter)]
pub enum ConfigItem {
    ClientPort,
    DefaultDownloadFolder,
    WatchFolder,
    GlobalDownloadLimit,
    GlobalUploadLimit,
}

#[derive(Default)]
#[allow(clippy::large_enum_variant)]
pub enum AppMode {
    Welcome,
    #[default]
    Normal,
    Help,
    Journal,
    PowerSaving,
    DeleteConfirm,
    Config,
    FileBrowser,
    Rss,
}

type AvailabilityTransitionLog = (String, bool, usize, Option<std::path::PathBuf>, Vec<String>);

#[derive(Debug, Clone)]
pub(crate) struct PendingIngestRecord {
    correlation_id: String,
    origin: IngestOrigin,
    ingest_kind: IngestKind,
    source_watch_folder: Option<PathBuf>,
    source_path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingControlRecord {
    correlation_id: String,
    request: ControlRequest,
    origin: ControlOrigin,
    source_watch_folder: Option<PathBuf>,
    source_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommandIngestResult {
    Added {
        info_hash: Option<Vec<u8>>,
        torrent_name: Option<String>,
    },
    Duplicate {
        info_hash: Option<Vec<u8>>,
        torrent_name: Option<String>,
    },
    Invalid {
        info_hash: Option<Vec<u8>>,
        torrent_name: Option<String>,
        message: String,
    },
    Failed {
        info_hash: Option<Vec<u8>>,
        torrent_name: Option<String>,
        message: String,
    },
}

#[cfg(test)]
fn move_file_with_fallback_impl<F>(
    source: &std::path::Path,
    destination: &std::path::Path,
    rename_op: F,
) -> std::io::Result<()>
where
    F: FnOnce(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
{
    crate::watch_inbox::move_file_with_fallback_impl(source, destination, rename_op)
}

fn ingest_kind_from_path(path: &std::path::Path) -> Option<IngestKind> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("torrent") => Some(IngestKind::TorrentFile),
        Some("magnet") => Some(IngestKind::MagnetFile),
        Some("path") => Some(IngestKind::PathFile),
        _ => None,
    }
}

fn event_correlation_id_for_path(path: &std::path::Path) -> String {
    hex::encode(sha1::Sha1::digest(path.to_string_lossy().as_bytes()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RssScreen {
    #[default]
    Unified,
    History,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RssSectionFocus {
    Links,
    Filters,
    #[default]
    Explorer,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum TorrentControlState {
    #[default]
    Running,
    Paused,
    Deleting,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TorrentMetrics {
    pub torrent_control_state: TorrentControlState,
    pub delete_files: bool,
    pub info_hash: Vec<u8>,
    pub torrent_or_magnet: String,
    pub torrent_name: String,
    pub download_path: Option<PathBuf>,
    pub container_name: Option<String>,
    #[serde(default)]
    pub is_multi_file: bool,
    pub file_count: Option<usize>,
    pub file_priorities: HashMap<usize, FilePriority>,
    pub data_available: bool,
    pub is_complete: bool,
    pub number_of_successfully_connected_peers: usize,
    pub number_of_pieces_total: u32,
    pub number_of_pieces_completed: u32,
    pub download_speed_bps: u64,
    pub upload_speed_bps: u64,
    pub bytes_downloaded_this_tick: u64,
    pub bytes_uploaded_this_tick: u64,
    pub session_total_downloaded: u64,
    pub session_total_uploaded: u64,
    pub eta: Duration,

    #[serde(skip)]
    pub peers: Vec<PeerInfo>,
    pub activity_message: String,
    pub next_announce_in: Duration,
    pub total_size: u64,
    pub bytes_written: u64,

    #[serde(skip)]
    pub blocks_in_history: Vec<u64>,

    #[serde(skip)]
    pub blocks_out_history: Vec<u64>,

    #[serde(skip)]
    pub file_activity_updates: Vec<FileActivityUpdate>,

    pub blocks_in_this_tick: u64,
    pub blocks_out_this_tick: u64,
}

impl Default for TorrentMetrics {
    fn default() -> Self {
        Self {
            torrent_control_state: TorrentControlState::default(),
            delete_files: false,
            info_hash: Vec::new(),
            torrent_or_magnet: String::new(),
            torrent_name: String::new(),
            download_path: None,
            container_name: None,
            is_multi_file: false,
            file_count: None,
            file_priorities: HashMap::new(),
            data_available: true,
            is_complete: false,
            number_of_successfully_connected_peers: 0,
            number_of_pieces_total: 0,
            number_of_pieces_completed: 0,
            download_speed_bps: 0,
            upload_speed_bps: 0,
            bytes_downloaded_this_tick: 0,
            bytes_uploaded_this_tick: 0,
            session_total_downloaded: 0,
            session_total_uploaded: 0,
            eta: Duration::default(),
            peers: Vec::new(),
            activity_message: String::new(),
            next_announce_in: Duration::default(),
            total_size: 0,
            bytes_written: 0,
            blocks_in_history: Vec::new(),
            blocks_out_history: Vec::new(),
            file_activity_updates: Vec::new(),
            blocks_in_this_tick: 0,
            blocks_out_this_tick: 0,
        }
    }
}

#[derive(Default, Debug)]
pub struct TorrentDisplayState {
    pub latest_state: TorrentMetrics,
    pub file_preview_tree: Vec<RawNode<TorrentPreviewPayload>>,
    pub recent_file_activity: HashMap<String, RecentFileActivity>,
    pub latest_file_probe_status: Option<TorrentFileProbeStatus>,
    pub integrity_next_probe_in: Option<Duration>,
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

    pub swarm_availability_history: Vec<Vec<u32>>,

    pub peers_discovered_this_tick: u64,
    pub peers_connected_this_tick: u64,
    pub peers_disconnected_this_tick: u64,
    pub peer_discovery_history: Vec<u64>,
    pub peer_connection_history: Vec<u64>,
    pub peer_disconnect_history: Vec<u64>,
    pub last_seen_session_total_downloaded: u64,
    pub last_seen_session_total_uploaded: u64,
}

#[derive(Debug, Clone, Default)]
pub struct RecentFileActivity {
    pub download_at: Option<Instant>,
    pub upload_at: Option<Instant>,
}

#[derive(Default)]
pub struct UiState {
    pub needs_redraw: bool,
    pub effects_phase_time: f64,
    pub effects_last_wall_time: f64,
    pub effects_speed_multiplier: f64,
    pub file_activity_download_phase: f64,
    pub file_activity_upload_phase: f64,
    pub selected_header: SelectedHeader,
    pub selected_torrent_index: usize,
    pub selected_peer_index: usize,
    pub show_torrent_files: bool,
    pub is_searching: bool,
    pub search_query: String,
    pub config: ConfigUiState,
    pub delete_confirm: DeleteConfirmUiState,
    pub file_browser: FileBrowserUiState,
    pub journal: JournalUiState,
    pub normal_paste_burst: PasteBurst,
    #[allow(dead_code)]
    pub rss: RssUiState,
}

#[derive(Default)]
pub struct ConfigUiState {
    pub settings_edit: Box<Settings>,
    pub selected_index: usize,
    pub items: Vec<ConfigItem>,
    pub editing: Option<(ConfigItem, String)>,
}

#[derive(Default)]
pub struct DeleteConfirmUiState {
    pub info_hash: Vec<u8>,
    pub with_files: bool,
}

#[derive(Default)]
pub struct FileBrowserUiState {
    pub state: TreeViewState,
    pub data: Vec<RawNode<FileMetadata>>,
    pub browser_mode: FileBrowserMode,
    pub is_searching: bool,
    pub search_query: String,
}

pub fn build_torrent_preview_tree(
    file_list: Vec<(Vec<String>, u64)>,
    file_priorities: &HashMap<usize, FilePriority>,
) -> Vec<RawNode<TorrentPreviewPayload>> {
    let entries = file_list
        .into_iter()
        .enumerate()
        .map(|(idx, (parts, size))| TorrentPreviewFileEntry {
            parts,
            file_index: idx,
            size,
        })
        .collect();

    build_torrent_preview_tree_from_entries(entries, file_priorities)
}

fn build_torrent_preview_tree_from_entries(
    file_entries: Vec<TorrentPreviewFileEntry>,
    file_priorities: &HashMap<usize, FilePriority>,
) -> Vec<RawNode<TorrentPreviewPayload>> {
    let file_count = file_entries.len();
    let preview_payloads: Vec<(Vec<String>, TorrentPreviewPayload)> = file_entries
        .into_iter()
        .map(|entry| {
            (
                entry.parts,
                TorrentPreviewPayload {
                    file_index: Some(entry.file_index),
                    size: entry.size,
                    priority: file_priorities
                        .get(&entry.file_index)
                        .copied()
                        .unwrap_or(FilePriority::Normal),
                },
            )
        })
        .collect();

    let tree = RawNode::from_path_list(None, preview_payloads);
    tracing::info!(
        target: "superseedr",
        file_count,
        tree_roots = tree.len(),
        "Built torrent preview tree"
    );
    tree
}

fn collect_torrent_preview_files(
    node: &RawNode<TorrentPreviewPayload>,
    path: &mut Vec<String>,
    files: &mut Vec<TorrentPreviewFileEntry>,
) {
    path.push(node.name.clone());
    if node.is_dir {
        for child in &node.children {
            collect_torrent_preview_files(child, path, files);
        }
    } else if let Some(file_index) = node.payload.file_index {
        files.push(TorrentPreviewFileEntry {
            parts: path.clone(),
            file_index,
            size: node.payload.size,
        });
    }
    path.pop();
}

fn rebuild_torrent_preview_tree(
    existing_tree: &[RawNode<TorrentPreviewPayload>],
    file_priorities: &HashMap<usize, FilePriority>,
) -> Vec<RawNode<TorrentPreviewPayload>> {
    let mut files = Vec::new();
    let mut path = Vec::new();
    for node in existing_tree {
        collect_torrent_preview_files(node, &mut path, &mut files);
    }
    build_torrent_preview_tree_from_entries(files, file_priorities)
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalFilter {
    #[default]
    All,
    Queue,
    Commands,
    Health,
}

impl JournalFilter {
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Queue,
            Self::Queue => Self::Commands,
            Self::Commands => Self::Health,
            Self::Health => Self::All,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::All => Self::Health,
            Self::Queue => Self::All,
            Self::Commands => Self::Queue,
            Self::Health => Self::Commands,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "ALL",
            Self::Queue => "QUEUE",
            Self::Commands => "COMMANDS",
            Self::Health => "HEALTH",
        }
    }
}

#[derive(Default)]
pub struct JournalUiState {
    pub filter: JournalFilter,
    pub selected_index: usize,
    pub status_message: Option<String>,
}

#[derive(Default)]
#[allow(dead_code)]
pub struct RssUiState {
    pub active_screen: RssScreen,
    pub focused_section: RssSectionFocus,
    pub selected_feed_index: usize,
    pub selected_filter_index: usize,
    pub selected_explorer_index: usize,
    pub selected_history_index: usize,
    pub is_searching: bool,
    pub search_query: String,
    pub is_editing: bool,
    pub edit_buffer: String,
    pub filter_draft: String,
    pub add_feed_buffer: String,
    pub add_filter_buffer: String,
    pub add_filter_mode: RssFilterMode,
    pub delete_confirm_armed: bool,
    pub status_message: Option<String>,
    pub last_sync_request_at: Option<Instant>,
}

#[derive(Default, Clone)]
pub struct RssRuntimeState {
    pub history: Vec<RssHistoryEntry>,
    pub preview_items: Vec<RssPreviewItem>,
    pub last_sync_at: Option<String>,
    pub next_sync_at: Option<String>,
    pub feed_errors: HashMap<String, FeedSyncError>,
}

#[derive(Default, Clone)]
pub struct RssFilterRuntimeStat {
    pub downloaded_matches: usize,
    pub history_age: String,
}

#[derive(Default, Clone)]
pub struct RssDerivedState {
    pub explorer_items: Vec<RssPreviewItem>,
    pub explorer_combined_match: Vec<bool>,
    pub explorer_prioritise_matches: bool,
    pub history_hash_by_dedupe: HashMap<String, Vec<u8>>,
    pub filter_runtime_stats: HashMap<usize, RssFilterRuntimeStat>,
}

#[derive(Default, Clone)]
#[allow(dead_code)]
pub struct RssPreviewItem {
    pub dedupe_key: String,
    pub title: String,
    pub link: Option<String>,
    pub guid: Option<String>,
    pub source: Option<String>,
    pub date_iso: Option<String>,
    pub is_match: bool,
    pub is_downloaded: bool,
}

#[derive(Default)]
pub struct AppState {
    pub update_available: Option<String>,
    pub should_quit: bool,
    pub shutdown_progress: f64,
    pub system_warning: Option<String>,
    pub system_error: Option<String>,
    pub limits: CalculatedLimits,

    pub screen_area: Rect,
    pub mode: AppMode,
    pub externally_accessable_port_v4: bool,
    pub externally_accessable_port_v6: bool,
    pub externally_accessable_port_v4_highlight_until: Option<Instant>,
    pub externally_accessable_port_v6_highlight_until: Option<Instant>,
    pub anonymize_torrent_names: bool,

    pub pending_torrent_path: Option<PathBuf>,
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

    pub ui: UiState,
    pub rss_runtime: RssRuntimeState,
    pub rss_derived: RssDerivedState,
    pub data_rate: DataRate,
    pub theme: Theme,

    pub torrent_sort: (TorrentSortColumn, SortDirection),
    pub peer_sort: (PeerSortColumn, SortDirection),

    pub chart_panel_view: ChartPanelView,
    pub graph_mode: GraphDisplayMode,
    pub minute_avg_dl_history: Vec<u64>,
    pub minute_avg_ul_history: Vec<u64>,
    pub network_history_state: NetworkHistoryPersistedState,
    pub network_history_rollups: NetworkHistoryRollupState,
    pub network_history_dirty: bool,
    pub network_history_restore_pending: bool,
    pub next_network_history_persist_request_id: u64,
    pub pending_network_history_persist_request_id: Option<u64>,
    pub activity_history_state: ActivityHistoryPersistedState,
    pub activity_history_rollups: ActivityHistoryRollupState,
    pub activity_history_dirty: bool,
    pub activity_history_restore_pending: bool,
    pub next_activity_history_persist_request_id: u64,
    pub pending_activity_history_persist_request_id: Option<u64>,
    pub event_journal_state: EventJournalState,

    pub last_tuning_score: u64,
    pub current_tuning_score: u64,
    pub tuning_countdown: u64,
    pub last_tuning_limits: CalculatedLimits,
    pub is_seeding: bool,
    pub baseline_speed_ema: f64,
    pub global_disk_thrash_score: f64,
    pub adaptive_max_scpb: f64,
    pub global_seek_cost_per_byte_history: Vec<f64>,
    pub disk_health_ema: f64,
    pub disk_health_phase: f64,
    pub disk_health_peak_hold: f64,
    pub disk_health_state_level: u8,

    pub recently_processed_files: HashMap<PathBuf, Instant>,
    pub pending_ingest_by_path: HashMap<PathBuf, PendingIngestRecord>,
    pub pending_control_by_path: HashMap<PathBuf, PendingControlRecord>,
    pub pending_watch_commands: VecDeque<AppCommand>,
    pub cluster_role_label: Option<String>,
    pub cluster_runtime_label: Option<String>,
}

pub struct App {
    pub app_state: AppState,
    pub client_configs: Settings,
    pub runtime_mode: AppRuntimeMode,
    pub shared_mode_enabled: bool,
    pub current_cluster_role: Option<AppClusterRole>,
    pub watched_paths: Vec<PathBuf>,
    pub base_system_warning: Option<String>,

    pub listener: Option<ListenerSet>,

    pub torrent_manager_incoming_peer_txs: HashMap<Vec<u8>, Sender<IncomingPeerSession>>,
    pub torrent_manager_command_txs: HashMap<Vec<u8>, Sender<ManagerCommand>>,
    pub global_peer_manager: GlobalPeerManager,
    pub dht_service: DhtService,
    pub dht_status_rx: watch::Receiver<DhtStatus>,
    pub resource_manager: ResourceManagerClient,
    pub global_dl_bucket: Arc<TokenBucket>,
    pub global_ul_bucket: Arc<TokenBucket>,

    pub torrent_metric_watch_rxs: HashMap<Vec<u8>, watch::Receiver<TorrentMetrics>>,
    pub manager_event_tx: mpsc::Sender<ManagerEvent>,
    pub manager_event_rx: mpsc::Receiver<ManagerEvent>,
    pub app_command_tx: mpsc::Sender<AppCommand>,
    pub app_command_rx: mpsc::Receiver<AppCommand>,
    pub rss_sync_tx: mpsc::Sender<()>,
    pub rss_downloaded_entry_tx: mpsc::Sender<RssHistoryEntry>,
    pub rss_settings_tx: watch::Sender<Settings>,
    pub tui_event_tx: mpsc::Sender<CrosstermEvent>,
    pub tui_event_rx: mpsc::Receiver<CrosstermEvent>,
    pub shutdown_tx: broadcast::Sender<()>,
    pub persistence_tx: Option<watch::Sender<Option<PersistPayload>>>,
    pub persistence_task: Option<tokio::task::JoinHandle<()>>,
    pub rss_sync_rx: Option<mpsc::Receiver<()>>,
    pub rss_downloaded_entry_rx: Option<mpsc::Receiver<RssHistoryEntry>>,
    pub rss_settings_rx: Option<watch::Receiver<Settings>>,
    pub rss_service_task: Option<tokio::task::JoinHandle<()>>,
    pub tui_task: Option<tokio::task::JoinHandle<()>>,
    pub notify_rx: mpsc::Receiver<Result<Event, NotifyError>>,
    pub watcher: RecommendedWatcher,
    pub tuning_controller: TuningController,
    pub next_tuning_at: time::Instant,
    pub integrity_scheduler: IntegrityScheduler,
    pub event_journal_host_id: Option<String>,
    pub status_dump_interval_override_secs: Option<u64>,
    pub next_status_dump_at: Option<time::Instant>,
    pub status_dump_generation: Arc<AtomicU64>,
    pub app_lock_handle: Option<File>,
    pub leader_status_snapshot: Option<AppOutputState>,
    pub startup_completion_suppressed_hashes: HashSet<Vec<u8>>,
    pub startup_deferred_load_queue: VecDeque<Vec<u8>>,
    pub next_startup_load_at: Option<time::Instant>,
}

#[derive(Clone)]
pub struct NetworkHistoryPersistRequest {
    pub request_id: u64,
    pub state: NetworkHistoryPersistedState,
}

#[derive(Clone)]
pub struct ActivityHistoryPersistRequest {
    pub request_id: u64,
    pub state: ActivityHistoryPersistedState,
}

#[derive(Clone)]
pub struct PersistPayload {
    pub settings: Settings,
    pub rss_state: RssPersistedState,
    pub network_history: Option<NetworkHistoryPersistRequest>,
    pub activity_history: Option<ActivityHistoryPersistRequest>,
    pub event_journal_state: EventJournalState,
}

fn initial_cluster_role_for_runtime_mode(runtime_mode: AppRuntimeMode) -> Option<AppClusterRole> {
    match runtime_mode {
        AppRuntimeMode::Normal => None,
        AppRuntimeMode::SharedLeader => Some(AppClusterRole::Leader),
        AppRuntimeMode::SharedFollower => Some(AppClusterRole::Follower),
    }
}

fn spawn_persistence_writer(
    app_command_tx: mpsc::Sender<AppCommand>,
) -> (
    watch::Sender<Option<PersistPayload>>,
    tokio::task::JoinHandle<()>,
) {
    let (persistence_tx, mut persistence_rx) = watch::channel::<Option<PersistPayload>>(None);
    let persistence_app_command_tx = app_command_tx.clone();
    let persistence_task = tokio::spawn(async move {
        while persistence_rx.changed().await.is_ok() {
            let Some(payload) = persistence_rx.borrow().clone() else {
                continue;
            };
            let network_history_request_id = payload
                .network_history
                .as_ref()
                .map(|request| request.request_id);
            let activity_history_request_id = payload
                .activity_history
                .as_ref()
                .map(|request| request.request_id);
            let write_result = tokio::task::spawn_blocking(move || {
                save_settings(&payload.settings)
                    .map_err(|e| format!("Failed to auto-save settings: {}", e))?;
                save_rss_state(&payload.rss_state)
                    .map_err(|e| format!("Failed to auto-save RSS state: {}", e))?;
                if let Some(network_history) = payload.network_history {
                    save_network_history_state(&network_history.state)
                        .map_err(|e| format!("Failed to auto-save network history state: {}", e))?;
                }
                if let Some(activity_history) = payload.activity_history {
                    save_activity_history_state(&activity_history.state).map_err(|e| {
                        format!("Failed to auto-save activity history state: {}", e)
                    })?;
                }
                save_event_journal_state(&payload.event_journal_state)
                    .map_err(|e| format!("Failed to auto-save event journal state: {}", e))?;
                Ok::<(), String>(())
            })
            .await;

            match write_result {
                Ok(Ok(())) => {
                    tracing_event!(Level::DEBUG, "Persistence payload auto-saved successfully.");
                    if let Some(request_id) = network_history_request_id {
                        let _ = persistence_app_command_tx
                            .send(AppCommand::NetworkHistoryPersisted {
                                request_id,
                                success: true,
                            })
                            .await;
                    }
                    if let Some(request_id) = activity_history_request_id {
                        let _ = persistence_app_command_tx
                            .send(AppCommand::ActivityHistoryPersisted {
                                request_id,
                                success: true,
                            })
                            .await;
                    }
                }
                Ok(Err(e)) => {
                    tracing_event!(Level::ERROR, "{}", e);
                    if let Some(request_id) = network_history_request_id {
                        let _ = persistence_app_command_tx
                            .send(AppCommand::NetworkHistoryPersisted {
                                request_id,
                                success: false,
                            })
                            .await;
                    }
                    if let Some(request_id) = activity_history_request_id {
                        let _ = persistence_app_command_tx
                            .send(AppCommand::ActivityHistoryPersisted {
                                request_id,
                                success: false,
                            })
                            .await;
                    }
                }
                Err(e) => {
                    tracing_event!(Level::ERROR, "Persistence writer join failed: {}", e);
                    if let Some(request_id) = network_history_request_id {
                        let _ = persistence_app_command_tx
                            .send(AppCommand::NetworkHistoryPersisted {
                                request_id,
                                success: false,
                            })
                            .await;
                    }
                    if let Some(request_id) = activity_history_request_id {
                        let _ = persistence_app_command_tx
                            .send(AppCommand::ActivityHistoryPersisted {
                                request_id,
                                success: false,
                            })
                            .await;
                    }
                }
            }
        }
    });

    (persistence_tx, persistence_task)
}

fn build_app_dht_service_config(client_configs: &Settings) -> DhtServiceConfig {
    let config = DhtServiceConfig::from_settings(client_configs);
    #[cfg(test)]
    {
        let mut config = config;
        if client_configs.client_port == 0 {
            config.preferred_backend = crate::dht::service::DhtBackendKind::Disabled;
        }
        config
    }
    #[cfg(not(test))]
    {
        config
    }
}

impl App {
    #[cfg(test)]
    pub async fn new(
        client_configs: Settings,
        runtime_mode: AppRuntimeMode,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_with_lock(client_configs, runtime_mode, None).await
    }

    pub async fn new_with_lock(
        mut client_configs: Settings,
        runtime_mode: AppRuntimeMode,
        app_lock_handle: Option<File>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let listener = Some(ListenerSet::bind(client_configs.client_port).await?);
        if client_configs.client_port == 0 {
            if let Some(bound_port) = listener.as_ref().and_then(ListenerSet::local_port) {
                client_configs.client_port = bound_port;
            }
        }

        let (manager_event_tx, manager_event_rx) = mpsc::channel::<ManagerEvent>(1000);
        let (app_command_tx, app_command_rx) = mpsc::channel::<AppCommand>(10);
        let (rss_sync_tx, rss_sync_rx) = mpsc::channel::<()>(8);
        let (rss_downloaded_entry_tx, rss_downloaded_entry_rx) =
            mpsc::channel::<RssHistoryEntry>(64);
        let (rss_settings_tx, rss_settings_rx) = watch::channel(client_configs.clone());
        let (tui_event_tx, tui_event_rx) = mpsc::channel::<CrosstermEvent>(100);
        let (shutdown_tx, _) = broadcast::channel(1);
        let shared_mode_enabled = runtime_mode.is_shared();
        let current_cluster_role = initial_cluster_role_for_runtime_mode(runtime_mode);
        let (persistence_tx, persistence_task) = if shared_mode_enabled
            && matches!(current_cluster_role, Some(AppClusterRole::Follower))
        {
            (None, None)
        } else {
            let (persistence_tx, persistence_task) =
                spawn_persistence_writer(app_command_tx.clone());
            (Some(persistence_tx), Some(persistence_task))
        };

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
            ResourceType::PeerHandshake,
            (
                peer_handshake_limit(limits.max_connected_peers),
                peer_handshake_limit(limits.max_connected_peers) * 2,
            ),
        );
        rm_limits.insert(
            ResourceType::PeerConnection,
            (limits.max_connected_peers, limits.max_connected_peers * 2),
        );
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

        let dht_service = DhtService::new(
            build_app_dht_service_config(&client_configs),
            shutdown_tx.subscribe(),
        )
        .await?;
        let dht_status_rx = dht_service.subscribe_status();

        let dl_limit = client_configs.global_download_limit_bps as f64;
        let ul_limit = client_configs.global_upload_limit_bps as f64;
        let global_dl_bucket = Arc::new(TokenBucket::new(dl_limit, dl_limit));
        let global_ul_bucket = Arc::new(TokenBucket::new(ul_limit, ul_limit));
        let _ = crate::config::ensure_watch_directories(&client_configs);
        let persisted_rss_state = load_rss_state();
        let persisted_event_journal_state = load_event_journal_state();

        let tuning_controller = TuningController::new_adaptive(limits.clone());
        let tuning_state = tuning_controller.state().clone();
        let app_state = AppState {
            system_warning: None,
            system_error: None,
            limits: limits.clone(),
            ui: UiState {
                needs_redraw: true,
                ..Default::default()
            },
            theme: Theme::builtin(client_configs.ui_theme),
            torrent_sort: (
                client_configs.torrent_sort_column,
                client_configs.torrent_sort_direction,
            ),
            peer_sort: (
                client_configs.peer_sort_column,
                client_configs.peer_sort_direction,
            ),
            rss_runtime: RssRuntimeState {
                history: persisted_rss_state.history,
                preview_items: Vec::new(),
                last_sync_at: persisted_rss_state.last_sync_at,
                next_sync_at: None,
                feed_errors: persisted_rss_state.feed_errors,
            },
            event_journal_state: persisted_event_journal_state,
            lifetime_downloaded_from_config: client_configs.lifetime_downloaded,
            lifetime_uploaded_from_config: client_configs.lifetime_uploaded,
            minute_disk_backoff_history_ms: VecDeque::with_capacity(24 * 60),
            max_disk_backoff_this_tick_ms: 0,
            last_tuning_score: tuning_state.last_tuning_score,
            current_tuning_score: tuning_state.current_tuning_score,
            tuning_countdown: tuning_controller.cadence_secs(),
            last_tuning_limits: tuning_state.last_tuning_limits,
            baseline_speed_ema: tuning_state.baseline_speed_ema,
            adaptive_max_scpb: 10.0,
            ..Default::default()
        };

        let watched_paths = runtime_watch_paths(
            &client_configs,
            shared_mode_enabled,
            matches!(current_cluster_role, Some(AppClusterRole::Leader)) || !shared_mode_enabled,
        );

        let (notify_tx, notify_rx) = mpsc::channel::<Result<Event, NotifyError>>(100);
        let watcher = watcher::create_watcher(&watched_paths, true, notify_tx)?;
        let initial_tuning_deadline =
            time::Instant::now() + Duration::from_secs(tuning_controller.cadence_secs());

        let mut app = Self {
            app_state,
            client_configs: client_configs.clone(),
            runtime_mode,
            shared_mode_enabled,
            current_cluster_role,
            watched_paths,
            base_system_warning: system_warning,
            listener,
            torrent_manager_incoming_peer_txs: HashMap::new(),
            torrent_manager_command_txs: HashMap::new(),
            global_peer_manager: GlobalPeerManager::with_session_limit(limits.max_connected_peers),
            dht_service,
            dht_status_rx,
            resource_manager: resource_manager_client,
            global_dl_bucket,
            global_ul_bucket,
            torrent_metric_watch_rxs: HashMap::new(),
            manager_event_tx,
            manager_event_rx,
            app_command_tx,
            app_command_rx,
            rss_sync_tx,
            rss_downloaded_entry_tx,
            rss_settings_tx,
            tui_event_tx,
            tui_event_rx,
            shutdown_tx,
            persistence_tx,
            persistence_task,
            rss_sync_rx: Some(rss_sync_rx),
            rss_downloaded_entry_rx: Some(rss_downloaded_entry_rx),
            rss_settings_rx: Some(rss_settings_rx),
            rss_service_task: None,
            tui_task: None,
            watcher,
            notify_rx,
            tuning_controller,
            next_tuning_at: initial_tuning_deadline,
            integrity_scheduler: IntegrityScheduler::new(Instant::now()),
            event_journal_host_id: shared_host_id(),
            status_dump_interval_override_secs: None,
            next_status_dump_at: None,
            status_dump_generation: Arc::new(AtomicU64::new(0)),
            app_lock_handle,
            leader_status_snapshot: None,
            startup_completion_suppressed_hashes: HashSet::new(),
            startup_deferred_load_queue: VecDeque::new(),
            next_startup_load_at: None,
        };
        app.sync_cluster_role_label();
        app.refresh_system_warning();

        app.ensure_leader_services_running();

        let mut torrents_to_load = app.client_configs.torrents.clone();
        torrents_to_load.sort_by_key(|t| !t.validation_status);
        let mut startup_running_torrents_started = 0usize;
        for torrent_config in torrents_to_load {
            let should_defer_running_torrent = matches!(
                torrent_config.torrent_control_state,
                TorrentControlState::Running
            ) && startup_running_torrents_started
                >= STARTUP_ROLLING_BATCH_SIZE
                && !app.should_suppress_follower_runtime_for_torrent(&torrent_config);

            if should_defer_running_torrent {
                if let Some(info_hash) =
                    info_hash_from_torrent_source(&torrent_config.torrent_or_magnet)
                {
                    app.startup_deferred_load_queue.push_back(info_hash);
                } else {
                    tracing_event!(
                        Level::WARN,
                        torrent = %torrent_config.torrent_or_magnet,
                        "Could not derive info hash for deferred startup torrent; restoring immediately"
                    );
                    app.load_runtime_torrent_from_settings(torrent_config).await;
                    startup_running_torrents_started =
                        startup_running_torrents_started.saturating_add(1);
                }
            } else {
                if matches!(
                    torrent_config.torrent_control_state,
                    TorrentControlState::Running
                ) {
                    startup_running_torrents_started =
                        startup_running_torrents_started.saturating_add(1);
                }
                app.load_runtime_torrent_from_settings(torrent_config).await;
            }
        }
        app.reschedule_startup_load_deadline();

        if app.app_state.torrents.is_empty() && app.app_state.lifetime_downloaded_from_config == 0 {
            app.app_state.mode = AppMode::Welcome;
        }

        let is_leeching = app.app_state.torrents.values().any(|t| {
            t.latest_state.number_of_pieces_completed < t.latest_state.number_of_pieces_total
        });
        app.app_state.is_seeding = !is_leeching;
        app.refresh_rss_derived();
        app.refresh_follower_read_model();

        Ok(app)
    }

    fn cluster_role_label_for_state(&self) -> Option<&'static str> {
        if !self.is_shared_mode_enabled() {
            return None;
        }

        if self.is_current_shared_leader() {
            Some("Leader")
        } else if self.is_current_shared_follower() {
            Some("Follower")
        } else {
            Some("Unknown")
        }
    }

    fn sync_cluster_role_label(&mut self) {
        self.app_state.cluster_role_label = self.cluster_role_label_for_state().map(str::to_string);
        self.app_state.cluster_runtime_label = if self.is_current_shared_follower() {
            Some("Reader".to_string())
        } else {
            None
        };
    }

    fn should_suppress_follower_runtime_for_torrent(&self, torrent: &TorrentSettings) -> bool {
        self.is_current_shared_follower() && !torrent.validation_status
    }

    fn display_state_from_torrent_settings(
        &self,
        torrent: &TorrentSettings,
    ) -> Option<TorrentDisplayState> {
        let info_hash = info_hash_from_torrent_source(&torrent.torrent_or_magnet)?;
        Some(TorrentDisplayState {
            latest_state: TorrentMetrics {
                torrent_control_state: torrent.torrent_control_state.clone(),
                delete_files: torrent.delete_files,
                info_hash,
                torrent_or_magnet: torrent.torrent_or_magnet.clone(),
                torrent_name: torrent.name.clone(),
                download_path: torrent
                    .download_path
                    .clone()
                    .or_else(|| self.client_configs.default_download_folder.clone()),
                container_name: torrent.container_name.clone(),
                file_priorities: torrent.file_priorities.clone(),
                is_complete: torrent.validation_status,
                activity_message: "Reader mode waiting for leader status".to_string(),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    fn ensure_display_only_torrent_from_settings(&mut self, torrent: &TorrentSettings) {
        let Some(display_state) = self.display_state_from_torrent_settings(torrent) else {
            return;
        };
        let info_hash = display_state.latest_state.info_hash.clone();
        if !self.app_state.torrents.contains_key(&info_hash) {
            self.app_state
                .torrents
                .insert(info_hash.clone(), display_state);
            self.app_state.torrent_list_order.push(info_hash);
            self.refresh_rss_derived();
        }
    }

    fn apply_leader_snapshot_to_display(&mut self, snapshot: &AppOutputState) {
        let configured_torrents = self.client_configs.torrents.clone();
        for torrent in &configured_torrents {
            let Some(info_hash) = info_hash_from_torrent_source(&torrent.torrent_or_magnet) else {
                continue;
            };

            if !self.app_state.torrents.contains_key(&info_hash) {
                self.ensure_display_only_torrent_from_settings(torrent);
            }

            let has_live_runtime = self.has_live_runtime_for_torrent(&info_hash);
            let Some(runtime) = self.app_state.torrents.get_mut(&info_hash) else {
                continue;
            };
            let Some(leader_metrics) = snapshot.torrents.get(&info_hash) else {
                if !has_live_runtime {
                    runtime.latest_state.activity_message =
                        "Leader runtime unavailable".to_string();
                    runtime.latest_state.download_speed_bps = 0;
                    runtime.latest_state.upload_speed_bps = 0;
                    runtime.latest_state.bytes_downloaded_this_tick = 0;
                    runtime.latest_state.bytes_uploaded_this_tick = 0;
                }
                continue;
            };

            let keep_local_seed_runtime = has_live_runtime && runtime.latest_state.is_complete;
            if !keep_local_seed_runtime {
                runtime.latest_state = leader_metrics.clone();
            }
        }

        self.sort_and_filter_torrent_list();
        self.app_state.ui.needs_redraw = true;
    }

    fn refresh_follower_read_model(&mut self) {
        if !self.is_current_shared_follower() {
            return;
        }

        for torrent in self.client_configs.torrents.clone() {
            if self.should_suppress_follower_runtime_for_torrent(&torrent) {
                self.ensure_display_only_torrent_from_settings(&torrent);
            }
        }

        match status::read_cluster_output_state() {
            Ok(snapshot) => {
                self.leader_status_snapshot = Some(snapshot.clone());
                self.apply_leader_snapshot_to_display(&snapshot);
            }
            Err(error) => {
                tracing_event!(
                    Level::DEBUG,
                    "Follower could not read leader status snapshot yet: {}",
                    error
                );
                self.leader_status_snapshot = None;
            }
        }
    }

    async fn start_missing_runtime_torrents_for_current_role(&mut self) {
        for torrent in self.client_configs.torrents.clone() {
            let Some(info_hash) = info_hash_from_torrent_source(&torrent.torrent_or_magnet) else {
                continue;
            };
            if self.has_live_runtime_for_torrent(&info_hash) {
                continue;
            }
            if self.should_suppress_follower_runtime_for_torrent(&torrent) {
                self.ensure_display_only_torrent_from_settings(&torrent);
                continue;
            }
            self.load_runtime_torrent_from_settings(torrent).await;
        }
    }

    pub fn is_shared_mode_enabled(&self) -> bool {
        self.shared_mode_enabled
    }

    pub fn is_current_shared_leader(&self) -> bool {
        matches!(self.current_cluster_role, Some(AppClusterRole::Leader))
    }

    pub fn is_current_shared_follower(&self) -> bool {
        self.is_shared_mode_enabled()
            && matches!(self.current_cluster_role, Some(AppClusterRole::Follower))
    }

    fn cluster_capabilities(&self) -> ClusterCapabilities {
        let is_shared_follower = self.is_current_shared_follower();
        ClusterCapabilities {
            can_write_shared_state: !is_shared_follower,
            can_queue_shared_commands: self.is_shared_mode_enabled(),
            can_edit_host_local_config: !self.is_shared_mode_enabled() || is_shared_follower,
            can_persist_local_runtime_state: !is_shared_follower,
            can_consume_shared_inbox: !self.is_shared_mode_enabled()
                || self.is_current_shared_leader(),
        }
    }

    fn can_run_leader_services(&self) -> bool {
        self.cluster_capabilities().can_consume_shared_inbox
    }

    fn can_write_shared_state(&self) -> bool {
        self.cluster_capabilities().can_write_shared_state
    }

    fn ensure_leader_services_running(&mut self) {
        if !self.can_run_leader_services() {
            return;
        }

        if self.persistence_tx.is_none() {
            let (tx, task) = spawn_persistence_writer(self.app_command_tx.clone());
            self.persistence_tx = Some(tx);
            self.persistence_task = Some(task);
        }

        if self.rss_service_task.is_none() {
            let Some(sync_now_rx) = self.rss_sync_rx.take() else {
                return;
            };
            let Some(downloaded_entry_rx) = self.rss_downloaded_entry_rx.take() else {
                return;
            };
            let Some(settings_rx) = self.rss_settings_rx.take() else {
                return;
            };
            self.rss_service_task = Some(rss_service::spawn_rss_service(
                self.client_configs.clone(),
                self.app_state.rss_runtime.history.clone(),
                self.app_command_tx.clone(),
                sync_now_rx,
                downloaded_entry_rx,
                settings_rx,
                self.shutdown_tx.clone(),
            ));
        }
    }

    fn current_shared_lock_path() -> io::Result<PathBuf> {
        shared_root_path()
            .map(|root| root.join("superseedr.lock"))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Shared lock path unavailable"))
    }

    fn try_acquire_shared_runtime_lock() -> io::Result<Option<File>> {
        let lock_path = Self::current_shared_lock_path()?;
        let file = File::create(lock_path)?;
        if file.try_lock().is_ok() {
            Ok(Some(file))
        } else {
            Ok(None)
        }
    }

    fn watch_path_if_needed(&mut self, path: PathBuf) -> io::Result<()> {
        if self.watched_paths.iter().any(|existing| existing == &path) {
            return Ok(());
        }

        self.watcher
            .watch(&path, RecursiveMode::NonRecursive)
            .map_err(io::Error::other)?;
        self.watched_paths.push(path);
        Ok(())
    }

    fn desired_watch_paths_for_settings(&self, settings: &Settings) -> Vec<PathBuf> {
        runtime_watch_paths(
            settings,
            self.shared_mode_enabled,
            self.cluster_capabilities().can_consume_shared_inbox,
        )
    }

    fn reconcile_watched_paths(&mut self, settings: &Settings) {
        let desired_paths = self.desired_watch_paths_for_settings(settings);
        let existing_paths = self.watched_paths.clone();

        for existing in existing_paths {
            if desired_paths.iter().any(|desired| desired == &existing) {
                continue;
            }

            if let Err(error) = self.watcher.unwatch(&existing) {
                tracing_event!(
                    Level::WARN,
                    "Failed to stop watching path {:?}: {}",
                    existing,
                    error
                );
            }
            self.watched_paths.retain(|path| path != &existing);
        }

        for desired in desired_paths {
            if let Err(error) = self.watch_path_if_needed(desired) {
                tracing_event!(
                    Level::WARN,
                    "Failed to watch updated path after config change: {}",
                    error
                );
            }
        }
    }

    fn control_priority_overrides(
        file_priorities: &HashMap<usize, FilePriority>,
    ) -> Vec<ControlFilePriorityOverride> {
        let mut overrides: Vec<_> = file_priorities
            .iter()
            .map(|(file_index, priority)| ControlFilePriorityOverride {
                file_index: *file_index,
                priority: *priority,
            })
            .collect();
        overrides.sort_by_key(|entry| entry.file_index);
        overrides
    }

    fn shared_add_staging_dir() -> Result<PathBuf, String> {
        shared_root_path()
            .map(|root| root.join("staged-adds"))
            .ok_or_else(|| "Shared add staging directory is unavailable".to_string())
    }

    fn is_shared_staged_add_path(path: &Path) -> bool {
        Self::shared_add_staging_dir()
            .map(|dir| path.starts_with(&dir))
            .unwrap_or(false)
    }

    fn cleanup_staged_add_file(path: &Path) {
        if !Self::is_shared_staged_add_path(path) {
            return;
        }

        if let Err(error) = fs::remove_file(path) {
            if error.kind() != ErrorKind::NotFound {
                tracing_event!(
                    Level::WARN,
                    "Failed to remove staged add file {:?}: {}",
                    path,
                    error
                );
            }
        }
    }

    pub(crate) fn prepare_add_torrent_file_request(
        &self,
        source_path: PathBuf,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        file_priorities: HashMap<usize, FilePriority>,
    ) -> Result<ControlRequest, String> {
        let request_source_path = if self.is_current_shared_follower() {
            let staging_dir = Self::shared_add_staging_dir()?;
            fs::create_dir_all(&staging_dir)
                .map_err(|error| format!("Failed to create shared staging directory: {}", error))?;
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let hash = hex::encode(sha1::Sha1::digest(
                format!(
                    "{}:{}:{}",
                    source_path.display(),
                    std::process::id(),
                    now_ms
                )
                .as_bytes(),
            ));
            let staged_path =
                staging_dir.join(format!("staged-{}-{}.torrent", now_ms, &hash[..12]));
            fs::copy(&source_path, &staged_path).map_err(|error| {
                format_filesystem_path_error(
                    "Failed to stage torrent file for leader processing",
                    &source_path,
                    &error,
                )
            })?;
            staged_path
        } else {
            source_path
        };

        Ok(ControlRequest::AddTorrentFile {
            source_path: request_source_path,
            download_path,
            container_name,
            file_priorities: Self::control_priority_overrides(&file_priorities),
        })
    }

    pub(crate) fn prepare_add_magnet_request(
        &self,
        magnet_link: String,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        file_priorities: HashMap<usize, FilePriority>,
    ) -> ControlRequest {
        ControlRequest::AddMagnet {
            magnet_link,
            download_path,
            container_name,
            file_priorities: Self::control_priority_overrides(&file_priorities),
        }
    }

    fn resolve_add_payload(
        &self,
        source: IngestSource,
        path: &Path,
    ) -> Result<ResolvedAddPayload, String> {
        match source {
            IngestSource::TorrentFile => Ok(ResolvedAddPayload::TorrentFile {
                source_path: path.to_path_buf(),
            }),
            IngestSource::TorrentPathFile => {
                let payload = fs::read_to_string(path).map_err(|error| {
                    format_filesystem_path_error("Failed to read torrent path file", path, &error)
                })?;
                let source_path =
                    crate::config::resolve_shared_cli_torrent_path(Path::new(payload.trim()))
                        .map_err(|error| {
                            format!(
                                "Failed to resolve shared torrent path from file {:?}: {}",
                                path, error
                            )
                        })?;
                Ok(ResolvedAddPayload::TorrentFile { source_path })
            }
            IngestSource::MagnetFile => {
                let payload = fs::read_to_string(path)
                    .map_err(|error| format!("Failed to read magnet file {:?}: {}", path, error))?;
                Ok(ResolvedAddPayload::MagnetLink {
                    magnet_link: payload.trim().to_string(),
                })
            }
        }
    }

    fn control_request_for_add_payload(
        &self,
        payload: &ResolvedAddPayload,
        download_path: Option<PathBuf>,
    ) -> Result<ControlRequest, String> {
        match payload {
            ResolvedAddPayload::TorrentFile { source_path } => self
                .prepare_add_torrent_file_request(
                    source_path.clone(),
                    download_path,
                    None,
                    HashMap::new(),
                ),
            ResolvedAddPayload::MagnetLink { magnet_link } => Ok(self.prepare_add_magnet_request(
                magnet_link.clone(),
                download_path,
                None,
                HashMap::new(),
            )),
        }
    }

    fn resolve_add_ingress_action(&self, source: IngestSource, path: &Path) -> AddIngressAction {
        let is_host_watch_path = self.is_host_watch_path(path);
        let is_shared_inbox_path = self.is_shared_inbox_path(path);

        if self.is_current_shared_follower()
            && is_host_watch_path
            && !matches!(source, IngestSource::TorrentPathFile)
        {
            return AddIngressAction::RelayRawWatchFile;
        }

        let payload = match self.resolve_add_payload(source, path) {
            Ok(payload) => payload,
            Err(message) => {
                if is_shared_inbox_path && matches!(path.try_exists(), Ok(false)) {
                    return AddIngressAction::IgnoreMissingSharedInboxItem { message };
                }
                return AddIngressAction::Fail { message };
            }
        };

        if self.is_current_shared_follower()
            && !is_shared_inbox_path
            && self.client_configs.default_download_folder.is_some()
        {
            return match self.control_request_for_add_payload(
                &payload,
                self.client_configs.default_download_folder.clone(),
            ) {
                Ok(request) => AddIngressAction::QueueControlRequest(request),
                Err(message) => AddIngressAction::Fail { message },
            };
        }

        if self.is_current_shared_follower()
            && is_host_watch_path
            && matches!(source, IngestSource::TorrentPathFile)
        {
            return AddIngressAction::Fail {
                message: "Follower .path ingest requires a default download folder so the referenced torrent can be staged for leader processing.".to_string(),
            };
        }

        if let Some(download_path) = self.client_configs.default_download_folder.clone() {
            AddIngressAction::ApplyDirectly {
                payload,
                download_path,
            }
        } else {
            AddIngressAction::OpenManualBrowser { payload }
        }
    }

    fn should_archive_processed_ingest(&self, source: IngestSource, path: &Path) -> bool {
        match source {
            IngestSource::TorrentFile => {
                self.is_host_watch_path(path) || self.is_shared_inbox_path(path)
            }
            IngestSource::TorrentPathFile | IngestSource::MagnetFile => true,
        }
    }

    fn update_pending_ingest_source_path(&mut self, path: &Path, final_path: PathBuf) {
        let correlation_id = self
            .app_state
            .pending_ingest_by_path
            .get_mut(path)
            .map(|record| {
                record.source_path = final_path.clone();
                record.correlation_id.clone()
            });

        let Some(correlation_id) = correlation_id else {
            return;
        };

        for entry in self.app_state.event_journal_state.entries.iter_mut().rev() {
            if entry.category != EventCategory::Ingest {
                continue;
            }
            if entry.correlation_id.as_deref() != Some(correlation_id.as_str()) {
                continue;
            }
            entry.source_path = Some(final_path.clone());
            if entry.event_type == EventType::IngestQueued {
                break;
            }
        }
    }

    fn archive_processed_ingest(&mut self, source: IngestSource, path: &Path) -> Option<PathBuf> {
        if !self.should_archive_processed_ingest(source, path) {
            return None;
        }

        match archive_watch_file(path, source.processed_archive_extension()) {
            Ok(destination) => {
                self.update_pending_ingest_source_path(path, destination.clone());
                Some(destination)
            }
            Err(error) => {
                tracing_event!(
                    Level::WARN,
                    "Failed to archive processed ingest file {:?}: {}",
                    path,
                    error
                );
                None
            }
        }
    }

    fn open_manual_browser_for_torrent_file(&mut self, path: PathBuf) -> Result<(), String> {
        let buffer = fs::read(&path).map_err(|error| {
            format_filesystem_path_error("Failed to read torrent file", &path, &error)
        })?;
        let torrent = from_bytes(&buffer)
            .map_err(|_| "Failed to parse torrent file for preview.".to_string())?;

        let final_path = if self.is_host_watch_path(&path) || self.is_shared_inbox_path(&path) {
            match archive_watch_file(&path, "torrent.added") {
                Ok(final_path) => {
                    self.update_pending_ingest_source_path(&path, final_path.clone());
                    final_path
                }
                Err(error) => {
                    tracing::error!("Failed to archive watched file for manual add: {}", error);
                    path.clone()
                }
            }
        } else {
            path.clone()
        };

        let info_hash = if torrent.info.meta_version == Some(2) {
            let mut hasher = Sha256::new();
            hasher.update(&torrent.info_dict_bencode);
            hasher.finalize()[0..20].to_vec()
        } else {
            let mut hasher = sha1::Sha1::new();
            hasher.update(&torrent.info_dict_bencode);
            hasher.finalize().to_vec()
        };

        let info_hash_hex = hex::encode(&info_hash);
        let default_container_name = format!("{} [{}]", torrent.info.name, info_hash_hex);
        let file_list = torrent.file_list();
        let should_enclose = file_list.len() > 1;
        let preview_payloads: Vec<(Vec<String>, TorrentPreviewPayload)> = file_list
            .into_iter()
            .enumerate()
            .map(|(idx, (parts, size))| {
                (
                    parts,
                    TorrentPreviewPayload {
                        file_index: Some(idx),
                        size,
                        priority: FilePriority::Normal,
                    },
                )
            })
            .collect();

        let preview_tree = RawNode::from_path_list(None, preview_payloads);
        let mut preview_state = TreeViewState::new();
        for node in &preview_tree {
            node.expand_all(&mut preview_state);
        }

        self.app_state.pending_torrent_path = Some(final_path);
        let initial_path = self.get_initial_destination_path();

        let _ = self.app_command_tx.try_send(AppCommand::FetchFileTree {
            path: initial_path,
            browser_mode: FileBrowserMode::DownloadLocSelection {
                torrent_files: vec![],
                container_name: default_container_name.clone(),
                use_container: should_enclose,
                is_editing_name: false,
                preview_tree,
                preview_state,
                focused_pane: BrowserPane::FileSystem,
                cursor_pos: 0,
                original_name_backup: default_container_name,
            },
            highlight_path: None,
        });
        Ok(())
    }

    fn open_manual_browser_for_payload(
        &mut self,
        source: IngestSource,
        payload: ResolvedAddPayload,
    ) -> Result<(), String> {
        match payload {
            ResolvedAddPayload::TorrentFile { source_path } => {
                if matches!(source, IngestSource::TorrentFile) {
                    self.open_manual_browser_for_torrent_file(source_path)
                } else {
                    self.app_state.pending_torrent_path = Some(source_path);
                    let initial_path = self.get_initial_destination_path();
                    let _ = self.app_command_tx.try_send(AppCommand::FetchFileTree {
                        path: initial_path,
                        browser_mode: FileBrowserMode::DownloadLocSelection {
                            torrent_files: vec![],
                            container_name: "New Torrent".to_string(),
                            use_container: true,
                            is_editing_name: false,
                            preview_tree: Vec::new(),
                            preview_state: TreeViewState::default(),
                            focused_pane: BrowserPane::FileSystem,
                            cursor_pos: 0,
                            original_name_backup: "New Torrent".to_string(),
                        },
                        highlight_path: None,
                    });
                    Ok(())
                }
            }
            ResolvedAddPayload::MagnetLink { magnet_link } => {
                self.app_state.pending_torrent_link = magnet_link;
                let initial_path = self.get_initial_destination_path();
                let _ = self.app_command_tx.try_send(AppCommand::FetchFileTree {
                    path: initial_path,
                    browser_mode: FileBrowserMode::DownloadLocSelection {
                        torrent_files: vec![],
                        container_name: "Magnet Download".to_string(),
                        use_container: true,
                        is_editing_name: false,
                        preview_tree: Vec::new(),
                        preview_state: TreeViewState::default(),
                        focused_pane: BrowserPane::FileSystem,
                        cursor_pos: 0,
                        original_name_backup: "Magnet Download".to_string(),
                    },
                    highlight_path: None,
                });
                Ok(())
            }
        }
    }

    async fn execute_add_ingress_action(
        &mut self,
        source: IngestSource,
        path: PathBuf,
        action: AddIngressAction,
    ) {
        match action {
            AddIngressAction::RelayRawWatchFile => {
                self.app_state.pending_ingest_by_path.remove(&path);
                self.relay_local_watch_file(&path, source.relay_archive_extension());
                self.save_state_to_disk();
            }
            AddIngressAction::QueueControlRequest(request) => {
                let origin = self.control_origin_for_ingest_path(&path);
                if self.is_host_watch_path(&path) {
                    self.app_state.pending_ingest_by_path.remove(&path);
                }
                match self.dispatch_cluster_control_request(request, origin).await {
                    Ok(_message) => {
                        self.archive_processed_ingest(source, &path);
                    }
                    Err(error) => {
                        self.app_state.system_error = Some(error);
                        self.app_state.ui.needs_redraw = true;
                    }
                }
            }
            AddIngressAction::ApplyDirectly {
                payload,
                download_path,
            } => {
                let ingest_result = match payload {
                    ResolvedAddPayload::TorrentFile { source_path } => {
                        self.add_torrent_from_file(
                            source_path,
                            Some(download_path),
                            false,
                            TorrentControlState::Running,
                            HashMap::new(),
                            None,
                        )
                        .await
                    }
                    ResolvedAddPayload::MagnetLink { magnet_link } => {
                        self.add_magnet_torrent(
                            "Fetching name...".to_string(),
                            magnet_link,
                            Some(download_path),
                            false,
                            TorrentControlState::Running,
                            HashMap::new(),
                            None,
                        )
                        .await
                    }
                };
                if let CommandIngestResult::Added {
                    info_hash: Some(info_hash),
                    ..
                } = &ingest_result
                {
                    tracing_event!(
                        Level::INFO,
                        info_hash = %hex::encode(info_hash),
                        torrent_count = self.app_state.torrents.len(),
                        present_in_runtime = self.app_state.torrents.contains_key(info_hash),
                        "Direct ingest added torrent to runtime before persistence"
                    );
                }
                self.record_ingest_result(&path, &ingest_result);
                self.save_state_to_disk();
                self.archive_processed_ingest(source, &path);
            }
            AddIngressAction::OpenManualBrowser { payload } => {
                if let Err(message) = self.open_manual_browser_for_payload(source, payload) {
                    self.app_state.system_error = Some(message.clone());
                    self.record_ingest_result(
                        &path,
                        &CommandIngestResult::Failed {
                            info_hash: None,
                            torrent_name: None,
                            message,
                        },
                    );
                    self.save_state_to_disk();
                }
                if !matches!(source, IngestSource::TorrentFile) {
                    self.archive_processed_ingest(source, &path);
                }
            }
            AddIngressAction::IgnoreMissingSharedInboxItem { message } => {
                tracing_event!(
                    Level::INFO,
                    path = ?path,
                    "{}",
                    message
                );
                self.app_state.pending_ingest_by_path.remove(&path);
                self.save_state_to_disk();
            }
            AddIngressAction::Fail { message } => {
                tracing_event!(Level::ERROR, "{}", message);
                self.app_state.system_error = Some(message.clone());
                self.record_ingest_result(
                    &path,
                    &CommandIngestResult::Failed {
                        info_hash: None,
                        torrent_name: None,
                        message,
                    },
                );
                self.save_state_to_disk();
                self.archive_processed_ingest(source, &path);
            }
        }
    }

    fn queue_control_request_for_leader(
        &mut self,
        request: ControlRequest,
        origin: ControlOrigin,
    ) -> Result<String, String> {
        if !self.cluster_capabilities().can_queue_shared_commands {
            return Err("Shared command queue is unavailable in this mode".to_string());
        }
        let watch_path = resolve_command_watch_path(&self.client_configs)
            .ok_or_else(|| "Could not resolve the shared command inbox".to_string())?;
        let queued_path = write_control_request(&request, &watch_path)
            .map_err(|error| format!("Failed to queue shared control request: {}", error))?;
        self.record_control_queued(queued_path, request.clone(), origin);
        self.save_state_to_disk();
        Ok(format!(
            "Queued for leader processing. {}",
            online_control_success_message(&request)
        ))
    }

    pub async fn dispatch_cluster_control_request(
        &mut self,
        request: ControlRequest,
        origin: ControlOrigin,
    ) -> Result<String, String> {
        if self.is_current_shared_follower() {
            self.queue_control_request_for_leader(request, origin)
        } else {
            self.apply_control_request(&request).await
        }
    }

    fn map_add_result_to_control_response(result: CommandIngestResult) -> Result<String, String> {
        match result {
            CommandIngestResult::Added { torrent_name, .. } => Ok(format!(
                "Added torrent '{}'",
                torrent_name.unwrap_or_else(|| "unknown".to_string())
            )),
            CommandIngestResult::Duplicate { torrent_name, .. } => Ok(format!(
                "Torrent '{}' was already present",
                torrent_name.unwrap_or_else(|| "unknown".to_string())
            )),
            CommandIngestResult::Invalid { message, .. }
            | CommandIngestResult::Failed { message, .. } => Err(message),
        }
    }

    async fn maybe_promote_to_shared_leader(&mut self) {
        if !self.is_current_shared_follower() {
            return;
        }

        let Ok(Some(lock_handle)) = Self::try_acquire_shared_runtime_lock() else {
            return;
        };

        tracing_event!(
            Level::INFO,
            "Acquired shared lock; promoting node to cluster leader."
        );
        self.app_lock_handle = Some(lock_handle);
        self.current_cluster_role = Some(AppClusterRole::Leader);
        self.runtime_mode = AppRuntimeMode::SharedLeader;
        self.leader_status_snapshot = None;
        self.sync_cluster_role_label();

        if let Some(shared_inbox) = shared_inbox_path() {
            if let Err(error) = self.watch_path_if_needed(shared_inbox) {
                tracing_event!(
                    Level::WARN,
                    "Failed to watch shared inbox after promotion: {}",
                    error
                );
            }
        }

        self.ensure_leader_services_running();

        match crate::config::load_settings() {
            Ok(new_settings) => {
                if new_settings != self.client_configs {
                    self.apply_settings_update(new_settings, false).await;
                }
                self.start_missing_runtime_torrents_for_current_role().await;
            }
            Err(error) => {
                tracing_event!(
                    Level::ERROR,
                    "Failed to reload shared config after promotion: {}",
                    error
                );
                self.app_state.system_error = Some(format!(
                    "Failed to reload shared config after promotion: {}",
                    error
                ));
            }
        }

        self.process_pending_commands().await;
    }

    pub async fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Ok(size) = terminal.size() {
            self.app_state.screen_area = Rect::new(0, 0, size.width, size.height);
        }

        self.process_pending_commands().await;

        self.startup_crossterm_event_listener();
        self.startup_network_history_restore();
        self.startup_activity_history_restore();

        let mut sys = System::new();

        let mut stats_interval = time::interval(Duration::from_secs(1));
        let mut version_interval = time::interval(Duration::from_secs(24 * 60 * 60));
        let mut network_history_persist_interval =
            time::interval(Duration::from_secs(NETWORK_HISTORY_PERSIST_INTERVAL_SECS));
        let mut watch_folder_rescan_interval =
            time::interval(Duration::from_secs(WATCH_FOLDER_RESCAN_INTERVAL_SECS));
        let mut shared_role_retry_interval =
            time::interval(Duration::from_secs(SHARED_ROLE_RETRY_INTERVAL_SECS));
        let mut integrity_scheduler_interval = time::interval(INTEGRITY_SCHEDULER_TICK_INTERVAL);
        self.reschedule_tuning_deadline();
        network_history_persist_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        watch_folder_rescan_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        shared_role_retry_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        integrity_scheduler_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        self.save_state_to_disk();
        self.dump_status_to_file();
        self.reschedule_status_dump_deadline();

        let mut next_draw_time = Instant::now();
        while !self.app_state.should_quit {
            self.flush_pending_watch_commands();

            let current_target_framerate = match self.app_state.mode {
                AppMode::Welcome => Duration::from_millis(16), // Force 60 FPS for animation
                AppMode::PowerSaving => Duration::from_secs(1), // Force 1 FPS for Zen mode
                _ => Duration::from_millis(self.app_state.data_rate.as_ms()), // User-defined FPS
            };
            let next_tuning_at = self.next_tuning_at;
            let next_paste_flush_at = self.app_state.ui.normal_paste_burst.next_deadline();
            let next_status_dump_at = self.next_status_dump_at;
            let next_startup_load_at = self.next_startup_load_at;

            tokio::select! {
                _ = signal::ctrl_c() => {
                    self.app_state.should_quit = true;
                }
                Ok(Ok((stream, _addr))) = async {
                    match &self.listener {
                        Some(listener) => tokio::time::timeout(Duration::from_secs(2), listener.accept()).await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.handle_incoming_peer(stream).await;

                }
                Some(event) = self.manager_event_rx.recv() => {
                    self.handle_manager_event(event);
                    self.app_state.ui.needs_redraw = true;
                }
                status_changed = self.dht_status_rx.changed() => {
                    if status_changed.is_ok() {
                        self.refresh_system_warning();
                        self.app_state.ui.needs_redraw = true;
                    }
                }

                Some(command) = self.app_command_rx.recv() => {
                    self.handle_app_command(command).await;
                },

                Some(event) = self.tui_event_rx.recv() => {
                    self.clamp_selected_indices();
                    events::handle_event(event, self).await;
                    next_draw_time = Instant::now();
                }

                Some(result) = self.notify_rx.recv() => {
                    self.handle_file_event(result).await;
                }

                _ = watch_folder_rescan_interval.tick() => {
                    self.process_pending_commands().await;
                }
                _ = shared_role_retry_interval.tick() => {
                    self.maybe_promote_to_shared_leader().await;
                    self.refresh_follower_read_model();
                }

                _ = async {
                    if let Some(deadline) = next_paste_flush_at {
                        time::sleep_until(deadline.into()).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    self.clamp_selected_indices();
                    events::flush_pending_paste_burst(self).await;
                    next_draw_time = Instant::now();
                }

                _ = stats_interval.tick() => {
                    self.calculate_stats(&mut sys);
                    self.app_state.ui.needs_redraw = true;
                }

                _ = time::sleep_until(next_tuning_at) => {
                    self.tuning_resource_limits().await;
                    self.reschedule_tuning_deadline();
                }

                _ = async {
                    if let Some(deadline) = next_status_dump_at {
                        time::sleep_until(deadline).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    self.trigger_status_dump_now();
                }
                _ = async {
                    if let Some(deadline) = next_startup_load_at {
                        time::sleep_until(deadline).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    self.load_next_startup_batch().await;
                }
                _ = network_history_persist_interval.tick() => {
                    if should_persist_network_history_on_interval(&self.app_state) {
                        self.save_state_to_disk();
                    }
                }
                _ = integrity_scheduler_interval.tick() => {
                    self.advance_integrity_scheduler(INTEGRITY_SCHEDULER_TICK_INTERVAL);
                }

                _ = time::sleep_until(next_draw_time.into()) => {
                    next_draw_time = Instant::now() + current_target_framerate;
                    self.drain_latest_torrent_metrics();
                    if Self::should_draw_this_frame(&self.app_state.mode, self.app_state.ui.needs_redraw) {
                        self.tick_ui_effects_clock();
                        terminal.draw(|f| {
                            draw(f, &self.app_state, &self.client_configs);
                        })?;
                        self.app_state.ui.needs_redraw = false;
                    }
                }
                _ = version_interval.tick() => {
                    let current_version = env!("CARGO_PKG_VERSION");
                    let tx = self.app_command_tx.clone();
                    let mut shutdown_rx = self.shutdown_tx.subscribe();

                    tokio::spawn(async move {
                        tokio::select! {
                            latest_result = App::fetch_latest_version() => {
                                if let Ok(latest) = latest_result {
                                    if latest != current_version {
                                        tracing::info!("New version found! Current: {} - Latest: {}", current_version, latest.clone());
                                        let _ = tx.send(AppCommand::UpdateVersionAvailable(latest)).await;
                                    }
                                    else {
                                        tracing::info!("Current version is latest! Current: {} - Latest: {}", current_version, latest);
                                    }
                                }
                            }
                            _ = shutdown_rx.recv() => {
                                tracing::debug!("Version check aborted due to shutdown");
                            }
                        }
                    });
                }
            }
        }

        self.save_state_to_disk();

        self.shutdown_sequence(terminal).await;
        self.flush_persistence_writer().await;

        Ok(())
    }

    fn should_draw_this_frame(mode: &AppMode, ui_needs_redraw: bool) -> bool {
        if matches!(mode, AppMode::PowerSaving) {
            ui_needs_redraw
        } else {
            true
        }
    }

    fn tick_ui_effects_clock(&mut self) {
        let now = Instant::now();
        let mut cleared_port_highlight = false;
        if self
            .app_state
            .externally_accessable_port_v4_highlight_until
            .is_some_and(|deadline| deadline <= now)
        {
            self.app_state.externally_accessable_port_v4_highlight_until = None;
            cleared_port_highlight = true;
        }
        if self
            .app_state
            .externally_accessable_port_v6_highlight_until
            .is_some_and(|deadline| deadline <= now)
        {
            self.app_state.externally_accessable_port_v6_highlight_until = None;
            cleared_port_highlight = true;
        }
        if cleared_port_highlight {
            self.app_state.ui.needs_redraw = true;
        }

        let frame_wall_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        let activity_speed_multiplier =
            compute_effects_activity_speed_multiplier(&self.app_state, &self.client_configs);

        if self.app_state.ui.effects_last_wall_time <= 0.0 {
            self.app_state.ui.effects_last_wall_time = frame_wall_time;
        }

        let frame_dt =
            (frame_wall_time - self.app_state.ui.effects_last_wall_time).clamp(0.0, 0.25);
        self.app_state.ui.effects_last_wall_time = frame_wall_time;
        self.app_state.ui.effects_speed_multiplier = activity_speed_multiplier;
        self.app_state.ui.effects_phase_time += frame_dt * activity_speed_multiplier;
        let selected_torrent = self
            .app_state
            .torrent_list_order
            .get(self.app_state.ui.selected_torrent_index)
            .and_then(|info_hash| self.app_state.torrents.get(info_hash));
        let download_steps_per_second = selected_torrent
            .map(|torrent| file_activity_wave_steps_per_second(torrent.smoothed_download_speed_bps))
            .unwrap_or_else(|| file_activity_wave_steps_per_second(0));
        let upload_steps_per_second = selected_torrent
            .map(|torrent| file_activity_wave_steps_per_second(torrent.smoothed_upload_speed_bps))
            .unwrap_or_else(|| file_activity_wave_steps_per_second(0));
        self.app_state.ui.file_activity_download_phase += frame_dt * download_steps_per_second;
        self.app_state.ui.file_activity_upload_phase += frame_dt * upload_steps_per_second;

        let disk_activity = self
            .app_state
            .disk_health_ema
            .max(self.app_state.disk_health_peak_hold)
            .clamp(0.0, 1.0);
        let disk_phase_speed = 1.6 + 5.0 * disk_activity;
        self.app_state.disk_health_phase = (self.app_state.disk_health_phase
            + frame_dt * disk_phase_speed)
            .rem_euclid(std::f64::consts::TAU);
    }

    fn refresh_system_warning(&mut self) {
        let dht_warning = self.dht_service.current_warning();
        self.app_state.system_warning =
            compose_system_warning(self.base_system_warning.as_deref(), dht_warning.as_deref());
    }

    fn startup_crossterm_event_listener(&mut self) {
        let tui_event_tx_clone = self.tui_event_tx.clone();
        let mut tui_shutdown_rx = self.shutdown_tx.subscribe();

        self.tui_task = Some(tokio::spawn(async move {
            loop {
                if tui_shutdown_rx.try_recv().is_ok() {
                    break;
                }

                // Run blocking poll to completion (do NOT use tokio::select!)
                // This ensures we never abandon a thread that is reading from stdin.
                // Keep the timeout relatively short (250ms) so the app remains responsive to shutdown.
                let event =
                    tokio::task::spawn_blocking(|| -> std::io::Result<Option<CrosstermEvent>> {
                        if event::poll(Duration::from_millis(250))? {
                            return Ok(Some(event::read()?));
                        }
                        Ok(None)
                    })
                    .await;

                match event {
                    Ok(Ok(Some(e))) => {
                        if tui_event_tx_clone.send(e).await.is_err() {
                            break;
                        }
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(e)) => {
                        tracing::error!("Crossterm event error: {}", e);
                        break;
                    }
                    Err(e) => {
                        tracing::error!("Blocking task join error: {}", e);
                        break;
                    }
                }

                if tui_shutdown_rx.try_recv().is_ok() {
                    break;
                }
            }
        }));
    }

    async fn flush_persistence_writer(&mut self) {
        flush_persistence_writer_parts(&mut self.persistence_tx, &mut self.persistence_task).await;
    }

    async fn shutdown_sequence(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) {
        let _ = self.shutdown_tx.send(());

        if let Some(handle) = self.tui_task.take() {
            tracing::info!("Waiting for TUI event listener to finish...");
            if let Err(e) = handle.await {
                tracing::error!("Error joining TUI task: {}", e);
            }
        }

        let total_managers_to_shut_down = self.torrent_manager_command_txs.len();
        let mut managers_shut_down = 0;

        for manager_tx in self.torrent_manager_command_txs.values() {
            let _ = manager_tx.try_send(ManagerCommand::Shutdown);
        }

        if total_managers_to_shut_down == 0 {
            return;
        }

        let shutdown_timeout = time::sleep(Duration::from_secs(SHUTDOWN_TIMEOUT_SECS));
        let mut draw_interval = time::interval(Duration::from_millis(100));
        tokio::pin!(shutdown_timeout);

        tracing_event!(
            Level::INFO,
            "Waiting for {} torrents to shut down...",
            total_managers_to_shut_down
        );

        loop {
            self.app_state.shutdown_progress =
                managers_shut_down as f64 / total_managers_to_shut_down as f64;
            self.tick_ui_effects_clock();
            let _ = terminal.draw(|f| {
                draw(f, &self.app_state, &self.client_configs);
            });

            tokio::select! {
                Some(event) = self.manager_event_rx.recv() => {
                    match event {
                        ManagerEvent::DeletionComplete(..) => {
                            managers_shut_down += 1;
                            if managers_shut_down == total_managers_to_shut_down {
                                tracing_event!(Level::INFO, "All torrents shut down gracefully.");
                                break;
                            }
                        }
                        _ => {
                            // CRITICAL: We must aggressively drain other events (Stats, BlockReceived, etc.)
                            // so the managers don't get blocked on a full channel while trying to die.
                        }
                    }
                }

                _ = draw_interval.tick() => {
                }

                _ = &mut shutdown_timeout => {
                    tracing_event!(Level::WARN, "Shutdown timed out. {}/{} managers did not reply. Forcing exit.",
                        total_managers_to_shut_down - managers_shut_down,
                        total_managers_to_shut_down
                    );
                    break;
                }
            }
        }
    }

    async fn handle_incoming_peer(&mut self, mut stream: TcpStream) {
        let resource_manager_clone = self.resource_manager.clone();
        let app_command_tx = self.app_command_tx.clone();
        let mut permit_shutdown_rx = self.shutdown_tx.subscribe();
        tokio::spawn(async move {
            let peer_addr = stream.peer_addr().ok();
            if let Some(peer_addr) = peer_addr {
                network_metrics::record(
                    "inbound_tcp_accepted",
                    None,
                    Some(peer_addr),
                    Some(network_metrics::peer_source_label(
                        crate::torrent_manager::PeerSource::Incoming,
                    )),
                    serde_json::json!({}),
                );
            }
            let permit_started_at = Instant::now();
            let Some(_handshake_permit) = (tokio::select! {
                permit_result = resource_manager_clone.acquire_peer_handshake() => {
                    match permit_result {
                        Ok(permit) => {
                            network_metrics::record(
                                "inbound_handshake_permit_wait",
                                None,
                                peer_addr,
                                Some(network_metrics::peer_source_label(
                                    crate::torrent_manager::PeerSource::Incoming,
                                )),
                                serde_json::json!({
                                    "elapsed_ms": network_metrics::elapsed_ms(permit_started_at),
                                    "outcome": "acquired",
                                }),
                            );
                            Some(permit)
                        }
                        Err(_) => {
                            network_metrics::record(
                                "inbound_handshake_permit_wait",
                                None,
                                peer_addr,
                                Some(network_metrics::peer_source_label(
                                    crate::torrent_manager::PeerSource::Incoming,
                                )),
                                serde_json::json!({
                                    "elapsed_ms": network_metrics::elapsed_ms(permit_started_at),
                                    "outcome": "shutdown",
                                }),
                            );
                            tracing_event!(Level::DEBUG, "Failed to acquire inbound handshake permit. Manager shut down?");
                            None
                        }
                    }
                }
                _ = permit_shutdown_rx.recv() => {
                    network_metrics::record(
                        "inbound_handshake_permit_wait",
                        None,
                        peer_addr,
                        Some(network_metrics::peer_source_label(
                            crate::torrent_manager::PeerSource::Incoming,
                        )),
                        serde_json::json!({
                            "elapsed_ms": network_metrics::elapsed_ms(permit_started_at),
                            "outcome": "shutdown",
                        }),
                    );
                    None
                }
            }) else {
                return;
            };
            let mut buffer = vec![0u8; 68];
            match time::timeout(
                Duration::from_secs(INCOMING_HANDSHAKE_TIMEOUT_SECS),
                stream.read_exact(&mut buffer),
            )
            .await
            {
                Ok(Ok(_)) => {
                    if !is_valid_incoming_bittorrent_handshake(&buffer) {
                        network_metrics::record(
                            "inbound_handshake_invalid",
                            None,
                            peer_addr,
                            Some(network_metrics::peer_source_label(
                                crate::torrent_manager::PeerSource::Incoming,
                            )),
                            serde_json::json!({}),
                        );
                        tracing::trace!(
                            "Rejected inbound TCP connection with invalid BitTorrent handshake."
                        );
                        return;
                    }

                    let peer_info_hash = &buffer[28..48];
                    if let Some(peer_addr) = peer_addr {
                        let _ = app_command_tx
                            .send(AppCommand::RouteIncomingPeer {
                                stream,
                                peer_addr,
                                handshake_response: buffer,
                            })
                            .await;
                    } else {
                        network_metrics::record(
                            "inbound_routing_failed",
                            Some(peer_info_hash),
                            None,
                            Some(network_metrics::peer_source_label(
                                crate::torrent_manager::PeerSource::Incoming,
                            )),
                            serde_json::json!({
                                "reason": "missing_peer_addr",
                            }),
                        );
                        tracing::trace!(
                            "Rejected inbound TCP connection because peer address was unavailable for hash {}.",
                            hex::encode(peer_info_hash)
                        );
                    }
                }
                Ok(Err(error)) => {
                    network_metrics::record(
                        "inbound_handshake_failed",
                        None,
                        peer_addr,
                        Some(network_metrics::peer_source_label(
                            crate::torrent_manager::PeerSource::Incoming,
                        )),
                        serde_json::json!({
                            "reason": error.kind().to_string(),
                            "error": error.to_string(),
                        }),
                    );
                }
                Err(_) => {
                    network_metrics::record(
                        "inbound_handshake_timeout",
                        None,
                        peer_addr,
                        Some(network_metrics::peer_source_label(
                            crate::torrent_manager::PeerSource::Incoming,
                        )),
                        serde_json::json!({
                            "timeout_secs": INCOMING_HANDSHAKE_TIMEOUT_SECS,
                        }),
                    );
                }
            }
        });
    }

    fn refresh_rss_derived(&mut self) {
        crate::tui::screens::rss::recompute_rss_derived(&mut self.app_state, &self.client_configs);
    }

    fn dispatch_scheduled_peer_candidates(&mut self, scheduled: Vec<DispatchedPeerCandidate>) {
        for scheduled_candidate in scheduled {
            network_metrics::record(
                "peer_candidate_admitted",
                Some(&scheduled_candidate.info_hash),
                Some(scheduled_candidate.candidate.addr),
                Some(network_metrics::peer_source_label(
                    scheduled_candidate.candidate.source,
                )),
                serde_json::json!({}),
            );
            if let Some(manager_tx) = self
                .torrent_manager_command_txs
                .get(&scheduled_candidate.info_hash)
            {
                let _ = manager_tx.try_send(ManagerCommand::ConnectPeer {
                    candidate: scheduled_candidate.candidate,
                });
            }
        }
    }

    fn sync_global_peer_manager_limits(&mut self) {
        let scheduled = self
            .global_peer_manager
            .update_session_limit(Instant::now(), self.app_state.limits.max_connected_peers);
        self.dispatch_scheduled_peer_candidates(scheduled);
    }

    fn active_running_torrents_for_dht_announce(&self) -> Vec<Vec<u8>> {
        self.app_state
            .torrents
            .iter()
            .filter(|(info_hash, display)| {
                display.latest_state.torrent_control_state == TorrentControlState::Running
                    && self.torrent_manager_command_txs.contains_key(*info_hash)
            })
            .map(|(info_hash, _)| info_hash.clone())
            .collect()
    }

    fn announce_torrents_to_dht<I>(&self, info_hashes: I)
    where
        I: IntoIterator<Item = Vec<u8>>,
    {
        let Some(port) =
            (self.client_configs.client_port > 0).then_some(self.client_configs.client_port)
        else {
            return;
        };

        let dht_handle = self.dht_service.handle();
        for info_hash in info_hashes {
            let dht_handle = dht_handle.clone();
            tokio::spawn(async move {
                let _ = dht_handle.announce_peer(info_hash, Some(port)).await;
            });
        }
    }

    fn build_resource_manager_limits_map(
        &self,
        limits: &CalculatedLimits,
    ) -> HashMap<ResourceType, usize> {
        let mut map = limits.clone().into_map();
        map.insert(
            ResourceType::PeerHandshake,
            peer_handshake_limit(limits.max_connected_peers),
        );
        map
    }

    fn remove_torrent_runtime(&mut self, info_hash: &[u8]) {
        self.app_state.torrents.remove(info_hash);
        self.startup_completion_suppressed_hashes.remove(info_hash);
        self.torrent_manager_command_txs.remove(info_hash);
        self.torrent_manager_incoming_peer_txs.remove(info_hash);
        let scheduled = self
            .global_peer_manager
            .remove_torrent(Instant::now(), info_hash);
        self.dispatch_scheduled_peer_candidates(scheduled);
        self.torrent_metric_watch_rxs.remove(info_hash);
        self.integrity_scheduler.remove_torrent(info_hash);
        self.app_state
            .torrent_list_order
            .retain(|candidate| candidate.as_slice() != info_hash);
        clamp_selected_indices_in_state(&mut self.app_state);
        self.refresh_rss_derived();
        self.dispatch_integrity_probe_batches();
    }

    async fn load_runtime_torrent_from_settings(
        &mut self,
        torrent_config: TorrentSettings,
    ) -> bool {
        if !should_load_persisted_torrent(&torrent_config) {
            tracing_event!(
                Level::WARN,
                torrent = %torrent_config.torrent_or_magnet,
                "Skipping persisted torrent left in transient Deleting state during startup or convergence"
            );
            return false;
        }

        tracing_event!(
            Level::INFO,
            torrent = %torrent_config.torrent_or_magnet,
            torrent_name = %torrent_config.name,
            validation_status = torrent_config.validation_status,
            "Restoring persisted torrent into runtime"
        );
        if torrent_config.validation_status {
            if let Some(info_hash) =
                info_hash_from_torrent_source(&torrent_config.torrent_or_magnet)
            {
                self.startup_completion_suppressed_hashes.insert(info_hash);
            }
        }

        if self.should_suppress_follower_runtime_for_torrent(&torrent_config) {
            self.ensure_display_only_torrent_from_settings(&torrent_config);
            return true;
        }

        let ingest_result = if torrent_config.torrent_or_magnet.starts_with("magnet:") {
            self.add_magnet_torrent(
                torrent_config.name.clone(),
                torrent_config.torrent_or_magnet.clone(),
                torrent_config.download_path.clone(),
                torrent_config.validation_status,
                torrent_config.torrent_control_state,
                torrent_config.file_priorities,
                torrent_config.container_name,
            )
            .await
        } else {
            self.add_torrent_from_file(
                PathBuf::from(&torrent_config.torrent_or_magnet),
                torrent_config.download_path.clone(),
                torrent_config.validation_status,
                torrent_config.torrent_control_state,
                torrent_config.file_priorities.clone(),
                torrent_config.container_name,
            )
            .await
        };

        matches!(
            ingest_result,
            CommandIngestResult::Added { .. } | CommandIngestResult::Duplicate { .. }
        )
    }

    async fn sync_runtime_torrents_from_settings(
        &mut self,
        old_settings: &Settings,
        new_settings: &Settings,
    ) {
        let old_by_hash: HashMap<Vec<u8>, &TorrentSettings> = old_settings
            .torrents
            .iter()
            .filter_map(|torrent| {
                info_hash_from_torrent_source(&torrent.torrent_or_magnet)
                    .map(|hash| (hash, torrent))
            })
            .collect();
        let new_by_hash: HashMap<Vec<u8>, &TorrentSettings> = new_settings
            .torrents
            .iter()
            .filter_map(|torrent| {
                info_hash_from_torrent_source(&torrent.torrent_or_magnet)
                    .map(|hash| (hash, torrent))
            })
            .collect();
        let added_torrents: Vec<TorrentSettings> = new_by_hash
            .iter()
            .filter(|(info_hash, _)| !old_by_hash.contains_key(*info_hash))
            .map(|(_, torrent)| (*torrent).clone())
            .collect();
        let default_download_changed =
            old_settings.default_download_folder != new_settings.default_download_folder;

        for (info_hash, torrent) in &new_by_hash {
            if let Some(runtime) = self.app_state.torrents.get_mut(info_hash) {
                runtime.latest_state.torrent_name = torrent.name.clone();
                runtime.latest_state.download_path = torrent
                    .download_path
                    .clone()
                    .or_else(|| new_settings.default_download_folder.clone());
                runtime.latest_state.container_name = torrent.container_name.clone();
                let updated_file_priorities = torrent.file_priorities.clone();
                runtime.latest_state.file_priorities = updated_file_priorities.clone();
                if !runtime.file_preview_tree.is_empty() {
                    runtime.file_preview_tree = rebuild_torrent_preview_tree(
                        &runtime.file_preview_tree,
                        &updated_file_priorities,
                    );
                }
                runtime.latest_state.torrent_control_state = torrent.torrent_control_state.clone();
                runtime.latest_state.delete_files = torrent.delete_files;
            }

            if self.should_suppress_follower_runtime_for_torrent(torrent) {
                if let Some(manager_tx) = self.torrent_manager_command_txs.get(info_hash) {
                    let _ = manager_tx.try_send(ManagerCommand::Shutdown);
                }
                self.ensure_display_only_torrent_from_settings(torrent);
                continue;
            }

            let Some(previous) = old_by_hash.get(info_hash) else {
                continue;
            };

            if previous.torrent_control_state != torrent.torrent_control_state {
                if let Some(manager_tx) = self.torrent_manager_command_txs.get(info_hash) {
                    let command = match torrent.torrent_control_state {
                        TorrentControlState::Paused => Some(ManagerCommand::Pause),
                        TorrentControlState::Running => Some(ManagerCommand::Resume),
                        TorrentControlState::Deleting => {
                            if torrent.delete_files {
                                Some(ManagerCommand::DeleteFile)
                            } else {
                                Some(ManagerCommand::Shutdown)
                            }
                        }
                    };
                    if let Some(command) = command {
                        let _ = manager_tx.try_send(command);
                    }
                }
            }

            if default_download_changed
                || previous.download_path != torrent.download_path
                || previous.container_name != torrent.container_name
                || previous.file_priorities != torrent.file_priorities
            {
                if let Some(torrent_data_path) = torrent
                    .download_path
                    .clone()
                    .or_else(|| new_settings.default_download_folder.clone())
                {
                    if let Some(manager_tx) = self.torrent_manager_command_txs.get(info_hash) {
                        let _ = manager_tx.try_send(ManagerCommand::SetUserTorrentConfig {
                            torrent_data_path,
                            file_priorities: torrent.file_priorities.clone(),
                            container_name: torrent.container_name.clone(),
                        });
                    }
                }
            }
        }

        for info_hash in old_by_hash.keys() {
            if new_by_hash.contains_key(info_hash) {
                continue;
            }

            if let Some(manager_tx) = self.torrent_manager_command_txs.get(info_hash) {
                let _ = manager_tx.try_send(ManagerCommand::Shutdown);
                if let Some(runtime) = self.app_state.torrents.get_mut(info_hash) {
                    runtime.latest_state.torrent_control_state = TorrentControlState::Deleting;
                    runtime.latest_state.delete_files = false;
                }
            } else {
                self.remove_torrent_runtime(info_hash);
            }
        }

        for torrent in added_torrents {
            self.load_runtime_torrent_from_settings(torrent).await;
        }

        if self.is_current_shared_follower() {
            self.refresh_follower_read_model();
        }
    }

    async fn apply_settings_update(&mut self, new_settings: Settings, persist: bool) {
        let old_settings = self.client_configs.clone();
        self.client_configs = new_settings.clone();
        let _ = self.rss_settings_tx.send(self.client_configs.clone());
        let rss_changed = rss_settings_changed(&old_settings, &new_settings);
        self.sync_runtime_torrents_from_settings(&old_settings, &new_settings)
            .await;

        if let Err(error) = crate::config::ensure_watch_directories(&self.client_configs) {
            tracing::warn!(
                "Failed to ensure configured watch directories exist after config update: {}",
                error
            );
        }
        self.reconcile_watched_paths(&new_settings);

        if new_settings.ui_theme != old_settings.ui_theme {
            self.app_state.theme = Theme::builtin(new_settings.ui_theme);
        }

        if new_settings.client_port != old_settings.client_port {
            tracing::info!(
                "Config update: Port changed to {}",
                new_settings.client_port
            );
            if !self.rebind_listener(new_settings.client_port).await {
                self.client_configs.client_port = old_settings.client_port;
                let _ = self.rss_settings_tx.send(self.client_configs.clone());
            }
        }

        if new_settings.bootstrap_nodes != old_settings.bootstrap_nodes {
            tracing::info!("Config update: DHT bootstrap nodes changed.");
            self.dht_service
                .reconfigure(DhtServiceConfig::from_settings(&self.client_configs));
        }

        if new_settings.global_download_limit_bps != old_settings.global_download_limit_bps {
            self.global_dl_bucket
                .set_rate(new_settings.global_download_limit_bps as f64);
        }
        if new_settings.global_upload_limit_bps != old_settings.global_upload_limit_bps {
            self.global_ul_bucket
                .set_rate(new_settings.global_upload_limit_bps as f64);
        }

        if self.status_dump_interval_override_secs.is_none() {
            self.reschedule_status_dump_deadline();
        }

        if rss_changed {
            prune_rss_feed_errors(
                &mut self.app_state.rss_runtime.feed_errors,
                &self.client_configs,
            );
            self.refresh_rss_derived();
            let _ = self.rss_sync_tx.try_send(());
        }

        if persist {
            self.save_state_to_disk();
        }

        self.app_state.system_error = None;
        self.app_state.ui.needs_redraw = true;
    }

    async fn handle_app_command(&mut self, command: AppCommand) {
        match command {
            AppCommand::AddTorrentFromFile(path) => {
                let action = self.resolve_add_ingress_action(IngestSource::TorrentFile, &path);
                self.execute_add_ingress_action(IngestSource::TorrentFile, path, action)
                    .await;
            }
            AppCommand::AddTorrentFromPathFile(path) => {
                let action = self.resolve_add_ingress_action(IngestSource::TorrentPathFile, &path);
                self.execute_add_ingress_action(IngestSource::TorrentPathFile, path, action)
                    .await;
            }
            AppCommand::AddMagnetFromFile(path) => {
                let action = self.resolve_add_ingress_action(IngestSource::MagnetFile, &path);
                self.execute_add_ingress_action(IngestSource::MagnetFile, path, action)
                    .await;
            }
            AppCommand::RouteIncomingPeer {
                stream,
                peer_addr,
                handshake_response,
            } => {
                let info_hash = handshake_response[28..48].to_vec();
                if !self
                    .torrent_manager_incoming_peer_txs
                    .contains_key(info_hash.as_slice())
                {
                    network_metrics::record(
                        "inbound_routing_failed",
                        Some(&info_hash),
                        Some(peer_addr),
                        Some(network_metrics::peer_source_label(
                            crate::torrent_manager::PeerSource::Incoming,
                        )),
                        serde_json::json!({
                            "reason": "missing_manager",
                        }),
                    );
                    tracing::trace!(
                        "ROUTING FAIL: No manager registered for hash: {}",
                        hex::encode(&info_hash)
                    );
                    return;
                }

                let candidate = PeerCandidate::from_incoming(peer_addr);
                if !self.global_peer_manager.reserve_incoming_candidate(
                    Instant::now(),
                    &info_hash,
                    candidate,
                ) {
                    return;
                }

                let tx = self.app_command_tx.clone();
                let resource_manager = self.resource_manager.clone();
                let mut shutdown_rx = self.shutdown_tx.subscribe();
                tokio::spawn(async move {
                    let permit_started_at = Instant::now();
                    let session_permit = tokio::select! {
                        permit_result = time::timeout(
                            Duration::from_secs(10),
                            resource_manager.acquire_peer_connection()
                        ) => {
                            match permit_result {
                                Ok(Ok(permit)) => {
                                    network_metrics::record(
                                        "inbound_session_permit_wait",
                                        Some(&info_hash),
                                        Some(peer_addr),
                                        Some(network_metrics::peer_source_label(
                                            crate::torrent_manager::PeerSource::Incoming,
                                        )),
                                        serde_json::json!({
                                            "elapsed_ms": network_metrics::elapsed_ms(permit_started_at),
                                            "outcome": "acquired",
                                        }),
                                    );
                                    Some(permit)
                                }
                                Ok(Err(_)) => {
                                    network_metrics::record(
                                        "inbound_session_permit_wait",
                                        Some(&info_hash),
                                        Some(peer_addr),
                                        Some(network_metrics::peer_source_label(
                                            crate::torrent_manager::PeerSource::Incoming,
                                        )),
                                        serde_json::json!({
                                            "elapsed_ms": network_metrics::elapsed_ms(permit_started_at),
                                            "outcome": "shutdown",
                                        }),
                                    );
                                    None
                                }
                                Err(_) => {
                                    network_metrics::record(
                                        "inbound_session_permit_wait",
                                        Some(&info_hash),
                                        Some(peer_addr),
                                        Some(network_metrics::peer_source_label(
                                            crate::torrent_manager::PeerSource::Incoming,
                                        )),
                                        serde_json::json!({
                                            "elapsed_ms": network_metrics::elapsed_ms(permit_started_at),
                                            "outcome": "timeout",
                                        }),
                                    );
                                    None
                                }
                            }
                        }
                        _ = shutdown_rx.recv() => {
                            network_metrics::record(
                                "inbound_session_permit_wait",
                                Some(&info_hash),
                                Some(peer_addr),
                                Some(network_metrics::peer_source_label(
                                    crate::torrent_manager::PeerSource::Incoming,
                                )),
                                serde_json::json!({
                                    "elapsed_ms": network_metrics::elapsed_ms(permit_started_at),
                                    "outcome": "shutdown",
                                }),
                            );
                            None
                        }
                    };

                    let command = match session_permit {
                        Some(session_permit) => AppCommand::FinalizeIncomingPeerAdmission {
                            info_hash,
                            peer_addr,
                            session: IncomingPeerSession {
                                stream,
                                handshake_response,
                                session_permit,
                            },
                        },
                        None => AppCommand::ReleaseIncomingPeerReservation {
                            info_hash,
                            peer_addr,
                        },
                    };
                    let _ = tx.send(command).await;
                });
            }
            AppCommand::FinalizeIncomingPeerAdmission {
                info_hash,
                peer_addr,
                session,
            } => {
                let send_result = if let Some(torrent_manager_tx) =
                    self.torrent_manager_incoming_peer_txs.get(&info_hash)
                {
                    let _ = self
                        .app_command_tx
                        .send(AppCommand::MarkPortOpen(peer_addr))
                        .await;
                    torrent_manager_tx.send(session).await
                } else {
                    tracing::trace!(
                        "ROUTING FAIL: No manager registered for hash: {}",
                        hex::encode(&info_hash)
                    );
                    Err(tokio::sync::mpsc::error::SendError(session))
                };

                if send_result.is_ok() {
                    network_metrics::record(
                        "inbound_peer_routed",
                        Some(&info_hash),
                        Some(peer_addr),
                        Some(network_metrics::peer_source_label(
                            crate::torrent_manager::PeerSource::Incoming,
                        )),
                        serde_json::json!({}),
                    );
                }
                if send_result.is_err() {
                    network_metrics::record(
                        "inbound_routing_failed",
                        Some(&info_hash),
                        Some(peer_addr),
                        Some(network_metrics::peer_source_label(
                            crate::torrent_manager::PeerSource::Incoming,
                        )),
                        serde_json::json!({
                            "reason": "manager_send_failed",
                        }),
                    );
                    let scheduled = self.global_peer_manager.release_pending_candidate(
                        Instant::now(),
                        &info_hash,
                        peer_addr,
                    );
                    self.dispatch_scheduled_peer_candidates(scheduled);
                }
            }
            AppCommand::ReleaseIncomingPeerReservation {
                info_hash,
                peer_addr,
            } => {
                network_metrics::record(
                    "inbound_routing_failed",
                    Some(&info_hash),
                    Some(peer_addr),
                    Some(network_metrics::peer_source_label(
                        crate::torrent_manager::PeerSource::Incoming,
                    )),
                    serde_json::json!({
                        "reason": "session_permit_unavailable",
                    }),
                );
                let scheduled = self.global_peer_manager.release_pending_candidate(
                    Instant::now(),
                    &info_hash,
                    peer_addr,
                );
                self.dispatch_scheduled_peer_candidates(scheduled);
            }
            AppCommand::MarkPortOpen(peer_addr) => {
                let highlight_until = Some(Instant::now() + PORT_FAMILY_HIGHLIGHT_DURATION);
                let just_opened;
                let open_flag = match peer_addr {
                    SocketAddr::V4(_) => {
                        self.app_state.externally_accessable_port_v4_highlight_until =
                            highlight_until;
                        &mut self.app_state.externally_accessable_port_v4
                    }
                    SocketAddr::V6(addr) if addr.ip().to_ipv4_mapped().is_some() => {
                        self.app_state.externally_accessable_port_v4_highlight_until =
                            highlight_until;
                        &mut self.app_state.externally_accessable_port_v4
                    }
                    SocketAddr::V6(_) => {
                        self.app_state.externally_accessable_port_v6_highlight_until =
                            highlight_until;
                        &mut self.app_state.externally_accessable_port_v6
                    }
                };
                just_opened = !*open_flag;
                if just_opened {
                    network_metrics::record(
                        "port_open_marked",
                        None,
                        Some(peer_addr),
                        Some(network_metrics::peer_source_label(
                            crate::torrent_manager::PeerSource::Incoming,
                        )),
                        serde_json::json!({}),
                    );
                    *open_flag = true;
                    let info_hashes = self.active_running_torrents_for_dht_announce();
                    self.announce_torrents_to_dht(info_hashes);
                }
                self.app_state.ui.needs_redraw = true;
            }
            AppCommand::SubmitControlRequest(request) => {
                if let Err(error) = self
                    .dispatch_cluster_control_request(request, ControlOrigin::CliOnline)
                    .await
                {
                    self.app_state.system_error = Some(error);
                    self.app_state.ui.needs_redraw = true;
                }
            }
            AppCommand::ControlRequest { path, request } => {
                if self.is_current_shared_follower() && self.is_host_watch_path(&path) {
                    self.app_state.pending_control_by_path.remove(&path);
                    self.relay_local_watch_file(&path, "control.forwarded");
                    self.save_state_to_disk();
                    return;
                }

                let result = self.apply_control_request(&request).await;
                self.record_control_result(&path, &request, result);
                self.save_state_to_disk();

                if let Err(error) = archive_watch_file(&path, "control.done") {
                    tracing_event!(
                        Level::WARN,
                        "Failed to archive processed control file {:?}: {}",
                        &path,
                        error
                    );
                }
            }
            AppCommand::ClientShutdown(path) => {
                tracing_event!(Level::INFO, "Shutdown command received via command file.");
                self.app_state.should_quit = true;
                if let Err(e) = fs::remove_file(&path) {
                    tracing_event!(
                        Level::WARN,
                        "Failed to remove command file {:?}: {}",
                        &path,
                        e
                    );
                }
            }
            AppCommand::PortFileChanged(path) => {
                self.handle_port_change(path).await;
            }

            AppCommand::FetchFileTree {
                path,
                browser_mode,
                highlight_path,
            } => {
                let tx = self.app_command_tx.clone();
                let mut shutdown_rx = self.shutdown_tx.subscribe();
                let path_clone = path.clone();
                let highlight_clone = highlight_path.clone();

                // 1. Update or Initialize the UI state immediately
                if matches!(self.app_state.mode, AppMode::FileBrowser) {
                    // If already in browser, just update the path we are viewing
                    self.app_state.ui.file_browser.state.current_path = path.clone();
                    self.app_state.ui.file_browser.browser_mode = browser_mode;
                } else {
                    // Otherwise, initialize the mode
                    let mut tree_state = crate::tui::tree::TreeViewState::new();
                    tree_state.current_path = path.clone();
                    self.app_state.ui.file_browser.state = tree_state;
                    self.app_state.ui.file_browser.data = Vec::new();
                    self.app_state.ui.file_browser.browser_mode = browser_mode;
                    self.app_state.mode = AppMode::FileBrowser;
                }

                // 2. Spawn the background crawl
                tokio::spawn(async move {
                    tokio::select! {
                        result = build_fs_tree(&path_clone, 0) => {
                            if let Ok(nodes) = result {
                                // Pass the highlight_path back so the Update arm can find it
                                let _ = tx.send(AppCommand::UpdateFileBrowserData {
                                    data: nodes,
                                    highlight_path: highlight_clone
                                }).await;
                            }
                        }
                        _ = shutdown_rx.recv() => {
                            tracing::debug!("Aborting FileBrowser crawl due to shutdown");
                        }
                    }
                });
            }

            AppCommand::UpdateFileBrowserData {
                mut data,
                highlight_path,
            } => {
                if matches!(self.app_state.mode, AppMode::FileBrowser) {
                    let state = &mut self.app_state.ui.file_browser.state;
                    let existing_data = &mut self.app_state.ui.file_browser.data;
                    let browser_mode = &mut self.app_state.ui.file_browser.browser_mode;
                    // --- 1. Apply Dynamic Sorting ---
                    if let FileBrowserMode::File(extensions) = browser_mode {
                        let target_exts: Vec<String> =
                            extensions.iter().map(|e| e.to_lowercase()).collect();
                        let has_target_files = data.iter().any(|node| {
                            !node.is_dir
                                && target_exts
                                    .iter()
                                    .any(|ext| node.name.to_lowercase().ends_with(ext))
                        });

                        if !has_target_files {
                            data.sort_by_key(|node| node.name.to_lowercase());
                        } else {
                            data.sort_by(|a, b| {
                                let a_matches = target_exts
                                    .iter()
                                    .any(|ext| a.name.to_lowercase().ends_with(ext));
                                let b_matches = target_exts
                                    .iter()
                                    .any(|ext| b.name.to_lowercase().ends_with(ext));

                                // 1. Priority: Torrents first
                                if a_matches != b_matches {
                                    return b_matches.cmp(&a_matches);
                                }

                                // 2. Priority: Folders second (ensures folders follow torrents directly)
                                if a.is_dir != b.is_dir {
                                    return b.is_dir.cmp(&a.is_dir); // Changed order to put folders higher
                                }

                                // 3. Final: Sort by newest date
                                b.payload.modified.cmp(&a.payload.modified)
                            });
                        }
                    }

                    // --- 2. Update Data ---
                    *existing_data = data;
                    state.top_most_offset = 0;

                    // --- 3. Smart Cursor Positioning ---
                    if let Some(target) = highlight_path {
                        // Find the index of the folder/file we want to highlight
                        if let Some(index) = existing_data
                            .iter()
                            .position(|node| node.full_path == target)
                        {
                            state.cursor_path = Some(target);

                            // Adjust scroll if the item is below the current visible area
                            let area = crate::tui::formatters::centered_rect(
                                75,
                                80,
                                self.app_state.screen_area,
                            );
                            let max_height = area.height.saturating_sub(2) as usize;
                            if index >= max_height {
                                state.top_most_offset = index.saturating_sub(max_height / 2);
                            }
                        } else {
                            state.cursor_path =
                                existing_data.first().map(|node| node.full_path.clone());
                        }
                    } else {
                        // Default: reset to top if entering a new folder
                        state.cursor_path =
                            existing_data.first().map(|node| node.full_path.clone());
                    }

                    self.app_state.ui.needs_redraw = true;
                }
            }
            AppCommand::RssSyncNow => {
                let _ = self.rss_sync_tx.try_send(());
                self.app_state.ui.needs_redraw = true;
            }
            AppCommand::RssPreviewUpdated(preview_items) => {
                self.app_state.rss_runtime.preview_items = preview_items;
                self.refresh_rss_derived();
                self.app_state.ui.needs_redraw = true;
            }
            AppCommand::RssSyncStatusUpdated {
                last_sync_at,
                next_sync_at,
            } => {
                self.app_state.rss_runtime.last_sync_at = last_sync_at;
                self.app_state.rss_runtime.next_sync_at = next_sync_at;
                self.save_state_to_disk();
                self.app_state.ui.needs_redraw = true;
            }
            AppCommand::RssFeedErrorUpdated { feed_url, error } => {
                if let Some(err) = error {
                    self.app_state.rss_runtime.feed_errors.insert(feed_url, err);
                } else {
                    self.app_state.rss_runtime.feed_errors.remove(&feed_url);
                }
                self.save_state_to_disk();
                self.app_state.ui.needs_redraw = true;
            }
            AppCommand::RssDownloadSelected {
                entry,
                command_path,
            } => {
                if let Some(command_path) = command_path {
                    let ingest_kind = ingest_kind_from_path(&command_path).unwrap_or_default();
                    let origin = match entry.added_via {
                        crate::config::RssAddedVia::Auto => IngestOrigin::RssAuto,
                        crate::config::RssAddedVia::Manual => IngestOrigin::RssManual,
                    };
                    self.record_rss_queued(command_path, origin, ingest_kind);
                }
                let existing_idx = self
                    .app_state
                    .rss_runtime
                    .history
                    .iter()
                    .position(|existing| existing.dedupe_key == entry.dedupe_key);
                if let Some(idx) = existing_idx {
                    if self.app_state.rss_runtime.history[idx].info_hash.is_none()
                        && entry.info_hash.is_some()
                    {
                        self.app_state.rss_runtime.history[idx].info_hash = entry.info_hash.clone();
                        self.save_state_to_disk();
                    }
                } else {
                    self.app_state.rss_runtime.history.push(entry);
                    self.save_state_to_disk();
                }
                self.refresh_rss_derived();
                self.app_state.ui.needs_redraw = true;
            }
            AppCommand::RssDownloadPreview(item) => {
                self.download_rss_preview_item(item).await;
                self.refresh_rss_derived();
                self.app_state.ui.needs_redraw = true;
            }
            AppCommand::NetworkHistoryLoaded(state) => {
                NetworkHistoryTelemetry::apply_loaded_state(&mut self.app_state, state);
                self.app_state.network_history_restore_pending = false;
                self.app_state.ui.needs_redraw = true;
            }
            AppCommand::ActivityHistoryLoaded(state) => {
                ActivityHistoryTelemetry::apply_loaded_state(&mut self.app_state, *state);
                self.app_state.activity_history_restore_pending = false;
                self.app_state.ui.needs_redraw = true;
            }
            AppCommand::NetworkHistoryPersisted {
                request_id,
                success,
            } => {
                apply_network_history_persist_result(&mut self.app_state, request_id, success);
            }
            AppCommand::ActivityHistoryPersisted {
                request_id,
                success,
            } => {
                apply_activity_history_persist_result(&mut self.app_state, request_id, success);
            }
            AppCommand::UpdateConfig(new_settings) => {
                let capabilities = self.cluster_capabilities();
                if capabilities.can_edit_host_local_config && self.is_current_shared_follower() {
                    match classify_shared_mode_settings_change(&self.client_configs, &new_settings)
                    {
                        SettingsChangeScope::NoChange => {}
                        SettingsChangeScope::HostOnly => {
                            match crate::config::save_settings(&new_settings) {
                                Ok(()) => self.apply_settings_update(new_settings, false).await,
                                Err(error) => {
                                    self.app_state.system_error = Some(format!(
                                        "Failed to save follower host-local settings: {}",
                                        error
                                    ));
                                    self.app_state.ui.needs_redraw = true;
                                }
                            }
                        }
                        SettingsChangeScope::SharedOrMixed => {
                            self.app_state.system_error = Some(
                                "Shared configuration and RSS edits are leader-only while this node is a follower. Only host-local client ID, port, and watch-folder changes are allowed."
                                    .to_string(),
                            );
                            self.app_state.ui.needs_redraw = true;
                        }
                    }
                } else {
                    self.apply_settings_update(new_settings, true).await;
                }
            }
            AppCommand::ReloadClusterState(_path) => match crate::config::load_settings() {
                Ok(new_settings) => {
                    if self.is_current_shared_leader() {
                        return;
                    }
                    if new_settings != self.client_configs {
                        self.apply_settings_update(new_settings, false).await;
                    }
                }
                Err(error) => {
                    tracing_event!(
                        Level::ERROR,
                        "Failed to reload shared cluster state: {}",
                        error
                    );
                }
            },
            AppCommand::UpdateVersionAvailable(latest_version) => {
                self.app_state.update_available = Some(latest_version);
            }
        }
    }

    fn handle_manager_event(&mut self, event: ManagerEvent) {
        match &event {
            ManagerEvent::PeerDiscovered {
                info_hash,
                candidate,
            } => {
                network_metrics::record(
                    "peer_candidate_discovered",
                    Some(info_hash),
                    Some(candidate.addr),
                    Some(network_metrics::peer_source_label(candidate.source)),
                    serde_json::json!({}),
                );
                let scheduled = self.global_peer_manager.submit_outgoing_candidate(
                    Instant::now(),
                    info_hash,
                    *candidate,
                );
                self.dispatch_scheduled_peer_candidates(scheduled);
            }
            ManagerEvent::PeerConnectionFailed {
                info_hash,
                peer_addr,
            } => {
                network_metrics::record(
                    "peer_backoff_applied",
                    Some(info_hash),
                    Some(*peer_addr),
                    None,
                    serde_json::json!({}),
                );
                let scheduled = self.global_peer_manager.record_connection_failure(
                    Instant::now(),
                    info_hash,
                    *peer_addr,
                );
                self.dispatch_scheduled_peer_candidates(scheduled);
            }
            ManagerEvent::PeerConnected {
                info_hash,
                peer_addr: Some(peer_addr),
            } => {
                self.global_peer_manager
                    .record_peer_connected(info_hash, *peer_addr);
            }
            ManagerEvent::PeerDisconnected {
                info_hash,
                peer_addr: Some(peer_addr),
            } => {
                let scheduled = self.global_peer_manager.record_peer_disconnected(
                    Instant::now(),
                    info_hash,
                    *peer_addr,
                );
                self.dispatch_scheduled_peer_candidates(scheduled);
            }
            ManagerEvent::PeerDisconnected {
                info_hash: _,
                peer_addr: None,
            } => {}
            ManagerEvent::TorrentPaused { info_hash } => {
                let scheduled = self
                    .global_peer_manager
                    .record_torrent_paused(Instant::now(), info_hash);
                self.dispatch_scheduled_peer_candidates(scheduled);
            }
            ManagerEvent::TorrentResumed { info_hash } => {
                let scheduled = self
                    .global_peer_manager
                    .record_torrent_resumed(Instant::now(), info_hash);
                self.dispatch_scheduled_peer_candidates(scheduled);
                if self.app_state.externally_accessable_port_v4
                    || self.app_state.externally_accessable_port_v6
                {
                    self.announce_torrents_to_dht(std::iter::once(info_hash.clone()));
                }
            }
            _ => {}
        }

        if UiTelemetry::on_manager_event_metrics(&mut self.app_state, &event) {
            return;
        }

        match event {
            ManagerEvent::DeletionComplete(info_hash, result) => {
                if let Err(e) = result {
                    tracing_event!(Level::ERROR, "Deletion failed for torrent: {}", e);
                }
                let should_remove_from_settings = self.can_write_shared_state()
                    && self
                        .client_configs
                        .torrents
                        .iter()
                        .find(|torrent| {
                            info_hash_from_torrent_source(&torrent.torrent_or_magnet).as_deref()
                                == Some(info_hash.as_slice())
                        })
                        .is_some_and(|torrent| {
                            torrent.torrent_control_state == TorrentControlState::Deleting
                                && torrent.delete_files
                        });

                if should_remove_from_settings {
                    self.client_configs.torrents.retain(|torrent| {
                        info_hash_from_torrent_source(&torrent.torrent_or_magnet).as_deref()
                            != Some(info_hash.as_slice())
                    });
                }

                self.app_state.torrents.remove(&info_hash);
                self.torrent_manager_command_txs.remove(&info_hash);
                self.torrent_manager_incoming_peer_txs.remove(&info_hash);
                let scheduled = self
                    .global_peer_manager
                    .remove_torrent(Instant::now(), &info_hash);
                self.dispatch_scheduled_peer_candidates(scheduled);
                self.torrent_metric_watch_rxs.remove(&info_hash);
                self.integrity_scheduler.remove_torrent(&info_hash);
                let scheduled = self
                    .global_peer_manager
                    .remove_torrent(Instant::now(), &info_hash);
                self.app_state
                    .torrent_list_order
                    .retain(|ih| *ih != info_hash);

                if self.app_state.ui.selected_torrent_index
                    >= self.app_state.torrent_list_order.len()
                    && !self.app_state.torrent_list_order.is_empty()
                {
                    self.app_state.ui.selected_torrent_index =
                        self.app_state.torrent_list_order.len() - 1;
                }

                self.save_state_to_disk();
                self.refresh_rss_derived();
                self.dispatch_integrity_probe_batches();
                self.dispatch_scheduled_peer_candidates(scheduled);

                self.app_state.ui.needs_redraw = true;
            }
            ManagerEvent::DataAvailabilityFault {
                info_hash,
                piece_index,
                error,
            } => {
                self.integrity_scheduler
                    .on_data_availability_fault(&info_hash);

                let mut availability_changed = false;
                if let Some(torrent) = self.app_state.torrents.get_mut(&info_hash) {
                    availability_changed = torrent.latest_state.data_available;
                    torrent.latest_state.data_available = false;
                }

                if let Some(torrent) = self.app_state.torrents.get(&info_hash) {
                    let saved_location = Self::torrent_saved_location(&torrent.latest_state);
                    tracing_event!(
                        Level::WARN,
                        info_hash = %hex::encode(&info_hash),
                        torrent = %torrent.latest_state.torrent_name,
                        piece = piece_index as usize,
                        saved_location = ?saved_location,
                        error = %error,
                        "Foreground disk read marked torrent data unavailable"
                    );
                }

                if availability_changed {
                    let torrent_name = self
                        .app_state
                        .torrents
                        .get(&info_hash)
                        .map(|torrent| torrent.latest_state.torrent_name.clone());
                    self.record_data_health_event(
                        &info_hash,
                        torrent_name,
                        EventType::DataUnavailable,
                        Vec::new(),
                        format!(
                            "Foreground disk read marked torrent data unavailable at piece {}",
                            piece_index
                        ),
                    );
                }

                if availability_changed {
                    self.save_state_to_disk();
                }

                self.dispatch_integrity_probe_batches();
                self.app_state.ui.needs_redraw = true;
            }
            ManagerEvent::FileProbeBatchResult { info_hash, result } => {
                let probe_result_availability = data_availability_from_file_probe_result(&result);
                let completed_sweep = self
                    .integrity_scheduler
                    .on_probe_batch_result(&info_hash, result);
                let mut availability_transition_log: Option<AvailabilityTransitionLog> = None;
                let mut should_notify_manager_unavailable = false;
                let mut should_request_recovery = false;
                let mut should_persist_unavailable = false;

                if let Some(torrent) = self.app_state.torrents.get_mut(&info_hash) {
                    if completed_sweep.is_some() && matches!(probe_result_availability, Some(false))
                    {
                        should_notify_manager_unavailable = torrent.latest_state.data_available;
                        torrent.latest_state.data_available = false;
                        should_persist_unavailable |= should_notify_manager_unavailable;
                    }

                    match completed_sweep {
                        Some(ProbeBatchOutcome::PendingMetadata) => {
                            torrent.latest_file_probe_status =
                                Some(TorrentFileProbeStatus::PendingMetadata);
                        }
                        Some(ProbeBatchOutcome::SweepInProgress) => {}
                        Some(ProbeBatchOutcome::CompletedSweep { problem_files }) => {
                            let was_available = torrent.latest_state.data_available;
                            let next_availability =
                                probe_result_availability.unwrap_or(was_available);
                            let issue_count = problem_files.len();
                            let issue_files = problem_files
                                .iter()
                                .map(|entry| {
                                    format!("{}: {}", entry.absolute_path.display(), entry.error)
                                })
                                .collect::<Vec<_>>();

                            torrent.latest_file_probe_status =
                                Some(TorrentFileProbeStatus::Files(problem_files));
                            if next_availability != was_available {
                                let saved_location =
                                    Self::torrent_saved_location(&torrent.latest_state);
                                availability_transition_log = Some((
                                    torrent.latest_state.torrent_name.clone(),
                                    next_availability,
                                    issue_count,
                                    saved_location,
                                    issue_files,
                                ));
                            }

                            if matches!(probe_result_availability, Some(false)) {
                                torrent.latest_state.data_available = false;
                                should_persist_unavailable |= was_available;
                            }
                            if matches!(probe_result_availability, Some(true)) && !was_available {
                                should_request_recovery = true;
                            }
                        }
                        None => {}
                    }
                }

                if should_notify_manager_unavailable {
                    if let Some(manager_tx) = self.torrent_manager_command_txs.get(&info_hash) {
                        let _ = manager_tx.try_send(ManagerCommand::SetDataAvailability(false));
                    }
                }
                if should_persist_unavailable && availability_transition_log.is_none() {
                    self.save_state_to_disk();
                }

                if let Some((
                    torrent_name,
                    is_available,
                    issue_count,
                    saved_location,
                    issue_files,
                )) = availability_transition_log
                {
                    if is_available {
                        tracing_event!(
                            Level::INFO,
                            info_hash = %hex::encode(&info_hash),
                            torrent = %torrent_name,
                            saved_location = ?saved_location,
                            "Torrent probe found data available; awaiting manager metrics confirmation"
                        );
                    } else {
                        tracing_event!(
                            Level::WARN,
                            info_hash = %hex::encode(&info_hash),
                            torrent = %torrent_name,
                            saved_location = ?saved_location,
                            issues = issue_count,
                            issue_files = ?issue_files,
                            "Torrent probe found data unavailable"
                        );
                        if should_persist_unavailable {
                            self.save_state_to_disk();
                        }
                    }

                    self.record_data_health_event(
                        &info_hash,
                        Some(torrent_name),
                        if is_available {
                            EventType::DataRecovered
                        } else {
                            EventType::DataUnavailable
                        },
                        issue_files,
                        if is_available {
                            "Torrent probe found data available".to_string()
                        } else {
                            format!(
                                "Torrent probe found data unavailable with {} issue(s)",
                                issue_count
                            )
                        },
                    );
                    if is_available || !should_persist_unavailable {
                        self.save_state_to_disk();
                    }
                }

                if should_request_recovery {
                    if let Some(manager_tx) = self.torrent_manager_command_txs.get(&info_hash) {
                        let _ = manager_tx.try_send(ManagerCommand::SetDataAvailability(true));
                    }
                }

                self.dispatch_integrity_probe_batches();
                self.app_state.ui.needs_redraw = true;
            }
            ManagerEvent::MetadataLoaded { info_hash, torrent } => {
                self.integrity_scheduler.on_metadata_loaded(&info_hash);

                let mut file_priorities = HashMap::new();
                if let Some(display) = self.app_state.torrents.get_mut(&info_hash) {
                    display.latest_state.is_multi_file = !torrent.info.files.is_empty();
                    display.latest_state.file_count = Some(torrent_file_count(&torrent));
                    display.latest_state.total_size = torrent.info.total_length().max(0) as u64;
                    file_priorities = display.latest_state.file_priorities.clone();
                    display.file_preview_tree =
                        build_torrent_preview_tree(torrent.file_list(), &file_priorities);
                }

                self.persist_torrent_metadata_snapshot(&info_hash, &torrent, &file_priorities);

                self.dispatch_integrity_probe_batches();

                if let FileBrowserMode::DownloadLocSelection {
                    preview_tree,
                    preview_state,
                    container_name,
                    original_name_backup,
                    use_container,
                    ..
                } = &mut self.app_state.ui.file_browser.browser_mode
                {
                    // 1. REDUNDANCY GUARD: Check if metadata was already processed
                    // If the tree is already populated, ignore subsequent peer metadata arrivals
                    if !preview_tree.is_empty() {
                        tracing::debug!(target: "superseedr", "Metadata already hydrated for {:?}, ignoring redundant peer update", hex::encode(&info_hash));
                        return;
                    }

                    // 2. Build the tree payloads
                    let file_list = torrent.file_list();
                    let payloads: Vec<(Vec<String>, TorrentPreviewPayload)> = file_list
                        .into_iter()
                        .enumerate()
                        .map(|(idx, (parts, size))| {
                            (
                                parts,
                                TorrentPreviewPayload {
                                    file_index: Some(idx),
                                    size,
                                    priority: FilePriority::Normal,
                                },
                            )
                        })
                        .collect();

                    // 3. Hydrate the tree structure
                    let has_multiple_files = payloads.len() > 1;
                    *preview_tree = RawNode::from_path_list(None, payloads);

                    // 4. Update Display Name and State
                    let info_hash_hex = hex::encode(&info_hash);
                    let name = format!("{} [{}]", torrent.info.name, &info_hash_hex);
                    *container_name = name.clone();
                    *original_name_backup = name;
                    *use_container = has_multiple_files;

                    // 5. INITIALIZE UI STATE: Set the initial cursor
                    if let Some(first) = preview_tree.first() {
                        preview_state.cursor_path = Some(std::path::PathBuf::from(&first.name));
                    }

                    // 6. Auto-expand all folders
                    for node in preview_tree.iter_mut() {
                        node.expand_all(preview_state);
                    }

                    // 7. Force UI redraw
                    self.app_state.ui.needs_redraw = true;
                    tracing::info!(target: "superseedr", "Magnet preview tree hydrated (first arrival)");
                }
            }
            ManagerEvent::DiskReadStarted { .. }
            | ManagerEvent::DiskReadFinished
            | ManagerEvent::DiskWriteStarted { .. }
            | ManagerEvent::DiskWriteFinished
            | ManagerEvent::DiskIoBackoff { .. }
            | ManagerEvent::PeerDiscovered { .. }
            | ManagerEvent::PeerConnectionFailed { .. }
            | ManagerEvent::TorrentPaused { .. }
            | ManagerEvent::TorrentResumed { .. }
            | ManagerEvent::PeerConnected { .. }
            | ManagerEvent::PeerDisconnected { .. }
            | ManagerEvent::BlockReceived { .. }
            | ManagerEvent::BlockSent { .. } => {}
        }
    }

    async fn handle_file_event(&mut self, result: Result<Event, notify::Error>) {
        match result {
            Ok(event) => {
                const DEBOUNCE_DURATION: Duration = Duration::from_millis(500);

                for path in event.paths {
                    if path.to_string_lossy().ends_with(".tmp") {
                        continue;
                    }

                    if let Some(cmd) = watcher::path_to_command(&path) {
                        self.enqueue_watch_command(cmd, DEBOUNCE_DURATION).await;
                    }
                }
            }
            Err(e) => {
                tracing_event!(Level::ERROR, "File watcher error: {}", e);
            }
        }
    }

    async fn handle_port_change(&mut self, path: PathBuf) {
        tracing_event!(Level::DEBUG, "Processing port file change...");
        let port_str = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing_event!(Level::ERROR, "Failed to read port file {:?}: {}", &path, e);
                return;
            }
        };

        match port_str.trim().parse::<u16>() {
            Ok(new_port) => {
                if new_port > 0 && new_port != self.client_configs.client_port {
                    tracing_event!(
                        Level::INFO,
                        "Port changed: {} -> {}. Attempting to re-bind listener.",
                        self.client_configs.client_port,
                        new_port
                    );

                    match ListenerSet::bind(new_port).await {
                        Ok(new_listener) => {
                            self.listener = Some(new_listener);
                            let bound_port = self
                                .listener
                                .as_ref()
                                .and_then(ListenerSet::local_port)
                                .unwrap_or(new_port);
                            self.client_configs.client_port = bound_port;

                            tracing_event!(
                                Level::INFO,
                                "Successfully bound to new port {}",
                                bound_port
                            );

                            // Persist the new port immediately
                            self.save_state_to_disk();

                            // Notify all running managers
                            for manager_tx in self.torrent_manager_command_txs.values() {
                                let _ = manager_tx
                                    .try_send(ManagerCommand::UpdateListenPort(bound_port));
                            }

                            tracing::event!(
                                Level::INFO,
                                "Reconfiguring DHT service for new port..."
                            );
                            self.dht_service
                                .reconfigure(DhtServiceConfig::from_settings(&self.client_configs));
                        }
                        Err(e) => {
                            tracing_event!(
                                Level::ERROR,
                                "Failed to bind to new port {}: {}. Retaining old listener.",
                                new_port,
                                e
                            );
                        }
                    }
                } else if new_port == self.client_configs.client_port {
                    tracing_event!(
                        Level::DEBUG,
                        "Port file updated, but port is unchanged ({}).",
                        new_port
                    );
                }
            }
            Err(e) => {
                tracing_event!(
                    Level::ERROR,
                    "Failed to parse new port from file {:?}: {}",
                    &path,
                    e
                );
            }
        }
    }

    fn calculate_stats(&mut self, sys: &mut System) {
        let was_seeding = self.app_state.is_seeding;
        UiTelemetry::on_second_tick(&mut self.app_state, sys);
        NetworkHistoryTelemetry::on_second_tick(&mut self.app_state);
        self.tuning_controller.on_second_tick();
        self.app_state.tuning_countdown = self.tuning_controller.countdown_secs();
        if was_seeding != self.app_state.is_seeding {
            self.reset_tuning_for_objective_change();
            self.sync_global_peer_manager_limits();

            let rm = self.resource_manager.clone();
            let limits_map = self.build_resource_manager_limits_map(&self.app_state.limits);
            tokio::spawn(async move {
                let _ = rm.update_limits(limits_map).await;
            });
        }

        let history = if !self.app_state.is_seeding {
            &self.app_state.avg_download_history
        } else {
            &self.app_state.avg_upload_history
        };
        let lookback = self.tuning_controller.lookback_secs();
        let relevant_history = &history[history.len().saturating_sub(lookback)..];
        self.tuning_controller.update_live_score(
            relevant_history,
            self.app_state.global_disk_thrash_score,
            self.app_state.adaptive_max_scpb,
        );
        self.sync_tuning_state_from_controller();
        ActivityHistoryTelemetry::on_second_tick(&mut self.app_state);
    }

    fn startup_network_history_restore(&mut self) {
        self.app_state.network_history_restore_pending = true;
        let tx = self.app_command_tx.clone();
        tokio::spawn(async move {
            let load_result = tokio::task::spawn_blocking(load_network_history_state).await;
            match load_result {
                Ok(state) => {
                    let _ = tx.send(AppCommand::NetworkHistoryLoaded(state)).await;
                }
                Err(e) => {
                    tracing_event!(
                        Level::ERROR,
                        "Network history restore task failed to join: {}",
                        e
                    );
                    let _ = tx
                        .send(AppCommand::NetworkHistoryLoaded(
                            NetworkHistoryPersistedState::default(),
                        ))
                        .await;
                }
            }
        });
    }

    fn startup_activity_history_restore(&mut self) {
        self.app_state.activity_history_restore_pending = true;
        let tx = self.app_command_tx.clone();
        tokio::spawn(async move {
            let load_result = tokio::task::spawn_blocking(load_activity_history_state).await;
            match load_result {
                Ok(state) => {
                    let _ = tx
                        .send(AppCommand::ActivityHistoryLoaded(Box::new(state)))
                        .await;
                }
                Err(e) => {
                    tracing_event!(
                        Level::ERROR,
                        "Activity history restore task failed to join: {}",
                        e
                    );
                    let _ = tx
                        .send(AppCommand::ActivityHistoryLoaded(Box::default()))
                        .await;
                }
            }
        });
    }

    fn drain_latest_torrent_metrics(&mut self) {
        let mut changed = false;
        let mut closed_info_hashes = Vec::new();
        let mut completion_events: Vec<(Vec<u8>, String)> = Vec::new();

        for (info_hash, rx) in self.torrent_metric_watch_rxs.iter_mut() {
            match rx.has_changed() {
                Ok(false) => {}
                Ok(true) => {
                    let was_complete = self
                        .app_state
                        .torrents
                        .get(info_hash)
                        .map(|torrent| !torrent_is_effectively_incomplete(&torrent.latest_state))
                        .unwrap_or(false);
                    let message = rx.borrow_and_update().clone();
                    UiTelemetry::on_metrics(&mut self.app_state, message);
                    let completion_record = self.app_state.torrents.get(info_hash).map(|torrent| {
                        (
                            !torrent_is_effectively_incomplete(&torrent.latest_state),
                            torrent.latest_state.torrent_name.clone(),
                        )
                    });
                    if let Some((is_complete, torrent_name)) = completion_record {
                        if !was_complete && is_complete {
                            tracing_event!(
                                Level::INFO,
                                info_hash = %hex::encode(info_hash.as_slice()),
                                torrent_name = %torrent_name,
                                "Torrent completion transition detected from metrics"
                            );
                            completion_events.push((info_hash.clone(), torrent_name));
                        }
                    }
                    changed = true;
                }
                Err(_) => {
                    closed_info_hashes.push(info_hash.clone());
                }
            }
        }

        for info_hash in closed_info_hashes {
            self.torrent_metric_watch_rxs.remove(&info_hash);
        }

        if !completion_events.is_empty() {
            for (info_hash, torrent_name) in completion_events {
                self.record_torrent_completed_event(&info_hash, Some(torrent_name));
            }
            self.save_state_to_disk();
        }

        if changed {
            self.sort_and_filter_torrent_list();
            // Keep RSS derived recomputation off the hot metrics path.
            // Full recompute is done on structural RSS changes (preview/filter/history/add/remove/search/edit).
            self.app_state.ui.needs_redraw = true;
        }
    }

    async fn tuning_resource_limits(&mut self) {
        let history = if !self.app_state.is_seeding {
            &self.app_state.avg_download_history
        } else {
            &self.app_state.avg_upload_history
        };

        let lookback = self.tuning_controller.lookback_secs();
        let relevant_history = &history[history.len().saturating_sub(lookback)..];
        let evaluation = self.tuning_controller.evaluate_cycle(
            &self.app_state.limits,
            relevant_history,
            self.app_state.global_disk_thrash_score,
            self.app_state.adaptive_max_scpb,
        );
        self.sync_tuning_state_from_controller();

        if evaluation.accepted_improvement {
            tracing_event!(
                Level::DEBUG,
                "Self-Tune: SUCCESS. New best score: {} (raw: {}, penalty: {:.2}x)",
                evaluation.new_score,
                evaluation.new_raw_score,
                evaluation.penalty_factor
            );
        } else {
            self.app_state.limits = evaluation.effective_limits.clone();
            if evaluation.reality_check_applied {
                tracing_event!(Level::DEBUG, "Self-Tune: REALITY CHECK. Score {} (raw: {}) failed. Old best {} is stale vs. baseline {}. Resetting best to baseline.", evaluation.new_score, evaluation.new_raw_score, evaluation.best_score_before, evaluation.baseline_u64);
            } else {
                tracing_event!(Level::DEBUG, "Self-Tune: REVERTING. Score {} (raw: {}, penalty: {:.2}x) was not better than {}. (Baseline is {})", evaluation.new_score, evaluation.new_raw_score, evaluation.penalty_factor, evaluation.best_score_before, evaluation.baseline_u64);
            }

            let _ = self
                .resource_manager
                .update_limits(self.build_resource_manager_limits_map(&self.app_state.limits))
                .await;
            self.sync_global_peer_manager_limits();
        }

        let (next_limits, desc) =
            make_random_adjustment(self.app_state.limits.clone(), self.app_state.is_seeding);
        self.app_state.limits = next_limits;

        tracing_event!(Level::DEBUG, "Self-Tune: Trying next change... {}", desc);
        let _ = self
            .resource_manager
            .update_limits(self.build_resource_manager_limits_map(&self.app_state.limits))
            .await;
        self.sync_global_peer_manager_limits();
    }

    fn reschedule_tuning_deadline(&mut self) {
        self.next_tuning_at =
            time::Instant::now() + Duration::from_secs(self.tuning_controller.cadence_secs());
    }

    fn reset_tuning_for_objective_change(&mut self) {
        self.app_state.limits =
            normalize_limits_for_mode(&self.app_state.limits, self.app_state.is_seeding);
        self.tuning_controller
            .reset_for_objective_change(&self.app_state.limits);
        self.sync_tuning_state_from_controller();
        self.reschedule_tuning_deadline();
        self.sync_global_peer_manager_limits();
    }

    fn sync_tuning_state_from_controller(&mut self) {
        let state = self.tuning_controller.state();
        self.app_state.last_tuning_score = state.last_tuning_score;
        self.app_state.current_tuning_score = state.current_tuning_score;
        self.app_state.last_tuning_limits = state.last_tuning_limits.clone();
        self.app_state.baseline_speed_ema = state.baseline_speed_ema;
        self.app_state.tuning_countdown = self.tuning_controller.countdown_secs();
    }

    fn save_state_to_disk(&mut self) {
        if !self.cluster_capabilities().can_persist_local_runtime_state {
            return;
        }

        let payload = build_persist_payload(
            &mut self.client_configs,
            &mut self.app_state,
            &self.startup_deferred_load_queue,
        );
        let network_history_request_id = payload
            .network_history
            .as_ref()
            .map(|request| request.request_id);
        let activity_history_request_id = payload
            .activity_history
            .as_ref()
            .map(|request| request.request_id);

        if queue_persistence_payload(self.persistence_tx.as_ref(), payload).is_ok() {
            self.app_state.pending_network_history_persist_request_id = network_history_request_id;
            self.app_state.pending_activity_history_persist_request_id =
                activity_history_request_id;
        } else {
            tracing_event!(
                Level::ERROR,
                "Failed to queue persistence payload: persistence task unavailable"
            );
        }
    }

    fn torrent_saved_location(metrics: &TorrentMetrics) -> Option<PathBuf> {
        let download_path = metrics.download_path.as_ref()?;

        match metrics.container_name.as_deref() {
            Some(container_name) if !container_name.is_empty() => {
                Some(download_path.join(container_name))
            }
            // Explicit empty-container multi-file torrents save directly into the root directory.
            Some(_) if metrics.is_multi_file => Some(download_path.clone()),
            // Flat payloads need a torrent-specific identity rather than the shared parent folder.
            _ => Some(download_path.join(&metrics.torrent_name)),
        }
    }

    fn current_integrity_snapshots(&self) -> Vec<TorrentIntegritySnapshot> {
        self.app_state
            .torrents
            .iter()
            .filter_map(|(info_hash, torrent)| {
                if torrent.latest_state.torrent_control_state == TorrentControlState::Deleting {
                    return None;
                }

                Some(TorrentIntegritySnapshot {
                    info_hash: info_hash.clone(),
                    data_available: torrent.latest_state.data_available,
                    is_downloading: !torrent.latest_state.is_complete,
                    file_count: torrent.latest_state.file_count,
                    saved_location: Self::torrent_saved_location(&torrent.latest_state),
                    download_speed_bps: torrent.latest_state.download_speed_bps,
                    upload_speed_bps: torrent.latest_state.upload_speed_bps,
                })
            })
            .collect()
    }

    fn dispatch_integrity_probe_batches(&mut self) {
        self.integrity_scheduler
            .sync_torrents(self.current_integrity_snapshots());

        for request in self.integrity_scheduler.drain_due_probe_requests() {
            let send_result = self
                .torrent_manager_command_txs
                .get(&request.info_hash)
                .map(|manager_tx| {
                    manager_tx.try_send(ManagerCommand::ProbeFileBatch {
                        epoch: request.epoch,
                        start_file_index: request.start_file_index,
                        max_files: request.max_files,
                    })
                });

            match send_result {
                Some(Ok(())) => {}
                _ => self
                    .integrity_scheduler
                    .on_dispatch_failed(&request.info_hash),
            }
        }

        self.sync_integrity_probe_deadlines();
    }

    fn advance_integrity_scheduler(&mut self, dt: Duration) {
        self.integrity_scheduler.advance_time(dt);
        self.dispatch_integrity_probe_batches();
    }

    fn sync_integrity_probe_deadlines(&mut self) {
        let probe_deadlines: Vec<(Vec<u8>, Option<Duration>)> = self
            .app_state
            .torrents
            .keys()
            .cloned()
            .map(|info_hash| {
                let next_probe_in = self.integrity_scheduler.next_probe_in(&info_hash);
                (info_hash, next_probe_in)
            })
            .collect();

        for (info_hash, next_probe_in) in probe_deadlines {
            if let Some(torrent) = self.app_state.torrents.get_mut(&info_hash) {
                torrent.integrity_next_probe_in = next_probe_in;
            }
        }
    }

    // Constantly ensures all table selected indices are in-bounds
    fn clamp_selected_indices(&mut self) {
        clamp_selected_indices_in_state(&mut self.app_state);
    }

    pub fn sort_and_filter_torrent_list(&mut self) {
        sort_and_filter_torrent_list_state(&mut self.app_state);
    }

    pub fn find_most_common_download_path(&mut self) -> Option<PathBuf> {
        let mut counts: HashMap<PathBuf, usize> = HashMap::new();

        for state in self.app_state.torrents.values() {
            if let Some(download_path) = &state.latest_state.download_path {
                if let Some(parent_path) = download_path.parent() {
                    *counts.entry(parent_path.to_path_buf()).or_insert(0) += 1;
                }
            }
        }

        counts
            .into_iter()
            .max_by_key(|&(_, count)| count)
            .map(|(path, _)| path)
    }

    pub fn get_initial_source_path(&self) -> PathBuf {
        UserDirs::new()
            .and_then(|ud| ud.download_dir().map(|p| p.to_path_buf()))
            .or_else(|| UserDirs::new().map(|ud| ud.home_dir().to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    pub fn get_initial_destination_path(&mut self) -> PathBuf {
        self.find_most_common_download_path()
            .or_else(|| UserDirs::new().and_then(|ud| ud.download_dir().map(|p| p.to_path_buf())))
            .or_else(|| UserDirs::new().map(|ud| ud.home_dir().to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    pub async fn add_torrent_from_file(
        &mut self,
        path: PathBuf,
        download_path: Option<PathBuf>,
        is_validated: bool,
        torrent_control_state: TorrentControlState,
        file_priorities: HashMap<usize, FilePriority>,
        container_name: Option<String>,
    ) -> CommandIngestResult {
        let buffer = match fs::read(&path) {
            Ok(buf) => buf,
            Err(e) => {
                let message =
                    format_filesystem_path_error("Failed to read torrent file", &path, &e);
                tracing_event!(Level::ERROR, "{}", message);
                return CommandIngestResult::Failed {
                    info_hash: None,
                    torrent_name: None,
                    message,
                };
            }
        };

        let torrent = match from_bytes(&buffer) {
            Ok(t) => t,
            Err(e) => {
                let file_size = buffer.len();
                let head_len = file_size.min(24);
                let tail_len = file_size.min(24);
                let head_hex = hex::encode(&buffer[..head_len]);
                let tail_hex = hex::encode(&buffer[file_size.saturating_sub(tail_len)..]);
                let likely_cause = if e.to_string().contains("End of stream") {
                    "likely truncated/incomplete .torrent file"
                } else {
                    "malformed or unsupported bencode payload"
                };
                let message = format!(
                    "Failed to parse torrent file {:?}: {} | size={} bytes | head={} | tail={} | hint={}",
                    &path, e, file_size, head_hex, tail_hex, likely_cause
                );
                tracing_event!(Level::ERROR, "{}", message);
                return CommandIngestResult::Invalid {
                    info_hash: None,
                    torrent_name: None,
                    message,
                };
            }
        };

        #[cfg(all(feature = "dht", feature = "pex"))]
        {
            if torrent.info.private == Some(1) {
                let message = format!(
                    "Rejected private torrent '{}' in normal build.",
                    torrent.info.name
                );
                tracing_event!(Level::ERROR, "{}", message);
                self.app_state.system_error = Some(format!(
                    "Private Torrent Rejected:'{}' This build (with DHT/PEX) is not safe for private trackers. Please use private builds for this torrent.",
                    torrent.info.name
                ));
                return CommandIngestResult::Failed {
                    info_hash: None,
                    torrent_name: Some(torrent.info.name.clone()),
                    message,
                };
            }
        }

        let info_hash = if torrent.info.meta_version == Some(2) {
            if !torrent.info.pieces.is_empty() {
                let mut hasher = sha1::Sha1::new();
                hasher.update(&torrent.info_dict_bencode);
                hasher.finalize().to_vec()
            } else {
                // Pure V2 -> Primary is V2 (SHA-256 Truncated)
                let mut hasher = Sha256::new();
                hasher.update(&torrent.info_dict_bencode);
                hasher.finalize()[0..20].to_vec()
            }
        } else {
            // V1 -> SHA-1
            let mut hasher = sha1::Sha1::new();
            hasher.update(&torrent.info_dict_bencode);
            hasher.finalize().to_vec()
        };

        if self.app_state.torrents.contains_key(&info_hash) {
            if !self.has_live_runtime_for_torrent(&info_hash) {
                self.clear_display_only_torrent(&info_hash);
            } else {
                let message = format!("Ignoring already present torrent: {}", torrent.info.name);
                tracing_event!(Level::INFO, "{}", message);
                return CommandIngestResult::Duplicate {
                    info_hash: Some(info_hash),
                    torrent_name: Some(torrent.info.name),
                };
            }
        }

        let torrent_files_dir = match crate::config::runtime_data_dir() {
            Some(data_dir) => data_dir.join("torrents"),
            None => {
                let message = "Could not determine application data directory.".to_string();
                tracing_event!(Level::ERROR, "{}", message);
                return CommandIngestResult::Failed {
                    info_hash: Some(info_hash),
                    torrent_name: Some(torrent.info.name.clone()),
                    message,
                };
            }
        };
        if let Err(e) = fs::create_dir_all(&torrent_files_dir) {
            let message = format!("Could not create torrents data directory: {}", e);
            tracing_event!(Level::ERROR, "{}", message);
            return CommandIngestResult::Failed {
                info_hash: Some(info_hash),
                torrent_name: Some(torrent.info.name.clone()),
                message,
            };
        }
        let permanent_torrent_path =
            torrent_files_dir.join(format!("{}.torrent", hex::encode(&info_hash)));
        let shared_torrent_path = crate::config::shared_torrent_file_path(&info_hash);

        let persist_torrent_copy = |destination: &PathBuf, label: &str| -> std::io::Result<()> {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }

            let temp_torrent_path =
                destination.with_extension(format!("torrent.{}.tmp", std::process::id()));
            fs::write(&temp_torrent_path, &buffer)?;
            if let Err(e) = fs::rename(&temp_torrent_path, destination) {
                if e.kind() == ErrorKind::AlreadyExists {
                    if let Err(remove_err) = fs::remove_file(destination) {
                        if remove_err.kind() != ErrorKind::NotFound {
                            let _ = fs::remove_file(&temp_torrent_path);
                            return Err(remove_err);
                        }
                    }
                    if let Err(retry_err) = fs::rename(&temp_torrent_path, destination) {
                        let _ = fs::remove_file(&temp_torrent_path);
                        return Err(retry_err);
                    }
                } else {
                    let _ = fs::remove_file(&temp_torrent_path);
                    return Err(e);
                }
            }

            tracing_event!(
                Level::DEBUG,
                "Persisted torrent file copy in {}: {:?}",
                label,
                destination
            );
            Ok(())
        };

        if let Err(e) = persist_torrent_copy(&permanent_torrent_path, "data directory") {
            let message = format!("Failed to persist torrent copy in data directory: {}", e);
            tracing_event!(Level::ERROR, "{}", message);
            return CommandIngestResult::Failed {
                info_hash: Some(info_hash),
                torrent_name: Some(torrent.info.name.clone()),
                message,
            };
        }

        if self.can_write_shared_state() {
            if let Some(shared_path) = &shared_torrent_path {
                if let Err(e) = persist_torrent_copy(shared_path, "shared config directory") {
                    let message = format!(
                        "Failed to persist torrent copy in shared config directory: {}",
                        e
                    );
                    tracing_event!(Level::ERROR, "{}", message);
                    return CommandIngestResult::Failed {
                        info_hash: Some(info_hash),
                        torrent_name: Some(torrent.info.name.clone()),
                        message,
                    };
                }
            }
        }

        self.persist_torrent_metadata_snapshot(&info_hash, &torrent, &file_priorities);

        let number_of_pieces_total = if !torrent.info.pieces.is_empty() {
            (torrent.info.pieces.len() / 20) as u32
        } else {
            // Handle v2 torrents (empty pieces list)
            let total_len = torrent.info.total_length();
            if torrent.info.piece_length > 0 {
                // ceil(total_len / piece_length)
                ((total_len as f64) / (torrent.info.piece_length as f64)).ceil() as u32
            } else {
                0
            }
        };

        let resolved_torrent_name = torrent.info.name.clone();
        let placeholder_state = TorrentDisplayState {
            latest_state: TorrentMetrics {
                torrent_control_state: torrent_control_state.clone(),
                delete_files: false,
                info_hash: info_hash.clone(),
                torrent_or_magnet: shared_torrent_path
                    .clone()
                    .unwrap_or_else(|| permanent_torrent_path.clone())
                    .to_string_lossy()
                    .to_string(),
                torrent_name: resolved_torrent_name.clone(),
                download_path: download_path.clone(),
                container_name: container_name.clone(),
                is_complete: is_validated,
                is_multi_file: !torrent.info.files.is_empty(),
                file_count: Some(torrent_file_count(&torrent)),
                number_of_pieces_total,
                file_priorities: file_priorities.clone(),
                ..Default::default()
            },
            file_preview_tree: build_torrent_preview_tree(torrent.file_list(), &file_priorities),
            ..Default::default()
        };
        self.app_state
            .torrents
            .insert(info_hash.clone(), placeholder_state);
        self.app_state.torrent_list_order.push(info_hash.clone());
        self.refresh_rss_derived();

        if matches!(self.app_state.mode, AppMode::Welcome) {
            self.app_state.mode = AppMode::Normal;
        }

        let (incoming_peer_tx, incoming_peer_rx) = mpsc::channel::<IncomingPeerSession>(100);
        self.torrent_manager_incoming_peer_txs
            .insert(info_hash.clone(), incoming_peer_tx);
        let (manager_command_tx, manager_command_rx) = mpsc::channel::<ManagerCommand>(100);
        self.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_command_tx);

        let (torrent_metrics_tx, torrent_metrics_rx) = watch::channel(TorrentMetrics::default());
        self.torrent_metric_watch_rxs
            .insert(info_hash.clone(), torrent_metrics_rx);
        let manager_event_tx_clone = self.manager_event_tx.clone();
        let resource_manager_clone = self.resource_manager.clone();
        let global_dl_bucket_clone = self.global_dl_bucket.clone();
        let global_ul_bucket_clone = self.global_ul_bucket.clone();

        let dht_clone = self.dht_service.handle();

        let torrent_params = TorrentParameters {
            dht_handle: dht_clone,
            incoming_peer_rx,
            metrics_tx: torrent_metrics_tx,
            torrent_validation_status: is_validated,
            torrent_data_path: download_path,
            container_name: container_name.clone(),
            manager_command_rx,
            manager_event_tx: manager_event_tx_clone,
            settings: Arc::clone(&Arc::new(self.client_configs.clone())),
            resource_manager: resource_manager_clone,
            global_dl_bucket: global_dl_bucket_clone,
            global_ul_bucket: global_ul_bucket_clone,
            file_priorities: file_priorities.clone(),
        };
        let start_paused = torrent_control_state == TorrentControlState::Paused;
        let should_announce_on_add = torrent_control_state == TorrentControlState::Running
            && (self.app_state.externally_accessable_port_v4
                || self.app_state.externally_accessable_port_v6);

        match TorrentManager::from_torrent(torrent_params, torrent) {
            Ok(torrent_manager) => {
                tokio::spawn(async move {
                    let _ = torrent_manager.run(start_paused).await;
                });
                if should_announce_on_add {
                    self.announce_torrents_to_dht(std::iter::once(info_hash.clone()));
                }
                tracing_event!(
                    Level::INFO,
                    info_hash = %hex::encode(&info_hash),
                    torrent_name = %resolved_torrent_name,
                    torrent_count = self.app_state.torrents.len(),
                    has_runtime_entry = self.app_state.torrents.contains_key(&info_hash),
                    "Magnet torrent manager created successfully"
                );
                self.dispatch_integrity_probe_batches();
                CommandIngestResult::Added {
                    info_hash: Some(info_hash),
                    torrent_name: Some(resolved_torrent_name),
                }
            }
            Err(e) => {
                let message = format!("Failed to create torrent manager from file: {:?}", e);
                tracing_event!(Level::ERROR, "{}", message);
                self.app_state.torrents.remove(&info_hash);
                self.app_state
                    .torrent_list_order
                    .retain(|ih| *ih != info_hash);
                self.torrent_metric_watch_rxs.remove(&info_hash);
                self.refresh_rss_derived();
                CommandIngestResult::Failed {
                    info_hash: Some(info_hash),
                    torrent_name: Some(resolved_torrent_name),
                    message,
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn add_magnet_torrent(
        &mut self,
        torrent_name: String,
        magnet_link: String,
        download_path: Option<PathBuf>,
        is_validated: bool,
        torrent_control_state: TorrentControlState,
        file_priorities: HashMap<usize, FilePriority>,
        container_name: Option<String>,
    ) -> CommandIngestResult {
        tracing::info!(target: "magnet_flow", "Engine: add_magnet_torrent entry. Link: {}", magnet_link);
        let magnet = match Magnet::new(&magnet_link) {
            Ok(m) => m,
            Err(e) => {
                let message = format!("Could not parse invalid magnet: {:?}", e);
                tracing_event!(Level::ERROR, "Could not parse invalid magnet: {:?}", e);
                return CommandIngestResult::Invalid {
                    info_hash: None,
                    torrent_name: None,
                    message,
                };
            }
        };

        let (v1_hash, v2_hash) = parse_hybrid_hashes(&magnet_link);
        let Some(info_hash) = v1_hash.clone().or_else(|| v2_hash.clone()) else {
            let message = "Magnet link is missing both btih and btmh hashes".to_string();
            tracing_event!(Level::ERROR, "{}", message);
            return CommandIngestResult::Invalid {
                info_hash: None,
                torrent_name: None,
                message,
            };
        };
        let resolved_name = resolve_magnet_torrent_name(&torrent_name, &magnet_link, &info_hash);
        let resolved_torrent_name = resolved_name.clone();

        if self.app_state.torrents.contains_key(&info_hash) {
            if !self.has_live_runtime_for_torrent(&info_hash) {
                self.clear_display_only_torrent(&info_hash);
            } else {
                if let Some(path) = download_path {
                    if let Some(manager_tx) = self.torrent_manager_command_txs.get(&info_hash) {
                        let _ = manager_tx.try_send(ManagerCommand::SetUserTorrentConfig {
                            torrent_data_path: path,
                            file_priorities: file_priorities.clone(),
                            container_name,
                        });
                    }
                }
                tracing_event!(Level::INFO, "Updated path for existing torrent from magnet");
                return CommandIngestResult::Duplicate {
                    info_hash: Some(info_hash),
                    torrent_name: Some(resolved_name),
                };
            }
        }

        let placeholder_state = TorrentDisplayState {
            latest_state: TorrentMetrics {
                torrent_control_state: torrent_control_state.clone(),
                delete_files: false,
                info_hash: info_hash.clone(),
                torrent_or_magnet: magnet_link.clone(),
                torrent_name: resolved_name.clone(),
                download_path: download_path.clone(),
                container_name: container_name.clone(),
                is_complete: is_validated,
                is_multi_file: false,
                file_count: None,
                ..Default::default()
            },
            ..Default::default()
        };
        self.app_state
            .torrents
            .insert(info_hash.clone(), placeholder_state);
        self.app_state.torrent_list_order.push(info_hash.clone());
        self.refresh_rss_derived();

        if matches!(self.app_state.mode, AppMode::Welcome) {
            self.app_state.mode = AppMode::Normal;
        }

        let (incoming_peer_tx, incoming_peer_rx) = mpsc::channel::<IncomingPeerSession>(100);
        self.torrent_manager_incoming_peer_txs
            .insert(info_hash.clone(), incoming_peer_tx);
        let (manager_command_tx, manager_command_rx) = mpsc::channel::<ManagerCommand>(100);
        self.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_command_tx);

        let dht_clone = self.dht_service.handle();
        let (torrent_metrics_tx, torrent_metrics_rx) = watch::channel(TorrentMetrics::default());
        self.torrent_metric_watch_rxs
            .insert(info_hash.clone(), torrent_metrics_rx);
        let manager_event_tx_clone = self.manager_event_tx.clone();
        let resource_manager_clone = self.resource_manager.clone();
        let global_dl_bucket_clone = self.global_dl_bucket.clone();
        let global_ul_bucket_clone = self.global_ul_bucket.clone();
        let torrent_params = TorrentParameters {
            dht_handle: dht_clone,
            incoming_peer_rx,
            metrics_tx: torrent_metrics_tx,
            torrent_validation_status: is_validated,
            torrent_data_path: download_path.clone(),
            container_name: container_name.clone(),
            manager_command_rx,
            manager_event_tx: manager_event_tx_clone,
            settings: Arc::clone(&Arc::new(self.client_configs.clone())),
            resource_manager: resource_manager_clone,
            global_dl_bucket: global_dl_bucket_clone,
            global_ul_bucket: global_ul_bucket_clone,
            file_priorities: file_priorities.clone(),
        };
        let start_paused = torrent_control_state == TorrentControlState::Paused;
        let should_announce_on_add = torrent_control_state == TorrentControlState::Running
            && (self.app_state.externally_accessable_port_v4
                || self.app_state.externally_accessable_port_v6);

        match TorrentManager::from_magnet(torrent_params, magnet, &magnet_link) {
            Ok(torrent_manager) => {
                tokio::spawn(async move {
                    let _ = torrent_manager.run(start_paused).await;
                });
                if should_announce_on_add {
                    self.announce_torrents_to_dht(std::iter::once(info_hash.clone()));
                }
                self.dispatch_integrity_probe_batches();
                CommandIngestResult::Added {
                    info_hash: Some(info_hash),
                    torrent_name: Some(resolved_torrent_name),
                }
            }
            Err(e) => {
                let message = format!("Failed to create new torrent manager from magnet: {:?}", e);
                tracing_event!(Level::ERROR, "{}", message);
                self.app_state.torrents.remove(&info_hash);
                self.app_state
                    .torrent_list_order
                    .retain(|ih| *ih != info_hash);
                self.torrent_metric_watch_rxs.remove(&info_hash);
                self.refresh_rss_derived();
                CommandIngestResult::Failed {
                    info_hash: Some(info_hash),
                    torrent_name: Some(resolved_name),
                    message,
                }
            }
        }
    }

    fn source_watch_folder_for_path(&self, path: &std::path::Path) -> Option<PathBuf> {
        path.parent().map(Path::to_path_buf)
    }

    fn has_live_runtime_for_torrent(&self, info_hash: &[u8]) -> bool {
        self.torrent_manager_command_txs.contains_key(info_hash)
    }

    fn clear_display_only_torrent(&mut self, info_hash: &[u8]) {
        if self.has_live_runtime_for_torrent(info_hash) {
            return;
        }

        self.app_state.torrents.remove(info_hash);
        self.app_state
            .torrent_list_order
            .retain(|existing| existing.as_slice() != info_hash);
    }

    fn is_host_watch_path(&self, path: &Path) -> bool {
        let Some(host_watch) = resolve_host_watch_path(&self.client_configs) else {
            return false;
        };
        watched_parent_matches(path, &host_watch)
    }

    fn is_shared_inbox_path(&self, path: &Path) -> bool {
        let Some(shared_inbox) = shared_inbox_path() else {
            return false;
        };
        watched_parent_matches(path, &shared_inbox)
    }

    fn relay_local_watch_file(&mut self, path: &Path, fallback_extension: &str) {
        match relay_watch_file_to_shared_inbox(path) {
            Ok(relayed_path) => {
                tracing_event!(
                    Level::INFO,
                    "Relayed local watch file {:?} to shared inbox {:?}",
                    path,
                    relayed_path
                );
            }
            Err(error) => {
                tracing_event!(
                    Level::WARN,
                    "Failed to relay local watch file {:?}: {}",
                    path,
                    error
                );
                if let Err(archive_error) = archive_watch_file(path, fallback_extension) {
                    tracing_event!(
                        Level::WARN,
                        "Failed to archive local watch file {:?}: {}",
                        path,
                        archive_error
                    );
                }
            }
        }
    }

    fn append_event_journal_entry(&mut self, entry: EventJournalEntry) {
        append_event_journal_entry(&mut self.app_state.event_journal_state, entry);
    }

    fn control_event_scope(&self) -> EventScope {
        if crate::config::is_shared_config_mode() {
            EventScope::Shared
        } else {
            EventScope::Host
        }
    }

    fn persist_torrent_metadata_snapshot(
        &self,
        info_hash: &[u8],
        torrent: &crate::torrent_file::Torrent,
        file_priorities: &HashMap<usize, FilePriority>,
    ) {
        if !self.cluster_capabilities().can_write_shared_state {
            return;
        }

        let entry = TorrentMetadataEntry {
            info_hash_hex: hex::encode(info_hash),
            torrent_name: torrent.info.name.clone(),
            total_size: torrent.info.total_length().max(0) as u64,
            is_multi_file: !torrent.info.files.is_empty(),
            files: torrent
                .file_list()
                .into_iter()
                .map(|(parts, length)| TorrentMetadataFileEntry {
                    relative_path: parts.join("/"),
                    length,
                })
                .collect(),
            file_priorities: file_priorities.clone(),
        };

        if let Err(error) = upsert_torrent_metadata(entry) {
            tracing_event!(
                Level::WARN,
                "Failed to persist torrent metadata snapshot: {}",
                error
            );
        }
    }

    fn record_ingest_queued(
        &mut self,
        path: PathBuf,
        origin: IngestOrigin,
        ingest_kind: IngestKind,
        source_watch_folder: Option<PathBuf>,
    ) -> bool {
        if self.app_state.pending_ingest_by_path.contains_key(&path) {
            return false;
        }

        let correlation_id = event_correlation_id_for_path(&path);
        self.app_state.pending_ingest_by_path.insert(
            path.clone(),
            PendingIngestRecord {
                correlation_id: correlation_id.clone(),
                origin,
                ingest_kind,
                source_watch_folder: source_watch_folder.clone(),
                source_path: path.clone(),
            },
        );
        self.append_event_journal_entry(EventJournalEntry {
            host_id: self.event_journal_host_id.clone(),
            ts_iso: chrono::Utc::now().to_rfc3339(),
            category: EventCategory::Ingest,
            event_type: EventType::IngestQueued,
            source_watch_folder,
            source_path: Some(path),
            correlation_id: Some(correlation_id),
            message: Some("Queued ingest item".to_string()),
            details: EventDetails::Ingest {
                origin,
                ingest_kind,
            },
            ..Default::default()
        });
        true
    }

    fn record_watch_path_discovered(&mut self, path: &Path) {
        if let Some(ingest_kind) = ingest_kind_from_path(path) {
            if self.record_ingest_queued(
                path.to_path_buf(),
                IngestOrigin::WatchFolder,
                ingest_kind,
                self.source_watch_folder_for_path(path),
            ) {
                self.save_state_to_disk();
            }
        }
    }

    fn record_rss_queued(&mut self, path: PathBuf, origin: IngestOrigin, ingest_kind: IngestKind) {
        if self.record_ingest_queued(path, origin, ingest_kind, shared_inbox_path()) {
            self.save_state_to_disk();
        }
    }

    fn control_origin_for_command_path(&self, path: &Path) -> ControlOrigin {
        if self.is_shared_inbox_path(path) {
            ControlOrigin::SharedRelay
        } else if self.is_host_watch_path(path) {
            ControlOrigin::WatchFolder
        } else {
            ControlOrigin::CliOnline
        }
    }

    fn control_origin_for_ingest_path(&self, path: &Path) -> ControlOrigin {
        match self
            .app_state
            .pending_ingest_by_path
            .get(path)
            .map(|record| record.origin)
        {
            Some(IngestOrigin::RssAuto) => ControlOrigin::RssAuto,
            Some(IngestOrigin::RssManual) => ControlOrigin::RssManual,
            Some(IngestOrigin::WatchFolder) | None => ControlOrigin::WatchFolder,
        }
    }

    fn record_control_queued(
        &mut self,
        path: PathBuf,
        request: ControlRequest,
        origin: ControlOrigin,
    ) -> bool {
        if self.app_state.pending_control_by_path.contains_key(&path) {
            return false;
        }

        let correlation_id = event_correlation_id_for_path(&path);
        let source_watch_folder = self.source_watch_folder_for_path(&path);
        self.app_state.pending_control_by_path.insert(
            path.clone(),
            PendingControlRecord {
                correlation_id: correlation_id.clone(),
                request: request.clone(),
                origin,
                source_watch_folder: source_watch_folder.clone(),
                source_path: path.clone(),
            },
        );
        self.append_event_journal_entry(EventJournalEntry {
            scope: self.control_event_scope(),
            host_id: self.event_journal_host_id.clone(),
            ts_iso: chrono::Utc::now().to_rfc3339(),
            category: EventCategory::Control,
            event_type: EventType::ControlQueued,
            source_watch_folder,
            source_path: Some(path),
            correlation_id: Some(correlation_id),
            message: Some(format!("Queued control action '{}'", request.action_name())),
            details: control_event_details(&request, origin),
            ..Default::default()
        });
        true
    }

    fn record_control_result(
        &mut self,
        path: &PathBuf,
        request: &ControlRequest,
        result: Result<String, String>,
    ) {
        let pending = self.app_state.pending_control_by_path.remove(path);
        let correlation_id = pending
            .as_ref()
            .map(|record| record.correlation_id.clone())
            .unwrap_or_else(|| event_correlation_id_for_path(path));
        let (source_watch_folder, source_path, request, origin) = pending
            .map(|record| {
                (
                    record.source_watch_folder,
                    Some(record.source_path),
                    record.request,
                    record.origin,
                )
            })
            .unwrap_or_else(|| {
                (
                    self.source_watch_folder_for_path(path),
                    Some(path.clone()),
                    request.clone(),
                    self.control_origin_for_command_path(path),
                )
            });
        let (event_type, message) = match result {
            Ok(message) => (EventType::ControlApplied, Some(message)),
            Err(message) => (EventType::ControlFailed, Some(message)),
        };
        self.append_event_journal_entry(EventJournalEntry {
            scope: self.control_event_scope(),
            host_id: self.event_journal_host_id.clone(),
            ts_iso: chrono::Utc::now().to_rfc3339(),
            category: EventCategory::Control,
            event_type,
            source_watch_folder,
            source_path,
            correlation_id: Some(correlation_id),
            message,
            details: control_event_details(&request, origin),
            ..Default::default()
        });
    }

    fn record_ingest_result(&mut self, path: &PathBuf, result: &CommandIngestResult) {
        let pending = self.app_state.pending_ingest_by_path.remove(path);
        let fallback_kind = ingest_kind_from_path(path).unwrap_or_default();
        let correlation_id = pending
            .as_ref()
            .map(|record| record.correlation_id.clone())
            .unwrap_or_else(|| event_correlation_id_for_path(path));
        let (origin, ingest_kind, source_watch_folder, source_path) = pending
            .map(|record| {
                (
                    record.origin,
                    record.ingest_kind,
                    record.source_watch_folder,
                    Some(record.source_path),
                )
            })
            .unwrap_or_else(|| {
                (
                    IngestOrigin::WatchFolder,
                    fallback_kind,
                    self.source_watch_folder_for_path(path),
                    Some(path.clone()),
                )
            });

        let (event_type, torrent_name, info_hash_hex, message) = match result {
            CommandIngestResult::Added {
                info_hash,
                torrent_name,
            } => (
                EventType::IngestAdded,
                torrent_name.clone(),
                info_hash.as_ref().map(hex::encode),
                Some("Added torrent from ingest item".to_string()),
            ),
            CommandIngestResult::Duplicate {
                info_hash,
                torrent_name,
            } => (
                EventType::IngestDuplicate,
                torrent_name.clone(),
                info_hash.as_ref().map(hex::encode),
                Some("Ignored duplicate ingest item".to_string()),
            ),
            CommandIngestResult::Invalid {
                info_hash,
                torrent_name,
                message,
            } => (
                EventType::IngestInvalid,
                torrent_name.clone(),
                info_hash.as_ref().map(hex::encode),
                Some(message.clone()),
            ),
            CommandIngestResult::Failed {
                info_hash,
                torrent_name,
                message,
            } => (
                EventType::IngestFailed,
                torrent_name.clone(),
                info_hash.as_ref().map(hex::encode),
                Some(message.clone()),
            ),
        };

        self.append_event_journal_entry(EventJournalEntry {
            host_id: self.event_journal_host_id.clone(),
            ts_iso: chrono::Utc::now().to_rfc3339(),
            category: EventCategory::Ingest,
            event_type,
            torrent_name,
            info_hash_hex,
            source_watch_folder,
            source_path,
            correlation_id: Some(correlation_id),
            message,
            details: EventDetails::Ingest {
                origin,
                ingest_kind,
            },
            ..Default::default()
        });
    }

    fn record_data_health_event(
        &mut self,
        info_hash: &[u8],
        torrent_name: Option<String>,
        event_type: EventType,
        issue_files: Vec<String>,
        message: String,
    ) {
        self.append_event_journal_entry(EventJournalEntry {
            host_id: self.event_journal_host_id.clone(),
            ts_iso: chrono::Utc::now().to_rfc3339(),
            category: EventCategory::DataHealth,
            event_type,
            torrent_name,
            info_hash_hex: Some(hex::encode(info_hash)),
            message: Some(message),
            details: EventDetails::DataHealth {
                issue_count: issue_files.len(),
                issue_files,
            },
            ..Default::default()
        });
    }

    fn record_torrent_completed_event(&mut self, info_hash: &[u8], torrent_name: Option<String>) {
        let info_hash_hex = hex::encode(info_hash);
        if self.startup_completion_suppressed_hashes.remove(info_hash) {
            tracing_event!(
                Level::INFO,
                info_hash = %info_hash_hex,
                torrent_name = %torrent_name.clone().unwrap_or_default(),
                "Skipping startup TorrentCompleted journal entry for restored complete torrent"
            );
            return;
        }
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
            tracing_event!(
                Level::INFO,
                info_hash = %info_hash_hex,
                torrent_name = %torrent_name.clone().unwrap_or_default(),
                "Skipping duplicate TorrentCompleted journal entry"
            );
            return;
        }

        tracing_event!(
            Level::INFO,
            info_hash = %info_hash_hex,
            torrent_name = %torrent_name.clone().unwrap_or_default(),
            "Recording TorrentCompleted journal entry"
        );
        self.append_event_journal_entry(EventJournalEntry {
            host_id: self.event_journal_host_id.clone(),
            ts_iso: chrono::Utc::now().to_rfc3339(),
            category: EventCategory::TorrentLifecycle,
            event_type: EventType::TorrentCompleted,
            torrent_name,
            info_hash_hex: Some(info_hash_hex),
            message: Some("Torrent completed".to_string()),
            ..Default::default()
        });
    }

    async fn apply_control_request(&mut self, request: &ControlRequest) -> Result<String, String> {
        match plan_control_request(&self.client_configs, request)? {
            ControlExecutionPlan::StatusNow => {
                self.trigger_status_dump_now();
                Ok("Wrote fresh status snapshot".to_string())
            }
            ControlExecutionPlan::StatusFollowStart { interval_secs } => {
                self.set_runtime_status_dump_interval_override(Some(interval_secs));
                self.trigger_status_dump_now();
                Ok(format!(
                    "Enabled runtime status dumps every {} seconds",
                    interval_secs
                ))
            }
            ControlExecutionPlan::StatusFollowStop => {
                self.set_runtime_status_dump_interval_override(Some(0));
                Ok("Stopped runtime status dumps".to_string())
            }
            ControlExecutionPlan::ApplySettings {
                next_settings,
                success_message,
            } => {
                self.apply_settings_update(next_settings, true).await;
                self.trigger_status_dump_after_successful_cluster_mutation();
                Ok(success_message)
            }
            ControlExecutionPlan::AddTorrentFile {
                source_path,
                download_path,
                container_name,
                file_priorities,
            } => {
                let ingest_result = self
                    .add_torrent_from_file(
                        source_path.clone(),
                        download_path,
                        false,
                        TorrentControlState::Running,
                        file_priorities,
                        container_name,
                    )
                    .await;
                Self::cleanup_staged_add_file(&source_path);
                if matches!(
                    ingest_result,
                    CommandIngestResult::Added { .. } | CommandIngestResult::Duplicate { .. }
                ) {
                    self.save_state_to_disk();
                    self.trigger_status_dump_after_successful_cluster_mutation();
                }
                Self::map_add_result_to_control_response(ingest_result)
            }
            ControlExecutionPlan::AddMagnet {
                magnet_link,
                download_path,
                container_name,
                file_priorities,
            } => {
                let ingest_result = self
                    .add_magnet_torrent(
                        "Fetching name...".to_string(),
                        magnet_link,
                        download_path,
                        false,
                        TorrentControlState::Running,
                        file_priorities,
                        container_name,
                    )
                    .await;
                if matches!(
                    ingest_result,
                    CommandIngestResult::Added { .. } | CommandIngestResult::Duplicate { .. }
                ) {
                    self.save_state_to_disk();
                    self.trigger_status_dump_after_successful_cluster_mutation();
                }
                Self::map_add_result_to_control_response(ingest_result)
            }
        }
    }

    fn watch_command_path(cmd: &AppCommand) -> Option<&PathBuf> {
        match cmd {
            AppCommand::AddTorrentFromFile(path)
            | AppCommand::AddTorrentFromPathFile(path)
            | AppCommand::AddMagnetFromFile(path)
            | AppCommand::ReloadClusterState(path)
            | AppCommand::ControlRequest { path, .. }
            | AppCommand::ClientShutdown(path)
            | AppCommand::PortFileChanged(path) => Some(path),
            _ => None,
        }
    }

    async fn enqueue_watch_command(&mut self, cmd: AppCommand, min_spacing: Duration) {
        if let Some(path) = Self::watch_command_path(&cmd).cloned() {
            let now = Instant::now();
            if let Some(last_time) = self.app_state.recently_processed_files.get(&path) {
                let elapsed = now.duration_since(*last_time);
                if elapsed < min_spacing {
                    return;
                }
            }

            self.app_state
                .recently_processed_files
                .insert(path.clone(), now);
            match &cmd {
                AppCommand::ControlRequest { request, .. } => {
                    let origin = self.control_origin_for_command_path(&path);
                    if self.record_control_queued(path, request.clone(), origin) {
                        self.save_state_to_disk();
                    }
                }
                _ => self.record_watch_path_discovered(&path),
            }
        }

        if let Err(error) = self.app_command_tx.try_send(cmd) {
            match error {
                tokio::sync::mpsc::error::TrySendError::Full(cmd) => {
                    self.app_state.pending_watch_commands.push_back(cmd);
                }
                tokio::sync::mpsc::error::TrySendError::Closed(_cmd) => {
                    tracing_event!(
                        Level::WARN,
                        "App command channel closed while queuing watch command"
                    );
                }
            }
        }
    }

    async fn process_pending_commands(&mut self) {
        for path in watcher::scan_watch_folder_paths(&self.watched_paths) {
            if let Some(cmd) = watcher::path_to_command(&path) {
                self.enqueue_watch_command(
                    cmd,
                    Duration::from_secs(WATCH_FOLDER_RESCAN_INTERVAL_SECS),
                )
                .await;
            }
        }
    }

    fn flush_pending_watch_commands(&mut self) {
        while let Some(cmd) = self.app_state.pending_watch_commands.pop_front() {
            if let Err(error) = self.app_command_tx.try_send(cmd) {
                match error {
                    tokio::sync::mpsc::error::TrySendError::Full(cmd) => {
                        self.app_state.pending_watch_commands.push_front(cmd);
                        break;
                    }
                    tokio::sync::mpsc::error::TrySendError::Closed(_cmd) => {
                        tracing_event!(
                            Level::WARN,
                            "App command channel closed while flushing pending watch commands"
                        );
                        break;
                    }
                }
            }
        }
    }

    async fn rebind_listener(&mut self, new_port: u16) -> bool {
        match ListenerSet::bind(new_port).await {
            Ok(new_listener) => {
                self.listener = Some(new_listener);
                // Note: client_configs.client_port is likely already updated by the caller (UpdateConfig)
                // but we ensure consistency here just in case.
                let bound_port = self
                    .listener
                    .as_ref()
                    .and_then(ListenerSet::local_port)
                    .unwrap_or(new_port);
                self.client_configs.client_port = bound_port;

                tracing_event!(
                    Level::INFO,
                    "Successfully rebound listener to port {}",
                    bound_port
                );

                // Notify all running managers of the new port
                for manager_tx in self.torrent_manager_command_txs.values() {
                    let _ = manager_tx.try_send(ManagerCommand::UpdateListenPort(bound_port));
                }

                self.dht_service
                    .reconfigure(DhtServiceConfig::from_settings(&self.client_configs));

                true
            }
            Err(e) => {
                tracing_event!(
                    Level::ERROR,
                    "Failed to bind to new port {}: {}. Listener not updated.",
                    new_port,
                    e
                );

                false
            }
        }
    }

    async fn download_rss_preview_item(&mut self, item: RssPreviewItem) {
        let Some(link) = item.link.clone() else {
            tracing_event!(
                Level::INFO,
                "Skipping RSS manual download: item has no link"
            );
            return;
        };

        let (added, info_hash, command_path) = if link.starts_with("magnet:") {
            let command_path = rss_ingest::write_magnet(&self.client_configs, link.as_str())
                .await
                .ok();
            let (v1_hash, v2_hash) = parse_hybrid_hashes(link.as_str());
            (command_path.is_some(), v1_hash.or(v2_hash), command_path)
        } else if link.starts_with("http://") || link.starts_with("https://") {
            self.download_rss_torrent_from_url(link.as_str()).await
        } else {
            tracing_event!(
                Level::INFO,
                "Skipping RSS manual download: unsupported link scheme '{}'",
                link
            );
            (false, None, None)
        };

        if !added {
            return;
        }

        if let Some(command_path) = command_path.clone() {
            let ingest_kind = ingest_kind_from_path(&command_path).unwrap_or_default();
            self.record_rss_queued(command_path, IngestOrigin::RssManual, ingest_kind);
        }

        for preview in &mut self.app_state.rss_runtime.preview_items {
            if preview.dedupe_key == item.dedupe_key {
                preview.is_downloaded = true;
            }
        }

        let entry = RssHistoryEntry {
            dedupe_key: item.dedupe_key.clone(),
            info_hash: info_hash.map(hex::encode),
            guid: item.guid.clone(),
            link: item.link.clone(),
            title: item.title.clone(),
            source: item.source.clone(),
            date_iso: item
                .date_iso
                .clone()
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
            added_via: crate::config::RssAddedVia::Manual,
        };
        let existing_idx = self
            .app_state
            .rss_runtime
            .history
            .iter()
            .position(|existing| existing.dedupe_key == entry.dedupe_key);
        if let Some(idx) = existing_idx {
            if self.app_state.rss_runtime.history[idx].info_hash.is_none()
                && entry.info_hash.is_some()
            {
                self.app_state.rss_runtime.history[idx].info_hash = entry.info_hash.clone();
                self.save_state_to_disk();
            }
        } else {
            self.app_state.rss_runtime.history.push(entry);
            self.save_state_to_disk();
        }

        if let Some(history_entry) = self
            .app_state
            .rss_runtime
            .history
            .iter()
            .find(|h| h.dedupe_key == item.dedupe_key)
            .cloned()
        {
            let _ = self.rss_downloaded_entry_tx.try_send(history_entry);
        }

        self.refresh_rss_derived();
    }

    async fn download_rss_torrent_from_url(
        &mut self,
        url: &str,
    ) -> (bool, Option<Vec<u8>>, Option<PathBuf>) {
        if !is_safe_rss_item_url(url).await {
            tracing_event!(
                Level::WARN,
                "RSS manual download blocked URL by network safety policy: {}",
                url
            );
            return (false, None, None);
        }

        let client = match reqwest::Client::builder()
            .user_agent("superseedr (https://github.com/Jagalite/superseedr)")
            .timeout(Duration::from_secs(RSS_MANUAL_DOWNLOAD_TIMEOUT_SECS))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing_event!(
                    Level::ERROR,
                    "RSS manual download failed to build HTTP client: {}",
                    e
                );
                return (false, None, None);
            }
        };

        let response = match client.get(url).send().await {
            Ok(resp) => resp,
            Err(e) => {
                tracing_event!(
                    Level::ERROR,
                    "RSS manual download request failed for {}: {}",
                    url,
                    e
                );
                return (false, None, None);
            }
        };
        if !response.status().is_success() {
            tracing_event!(
                Level::ERROR,
                "RSS manual download HTTP status {} for {}",
                response.status(),
                url
            );
            return (false, None, None);
        }

        let bytes = match response.bytes().await {
            Ok(b) => b,
            Err(e) => {
                tracing_event!(
                    Level::ERROR,
                    "RSS manual download body read failed for {}: {}",
                    url,
                    e
                );
                return (false, None, None);
            }
        };
        if bytes.len() > RSS_MAX_TORRENT_DOWNLOAD_BYTES {
            tracing_event!(
                Level::ERROR,
                "RSS manual download exceeded max size for {} ({} bytes)",
                url,
                bytes.len()
            );
            return (false, None, None);
        }
        let Some(info_hash) = info_hash_from_torrent_bytes(bytes.as_ref()) else {
            tracing_event!(
                Level::ERROR,
                "RSS manual download produced invalid torrent payload for {}",
                url
            );
            return (false, None, None);
        };

        match rss_ingest::write_torrent_bytes(&self.client_configs, url, bytes.as_ref()).await {
            Ok(path) => (true, Some(info_hash), Some(path)),
            Err(e) => {
                tracing_event!(
                    Level::ERROR,
                    "RSS manual download failed to queue torrent file for {}: {}",
                    url,
                    e
                );
                (false, None, None)
            }
        }
    }

    async fn fetch_latest_version() -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let client = reqwest::Client::builder()
            .user_agent("superseedr (https://github.com/Jagalite/superseedr)")
            .build()?;

        let url = "https://crates.io/api/v1/crates/superseedr";
        let resp: CratesResponse = client.get(url).send().await?.json().await?;

        Ok(resp.krate.max_version)
    }

    pub fn generate_output_state(&self) -> AppOutputState {
        let s = &self.app_state;
        let torrent_metrics = s
            .torrents
            .iter()
            .map(|(k, v)| (k.clone(), v.latest_state.clone()))
            .collect();

        AppOutputState {
            run_time: s.run_time,
            cpu_usage: s.cpu_usage,
            ram_usage_percent: s.ram_usage_percent,
            total_download_bps: s.avg_download_history.last().copied().unwrap_or(0),
            total_upload_bps: s.avg_upload_history.last().copied().unwrap_or(0),
            status_config: status::status_config_from_settings(&self.client_configs),
            dht: self.dht_service.current_status(),
            torrents: torrent_metrics,
        }
    }

    pub fn dump_status_to_file(&self) {
        if self.is_current_shared_follower() {
            return;
        }

        let generation = self
            .status_dump_generation
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);

        status::dump(
            self.generate_output_state(),
            self.shutdown_tx.clone(),
            self.is_current_shared_leader(),
            generation,
            self.status_dump_generation.clone(),
        );
    }

    fn effective_status_dump_interval_secs(&self) -> u64 {
        let configured_interval = self
            .status_dump_interval_override_secs
            .unwrap_or(self.client_configs.output_status_interval);
        if configured_interval == 0 && self.is_current_shared_leader() {
            5
        } else {
            configured_interval
        }
    }

    fn reschedule_status_dump_deadline(&mut self) {
        let interval_secs = self.effective_status_dump_interval_secs();
        self.next_status_dump_at = if interval_secs > 0 {
            Some(time::Instant::now() + Duration::from_secs(interval_secs))
        } else {
            None
        };
    }

    fn trigger_status_dump_now(&mut self) {
        self.dump_status_to_file();
        self.reschedule_status_dump_deadline();
    }

    fn trigger_status_dump_after_successful_cluster_mutation(&mut self) {
        if self.is_current_shared_leader() {
            self.trigger_status_dump_now();
        }
    }

    fn set_runtime_status_dump_interval_override(&mut self, interval_secs: Option<u64>) {
        self.status_dump_interval_override_secs = interval_secs;
        self.reschedule_status_dump_deadline();
    }

    fn reschedule_startup_load_deadline(&mut self) {
        self.next_startup_load_at = if self.startup_deferred_load_queue.is_empty() {
            None
        } else {
            Some(time::Instant::now() + Duration::from_secs(STARTUP_ROLLING_BATCH_INTERVAL_SECS))
        };
    }

    async fn load_next_startup_batch(&mut self) {
        let mut loaded_count = 0usize;

        for _ in 0..STARTUP_ROLLING_LOADS_PER_INTERVAL {
            let Some(info_hash) = self.startup_deferred_load_queue.front().cloned() else {
                break;
            };

            if self.has_live_runtime_for_torrent(&info_hash) {
                self.startup_deferred_load_queue.pop_front();
                continue;
            }

            let Some(torrent_config) = self
                .client_configs
                .torrents
                .iter()
                .find(|torrent| {
                    info_hash_from_torrent_source(&torrent.torrent_or_magnet).as_deref()
                        == Some(info_hash.as_slice())
                })
                .cloned()
            else {
                tracing_event!(
                    Level::WARN,
                    info_hash = %hex::encode(&info_hash),
                    "Skipping deferred startup torrent because it is no longer configured"
                );
                self.startup_deferred_load_queue.pop_front();
                continue;
            };

            if !should_load_persisted_torrent(&torrent_config) {
                self.startup_deferred_load_queue.pop_front();
                continue;
            }

            if self
                .load_runtime_torrent_from_settings(torrent_config)
                .await
            {
                self.startup_deferred_load_queue.pop_front();
                loaded_count = loaded_count.saturating_add(1);
            } else {
                if let Some(failed_info_hash) = self.startup_deferred_load_queue.pop_front() {
                    self.startup_deferred_load_queue.push_back(failed_info_hash);
                }
                tracing_event!(
                    Level::WARN,
                    info_hash = %hex::encode(&info_hash),
                    "Deferred startup torrent restore failed; moving it to the back of the queue"
                );
                continue;
            }
        }

        self.reschedule_startup_load_deadline();

        if loaded_count > 0 {
            tracing_event!(
                Level::INFO,
                loaded = loaded_count,
                remaining = self.startup_deferred_load_queue.len(),
                "Loaded deferred startup torrent batch"
            );
            self.app_state.ui.needs_redraw = true;
            self.save_state_to_disk();
        }
    }
}

fn is_valid_incoming_bittorrent_handshake(buffer: &[u8]) -> bool {
    buffer.len() >= 48
        && buffer[0] as usize == BITTORRENT_PROTOCOL_STR.len()
        && buffer[1..(1 + BITTORRENT_PROTOCOL_STR.len())] == *BITTORRENT_PROTOCOL_STR
}

fn persisted_validation_status_from_metrics(
    metrics: &TorrentMetrics,
    previous_validation_status: bool,
) -> bool {
    // Metadata may not be available yet for magnet sessions; preserve prior validation
    // only for the unknown 0/0 snapshot when we also have no explicit completion signal.
    if metrics.number_of_pieces_total == 0
        && metrics.number_of_pieces_completed == 0
        && !metrics.is_complete
        && !activity_marks_torrent_complete(&metrics.activity_message)
        && !torrent_has_skipped_files(metrics)
    {
        return previous_validation_status;
    }

    metrics.is_complete || !torrent_is_effectively_incomplete(metrics)
}

fn activity_marks_torrent_complete(activity_message: &str) -> bool {
    activity_message.contains("Seeding") || activity_message.contains("Finished")
}

fn torrent_has_skipped_files(metrics: &TorrentMetrics) -> bool {
    metrics
        .file_priorities
        .values()
        .any(|p| matches!(p, FilePriority::Skip))
}

pub fn torrent_is_effectively_incomplete(metrics: &TorrentMetrics) -> bool {
    if activity_marks_torrent_complete(&metrics.activity_message) {
        return false;
    }
    if torrent_has_skipped_files(metrics) {
        return false;
    }
    metrics.number_of_pieces_total > 0
        && metrics.number_of_pieces_completed < metrics.number_of_pieces_total
}

pub fn torrent_completion_percent(metrics: &TorrentMetrics) -> f64 {
    if activity_marks_torrent_complete(&metrics.activity_message) {
        return 100.0;
    }
    if torrent_has_skipped_files(metrics) {
        return 100.0;
    }
    if metrics.number_of_pieces_total == 0 {
        return 0.0;
    }

    ((metrics.number_of_pieces_completed as f64 / metrics.number_of_pieces_total as f64) * 100.0)
        .min(100.0)
}

fn calculate_adaptive_limits(client_configs: &Settings) -> (CalculatedLimits, Option<String>) {
    let effective_limit;
    let mut system_warning = None;
    const RECOMMENDED_MINIMUM: usize = 1024;

    if let Some(override_val) = client_configs.resource_limit_override {
        effective_limit = override_val;
        if effective_limit < RECOMMENDED_MINIMUM {
            system_warning = Some(format!(
                "Warning: Resource limit is set to {}. Performance may be degraded. Consider increasing with 'ulimit -n 65536'.",
                effective_limit
            ));
        }
    } else {
        #[cfg(unix)]
        {
            if let Ok((soft_limit, _)) = Resource::NOFILE.get() {
                effective_limit = soft_limit as usize;
                if effective_limit < RECOMMENDED_MINIMUM {
                    system_warning = Some(format!(
                        "Warning: System file handle limit is {}. Consider increasing with 'ulimit -n 65536'.",
                        effective_limit
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

fn compose_system_warning(
    base_warning: Option<&str>,
    dht_bootstrap_warning: Option<&str>,
) -> Option<String> {
    match (base_warning, dht_bootstrap_warning) {
        (Some(base), Some(dht)) => Some(format!("{} | {}", base, dht)),
        (Some(base), None) => Some(base.to_string()),
        (None, Some(dht)) => Some(dht.to_string()),
        (None, None) => None,
    }
}

pub fn parse_hybrid_hashes(magnet_link: &str) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    crate::torrent_identity::parse_hybrid_hashes(magnet_link)
}

pub fn info_hash_from_torrent_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
    crate::torrent_identity::info_hash_from_torrent_bytes(bytes)
}

fn resolve_magnet_torrent_name(
    requested_name: &str,
    magnet_link: &str,
    info_hash: &[u8],
) -> String {
    let is_placeholder = requested_name.trim().is_empty() || requested_name == "Fetching name...";
    if !is_placeholder {
        return requested_name.to_string();
    }

    extract_magnet_display_name(magnet_link)
        .unwrap_or_else(|| format!("Magnet {}", hex::encode(info_hash)))
}

fn torrent_file_count(torrent: &crate::torrent_file::Torrent) -> usize {
    if torrent.info.files.is_empty() {
        1
    } else {
        torrent.info.files.len()
    }
}

fn extract_magnet_display_name(magnet_link: &str) -> Option<String> {
    for raw_part in magnet_link.split('&') {
        let part = raw_part.strip_prefix("magnet:?").unwrap_or(raw_part);
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if key.eq_ignore_ascii_case("dn") {
            let value_for_decode = value.replace('+', "%20");
            if let Ok(decoded) = urlencoding::decode(&value_for_decode) {
                let name = decoded.trim();
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

pub(crate) fn clamp_selected_indices_in_state(app_state: &mut AppState) {
    let torrent_count = app_state.torrent_list_order.len();

    if torrent_count == 0 {
        app_state.ui.selected_torrent_index = 0;
    } else if app_state.ui.selected_torrent_index >= torrent_count {
        app_state.ui.selected_torrent_index = torrent_count - 1;
    }

    let peer_count = app_state
        .torrent_list_order
        .get(app_state.ui.selected_torrent_index)
        .and_then(|info_hash| app_state.torrents.get(info_hash))
        .map_or(0, |torrent| torrent.latest_state.peers.len());

    if peer_count == 0 {
        app_state.ui.selected_peer_index = 0;
    } else if app_state.ui.selected_peer_index >= peer_count {
        app_state.ui.selected_peer_index = peer_count - 1;
    }
}

pub(crate) fn file_activity_wave_steps_per_second(speed_bps: u64) -> f64 {
    if speed_bps == 0 {
        12.0
    } else if speed_bps < 50_000 {
        11.0
    } else if speed_bps < 500_000 {
        12.5
    } else if speed_bps < 2_000_000 {
        14.0
    } else if speed_bps < 10_000_000 {
        16.0
    } else if speed_bps < 20_000_000 {
        17.5
    } else if speed_bps < 50_000_000 {
        19.0
    } else if speed_bps < 100_000_000 {
        21.0
    } else {
        23.0
    }
}

pub(crate) fn sort_and_filter_torrent_list_state(app_state: &mut AppState) {
    let torrents_map = &app_state.torrents;
    let (sort_by, sort_direction) = app_state.torrent_sort;
    let search_query = &app_state.ui.search_query;

    let matcher = fuzzy_matcher::skim::SkimMatcherV2::default();
    let mut torrent_list: Vec<Vec<u8>> = torrents_map.keys().cloned().collect();

    if !search_query.is_empty() {
        torrent_list.retain(|info_hash| {
            let torrent_name = torrents_map
                .get(info_hash)
                .map_or("", |t| &t.latest_state.torrent_name);
            matcher.fuzzy_match(torrent_name, search_query).is_some()
        });
    }

    torrent_list.sort_by(|a_info_hash, b_info_hash| {
        let Some(a_torrent) = torrents_map.get(a_info_hash) else {
            return std::cmp::Ordering::Equal;
        };
        let Some(b_torrent) = torrents_map.get(b_info_hash) else {
            return std::cmp::Ordering::Equal;
        };

        let availability_ordering = a_torrent
            .latest_state
            .data_available
            .cmp(&b_torrent.latest_state.data_available);
        if availability_ordering != std::cmp::Ordering::Equal {
            return availability_ordering;
        }

        let ordering = match sort_by {
            TorrentSortColumn::Name => a_torrent
                .latest_state
                .torrent_name
                .cmp(&b_torrent.latest_state.torrent_name),
            TorrentSortColumn::Down => b_torrent
                .smoothed_download_speed_bps
                .cmp(&a_torrent.smoothed_download_speed_bps),
            TorrentSortColumn::Up => b_torrent
                .smoothed_upload_speed_bps
                .cmp(&a_torrent.smoothed_upload_speed_bps),
            TorrentSortColumn::Progress => {
                let calc_progress = |t: &TorrentDisplayState| -> f64 {
                    if t.latest_state.number_of_pieces_total == 0 {
                        0.0
                    } else {
                        t.latest_state.number_of_pieces_completed as f64
                            / t.latest_state.number_of_pieces_total as f64
                    }
                };

                let a_prog = calc_progress(a_torrent);
                let b_prog = calc_progress(b_torrent);
                a_prog.total_cmp(&b_prog)
            }
        };

        let default_direction = match sort_by {
            TorrentSortColumn::Name => SortDirection::Ascending,
            _ => SortDirection::Descending,
        };
        let primary_ordering = if sort_direction != default_direction {
            ordering.reverse()
        } else {
            ordering
        };

        primary_ordering.then_with(|| {
            let calculate_weighted_activity = |t: &TorrentDisplayState| -> u64 {
                let window = 60;
                let mut score = 0;
                let mut sum_vec = |history: &Vec<u64>| {
                    for (i, &count) in history.iter().rev().take(window).enumerate() {
                        if count > 0 {
                            let weight = if i < 5 { (5 - i) as u64 * 10 } else { 1 };
                            score += count * weight;
                        }
                    }
                };
                sum_vec(&t.peer_discovery_history);
                sum_vec(&t.peer_connection_history);
                sum_vec(&t.peer_disconnect_history);
                score
            };

            let a_activity = calculate_weighted_activity(a_torrent);
            let b_activity = calculate_weighted_activity(b_torrent);
            b_activity.cmp(&a_activity)
        })
    });

    app_state.torrent_list_order = torrent_list;
    clamp_selected_indices_in_state(app_state);
}

fn rss_settings_changed(old_settings: &Settings, new_settings: &Settings) -> bool {
    new_settings.rss != old_settings.rss
}

fn should_load_persisted_torrent(torrent_settings: &TorrentSettings) -> bool {
    torrent_settings.torrent_control_state != TorrentControlState::Deleting
}

fn build_persist_payload(
    client_configs: &mut Settings,
    app_state: &mut AppState,
    startup_deferred_load_queue: &VecDeque<Vec<u8>>,
) -> PersistPayload {
    client_configs.lifetime_downloaded =
        app_state.lifetime_downloaded_from_config + app_state.session_total_downloaded;
    client_configs.lifetime_uploaded =
        app_state.lifetime_uploaded_from_config + app_state.session_total_uploaded;

    client_configs.torrent_sort_column = app_state.torrent_sort.0;
    client_configs.torrent_sort_direction = app_state.torrent_sort.1;
    client_configs.peer_sort_column = app_state.peer_sort.0;
    client_configs.peer_sort_direction = app_state.peer_sort.1;
    let old_validation_statuses: HashMap<String, bool> = client_configs
        .torrents
        .iter()
        .map(|cfg| (cfg.torrent_or_magnet.clone(), cfg.validation_status))
        .collect();
    let previous_torrents = client_configs.torrents.clone();
    let deferred_hashes: HashSet<Vec<u8>> = startup_deferred_load_queue.iter().cloned().collect();
    let mut persisted_info_hashes: HashSet<Vec<u8>> = app_state.torrents.keys().cloned().collect();

    let mut persisted_torrents: Vec<TorrentSettings> = app_state
        .torrents
        .values()
        .map(|torrent| {
            let torrent_state = &torrent.latest_state;
            let previous_validation_status = old_validation_statuses
                .get(&torrent_state.torrent_or_magnet)
                .copied()
                .unwrap_or(false);

            let final_validation_status =
                persisted_validation_status_from_metrics(torrent_state, previous_validation_status);

            TorrentSettings {
                torrent_or_magnet: torrent_state.torrent_or_magnet.clone(),
                name: torrent_state.torrent_name.clone(),
                validation_status: final_validation_status,
                download_path: torrent_state.download_path.clone(),
                container_name: torrent_state.container_name.clone(),
                torrent_control_state: torrent_state.torrent_control_state.clone(),
                delete_files: torrent_state.delete_files,
                file_priorities: torrent_state.file_priorities.clone(),
            }
        })
        .collect();

    for torrent in previous_torrents {
        let Some(info_hash) = info_hash_from_torrent_source(&torrent.torrent_or_magnet) else {
            continue;
        };

        if deferred_hashes.contains(&info_hash) && persisted_info_hashes.insert(info_hash) {
            persisted_torrents.push(torrent);
        }
    }

    client_configs.torrents = persisted_torrents;

    const RSS_HISTORY_LIMIT: usize = 1000;
    if app_state.rss_runtime.history.len() > RSS_HISTORY_LIMIT {
        let overflow = app_state.rss_runtime.history.len() - RSS_HISTORY_LIMIT;
        app_state.rss_runtime.history.drain(0..overflow);
    }

    let rss_state = RssPersistedState {
        history: app_state.rss_runtime.history.clone(),
        last_sync_at: app_state.rss_runtime.last_sync_at.clone(),
        feed_errors: app_state.rss_runtime.feed_errors.clone(),
    };

    let network_history = if app_state.network_history_restore_pending {
        None
    } else {
        app_state.network_history_state.rollups = app_state.network_history_rollups.to_snapshot();
        app_state.network_history_state.updated_at_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        app_state.next_network_history_persist_request_id = app_state
            .next_network_history_persist_request_id
            .saturating_add(1);
        Some(NetworkHistoryPersistRequest {
            request_id: app_state.next_network_history_persist_request_id,
            state: app_state.network_history_state.clone(),
        })
    };

    let activity_history = if app_state.activity_history_restore_pending {
        None
    } else {
        app_state
            .activity_history_rollups
            .sync_snapshots_to_state(&mut app_state.activity_history_state);
        app_state.activity_history_state.updated_at_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        app_state.next_activity_history_persist_request_id = app_state
            .next_activity_history_persist_request_id
            .saturating_add(1);
        Some(ActivityHistoryPersistRequest {
            request_id: app_state.next_activity_history_persist_request_id,
            state: app_state.activity_history_state.clone(),
        })
    };

    PersistPayload {
        settings: client_configs.clone(),
        rss_state,
        network_history,
        activity_history,
        event_journal_state: app_state.event_journal_state.clone(),
    }
}

fn apply_network_history_persist_result(app_state: &mut AppState, request_id: u64, success: bool) {
    if success && app_state.pending_network_history_persist_request_id == Some(request_id) {
        app_state.network_history_dirty = false;
        app_state.pending_network_history_persist_request_id = None;
    }
}

fn apply_activity_history_persist_result(app_state: &mut AppState, request_id: u64, success: bool) {
    if success && app_state.pending_activity_history_persist_request_id == Some(request_id) {
        app_state.activity_history_dirty = false;
        app_state.pending_activity_history_persist_request_id = None;
    }
}

fn should_persist_network_history_on_interval(app_state: &AppState) -> bool {
    app_state.network_history_dirty || app_state.activity_history_dirty
}

fn queue_persistence_payload(
    tx: Option<&watch::Sender<Option<PersistPayload>>>,
    payload: PersistPayload,
) -> Result<(), ()> {
    let Some(tx) = tx else {
        return Err(());
    };
    tx.send_replace(Some(payload));
    if tx.is_closed() {
        return Err(());
    }
    Ok(())
}

async fn flush_persistence_writer_parts(
    persistence_tx: &mut Option<watch::Sender<Option<PersistPayload>>>,
    persistence_task: &mut Option<tokio::task::JoinHandle<()>>,
) {
    *persistence_tx = None;
    if let Some(handle) = persistence_task.take() {
        if let Err(e) = handle.await {
            tracing_event!(Level::ERROR, "Error joining persistence task: {}", e);
        }
    }
}

fn prune_rss_feed_errors(
    feed_errors: &mut HashMap<String, FeedSyncError>,
    settings: &Settings,
) -> bool {
    let configured_feed_urls: std::collections::HashSet<&str> = settings
        .rss
        .feeds
        .iter()
        .map(|feed| feed.url.as_str())
        .collect();
    let before = feed_errors.len();
    feed_errors.retain(|feed_url, _| configured_feed_urls.contains(feed_url.as_str()));
    feed_errors.len() != before
}

fn watched_parent_matches(path: &Path, watch_dir: &Path) -> bool {
    path.parent()
        .is_some_and(|parent| normalized_watch_path(parent) == normalized_watch_path(watch_dir))
}

#[cfg(windows)]
fn normalized_watch_path(path: &Path) -> PathBuf {
    let raw = path.as_os_str().to_string_lossy();
    let stripped = raw.strip_prefix(r"\\?\").unwrap_or(raw.as_ref());
    PathBuf::from(stripped.to_ascii_lowercase())
}

#[cfg(not(windows))]
fn normalized_watch_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::await_holding_lock)]

    use super::{
        apply_network_history_persist_result, build_persist_payload, build_torrent_preview_tree,
        clamp_selected_indices_in_state, compose_system_warning, extract_magnet_display_name,
        flush_persistence_writer_parts, format_filesystem_path_error,
        is_valid_incoming_bittorrent_handshake, move_file_with_fallback_impl, parse_hybrid_hashes,
        persisted_validation_status_from_metrics, prune_rss_feed_errors, queue_persistence_payload,
        resolve_magnet_torrent_name, rss_settings_changed, should_load_persisted_torrent,
        should_persist_network_history_on_interval, sort_and_filter_torrent_list_state,
        torrent_completion_percent, torrent_is_effectively_incomplete, watched_parent_matches, App,
        AppClusterRole, AppCommand, AppMode, AppRuntimeMode, AppState, CommandIngestResult,
        FilePriority, IngestSource, ListenerSet, PeerInfo, PersistPayload, SelectedHeader,
        SortDirection, TorrentControlState, TorrentDisplayState, TorrentMetrics,
        TorrentPreviewPayload, TorrentSortColumn, UiState, BITTORRENT_PROTOCOL_STR,
    };
    use crate::config::{
        clear_shared_config_state_for_tests, set_app_paths_override_for_tests, TorrentSettings,
    };
    use crate::control_service::control_event_details;
    use crate::dht::service::{DhtService, TestDhtRecorder};
    use crate::errors::StorageError;
    use crate::integrations::control::{read_control_request, ControlRequest};
    use crate::integrations::status::{self, AppOutputState};
    use crate::persistence::event_journal::{
        ControlOrigin, EventDetails, EventJournalState, EventType, IngestKind, IngestOrigin,
    };
    use crate::persistence::event_journal::{EventCategory, EventJournalEntry};
    use crate::telemetry::ui_telemetry::UiTelemetry;
    use crate::torrent_identity::info_hash_from_torrent_source;
    use crate::torrent_manager::{
        FileProbeBatchResult, FileProbeEntry, ManagerCommand, ManagerEvent, TorrentFileProbeStatus,
    };
    use std::collections::{HashMap, VecDeque};
    use std::env;
    use std::io;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio::sync::watch;
    use tokio::time;

    fn mock_display(name: &str, peer_count: usize) -> TorrentDisplayState {
        let mut display = TorrentDisplayState::default();
        display.latest_state.torrent_name = name.to_string();
        display.latest_state.peers = (0..peer_count)
            .map(|i| PeerInfo {
                address: format!("127.0.0.1:{}", 6000 + i),
                ..Default::default()
            })
            .collect();
        display
    }

    fn shared_env_guard() -> &'static std::sync::Mutex<()> {
        crate::config::shared_env_guard_for_tests()
    }

    fn lock_shared_env() -> std::sync::MutexGuard<'static, ()> {
        shared_env_guard()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn configure_temp_app_paths_for_test() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create tempdir");
        let config_dir = dir.path().join("config");
        let data_dir = dir.path().join("data");
        set_app_paths_override_for_tests(Some((config_dir, data_dir)));
        dir
    }

    #[test]
    fn format_filesystem_path_error_reports_directory_as_file_mismatch() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("folder");
        std::fs::create_dir(&path).expect("create folder");

        let error = io::Error::other("raw os text");
        let message = format_filesystem_path_error("Failed to read torrent file", &path, &error);

        assert!(message.contains("Failed to read torrent file"));
        assert!(message.contains("expected a file here, but the path points to a directory"));
    }

    #[test]
    fn format_filesystem_path_error_reports_missing_path_clearly() {
        let path = PathBuf::from("/tmp/superseedr-missing-sample.torrent");
        let error = io::Error::new(io::ErrorKind::NotFound, "No such file or directory");
        let message = format_filesystem_path_error("Failed to read torrent file", &path, &error);

        assert!(message.contains("file or directory was not found"));
    }

    #[test]
    fn move_file_with_fallback_copies_when_rename_crosses_devices() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let source = dir.path().join("bridge.magnet");
        let destination = dir.path().join("processed").join("bridge.magnet");
        std::fs::write(
            &source,
            b"magnet:?xt=urn:btih:1111111111111111111111111111111111111111",
        )
        .expect("write source file");

        move_file_with_fallback_impl(&source, &destination, |_src, _dst| {
            Err(std::io::Error::from_raw_os_error(18))
        })
        .expect("fallback move should succeed");

        assert!(!source.exists());
        assert_eq!(
            std::fs::read_to_string(&destination).expect("read copied destination"),
            "magnet:?xt=urn:btih:1111111111111111111111111111111111111111"
        );
    }

    #[test]
    fn persisted_validation_status_is_true_only_when_complete() {
        assert!(!persisted_validation_status_from_metrics(
            &TorrentMetrics::default(),
            false
        ));
        assert!(!persisted_validation_status_from_metrics(
            &TorrentMetrics {
                number_of_pieces_total: 10,
                number_of_pieces_completed: 9,
                ..Default::default()
            },
            false
        ));
        assert!(persisted_validation_status_from_metrics(
            &TorrentMetrics {
                number_of_pieces_total: 10,
                number_of_pieces_completed: 10,
                ..Default::default()
            },
            false
        ));
    }

    #[test]
    fn persisted_validation_status_downgrades_when_incomplete() {
        assert!(
            !persisted_validation_status_from_metrics(
                &TorrentMetrics {
                    number_of_pieces_total: 10,
                    number_of_pieces_completed: 8,
                    ..Default::default()
                },
                true
            ),
            "Validation status must not stay true once piece completion regresses"
        );
    }

    #[test]
    fn persisted_validation_status_preserves_prior_true_for_metadata_unavailable_snapshot() {
        assert!(
            persisted_validation_status_from_metrics(&TorrentMetrics::default(), true),
            "0/0 snapshot should preserve prior validated status (magnet metadata pending)"
        );
    }

    #[test]
    fn persisted_validation_status_treats_effectively_complete_torrents_as_complete() {
        assert!(persisted_validation_status_from_metrics(
            &TorrentMetrics {
                activity_message: "Seeding".to_string(),
                ..Default::default()
            },
            false
        ));
        assert!(persisted_validation_status_from_metrics(
            &TorrentMetrics {
                file_priorities: HashMap::from([(0, FilePriority::Skip)]),
                number_of_pieces_total: 10,
                number_of_pieces_completed: 8,
                ..Default::default()
            },
            false
        ));
    }

    #[test]
    fn build_persist_payload_keeps_deferred_startup_torrents_in_settings() {
        let deferred_hash = vec![0x55; 20];
        let loaded_hash = vec![0x66; 20];
        let deferred_magnet =
            "magnet:?xt=urn:btih:5555555555555555555555555555555555555555".to_string();
        let loaded_magnet =
            "magnet:?xt=urn:btih:6666666666666666666666666666666666666666".to_string();
        let mut settings = crate::config::Settings {
            torrents: vec![
                TorrentSettings {
                    torrent_or_magnet: deferred_magnet.clone(),
                    name: "sample-deferred".to_string(),
                    torrent_control_state: TorrentControlState::Running,
                    ..Default::default()
                },
                TorrentSettings {
                    torrent_or_magnet: loaded_magnet.clone(),
                    name: "sample-loaded".to_string(),
                    torrent_control_state: TorrentControlState::Running,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut app_state = AppState::default();
        app_state.torrents.insert(
            loaded_hash,
            TorrentDisplayState {
                latest_state: TorrentMetrics {
                    info_hash: vec![0x66; 20],
                    torrent_or_magnet: loaded_magnet,
                    torrent_name: "sample-loaded".to_string(),
                    torrent_control_state: TorrentControlState::Running,
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        let deferred_queue = VecDeque::from([deferred_hash]);
        let payload = build_persist_payload(&mut settings, &mut app_state, &deferred_queue);

        assert_eq!(payload.settings.torrents.len(), 2);
        assert!(payload.settings.torrents.iter().any(|torrent| {
            torrent.torrent_or_magnet == deferred_magnet
                && torrent.torrent_control_state == TorrentControlState::Running
        }));
    }

    #[test]
    fn should_draw_every_frame_in_normal_mode() {
        assert!(App::should_draw_this_frame(&AppMode::Normal, false));
        assert!(App::should_draw_this_frame(&AppMode::Normal, true));
    }

    #[test]
    fn should_draw_every_frame_in_welcome_mode() {
        assert!(App::should_draw_this_frame(&AppMode::Welcome, false));
        assert!(App::should_draw_this_frame(&AppMode::Welcome, true));
    }

    #[test]
    fn should_only_draw_dirty_in_power_saving_mode() {
        assert!(!App::should_draw_this_frame(&AppMode::PowerSaving, false));
        assert!(App::should_draw_this_frame(&AppMode::PowerSaving, true));
    }

    #[test]
    fn completion_helper_marks_seeding_complete() {
        let mut metrics = TorrentMetrics {
            number_of_pieces_total: 100,
            number_of_pieces_completed: 0,
            ..Default::default()
        };
        metrics.activity_message = "Seeding".to_string();

        assert!(!torrent_is_effectively_incomplete(&metrics));
        assert_eq!(torrent_completion_percent(&metrics), 100.0);
    }

    #[test]
    fn completion_helper_marks_skipped_files_complete() {
        let metrics = TorrentMetrics {
            number_of_pieces_total: 8,
            number_of_pieces_completed: 2,
            file_priorities: HashMap::from([(0, FilePriority::Skip)]),
            ..Default::default()
        };

        assert!(!torrent_is_effectively_incomplete(&metrics));
        assert_eq!(torrent_completion_percent(&metrics), 100.0);
    }

    #[test]
    fn torrent_saved_location_uses_file_path_for_flat_torrents() {
        let metrics = TorrentMetrics {
            torrent_name: "flat.bin".to_string(),
            download_path: Some("/downloads/shared".into()),
            container_name: None,
            is_multi_file: false,
            file_count: Some(1),
            ..Default::default()
        };

        assert_eq!(
            App::torrent_saved_location(&metrics),
            Some(PathBuf::from("/downloads/shared/flat.bin"))
        );
    }

    #[test]
    fn torrent_saved_location_uses_root_for_explicit_empty_container_multi_file_torrents() {
        let metrics = TorrentMetrics {
            torrent_name: "folderless-multi".to_string(),
            download_path: Some("/downloads/shared".into()),
            container_name: Some(String::new()),
            is_multi_file: true,
            file_count: Some(2),
            ..Default::default()
        };

        assert_eq!(
            App::torrent_saved_location(&metrics),
            Some(PathBuf::from("/downloads/shared"))
        );
    }

    #[test]
    fn torrent_saved_location_uses_root_for_single_entry_multi_file_torrents_without_container() {
        let metrics = TorrentMetrics {
            torrent_name: "single-entry-multi".to_string(),
            download_path: Some("/downloads/shared".into()),
            container_name: Some(String::new()),
            is_multi_file: true,
            file_count: Some(1),
            ..Default::default()
        };

        assert_eq!(
            App::torrent_saved_location(&metrics),
            Some(PathBuf::from("/downloads/shared"))
        );
    }

    #[test]
    fn clamp_selected_indices_clamps_torrent_and_peer_to_bounds() {
        let mut app_state = AppState::default();
        let hash_a = b"hash_a".to_vec();
        let hash_b = b"hash_b".to_vec();
        app_state
            .torrents
            .insert(hash_a.clone(), mock_display("alpha", 0));
        app_state
            .torrents
            .insert(hash_b.clone(), mock_display("beta", 2));
        app_state.torrent_list_order = vec![hash_a, hash_b];
        app_state.ui.selected_torrent_index = 99;
        app_state.ui.selected_peer_index = 99;

        clamp_selected_indices_in_state(&mut app_state);

        assert_eq!(app_state.ui.selected_torrent_index, 1);
        assert_eq!(app_state.ui.selected_peer_index, 1);
    }

    #[test]
    fn sort_and_filter_applies_query_and_clamps_selection() {
        let mut app_state = AppState {
            torrent_sort: (TorrentSortColumn::Name, SortDirection::Ascending),
            ui: UiState {
                selected_header: SelectedHeader::Torrent(0),
                selected_torrent_index: 5,
                search_query: "spha".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };

        let hash_a = b"hash_a".to_vec();
        let hash_b = b"hash_b".to_vec();
        app_state
            .torrents
            .insert(hash_a.clone(), mock_display("samplealpha-24.04.iso", 0));
        app_state
            .torrents
            .insert(hash_b.clone(), mock_display("samplelinux.iso", 0));

        sort_and_filter_torrent_list_state(&mut app_state);

        assert_eq!(app_state.torrent_list_order, vec![hash_a]);
        assert_eq!(app_state.ui.selected_torrent_index, 0);
    }

    #[test]
    fn sort_and_filter_prioritizes_unavailable_torrents() {
        let mut app_state = AppState {
            torrent_sort: (TorrentSortColumn::Down, SortDirection::Descending),
            ..Default::default()
        };

        let unavailable_hash = b"unavailable_hash".to_vec();
        let available_hash = b"available_hash".to_vec();

        let mut unavailable = mock_display("sample-unavailable.iso", 0);
        unavailable.latest_state.data_available = false;
        unavailable.smoothed_download_speed_bps = 1;

        let mut available = mock_display("sample-available.iso", 0);
        available.smoothed_download_speed_bps = 10_000;

        app_state
            .torrents
            .insert(unavailable_hash.clone(), unavailable);
        app_state.torrents.insert(available_hash.clone(), available);

        sort_and_filter_torrent_list_state(&mut app_state);

        assert_eq!(
            app_state.torrent_list_order,
            vec![unavailable_hash, available_hash]
        );
    }

    #[test]
    fn extract_magnet_display_name_decodes_dn() {
        let magnet =
            "magnet:?xt=urn:btih:1111111111111111111111111111111111111111&dn=SampleAlpha+24.04+ISO";
        assert_eq!(
            extract_magnet_display_name(magnet),
            Some("SampleAlpha 24.04 ISO".to_string())
        );
    }

    #[test]
    fn resolve_magnet_name_uses_dn_for_placeholder() {
        let info_hash = vec![0x11; 20];
        let magnet = "magnet:?xt=urn:btih:1111111111111111111111111111111111111111&dn=SampleBeta";
        assert_eq!(
            resolve_magnet_torrent_name("Fetching name...", magnet, &info_hash),
            "SampleBeta".to_string()
        );
    }

    #[test]
    fn resolve_magnet_name_falls_back_to_hash_label_when_dn_missing() {
        let info_hash = vec![0x22; 20];
        let magnet = "magnet:?xt=urn:btih:2222222222222222222222222222222222222222";
        assert_eq!(
            resolve_magnet_torrent_name("Fetching name...", magnet, &info_hash),
            format!("Magnet {}", hex::encode(&info_hash))
        );
    }

    #[test]
    fn extract_magnet_display_name_skips_malformed_segments() {
        let magnet = "magnet:?xt=urn:btih:1111111111111111111111111111111111111111&badsegment&dn=SampleGamma+Netinst";
        assert_eq!(
            extract_magnet_display_name(magnet),
            Some("SampleGamma Netinst".to_string())
        );
    }

    #[test]
    fn parse_hybrid_hashes_handles_case_insensitive_xt_and_urn_prefixes() {
        let magnet = "magnet:?XT=URN:BTIH:1111111111111111111111111111111111111111&xT=urn:BTMH:12201111111111111111111111111111111111111111111111111111111111111111";
        let (v1, v2) = parse_hybrid_hashes(magnet);
        assert_eq!(v1, Some(vec![0x11; 20]));
        assert_eq!(v2, Some(vec![0x11; 20]));
    }

    #[test]
    fn rss_settings_changed_detects_filter_updates() {
        let old = crate::config::Settings::default();
        let mut new = old.clone();
        new.rss.filters.push(crate::config::RssFilter {
            query: "samplealpha".to_string(),
            mode: crate::config::RssFilterMode::Fuzzy,
            enabled: true,
        });

        assert!(rss_settings_changed(&old, &new));
    }

    #[test]
    fn rss_settings_changed_ignores_non_rss_updates() {
        let old = crate::config::Settings::default();
        let mut new = old.clone();
        new.global_download_limit_bps += 1;

        assert!(!rss_settings_changed(&old, &new));
    }

    #[test]
    fn prune_rss_feed_errors_removes_deleted_feed_urls() {
        let mut settings = crate::config::Settings::default();
        settings.rss.feeds.push(crate::config::RssFeed {
            url: "https://active.example/rss.xml".to_string(),
            enabled: true,
        });

        let mut feed_errors = HashMap::new();
        feed_errors.insert(
            "https://active.example/rss.xml".to_string(),
            crate::config::FeedSyncError {
                message: "timeout".to_string(),
                occurred_at_iso: "2026-02-18T10:00:00Z".to_string(),
            },
        );
        feed_errors.insert(
            "https://removed.example/rss.xml".to_string(),
            crate::config::FeedSyncError {
                message: "403".to_string(),
                occurred_at_iso: "2026-02-18T10:01:00Z".to_string(),
            },
        );

        let changed = prune_rss_feed_errors(&mut feed_errors, &settings);
        assert!(changed);
        assert_eq!(feed_errors.len(), 1);
        assert!(feed_errors.contains_key("https://active.example/rss.xml"));
    }

    #[test]
    fn prune_rss_feed_errors_is_noop_when_all_urls_still_configured() {
        let mut settings = crate::config::Settings::default();
        settings.rss.feeds.push(crate::config::RssFeed {
            url: "https://active.example/rss.xml".to_string(),
            enabled: true,
        });

        let mut feed_errors = HashMap::new();
        feed_errors.insert(
            "https://active.example/rss.xml".to_string(),
            crate::config::FeedSyncError {
                message: "timeout".to_string(),
                occurred_at_iso: "2026-02-18T10:00:00Z".to_string(),
            },
        );

        let changed = prune_rss_feed_errors(&mut feed_errors, &settings);
        assert!(!changed);
        assert_eq!(feed_errors.len(), 1);
    }

    #[test]
    fn compose_system_warning_merges_base_and_dht_messages() {
        let composed = compose_system_warning(Some("base warning"), Some("dht warning"));
        assert_eq!(composed, Some("base warning | dht warning".to_string()));
    }

    #[test]
    fn compose_system_warning_handles_single_or_no_messages() {
        assert_eq!(
            compose_system_warning(Some("base warning"), None),
            Some("base warning".to_string())
        );
        assert_eq!(
            compose_system_warning(None, Some("dht warning")),
            Some("dht warning".to_string())
        );
        assert_eq!(compose_system_warning(None, None), None);
    }

    #[test]
    fn incoming_handshake_validator_accepts_bittorrent_handshake_prefix() {
        let mut handshake = vec![0u8; 68];
        handshake[0] = BITTORRENT_PROTOCOL_STR.len() as u8;
        handshake[1..(1 + BITTORRENT_PROTOCOL_STR.len())].copy_from_slice(BITTORRENT_PROTOCOL_STR);

        assert!(is_valid_incoming_bittorrent_handshake(&handshake));
    }

    #[test]
    fn incoming_handshake_validator_rejects_non_bittorrent_prefix() {
        let mut handshake = vec![0u8; 68];
        handshake[0] = BITTORRENT_PROTOCOL_STR.len() as u8;
        handshake[1..(1 + BITTORRENT_PROTOCOL_STR.len())].copy_from_slice(b"NotTorrent protocol");

        assert!(!is_valid_incoming_bittorrent_handshake(&handshake));
    }

    #[tokio::test]
    async fn mark_port_open_command_tracks_ipv4_and_ipv6_independently() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");

        assert!(!app.app_state.externally_accessable_port_v4);
        assert!(!app.app_state.externally_accessable_port_v6);

        app.handle_app_command(AppCommand::MarkPortOpen(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            6681,
        )))
        .await;

        assert!(app.app_state.externally_accessable_port_v4);
        assert!(!app.app_state.externally_accessable_port_v6);

        app.handle_app_command(AppCommand::MarkPortOpen(SocketAddr::new(
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            6681,
        )))
        .await;

        assert!(app.app_state.externally_accessable_port_v4);
        assert!(app.app_state.externally_accessable_port_v6);
    }

    #[tokio::test]
    async fn mark_port_open_announces_running_torrents_once_per_family_transition() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        app.client_configs.client_port = 6681;
        let recorder = TestDhtRecorder::default();
        app.dht_service = DhtService::from_test_recorder(recorder.clone());
        app.dht_status_rx = app.dht_service.subscribe_status();

        let running_hash = vec![1; 20];
        let paused_hash = vec![2; 20];
        let (running_tx, _running_rx) = mpsc::channel(1);
        let (paused_tx, _paused_rx) = mpsc::channel(1);
        app.torrent_manager_command_txs
            .insert(running_hash.clone(), running_tx);
        app.torrent_manager_command_txs
            .insert(paused_hash.clone(), paused_tx);

        let mut running_display = TorrentDisplayState::default();
        running_display.latest_state.info_hash = running_hash.clone();
        running_display.latest_state.torrent_name = "announce running torrent".to_string();
        running_display.latest_state.torrent_control_state = TorrentControlState::Running;
        app.app_state
            .torrents
            .insert(running_hash.clone(), running_display);

        let mut paused_display = TorrentDisplayState::default();
        paused_display.latest_state.info_hash = paused_hash.clone();
        paused_display.latest_state.torrent_name = "announce paused torrent".to_string();
        paused_display.latest_state.torrent_control_state = TorrentControlState::Paused;
        app.app_state
            .torrents
            .insert(paused_hash.clone(), paused_display);

        app.handle_app_command(AppCommand::MarkPortOpen(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            6681,
        )))
        .await;
        tokio::task::yield_now().await;

        assert_eq!(
            recorder.recorded_announces(),
            vec![(running_hash.clone(), Some(6681))]
        );

        app.handle_app_command(AppCommand::MarkPortOpen(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            6681,
        )))
        .await;
        tokio::task::yield_now().await;

        assert_eq!(
            recorder.recorded_announces(),
            vec![(running_hash.clone(), Some(6681))]
        );

        app.handle_app_command(AppCommand::MarkPortOpen(SocketAddr::new(
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            6681,
        )))
        .await;
        tokio::task::yield_now().await;

        assert_eq!(
            recorder.recorded_announces(),
            vec![
                (running_hash.clone(), Some(6681)),
                (running_hash, Some(6681))
            ]
        );
    }

    #[tokio::test]
    async fn mark_port_open_command_treats_ipv4_mapped_ipv6_as_ipv4_reachability() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");

        assert!(!app.app_state.externally_accessable_port_v4);
        assert!(!app.app_state.externally_accessable_port_v6);

        let mapped_addr = SocketAddr::new(IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped()), 6681);
        app.handle_app_command(AppCommand::MarkPortOpen(mapped_addr))
            .await;

        assert!(app.app_state.externally_accessable_port_v4);
        assert!(!app.app_state.externally_accessable_port_v6);
    }

    #[tokio::test]
    async fn rebind_listener_with_ephemeral_port_notifies_managers_with_bound_port() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let (manager_tx, mut manager_rx) = mpsc::channel(4);
        app.torrent_manager_command_txs
            .insert(b"port-update-test".to_vec(), manager_tx);

        assert!(app.rebind_listener(0).await);

        let bound_port = app.client_configs.client_port;
        assert_ne!(bound_port, 0);

        let command = manager_rx
            .recv()
            .await
            .expect("manager should receive update");
        assert!(matches!(
            command,
            ManagerCommand::UpdateListenPort(port) if port == bound_port
        ));

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn apply_settings_update_restores_previous_port_when_rebind_fails() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let original_port = app.client_configs.client_port;
        let occupied_v4 = tokio::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .expect("bind occupied IPv4 port");
        let occupied_port = occupied_v4
            .local_addr()
            .expect("occupied local addr")
            .port();
        let _occupied_v6 =
            if TcpListener::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0))
                .await
                .is_ok()
            {
                match TcpListener::bind(SocketAddr::new(
                    IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                    occupied_port,
                ))
                .await
                {
                    Ok(listener) => Some(listener),
                    Err(error) if error.kind() == io::ErrorKind::AddrInUse => None,
                    Err(error) => panic!("bind occupied IPv6 port: {error}"),
                }
            } else {
                None
            };

        let mut next_settings = app.client_configs.clone();
        next_settings.client_port = occupied_port;

        app.apply_settings_update(next_settings, false).await;

        let rebound_port = app
            .listener
            .as_ref()
            .and_then(ListenerSet::local_port)
            .expect("listener should remain bound");
        assert_eq!(app.client_configs.client_port, original_port);
        assert_eq!(rebound_port, original_port);

        let _ = app.shutdown_tx.send(());
    }

    #[test]
    fn should_load_persisted_torrent_skips_only_deleting_entries() {
        let running = TorrentSettings {
            torrent_control_state: TorrentControlState::Running,
            ..Default::default()
        };
        let paused = TorrentSettings {
            torrent_control_state: TorrentControlState::Paused,
            ..Default::default()
        };
        let deleting = TorrentSettings {
            torrent_control_state: TorrentControlState::Deleting,
            ..Default::default()
        };

        assert!(should_load_persisted_torrent(&running));
        assert!(should_load_persisted_torrent(&paused));
        assert!(!should_load_persisted_torrent(&deleting));
    }

    #[tokio::test]
    async fn reset_tuning_for_objective_change_reschedules_deadline() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        app.tuning_controller.on_second_tick();
        app.app_state.tuning_countdown = app.tuning_controller.countdown_secs();
        let stale_deadline = time::Instant::now() + Duration::from_secs(300);
        app.next_tuning_at = stale_deadline;

        app.reset_tuning_for_objective_change();

        let reset_cadence = app.tuning_controller.cadence_secs();
        let remaining = app
            .next_tuning_at
            .saturating_duration_since(time::Instant::now());

        assert_eq!(app.app_state.tuning_countdown, reset_cadence);
        assert!(app.next_tuning_at < stale_deadline);
        assert!(remaining <= Duration::from_secs(reset_cadence));

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn peer_discovered_event_dedupes_connect_commands() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");

        let info_hash = vec![7; 20];
        let (manager_tx, mut manager_rx) = mpsc::channel(8);
        app.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_tx);

        let candidate = crate::torrent_manager::PeerCandidate::new(
            "127.0.0.1:6881".parse().unwrap(),
            crate::torrent_manager::PeerSource::Dht,
        );

        app.handle_manager_event(ManagerEvent::PeerDiscovered {
            info_hash: info_hash.clone(),
            candidate,
        });

        assert!(matches!(
            manager_rx.try_recv(),
            Ok(ManagerCommand::ConnectPeer { candidate: admitted })
                if admitted == candidate
        ));

        app.handle_manager_event(ManagerEvent::PeerDiscovered {
            info_hash,
            candidate,
        });

        assert!(
            manager_rx.try_recv().is_err(),
            "duplicate candidate should be suppressed by the global peer manager"
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn peer_connection_failure_puts_candidate_on_global_cooldown_until_lifecycle_resets() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");

        let info_hash = vec![8; 20];
        let (manager_tx, mut manager_rx) = mpsc::channel(8);
        app.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_tx);

        let candidate = crate::torrent_manager::PeerCandidate::new(
            "127.0.0.1:6882".parse().unwrap(),
            crate::torrent_manager::PeerSource::TrackerUdp,
        );

        app.handle_manager_event(ManagerEvent::PeerConnectionFailed {
            info_hash: info_hash.clone(),
            peer_addr: candidate.addr,
        });
        app.handle_manager_event(ManagerEvent::PeerDiscovered {
            info_hash: info_hash.clone(),
            candidate,
        });

        assert!(
            manager_rx.try_recv().is_err(),
            "candidate should be suppressed while the endpoint is cooling down"
        );

        app.handle_manager_event(ManagerEvent::PeerConnected {
            info_hash: info_hash.clone(),
            peer_addr: Some(candidate.addr),
        });
        app.handle_manager_event(ManagerEvent::PeerDisconnected {
            info_hash: info_hash.clone(),
            peer_addr: Some(candidate.addr),
        });
        app.handle_manager_event(ManagerEvent::PeerDiscovered {
            info_hash,
            candidate,
        });

        assert!(matches!(
            manager_rx.try_recv(),
            Ok(ManagerCommand::ConnectPeer { candidate: admitted })
                if admitted == candidate
        ));

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn connected_peer_candidate_is_suppressed_until_disconnect_event_arrives() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");

        let info_hash = vec![9; 20];
        let (manager_tx, mut manager_rx) = mpsc::channel(8);
        app.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_tx);

        let candidate = crate::torrent_manager::PeerCandidate::new(
            "127.0.0.1:6883".parse().unwrap(),
            crate::torrent_manager::PeerSource::Pex,
        );

        app.handle_manager_event(ManagerEvent::PeerConnected {
            info_hash: info_hash.clone(),
            peer_addr: Some(candidate.addr),
        });
        app.handle_manager_event(ManagerEvent::PeerDiscovered {
            info_hash: info_hash.clone(),
            candidate,
        });

        assert!(
            manager_rx.try_recv().is_err(),
            "active peers should not be re-admitted while still connected"
        );

        app.handle_manager_event(ManagerEvent::PeerDisconnected {
            info_hash: info_hash.clone(),
            peer_addr: Some(candidate.addr),
        });
        app.handle_manager_event(ManagerEvent::PeerDiscovered {
            info_hash,
            candidate,
        });

        assert!(matches!(
            manager_rx.try_recv(),
            Ok(ManagerCommand::ConnectPeer { candidate: admitted })
                if admitted == candidate
        ));

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn torrent_resumed_event_requeues_known_peer_via_global_peer_manager() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");

        let info_hash = vec![10; 20];
        let (manager_tx, mut manager_rx) = mpsc::channel(8);
        app.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_tx);

        let peer_addr: SocketAddr = "127.0.0.1:6884".parse().unwrap();

        app.handle_manager_event(ManagerEvent::PeerConnected {
            info_hash: info_hash.clone(),
            peer_addr: Some(peer_addr),
        });
        app.handle_manager_event(ManagerEvent::TorrentPaused {
            info_hash: info_hash.clone(),
        });
        app.handle_manager_event(ManagerEvent::TorrentResumed {
            info_hash: info_hash.clone(),
        });

        assert!(matches!(
            manager_rx.try_recv(),
            Ok(ManagerCommand::ConnectPeer { candidate })
                if candidate.addr == peer_addr
                    && candidate.source == crate::torrent_manager::PeerSource::Resume
        ));

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn torrent_resumed_event_announces_to_dht_when_port_is_known_open() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        app.client_configs.client_port = 6681;
        let recorder = TestDhtRecorder::default();
        app.dht_service = DhtService::from_test_recorder(recorder.clone());
        app.dht_status_rx = app.dht_service.subscribe_status();
        app.app_state.externally_accessable_port_v4 = true;

        let info_hash = vec![11; 20];
        app.handle_manager_event(ManagerEvent::TorrentResumed {
            info_hash: info_hash.clone(),
        });
        tokio::task::yield_now().await;

        assert_eq!(recorder.recorded_announces(), vec![(info_hash, Some(6681))]);

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn handle_manager_event_file_probe_status_marks_data_unavailable() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let info_hash = b"probe_hash".to_vec();

        let mut display = TorrentDisplayState::default();
        display.latest_state.torrent_name = "probe torrent".to_string();
        display.latest_state.torrent_control_state = TorrentControlState::Running;
        app.app_state.torrents.insert(info_hash.clone(), display);
        app.integrity_scheduler
            .sync_torrents(app.current_integrity_snapshots());

        app.handle_manager_event(ManagerEvent::FileProbeBatchResult {
            info_hash: info_hash.clone(),
            result: FileProbeBatchResult {
                epoch: 0,
                scanned_files: 2,
                next_file_index: 0,
                reached_end_of_manifest: true,
                pending_metadata: false,
                problem_files: vec![FileProbeEntry {
                    relative_path: "missing.bin".into(),
                    absolute_path: "/tmp/missing.bin".into(),
                    error: StorageError::from(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "No such file or directory",
                    )),
                    expected_size: 10,
                    observed_size: None,
                }],
            },
        });

        let torrent = app
            .app_state
            .torrents
            .get(&info_hash)
            .expect("torrent display should exist");
        assert!(!torrent.latest_state.data_available);
        assert_eq!(
            torrent.latest_state.torrent_control_state,
            TorrentControlState::Running
        );
        assert!(app.app_state.ui.needs_redraw);

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn load_next_startup_batch_loads_only_one_deferred_torrent() {
        let mut settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        for index in 0..6 {
            let hash_digit = char::from_digit((index + 1) as u32, 16).expect("hex digit");
            settings.torrents.push(TorrentSettings {
                torrent_or_magnet: format!(
                    "magnet:?xt=urn:btih:{}",
                    hash_digit.to_string().repeat(40)
                ),
                name: format!("sample-start-{}", index),
                torrent_control_state: TorrentControlState::Running,
                ..Default::default()
            });
        }

        let mut app = App::new(
            crate::config::Settings {
                client_port: 0,
                ..Default::default()
            },
            AppRuntimeMode::Normal,
        )
        .await
        .expect("build app");
        app.client_configs.torrents = settings.torrents.clone();
        app.startup_deferred_load_queue = settings
            .torrents
            .iter()
            .filter_map(|torrent| info_hash_from_torrent_source(&torrent.torrent_or_magnet))
            .collect();

        app.load_next_startup_batch().await;

        assert_eq!(app.app_state.torrents.len(), 1);
        assert_eq!(app.startup_deferred_load_queue.len(), 5);
        assert!(app.next_startup_load_at.is_some());

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn load_next_startup_batch_keeps_failed_deferred_torrent_queued() {
        let info_hash_hex = "1".repeat(40);
        let missing_torrent_path = format!("/tmp/{}.torrent", info_hash_hex);
        let torrent = TorrentSettings {
            torrent_or_magnet: missing_torrent_path.clone(),
            name: "missing-startup".to_string(),
            torrent_control_state: TorrentControlState::Running,
            ..Default::default()
        };

        let mut app = App::new(
            crate::config::Settings {
                client_port: 0,
                ..Default::default()
            },
            AppRuntimeMode::Normal,
        )
        .await
        .expect("build app");
        app.client_configs.torrents = vec![torrent.clone()];
        app.startup_deferred_load_queue =
            VecDeque::from([info_hash_from_torrent_source(&torrent.torrent_or_magnet)
                .expect("derive info hash from path")]);

        app.load_next_startup_batch().await;

        assert!(app.app_state.torrents.is_empty());
        assert_eq!(app.startup_deferred_load_queue.len(), 1);
        assert!(app.next_startup_load_at.is_some());

        let payload = build_persist_payload(
            &mut app.client_configs,
            &mut app.app_state,
            &app.startup_deferred_load_queue,
        );
        assert_eq!(payload.settings.torrents.len(), 1);
        assert_eq!(
            payload.settings.torrents[0].torrent_or_magnet,
            missing_torrent_path
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn load_next_startup_batch_rotates_failed_deferred_torrent_behind_later_entries() {
        let failed_info_hash_hex = "1".repeat(40);
        let failed_torrent = TorrentSettings {
            torrent_or_magnet: format!("/tmp/{}.torrent", failed_info_hash_hex),
            name: "missing-startup".to_string(),
            torrent_control_state: TorrentControlState::Running,
            ..Default::default()
        };
        let deferred_running_torrent = TorrentSettings {
            torrent_or_magnet: format!("magnet:?xt=urn:btih:{}", "2".repeat(40)),
            name: "later-startup".to_string(),
            torrent_control_state: TorrentControlState::Running,
            ..Default::default()
        };
        let failed_info_hash = info_hash_from_torrent_source(&failed_torrent.torrent_or_magnet)
            .expect("derive failed info hash");
        let deferred_running_hash =
            info_hash_from_torrent_source(&deferred_running_torrent.torrent_or_magnet)
                .expect("derive deferred running hash");

        let mut app = App::new(
            crate::config::Settings {
                client_port: 0,
                ..Default::default()
            },
            AppRuntimeMode::Normal,
        )
        .await
        .expect("build app");
        app.client_configs.torrents = vec![failed_torrent.clone(), deferred_running_torrent];
        app.startup_deferred_load_queue =
            VecDeque::from([failed_info_hash.clone(), deferred_running_hash.clone()]);

        app.load_next_startup_batch().await;
        assert_eq!(
            app.startup_deferred_load_queue,
            VecDeque::from([deferred_running_hash.clone(), failed_info_hash.clone()])
        );
        assert!(app.app_state.torrents.is_empty());

        app.load_next_startup_batch().await;

        assert_eq!(app.app_state.torrents.len(), 1);
        assert_eq!(
            app.startup_deferred_load_queue,
            VecDeque::from([failed_info_hash.clone()])
        );

        let payload = build_persist_payload(
            &mut app.client_configs,
            &mut app.app_state,
            &app.startup_deferred_load_queue,
        );
        assert_eq!(payload.settings.torrents.len(), 2);
        assert!(payload
            .settings
            .torrents
            .iter()
            .any(|torrent| torrent.torrent_or_magnet == failed_torrent.torrent_or_magnet));
        assert!(payload.settings.torrents.iter().any(|torrent| {
            torrent
                .torrent_or_magnet
                .starts_with("magnet:?xt=urn:btih:")
                && info_hash_from_torrent_source(&torrent.torrent_or_magnet).as_deref()
                    == Some(deferred_running_hash.as_slice())
        }));

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn data_availability_fault_records_event_journal_entry() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let info_hash = b"fault_journal_hash".to_vec();

        let mut display = TorrentDisplayState::default();
        display.latest_state.info_hash = info_hash.clone();
        display.latest_state.torrent_name = "Sample Fault".to_string();
        display.latest_state.torrent_control_state = TorrentControlState::Running;
        display.latest_state.data_available = true;
        app.app_state.torrents.insert(info_hash.clone(), display);
        app.integrity_scheduler
            .sync_torrents(app.current_integrity_snapshots());

        app.handle_manager_event(ManagerEvent::DataAvailabilityFault {
            info_hash: info_hash.clone(),
            piece_index: 4,
            error: StorageError::from(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "No such file or directory",
            )),
        });

        let journal_entry = app
            .app_state
            .event_journal_state
            .entries
            .iter()
            .find(|entry| entry.event_type == EventType::DataUnavailable)
            .expect("expected data unavailable event");
        let expected_hash = hex::encode(&info_hash);
        assert_eq!(
            journal_entry.info_hash_hex.as_deref(),
            Some(expected_hash.as_str())
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn ingest_journal_records_queue_and_terminal_result_with_shared_correlation() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let queued_path = std::env::temp_dir().join("event-journal-alpha.magnet");

        app.record_watch_path_discovered(&queued_path);
        app.record_ingest_result(
            &queued_path,
            &CommandIngestResult::Duplicate {
                info_hash: Some(vec![0x11; 20]),
                torrent_name: Some("Sample Alpha".to_string()),
            },
        );

        let entries = &app.app_state.event_journal_state.entries;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].event_type, EventType::IngestQueued);
        assert_eq!(entries[1].event_type, EventType::IngestDuplicate);
        assert_eq!(entries[0].correlation_id, entries[1].correlation_id);
        assert_eq!(entries[0].source_path.as_ref(), Some(&queued_path));
        assert_eq!(entries[1].source_path.as_ref(), Some(&queued_path));
        assert_eq!(
            entries[0].details,
            EventDetails::Ingest {
                origin: IngestOrigin::WatchFolder,
                ingest_kind: IngestKind::MagnetFile,
            }
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn control_journal_preserves_watch_folder_origin() {
        let _guard = lock_shared_env();
        let _temp_paths = configure_temp_app_paths_for_test();
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let queued_path = std::env::temp_dir().join("event-journal-alpha.control");
        let request = ControlRequest::Pause {
            info_hash_hex: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        };

        assert!(app.record_control_queued(
            queued_path.clone(),
            request.clone(),
            ControlOrigin::WatchFolder
        ));
        app.record_control_result(&queued_path, &request, Ok("Paused torrent".to_string()));

        let entries = &app.app_state.event_journal_state.entries;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].event_type, EventType::ControlQueued);
        assert_eq!(entries[1].event_type, EventType::ControlApplied);
        assert_eq!(entries[0].correlation_id, entries[1].correlation_id);
        assert_eq!(
            entries[0].details,
            control_event_details(&request, ControlOrigin::WatchFolder)
        );
        assert_eq!(
            entries[1].details,
            control_event_details(&request, ControlOrigin::WatchFolder)
        );

        let _ = app.shutdown_tx.send(());
        set_app_paths_override_for_tests(None);
    }

    #[tokio::test]
    async fn control_origin_for_ingest_path_uses_rss_origin_when_available() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let queued_path = std::env::temp_dir().join("event-journal-rss.magnet");

        app.record_rss_queued(
            queued_path.clone(),
            IngestOrigin::RssManual,
            IngestKind::MagnetFile,
        );

        assert_eq!(
            app.control_origin_for_ingest_path(&queued_path),
            ControlOrigin::RssManual
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn manual_torrent_browser_moves_standalone_watch_file_to_processed_and_updates_journal() {
        let _guard = lock_shared_env();
        let dir = configure_temp_app_paths_for_test();
        let data_dir = dir.path().join("data");
        let watch_dir = data_dir.join("watch_files");
        let processed_dir = data_dir.join("processed_files");
        std::fs::create_dir_all(&watch_dir).expect("create watch dir");

        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("integration_tests")
            .join("torrents")
            .join("v1")
            .join("single_4k.bin.torrent");
        let watched_path = watch_dir.join("manual-input.torrent");
        std::fs::copy(&fixture, &watched_path).expect("copy fixture");

        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");

        app.record_watch_path_discovered(&watched_path);
        app.open_manual_browser_for_torrent_file(watched_path.clone())
            .expect("open manual browser");

        let final_path = processed_dir.join("manual-input.torrent");
        assert_eq!(app.app_state.pending_torrent_path, Some(final_path.clone()));
        assert!(final_path.exists());
        assert!(!watched_path.exists());
        assert_eq!(
            app.app_state
                .event_journal_state
                .entries
                .iter()
                .rev()
                .find(|entry| entry.event_type == EventType::IngestQueued)
                .and_then(|entry| entry.source_path.clone()),
            Some(final_path)
        );

        let _ = app.shutdown_tx.send(());
        set_app_paths_override_for_tests(None);
    }

    #[tokio::test]
    async fn manual_torrent_browser_moves_shared_inbox_file_to_shared_processed_and_updates_journal(
    ) {
        let _guard = lock_shared_env();
        let shared_root = tempfile::tempdir().expect("create shared root");
        let effective_root = shared_root.path().join("superseedr-config");
        let original_shared_dir = env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
        let original_host_id = env::var_os("SUPERSEEDR_SHARED_HOST_ID");

        env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", shared_root.path());
        env::set_var("SUPERSEEDR_SHARED_HOST_ID", "node-a");
        clear_shared_config_state_for_tests();

        std::fs::create_dir_all(effective_root.join("hosts").join("node-a"))
            .expect("create hosts dir");
        std::fs::write(
            effective_root
                .join("hosts")
                .join("node-a")
                .join("config.toml"),
            "client_port = 0\n",
        )
        .expect("write host config");
        std::fs::create_dir_all(effective_root.join("inbox")).expect("create shared inbox");

        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("integration_tests")
            .join("torrents")
            .join("v1")
            .join("single_4k.bin.torrent");
        let watched_path = effective_root.join("inbox").join("manual-input.torrent");
        std::fs::copy(&fixture, &watched_path).expect("copy fixture");

        let settings = crate::config::load_settings().expect("load shared settings");
        let mut app = App::new(settings, AppRuntimeMode::SharedLeader)
            .await
            .expect("build shared app");

        assert!(app.record_ingest_queued(
            watched_path.clone(),
            IngestOrigin::WatchFolder,
            IngestKind::TorrentFile,
            crate::config::shared_inbox_path(),
        ));
        app.open_manual_browser_for_torrent_file(watched_path.clone())
            .expect("open manual browser");

        let final_path = effective_root
            .join("processed")
            .join("manual-input.torrent");
        assert_eq!(app.app_state.pending_torrent_path, Some(final_path.clone()));
        assert!(final_path.exists());
        assert!(!watched_path.exists());
        assert_eq!(
            app.app_state
                .event_journal_state
                .entries
                .iter()
                .rev()
                .find(|entry| entry.event_type == EventType::IngestQueued)
                .and_then(|entry| entry.source_path.clone()),
            Some(final_path)
        );

        let _ = app.shutdown_tx.send(());
        if let Some(value) = original_shared_dir {
            env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
        }
        if let Some(value) = original_host_id {
            env::set_var("SUPERSEEDR_SHARED_HOST_ID", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_HOST_ID");
        }
        clear_shared_config_state_for_tests();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn missing_verbatim_shared_inbox_magnet_is_ignored() {
        let _guard = lock_shared_env();
        let shared_root = tempfile::tempdir().expect("create shared root");
        let effective_root = shared_root.path().join("superseedr-config");
        let original_shared_dir = env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
        let original_host_id = env::var_os("SUPERSEEDR_SHARED_HOST_ID");

        env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", shared_root.path());
        env::set_var("SUPERSEEDR_SHARED_HOST_ID", "node-a");
        clear_shared_config_state_for_tests();

        std::fs::create_dir_all(effective_root.join("hosts").join("node-a"))
            .expect("create hosts dir");
        std::fs::write(
            effective_root
                .join("hosts")
                .join("node-a")
                .join("config.toml"),
            "client_port = 0\n",
        )
        .expect("write host config");
        std::fs::create_dir_all(effective_root.join("inbox")).expect("create shared inbox");

        let app = App::new(
            crate::config::load_settings().expect("load shared settings"),
            AppRuntimeMode::SharedLeader,
        )
        .await
        .expect("build shared app");

        let verbatim_missing_path = PathBuf::from(format!(
            r"\\?\{}",
            effective_root
                .join("inbox")
                .join("stale-event.magnet")
                .display()
        ));

        assert!(watched_parent_matches(
            &verbatim_missing_path,
            &effective_root.join("inbox")
        ));
        assert!(matches!(
            app.resolve_add_ingress_action(IngestSource::MagnetFile, &verbatim_missing_path),
            super::AddIngressAction::IgnoreMissingSharedInboxItem { .. }
        ));

        let _ = app.shutdown_tx.send(());
        if let Some(value) = original_shared_dir {
            env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
        }
        if let Some(value) = original_host_id {
            env::set_var("SUPERSEEDR_SHARED_HOST_ID", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_HOST_ID");
        }
        clear_shared_config_state_for_tests();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_shared_inbox_magnet_is_not_ignored_as_missing() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = lock_shared_env();
        let shared_root = tempfile::tempdir().expect("create shared root");
        let effective_root = shared_root.path().join("superseedr-config");
        let shared_inbox = effective_root.join("inbox");
        let original_shared_dir = env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
        let original_host_id = env::var_os("SUPERSEEDR_SHARED_HOST_ID");

        env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", shared_root.path());
        env::set_var("SUPERSEEDR_SHARED_HOST_ID", "node-a");
        clear_shared_config_state_for_tests();

        std::fs::create_dir_all(effective_root.join("hosts").join("node-a"))
            .expect("create hosts dir");
        std::fs::write(
            effective_root
                .join("hosts")
                .join("node-a")
                .join("config.toml"),
            "client_port = 0\n",
        )
        .expect("write host config");
        std::fs::create_dir_all(&shared_inbox).expect("create shared inbox");

        let app = App::new(
            crate::config::load_settings().expect("load shared settings"),
            AppRuntimeMode::SharedLeader,
        )
        .await
        .expect("build shared app");

        let unreadable_path = shared_inbox.join("permission-denied.magnet");
        std::fs::set_permissions(&shared_inbox, std::fs::Permissions::from_mode(0o000))
            .expect("make shared inbox unreadable");

        let action = app.resolve_add_ingress_action(IngestSource::MagnetFile, &unreadable_path);

        std::fs::set_permissions(&shared_inbox, std::fs::Permissions::from_mode(0o700))
            .expect("restore shared inbox permissions");

        assert!(matches!(action, super::AddIngressAction::Fail { .. }));

        let _ = app.shutdown_tx.send(());
        if let Some(value) = original_shared_dir {
            env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
        }
        if let Some(value) = original_host_id {
            env::set_var("SUPERSEEDR_SHARED_HOST_ID", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_HOST_ID");
        }
        clear_shared_config_state_for_tests();
    }

    #[tokio::test]
    async fn partial_probe_result_does_not_clear_previous_unavailable_state() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let info_hash = b"partial_probe_hash".to_vec();

        let mut display = TorrentDisplayState::default();
        display.latest_state.torrent_name = "partial probe torrent".to_string();
        display.latest_state.data_available = false;
        display.latest_file_probe_status =
            Some(TorrentFileProbeStatus::Files(vec![FileProbeEntry {
                relative_path: "missing.bin".into(),
                absolute_path: "/tmp/missing.bin".into(),
                error: StorageError::from(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "No such file or directory",
                )),
                expected_size: 10,
                observed_size: None,
            }]));
        app.app_state.torrents.insert(info_hash.clone(), display);
        app.integrity_scheduler
            .sync_torrents(app.current_integrity_snapshots());

        app.handle_manager_event(ManagerEvent::FileProbeBatchResult {
            info_hash: info_hash.clone(),
            result: FileProbeBatchResult {
                epoch: 0,
                scanned_files: 128,
                next_file_index: 128,
                reached_end_of_manifest: false,
                pending_metadata: false,
                problem_files: Vec::new(),
            },
        });

        let torrent = app
            .app_state
            .torrents
            .get(&info_hash)
            .expect("torrent display should exist");
        assert!(!torrent.latest_state.data_available);
        assert_eq!(
            torrent.latest_file_probe_status,
            Some(TorrentFileProbeStatus::Files(vec![FileProbeEntry {
                relative_path: "missing.bin".into(),
                absolute_path: "/tmp/missing.bin".into(),
                error: StorageError::from(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "No such file or directory",
                )),
                expected_size: 10,
                observed_size: None,
            }]))
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn dispatch_integrity_probe_batches_requests_work_immediately() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let info_hash = b"dispatch_probe_hash".to_vec();

        let mut display = TorrentDisplayState::default();
        display.latest_state.info_hash = info_hash.clone();
        display.latest_state.torrent_name = "dispatch probe torrent".to_string();
        display.latest_state.torrent_control_state = TorrentControlState::Running;
        display.latest_state.is_complete = true;
        app.app_state.torrents.insert(info_hash.clone(), display);

        let (manager_tx, mut manager_rx) = mpsc::channel(4);
        app.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_tx);

        app.dispatch_integrity_probe_batches();

        let command = tokio::time::timeout(std::time::Duration::from_secs(1), manager_rx.recv())
            .await
            .expect("probe command timed out")
            .expect("expected probe command");
        assert!(matches!(
            command,
            ManagerCommand::ProbeFileBatch {
                epoch: 0,
                start_file_index: 0,
                max_files: _
            }
        ));

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn metadata_loaded_dispatches_probe_without_waiting_for_tick() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let info_hash = b"metadata_probe_hash".to_vec();

        let mut display = TorrentDisplayState::default();
        display.latest_state.info_hash = info_hash.clone();
        display.latest_state.torrent_name = "metadata probe torrent".to_string();
        display.latest_state.torrent_control_state = TorrentControlState::Running;
        display.latest_state.is_complete = true;
        app.app_state.torrents.insert(info_hash.clone(), display);

        let (manager_tx, mut manager_rx) = mpsc::channel(4);
        app.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_tx);
        app.dispatch_integrity_probe_batches();

        let first_command =
            tokio::time::timeout(std::time::Duration::from_secs(1), manager_rx.recv())
                .await
                .expect("initial probe command timed out")
                .expect("expected initial probe command");
        assert!(matches!(
            first_command,
            ManagerCommand::ProbeFileBatch { .. }
        ));

        app.handle_manager_event(ManagerEvent::FileProbeBatchResult {
            info_hash: info_hash.clone(),
            result: FileProbeBatchResult {
                epoch: 0,
                scanned_files: 0,
                next_file_index: 0,
                reached_end_of_manifest: false,
                pending_metadata: true,
                problem_files: Vec::new(),
            },
        });

        let torrent = crate::torrent_file::Torrent::default();
        app.handle_manager_event(ManagerEvent::MetadataLoaded {
            info_hash: info_hash.clone(),
            torrent: Box::new(torrent),
        });

        let second_command =
            tokio::time::timeout(std::time::Duration::from_secs(1), manager_rx.recv())
                .await
                .expect("post-metadata probe command timed out")
                .expect("expected immediate post-metadata probe command");
        assert!(matches!(
            second_command,
            ManagerCommand::ProbeFileBatch { .. }
        ));

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn metadata_loaded_updates_layout_before_fault_fanout_for_single_entry_multi_file() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let faulted_info_hash = b"metadata_faulted_hash".to_vec();
        let sibling_info_hash = b"metadata_sibling_hash".to_vec();

        let mut faulted = TorrentDisplayState::default();
        faulted.latest_state.info_hash = faulted_info_hash.clone();
        faulted.latest_state.torrent_name = "shared-name".to_string();
        faulted.latest_state.torrent_control_state = TorrentControlState::Running;
        faulted.latest_state.download_path = Some("/downloads/shared".into());
        faulted.latest_state.container_name = Some(String::new());
        app.app_state
            .torrents
            .insert(faulted_info_hash.clone(), faulted);

        let mut sibling = TorrentDisplayState::default();
        sibling.latest_state.info_hash = sibling_info_hash.clone();
        sibling.latest_state.torrent_name = "shared-name".to_string();
        sibling.latest_state.torrent_control_state = TorrentControlState::Running;
        sibling.latest_state.download_path = Some("/downloads/shared".into());
        sibling.latest_state.file_count = Some(1);
        app.app_state
            .torrents
            .insert(sibling_info_hash.clone(), sibling);

        let (faulted_tx, mut faulted_rx) = mpsc::channel(8);
        let (sibling_tx, mut sibling_rx) = mpsc::channel(8);
        app.torrent_manager_command_txs
            .insert(faulted_info_hash.clone(), faulted_tx);
        app.torrent_manager_command_txs
            .insert(sibling_info_hash.clone(), sibling_tx);
        app.integrity_scheduler
            .sync_torrents(app.current_integrity_snapshots());

        let torrent = crate::torrent_file::Torrent {
            info: crate::torrent_file::Info {
                name: "shared-name".to_string(),
                files: vec![crate::torrent_file::InfoFile {
                    length: 1,
                    path: vec!["entry.bin".to_string()],
                    md5sum: None,
                    attr: None,
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        app.handle_manager_event(ManagerEvent::MetadataLoaded {
            info_hash: faulted_info_hash.clone(),
            torrent: Box::new(torrent),
        });

        while faulted_rx.try_recv().is_ok() {}
        while sibling_rx.try_recv().is_ok() {}

        app.handle_manager_event(ManagerEvent::DataAvailabilityFault {
            info_hash: faulted_info_hash.clone(),
            piece_index: 7,
            error: StorageError::from(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "No such file or directory",
            )),
        });

        let faulted_command = faulted_rx
            .recv()
            .await
            .expect("expected faulted torrent probe command");
        assert!(matches!(
            faulted_command,
            ManagerCommand::ProbeFileBatch {
                start_file_index: 0,
                ..
            }
        ));
        assert!(matches!(
            sibling_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn data_availability_fault_does_not_fan_out_across_flat_torrents_in_same_directory() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let faulted_info_hash = b"faulted_probe_hash".to_vec();
        let sibling_info_hash = b"sibling_probe_hash".to_vec();

        let mut faulted = TorrentDisplayState::default();
        faulted.latest_state.info_hash = faulted_info_hash.clone();
        faulted.latest_state.torrent_name = "faulted probe torrent".to_string();
        faulted.latest_state.torrent_control_state = TorrentControlState::Running;
        faulted.latest_state.download_path = Some("/downloads/shared".into());
        faulted.latest_state.file_count = Some(1);
        app.app_state
            .torrents
            .insert(faulted_info_hash.clone(), faulted);

        let mut sibling = TorrentDisplayState::default();
        sibling.latest_state.info_hash = sibling_info_hash.clone();
        sibling.latest_state.torrent_name = "sibling probe torrent".to_string();
        sibling.latest_state.torrent_control_state = TorrentControlState::Running;
        sibling.latest_state.download_path = Some("/downloads/shared".into());
        sibling.latest_state.file_count = Some(1);
        app.app_state
            .torrents
            .insert(sibling_info_hash.clone(), sibling);

        let (faulted_tx, mut faulted_rx) = mpsc::channel(4);
        let (sibling_tx, mut sibling_rx) = mpsc::channel(4);
        app.torrent_manager_command_txs
            .insert(faulted_info_hash.clone(), faulted_tx);
        app.torrent_manager_command_txs
            .insert(sibling_info_hash.clone(), sibling_tx);
        app.integrity_scheduler
            .sync_torrents(app.current_integrity_snapshots());
        for request in app.integrity_scheduler.drain_due_probe_requests() {
            let _ = app.integrity_scheduler.on_probe_batch_result(
                &request.info_hash,
                FileProbeBatchResult {
                    epoch: request.epoch,
                    scanned_files: 1,
                    next_file_index: 0,
                    reached_end_of_manifest: true,
                    pending_metadata: false,
                    problem_files: Vec::new(),
                },
            );
        }

        app.handle_manager_event(ManagerEvent::DataAvailabilityFault {
            info_hash: faulted_info_hash.clone(),
            piece_index: 5,
            error: StorageError::from(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "No such file or directory",
            )),
        });

        let faulted_command = faulted_rx
            .recv()
            .await
            .expect("expected faulted torrent probe command");
        assert!(matches!(
            faulted_command,
            ManagerCommand::ProbeFileBatch {
                start_file_index: 0,
                ..
            }
        ));
        assert!(matches!(
            sibling_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        let faulted_torrent = app
            .app_state
            .torrents
            .get(&faulted_info_hash)
            .expect("faulted torrent display should exist");
        let sibling_torrent = app
            .app_state
            .torrents
            .get(&sibling_info_hash)
            .expect("sibling torrent display should exist");
        assert!(!faulted_torrent.latest_state.data_available);
        assert!(sibling_torrent.latest_state.data_available);

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn partial_probe_marks_torrent_unavailable_before_sweep_completion() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let info_hash = b"partial_unavailable_probe_hash".to_vec();

        let mut display = TorrentDisplayState::default();
        display.latest_state.info_hash = info_hash.clone();
        display.latest_state.torrent_name = "partial probe torrent".to_string();
        display.latest_state.torrent_control_state = TorrentControlState::Running;
        display.latest_state.data_available = true;
        app.app_state.torrents.insert(info_hash.clone(), display);
        app.integrity_scheduler
            .sync_torrents(app.current_integrity_snapshots());

        let (manager_tx, mut manager_rx) = mpsc::channel(4);
        app.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_tx);

        app.handle_manager_event(ManagerEvent::FileProbeBatchResult {
            info_hash: info_hash.clone(),
            result: FileProbeBatchResult {
                epoch: 0,
                scanned_files: 256,
                next_file_index: 256,
                reached_end_of_manifest: false,
                pending_metadata: false,
                problem_files: vec![FileProbeEntry {
                    relative_path: "missing-segment.bin".into(),
                    absolute_path: "/downloads/shared/missing-segment.bin".into(),
                    error: StorageError::from(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "No such file or directory",
                    )),
                    expected_size: 1,
                    observed_size: None,
                }],
            },
        });

        let manager_command = manager_rx
            .recv()
            .await
            .expect("expected manager availability downgrade");
        assert!(matches!(
            manager_command,
            ManagerCommand::SetDataAvailability(false)
        ));
        let replacement_probe = manager_rx
            .recv()
            .await
            .expect("expected continuation probe batch");
        assert!(matches!(
            replacement_probe,
            ManagerCommand::ProbeFileBatch {
                start_file_index: 256,
                ..
            }
        ));

        let torrent = app
            .app_state
            .torrents
            .get(&info_hash)
            .expect("torrent display should exist");
        assert!(!torrent.latest_state.data_available);
        assert!(torrent.latest_file_probe_status.is_none());

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn healthy_probe_requests_manager_recovery_but_does_not_flip_ui_until_metrics() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let info_hash = b"recovery_probe_hash".to_vec();

        let mut display = TorrentDisplayState::default();
        display.latest_state.info_hash = info_hash.clone();
        display.latest_state.torrent_name = "recovery probe torrent".to_string();
        display.latest_state.torrent_control_state = TorrentControlState::Running;
        display.latest_state.data_available = false;
        app.app_state.torrents.insert(info_hash.clone(), display);
        app.integrity_scheduler
            .sync_torrents(app.current_integrity_snapshots());

        let (manager_tx, mut manager_rx) = mpsc::channel(4);
        app.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_tx);

        app.handle_manager_event(ManagerEvent::FileProbeBatchResult {
            info_hash: info_hash.clone(),
            result: FileProbeBatchResult {
                epoch: 0,
                scanned_files: 1,
                next_file_index: 0,
                reached_end_of_manifest: true,
                pending_metadata: false,
                problem_files: Vec::new(),
            },
        });

        let recovery_command = manager_rx.recv().await.expect("expected recovery command");
        assert!(matches!(
            recovery_command,
            ManagerCommand::SetDataAvailability(true)
        ));
        assert!(matches!(
            manager_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        let torrent = app
            .app_state
            .torrents
            .get(&info_hash)
            .expect("torrent display should exist");
        assert!(!torrent.latest_state.data_available);
        let recovery_entry = app
            .app_state
            .event_journal_state
            .entries
            .iter()
            .find(|entry| entry.event_type == EventType::DataRecovered)
            .expect("expected data recovery event");
        let expected_hash = hex::encode(&info_hash);
        assert_eq!(
            recovery_entry.info_hash_hex.as_deref(),
            Some(expected_hash.as_str())
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn completion_transition_records_single_torrent_completed_event() {
        let _guard = lock_shared_env();
        let _temp_paths = configure_temp_app_paths_for_test();
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let info_hash = b"completion_journal_hash".to_vec();

        let mut display = TorrentDisplayState::default();
        display.latest_state.info_hash = info_hash.clone();
        display.latest_state.torrent_name = "Sample Completion".to_string();
        display.latest_state.number_of_pieces_total = 10;
        display.latest_state.number_of_pieces_completed = 3;
        display.latest_state.activity_message = "Downloading".to_string();
        app.app_state.torrents.insert(info_hash.clone(), display);

        let (tx, rx) = watch::channel(TorrentMetrics {
            info_hash: info_hash.clone(),
            torrent_name: "Sample Completion".to_string(),
            number_of_pieces_total: 10,
            number_of_pieces_completed: 3,
            activity_message: "Downloading".to_string(),
            ..Default::default()
        });
        app.torrent_metric_watch_rxs.insert(info_hash.clone(), rx);

        tx.send(TorrentMetrics {
            info_hash: info_hash.clone(),
            torrent_name: "Sample Completion".to_string(),
            number_of_pieces_total: 10,
            number_of_pieces_completed: 10,
            is_complete: true,
            activity_message: "Seeding".to_string(),
            ..Default::default()
        })
        .expect("send completion metrics");
        app.drain_latest_torrent_metrics();

        tx.send(TorrentMetrics {
            info_hash: info_hash.clone(),
            torrent_name: "Sample Completion".to_string(),
            number_of_pieces_total: 10,
            number_of_pieces_completed: 10,
            is_complete: true,
            activity_message: "Seeding".to_string(),
            ..Default::default()
        })
        .expect("send steady completion metrics");
        app.drain_latest_torrent_metrics();

        let completion_entries = app
            .app_state
            .event_journal_state
            .entries
            .iter()
            .filter(|entry| entry.event_type == EventType::TorrentCompleted)
            .count();
        assert_eq!(completion_entries, 1);

        let _ = app.shutdown_tx.send(());
        set_app_paths_override_for_tests(None);
    }

    #[tokio::test]
    async fn completed_torrents_restored_as_complete_do_not_rejournal_on_metrics_refresh() {
        let _guard = lock_shared_env();
        let _temp_paths = configure_temp_app_paths_for_test();
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let info_hash = b"restored_complete_hash".to_vec();

        let mut display = TorrentDisplayState::default();
        display.latest_state.info_hash = info_hash.clone();
        display.latest_state.torrent_name = "Sample Restore".to_string();
        display.latest_state.number_of_pieces_total = 10;
        display.latest_state.number_of_pieces_completed = 10;
        display.latest_state.is_complete = true;
        display.latest_state.activity_message = "Seeding".to_string();
        app.app_state.torrents.insert(info_hash.clone(), display);

        let (tx, rx) = watch::channel(TorrentMetrics {
            info_hash: info_hash.clone(),
            torrent_name: "Sample Restore".to_string(),
            number_of_pieces_total: 10,
            number_of_pieces_completed: 10,
            is_complete: true,
            activity_message: "Seeding".to_string(),
            ..Default::default()
        });
        app.torrent_metric_watch_rxs.insert(info_hash.clone(), rx);

        tx.send(TorrentMetrics {
            info_hash: info_hash.clone(),
            torrent_name: "Sample Restore".to_string(),
            number_of_pieces_total: 10,
            number_of_pieces_completed: 10,
            is_complete: true,
            activity_message: "Seeding".to_string(),
            ..Default::default()
        })
        .expect("send completed metrics");
        app.drain_latest_torrent_metrics();

        let completion_entries = app
            .app_state
            .event_journal_state
            .entries
            .iter()
            .filter(|entry| entry.event_type == EventType::TorrentCompleted)
            .count();
        assert_eq!(completion_entries, 0);

        let _ = app.shutdown_tx.send(());
        set_app_paths_override_for_tests(None);
    }

    #[tokio::test]
    async fn completed_torrents_do_not_duplicate_existing_completion_journal_entries() {
        let _guard = lock_shared_env();
        let _temp_paths = configure_temp_app_paths_for_test();
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let info_hash = b"existing_complete_hash".to_vec();
        let info_hash_hex = hex::encode(&info_hash);

        app.app_state
            .event_journal_state
            .entries
            .push(EventJournalEntry {
                id: 1,
                category: EventCategory::TorrentLifecycle,
                event_type: EventType::TorrentCompleted,
                torrent_name: Some("Sample Existing".to_string()),
                info_hash_hex: Some(info_hash_hex.clone()),
                ..Default::default()
            });
        app.app_state.event_journal_state.next_id = 2;

        let mut display = TorrentDisplayState::default();
        display.latest_state.info_hash = info_hash.clone();
        display.latest_state.torrent_name = "Sample Existing".to_string();
        display.latest_state.number_of_pieces_total = 10;
        display.latest_state.number_of_pieces_completed = 0;
        display.latest_state.is_complete = false;
        app.app_state.torrents.insert(info_hash.clone(), display);

        let (tx, rx) = watch::channel(TorrentMetrics {
            info_hash: info_hash.clone(),
            torrent_name: "Sample Existing".to_string(),
            number_of_pieces_total: 10,
            number_of_pieces_completed: 0,
            is_complete: false,
            ..Default::default()
        });
        app.torrent_metric_watch_rxs.insert(info_hash.clone(), rx);

        tx.send(TorrentMetrics {
            info_hash: info_hash.clone(),
            torrent_name: "Sample Existing".to_string(),
            number_of_pieces_total: 10,
            number_of_pieces_completed: 10,
            is_complete: true,
            activity_message: "Seeding".to_string(),
            ..Default::default()
        })
        .expect("send completed metrics");
        app.drain_latest_torrent_metrics();

        let completion_entries = app
            .app_state
            .event_journal_state
            .entries
            .iter()
            .filter(|entry| {
                entry.event_type == EventType::TorrentCompleted
                    && entry.info_hash_hex.as_deref() == Some(info_hash_hex.as_str())
            })
            .count();
        assert_eq!(completion_entries, 1);

        let _ = app.shutdown_tx.send(());
        set_app_paths_override_for_tests(None);
    }

    #[tokio::test]
    async fn restored_completed_torrents_skip_startup_recompletion_once() {
        let _guard = lock_shared_env();
        let _temp_paths = configure_temp_app_paths_for_test();
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let info_hash = b"startup_recompletion_hash".to_vec();

        let mut display = TorrentDisplayState::default();
        display.latest_state.info_hash = info_hash.clone();
        display.latest_state.torrent_name = "Sample Startup Restore".to_string();
        display.latest_state.number_of_pieces_total = 10;
        display.latest_state.number_of_pieces_completed = 10;
        display.latest_state.is_complete = true;
        display.latest_state.activity_message = "Seeding".to_string();
        app.app_state.torrents.insert(info_hash.clone(), display);
        app.startup_completion_suppressed_hashes
            .insert(info_hash.clone());

        let (tx, rx) = watch::channel(TorrentMetrics {
            info_hash: info_hash.clone(),
            torrent_name: "Sample Startup Restore".to_string(),
            number_of_pieces_total: 10,
            number_of_pieces_completed: 10,
            is_complete: true,
            activity_message: "Seeding".to_string(),
            ..Default::default()
        });
        app.torrent_metric_watch_rxs.insert(info_hash.clone(), rx);

        tx.send(TorrentMetrics {
            info_hash: info_hash.clone(),
            torrent_name: "Sample Startup Restore".to_string(),
            number_of_pieces_total: 10,
            number_of_pieces_completed: 0,
            is_complete: false,
            activity_message: "Validating 0% (0/10)".to_string(),
            ..Default::default()
        })
        .expect("send startup validating metrics");
        app.drain_latest_torrent_metrics();

        tx.send(TorrentMetrics {
            info_hash: info_hash.clone(),
            torrent_name: "Sample Startup Restore".to_string(),
            number_of_pieces_total: 10,
            number_of_pieces_completed: 10,
            is_complete: true,
            activity_message: "Seeding".to_string(),
            ..Default::default()
        })
        .expect("send recovered complete metrics");
        app.drain_latest_torrent_metrics();

        let completion_entries = app
            .app_state
            .event_journal_state
            .entries
            .iter()
            .filter(|entry| entry.event_type == EventType::TorrentCompleted)
            .count();
        assert_eq!(completion_entries, 0);
        assert!(
            !app.startup_completion_suppressed_hashes
                .contains(&info_hash),
            "startup suppression should clear after the first skipped re-completion"
        );

        tx.send(TorrentMetrics {
            info_hash: info_hash.clone(),
            torrent_name: "Sample Startup Restore".to_string(),
            number_of_pieces_total: 10,
            number_of_pieces_completed: 0,
            is_complete: false,
            activity_message: "Checking".to_string(),
            ..Default::default()
        })
        .expect("send later incomplete metrics");
        app.drain_latest_torrent_metrics();

        tx.send(TorrentMetrics {
            info_hash: info_hash.clone(),
            torrent_name: "Sample Startup Restore".to_string(),
            number_of_pieces_total: 10,
            number_of_pieces_completed: 10,
            is_complete: true,
            activity_message: "Seeding".to_string(),
            ..Default::default()
        })
        .expect("send later complete metrics");
        app.drain_latest_torrent_metrics();

        let completion_entries = app
            .app_state
            .event_journal_state
            .entries
            .iter()
            .filter(|entry| entry.event_type == EventType::TorrentCompleted)
            .count();
        assert_eq!(completion_entries, 1);

        let _ = app.shutdown_tx.send(());
        set_app_paths_override_for_tests(None);
    }

    #[tokio::test]
    async fn control_request_pause_updates_runtime_config() {
        let info_hash_hex = "1111111111111111111111111111111111111111";
        let settings = crate::config::Settings {
            client_port: 0,
            torrents: vec![crate::config::TorrentSettings {
                torrent_or_magnet: format!("magnet:?xt=urn:btih:{}", info_hash_hex),
                name: "Sample Alpha".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");

        let result = app
            .apply_control_request(&ControlRequest::Pause {
                info_hash_hex: info_hash_hex.to_string(),
            })
            .await;

        assert!(result.is_ok());
        assert_eq!(
            app.client_configs.torrents[0].torrent_control_state,
            TorrentControlState::Paused
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn shared_follower_suppresses_incomplete_runtime_and_converges_display_state() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::SharedFollower)
            .await
            .expect("build shared follower app");

        assert!(app.listener.is_some());

        let next_settings = crate::config::Settings {
            client_port: app.client_configs.client_port,
            torrents: vec![crate::config::TorrentSettings {
                torrent_or_magnet: "magnet:?xt=urn:btih:1111111111111111111111111111111111111111"
                    .to_string(),
                name: "Sample Delta".to_string(),
                torrent_control_state: TorrentControlState::Paused,
                ..Default::default()
            }],
            ..app.client_configs.clone()
        };

        app.apply_settings_update(next_settings, false).await;

        assert_eq!(app.app_state.torrents.len(), 1);
        assert!(
            app.torrent_manager_command_txs.is_empty(),
            "incomplete torrents should not start local follower runtime in phase 1"
        );
        let metrics = app
            .app_state
            .torrents
            .values()
            .next()
            .expect("cluster follower should load converged torrent");
        assert_eq!(metrics.latest_state.torrent_name, "Sample Delta");
        assert_eq!(
            metrics.latest_state.torrent_control_state,
            TorrentControlState::Paused
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn apply_settings_update_refreshes_file_preview_tree_priorities() {
        let magnet = "magnet:?xt=urn:btih:3333333333333333333333333333333333333333".to_string();
        let settings = crate::config::Settings {
            client_port: 0,
            torrents: vec![crate::config::TorrentSettings {
                torrent_or_magnet: magnet.clone(),
                name: "Sample Foxtrot".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let info_hash = info_hash_from_torrent_source(&magnet).expect("info hash");
        let runtime = app
            .app_state
            .torrents
            .get_mut(&info_hash)
            .expect("torrent runtime should exist");
        runtime.file_preview_tree = build_torrent_preview_tree(
            vec![
                (vec!["folder".to_string(), "alpha.bin".to_string()], 10),
                (vec!["folder".to_string(), "beta.bin".to_string()], 20),
            ],
            &HashMap::new(),
        );

        let mut next_settings = app.client_configs.clone();
        next_settings.torrents[0].file_priorities =
            HashMap::from([(0, FilePriority::Skip), (1, FilePriority::High)]);
        app.apply_settings_update(next_settings, false).await;

        let runtime = app
            .app_state
            .torrents
            .get(&info_hash)
            .expect("torrent runtime should remain present");
        let mut priorities = HashMap::new();
        for node in &runtime.file_preview_tree {
            node.collect_priorities(&mut priorities);
        }
        assert_eq!(
            priorities,
            HashMap::from([(0, FilePriority::Skip), (1, FilePriority::High)])
        );
        assert_eq!(
            runtime.file_preview_tree[0].payload.priority,
            FilePriority::Mixed
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn apply_settings_update_preserves_preview_file_indices_for_nonlexical_order() {
        fn collect_preview_files(
            node: &crate::tui::tree::RawNode<TorrentPreviewPayload>,
            path: &mut Vec<String>,
            files: &mut Vec<(Vec<String>, usize, FilePriority)>,
        ) {
            path.push(node.name.clone());
            if node.is_dir {
                for child in &node.children {
                    collect_preview_files(child, path, files);
                }
            } else if let Some(file_index) = node.payload.file_index {
                files.push((path.clone(), file_index, node.payload.priority));
            }
            path.pop();
        }

        let magnet = "magnet:?xt=urn:btih:4444444444444444444444444444444444444444".to_string();
        let settings = crate::config::Settings {
            client_port: 0,
            torrents: vec![crate::config::TorrentSettings {
                torrent_or_magnet: magnet.clone(),
                name: "Sample Golf".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let info_hash = info_hash_from_torrent_source(&magnet).expect("info hash");
        let runtime = app
            .app_state
            .torrents
            .get_mut(&info_hash)
            .expect("torrent runtime should exist");
        runtime.file_preview_tree = build_torrent_preview_tree(
            vec![
                (vec!["folder".to_string(), "beta.bin".to_string()], 20),
                (vec!["folder".to_string(), "alpha.bin".to_string()], 10),
            ],
            &HashMap::new(),
        );

        let mut next_settings = app.client_configs.clone();
        next_settings.torrents[0].file_priorities =
            HashMap::from([(0, FilePriority::Skip), (1, FilePriority::High)]);
        app.apply_settings_update(next_settings, false).await;

        let runtime = app
            .app_state
            .torrents
            .get(&info_hash)
            .expect("torrent runtime should remain present");
        let mut files = Vec::new();
        let mut path = Vec::new();
        for node in &runtime.file_preview_tree {
            collect_preview_files(node, &mut path, &mut files);
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(
            files,
            vec![
                (
                    vec!["folder".to_string(), "alpha.bin".to_string()],
                    1,
                    FilePriority::High,
                ),
                (
                    vec!["folder".to_string(), "beta.bin".to_string()],
                    0,
                    FilePriority::Skip,
                ),
            ]
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn shared_follower_promotion_starts_previously_suppressed_runtime() {
        let settings = crate::config::Settings {
            client_port: 0,
            torrents: vec![crate::config::TorrentSettings {
                torrent_or_magnet: "magnet:?xt=urn:btih:2222222222222222222222222222222222222222"
                    .to_string(),
                name: "Sample Echo".to_string(),
                torrent_control_state: TorrentControlState::Running,
                validation_status: false,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::SharedFollower)
            .await
            .expect("build shared follower app");

        assert_eq!(app.app_state.torrents.len(), 1);
        assert!(
            app.torrent_manager_command_txs.is_empty(),
            "follower should suppress incomplete runtime before promotion"
        );

        app.current_cluster_role = Some(AppClusterRole::Leader);
        app.runtime_mode = AppRuntimeMode::SharedLeader;
        app.sync_cluster_role_label();
        app.start_missing_runtime_torrents_for_current_role().await;

        assert_eq!(
            app.torrent_manager_command_txs.len(),
            1,
            "promotion should start the previously suppressed runtime"
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn cluster_revision_reload_applies_for_followers_and_stops_after_promotion() {
        let _guard = lock_shared_env();
        let shared_root = tempfile::tempdir().expect("create shared root");
        let effective_root = shared_root.path().join("superseedr-config");
        let original_shared_dir = env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
        let original_host_id = env::var_os("SUPERSEEDR_SHARED_HOST_ID");

        env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", shared_root.path());
        env::set_var("SUPERSEEDR_SHARED_HOST_ID", "node-a");
        clear_shared_config_state_for_tests();

        std::fs::create_dir_all(effective_root.join("hosts").join("node-a"))
            .expect("create hosts dir");
        std::fs::write(
            effective_root
                .join("hosts")
                .join("node-a")
                .join("config.toml"),
            "client_port = 0\n",
        )
        .expect("write host config");

        let initial_settings =
            crate::config::load_settings().expect("load initial shared settings");
        let mut app = App::new(initial_settings.clone(), AppRuntimeMode::SharedFollower)
            .await
            .expect("build shared follower app");

        let revision_path =
            crate::config::shared_cluster_revision_path().expect("shared cluster revision path");

        let mut follower_reload_settings = initial_settings.clone();
        follower_reload_settings.global_download_limit_bps = 42;
        crate::config::save_settings(&follower_reload_settings)
            .expect("save follower reload settings");

        app.handle_app_command(AppCommand::ReloadClusterState(revision_path.clone()))
            .await;
        assert_eq!(app.client_configs.global_download_limit_bps, 42);

        app.current_cluster_role = Some(AppClusterRole::Leader);
        app.runtime_mode = AppRuntimeMode::SharedLeader;
        app.sync_cluster_role_label();

        let mut leader_ignored_settings = follower_reload_settings.clone();
        leader_ignored_settings.global_download_limit_bps = 99;
        crate::config::save_settings(&leader_ignored_settings)
            .expect("save leader ignored settings");

        app.handle_app_command(AppCommand::ReloadClusterState(revision_path.clone()))
            .await;
        assert_eq!(
            app.client_configs.global_download_limit_bps, 42,
            "leader should ignore revision-triggered reloads"
        );

        app.current_cluster_role = Some(AppClusterRole::Follower);
        app.runtime_mode = AppRuntimeMode::SharedFollower;
        app.sync_cluster_role_label();

        app.handle_app_command(AppCommand::ReloadClusterState(revision_path))
            .await;
        assert_eq!(
            app.client_configs.global_download_limit_bps, 99,
            "follower should resume applying revision-triggered reloads after demotion"
        );

        let _ = app.shutdown_tx.send(());
        if let Some(value) = original_shared_dir {
            env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
        }
        if let Some(value) = original_host_id {
            env::set_var("SUPERSEEDR_SHARED_HOST_ID", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_HOST_ID");
        }
        clear_shared_config_state_for_tests();
    }

    #[tokio::test]
    async fn shared_follower_read_model_prefers_leader_snapshot_for_incomplete_torrents() {
        let _guard = lock_shared_env();
        let shared_root = tempfile::tempdir().expect("create shared root");
        let effective_root = shared_root.path().join("superseedr-config");
        let original_shared_dir = env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
        let original_host_id = env::var_os("SUPERSEEDR_SHARED_HOST_ID");

        env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", shared_root.path());
        env::set_var("SUPERSEEDR_SHARED_HOST_ID", "node-a");
        clear_shared_config_state_for_tests();

        std::fs::create_dir_all(effective_root.join("hosts").join("node-a"))
            .expect("create hosts dir");
        std::fs::write(
            effective_root
                .join("hosts")
                .join("node-a")
                .join("config.toml"),
            "client_port = 0\n",
        )
        .expect("write host config");

        let settings = crate::config::Settings {
            client_port: 0,
            torrents: vec![crate::config::TorrentSettings {
                torrent_or_magnet: "magnet:?xt=urn:btih:3333333333333333333333333333333333333333"
                    .to_string(),
                name: "Sample Foxtrot".to_string(),
                torrent_control_state: TorrentControlState::Running,
                validation_status: false,
                ..Default::default()
            }],
            ..crate::config::load_settings().expect("load shared settings")
        };
        crate::config::save_settings(&settings).expect("save shared settings");

        let mut app = App::new(settings.clone(), AppRuntimeMode::SharedFollower)
            .await
            .expect("build shared follower app");

        let info_hash = app
            .app_state
            .torrents
            .keys()
            .next()
            .expect("placeholder torrent should exist")
            .clone();

        let mut snapshot = status::offline_output_state(&settings);
        let metrics = snapshot
            .torrents
            .get_mut(&info_hash)
            .expect("leader snapshot torrent metrics");
        metrics.activity_message = "Leader downloading".to_string();
        metrics.number_of_pieces_total = 10;
        metrics.number_of_pieces_completed = 4;
        metrics.download_speed_bps = 1234;
        metrics.upload_speed_bps = 55;
        metrics.eta = Duration::from_secs(42);
        metrics.is_complete = false;

        let leader_status_path =
            crate::config::shared_leader_status_path().expect("leader status path");
        std::fs::create_dir_all(
            leader_status_path
                .parent()
                .expect("leader status parent directory"),
        )
        .expect("create status dir");
        std::fs::write(
            &leader_status_path,
            crate::fs_atomic::serialize_versioned_json(&snapshot)
                .expect("serialize leader snapshot"),
        )
        .expect("write leader snapshot");

        let reread = status::read_cluster_output_state().expect("read leader snapshot");
        let reread_metrics = reread
            .torrents
            .get(&info_hash)
            .expect("reread leader metrics by info hash");
        assert_eq!(reread_metrics.activity_message, "Leader downloading");
        assert_eq!(reread_metrics.download_speed_bps, 1234);

        app.refresh_follower_read_model();

        let display = app
            .app_state
            .torrents
            .get(&info_hash)
            .expect("display state for shared follower");
        assert_eq!(display.latest_state.activity_message, "Leader downloading");
        assert_eq!(display.latest_state.download_speed_bps, 1234);
        assert_eq!(display.latest_state.eta, Duration::from_secs(42));
        assert_eq!(display.latest_state.number_of_pieces_completed, 4);
        assert!(app.leader_status_snapshot.is_some());

        let _ = app.shutdown_tx.send(());
        if let Some(value) = original_shared_dir {
            env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
        }
        if let Some(value) = original_host_id {
            env::set_var("SUPERSEEDR_SHARED_HOST_ID", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_HOST_ID");
        }
        clear_shared_config_state_for_tests();
    }

    #[tokio::test]
    async fn shared_leader_dump_writes_host_and_cluster_status_files() {
        let _guard = lock_shared_env();
        let shared_root = tempfile::tempdir().expect("create shared root");
        let effective_root = shared_root.path().join("superseedr-config");
        let original_shared_dir = env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
        let original_host_id = env::var_os("SUPERSEEDR_SHARED_HOST_ID");

        env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", shared_root.path());
        env::set_var("SUPERSEEDR_SHARED_HOST_ID", "node-a");
        clear_shared_config_state_for_tests();

        std::fs::create_dir_all(effective_root.join("hosts").join("node-a"))
            .expect("create hosts dir");
        std::fs::write(
            effective_root
                .join("hosts")
                .join("node-a")
                .join("config.toml"),
            "client_port = 0\n",
        )
        .expect("write host config");

        let settings = crate::config::load_settings().expect("load shared settings");
        let app = App::new(settings, AppRuntimeMode::SharedLeader)
            .await
            .expect("build shared leader app");

        app.dump_status_to_file();
        time::sleep(Duration::from_millis(100)).await;

        let host_status_path = crate::config::shared_status_path().expect("host status path");
        let leader_status_path =
            crate::config::shared_leader_status_path().expect("leader status path");

        assert!(host_status_path.exists());
        assert!(leader_status_path.exists());

        let host_snapshot: AppOutputState = crate::fs_atomic::deserialize_versioned_json(
            &std::fs::read_to_string(&host_status_path).expect("read host status"),
        )
        .expect("parse host status");
        let leader_snapshot: AppOutputState = crate::fs_atomic::deserialize_versioned_json(
            &std::fs::read_to_string(&leader_status_path).expect("read leader status"),
        )
        .expect("parse leader status");
        assert_eq!(host_snapshot, leader_snapshot);

        let _ = app.shutdown_tx.send(());
        if let Some(value) = original_shared_dir {
            env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
        }
        if let Some(value) = original_host_id {
            env::set_var("SUPERSEEDR_SHARED_HOST_ID", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_HOST_ID");
        }
        clear_shared_config_state_for_tests();
    }

    #[tokio::test]
    async fn shared_leader_defaults_status_follow_to_five_seconds() {
        let _guard = lock_shared_env();
        let shared_root = tempfile::tempdir().expect("create shared root");
        let effective_root = shared_root.path().join("superseedr-config");
        let original_shared_dir = env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
        let original_host_id = env::var_os("SUPERSEEDR_SHARED_HOST_ID");

        env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", shared_root.path());
        env::set_var("SUPERSEEDR_SHARED_HOST_ID", "node-a");
        clear_shared_config_state_for_tests();

        std::fs::create_dir_all(effective_root.join("hosts").join("node-a"))
            .expect("create hosts dir");
        std::fs::write(
            effective_root
                .join("hosts")
                .join("node-a")
                .join("config.toml"),
            "client_port = 0\n",
        )
        .expect("write host config");

        let settings = crate::config::load_settings().expect("load shared settings");
        let app = App::new(settings, AppRuntimeMode::SharedLeader)
            .await
            .expect("build shared leader app");

        assert_eq!(app.client_configs.output_status_interval, 0);
        assert_eq!(app.effective_status_dump_interval_secs(), 5);

        let _ = app.shutdown_tx.send(());
        if let Some(value) = original_shared_dir {
            env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
        }
        if let Some(value) = original_host_id {
            env::set_var("SUPERSEEDR_SHARED_HOST_ID", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_HOST_ID");
        }
        clear_shared_config_state_for_tests();
    }

    #[tokio::test]
    async fn shared_follower_path_file_with_default_download_routes_through_control_request() {
        let _guard = lock_shared_env();
        let shared_root = tempfile::tempdir().expect("create shared root");
        let effective_root = shared_root.path().join("superseedr-config");
        let local_dir = tempfile::tempdir().expect("create local dir");
        let original_shared_dir = env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
        let original_host_id = env::var_os("SUPERSEEDR_SHARED_HOST_ID");

        env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", shared_root.path());
        env::set_var("SUPERSEEDR_SHARED_HOST_ID", "node-a");
        clear_shared_config_state_for_tests();

        std::fs::create_dir_all(effective_root.join("hosts").join("node-a"))
            .expect("create hosts dir");
        std::fs::write(
            effective_root
                .join("hosts")
                .join("node-a")
                .join("config.toml"),
            "client_port = 0\n",
        )
        .expect("write host config");

        let mut settings = crate::config::load_settings().expect("load shared settings");
        settings.client_port = 0;
        settings.default_download_folder = Some(effective_root.join("data").join("downloads"));
        crate::config::save_settings(&settings).expect("save shared settings");

        let mut app = App::new(settings, AppRuntimeMode::SharedFollower)
            .await
            .expect("build shared follower app");
        let torrent_path = local_dir.path().join("sample-input.torrent");
        let path_file = local_dir.path().join("sample.path");
        std::fs::write(&torrent_path, b"placeholder torrent payload").expect("write torrent file");
        std::fs::write(&path_file, torrent_path.to_string_lossy().to_string())
            .expect("write path file");

        app.handle_app_command(AppCommand::AddTorrentFromPathFile(path_file))
            .await;

        assert!(app.app_state.torrents.is_empty());
        let inbox_entries: Vec<_> = std::fs::read_dir(effective_root.join("inbox"))
            .expect("read shared inbox")
            .collect();
        assert_eq!(inbox_entries.len(), 1);
        let queued_path = inbox_entries[0]
            .as_ref()
            .expect("queued inbox entry")
            .path();
        let queued_request = read_control_request(&queued_path).expect("read queued request");

        match queued_request {
            ControlRequest::AddTorrentFile {
                source_path,
                download_path,
                ..
            } => {
                assert!(source_path.starts_with(effective_root.join("staged-adds")));
                assert!(source_path.exists());
                assert_eq!(
                    download_path,
                    Some(effective_root.join("data").join("downloads"))
                );
            }
            other => panic!("unexpected queued request: {:?}", other),
        }

        let _ = app.shutdown_tx.send(());
        if let Some(value) = original_shared_dir {
            env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
        }
        if let Some(value) = original_host_id {
            env::set_var("SUPERSEEDR_SHARED_HOST_ID", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_HOST_ID");
        }
        clear_shared_config_state_for_tests();
    }

    #[tokio::test]
    async fn shared_follower_allows_host_local_config_updates_and_rewatches_host_folder() {
        let _guard = lock_shared_env();
        let shared_root = tempfile::tempdir().expect("create shared root");
        let effective_root = shared_root.path().join("superseedr-config");
        let original_shared_dir = env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
        let original_host_id = env::var_os("SUPERSEEDR_SHARED_HOST_ID");
        let old_watch = shared_root.path().join("old-watch");
        let new_watch = shared_root.path().join("new-watch");

        env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", shared_root.path());
        env::set_var("SUPERSEEDR_SHARED_HOST_ID", "node-a");
        clear_shared_config_state_for_tests();

        std::fs::create_dir_all(effective_root.join("hosts").join("node-a"))
            .expect("create hosts dir");
        std::fs::write(
            effective_root
                .join("hosts")
                .join("node-a")
                .join("config.toml"),
            format!(
                "client_port = 0\nwatch_folder = {:?}\n",
                old_watch.to_string_lossy()
            ),
        )
        .expect("write host config");

        let settings = crate::config::load_settings().expect("load shared settings");
        let mut app = App::new(settings, AppRuntimeMode::SharedFollower)
            .await
            .expect("build shared follower app");
        let mut next_settings = app.client_configs.clone();
        next_settings.watch_folder = Some(new_watch.clone());
        next_settings.client_port = app.client_configs.client_port;

        app.handle_app_command(AppCommand::UpdateConfig(next_settings))
            .await;

        assert_eq!(app.client_configs.watch_folder, Some(new_watch.clone()));
        assert!(app.watched_paths.contains(&new_watch));
        assert!(!app.watched_paths.contains(&old_watch));

        let reloaded = crate::config::load_settings().expect("reload shared settings");
        assert_eq!(reloaded.watch_folder, Some(new_watch));

        let _ = app.shutdown_tx.send(());
        if let Some(value) = original_shared_dir {
            env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
        }
        if let Some(value) = original_host_id {
            env::set_var("SUPERSEEDR_SHARED_HOST_ID", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_HOST_ID");
        }
        clear_shared_config_state_for_tests();
    }

    #[tokio::test]
    async fn control_request_status_follow_start_sets_runtime_override() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");

        let result = app
            .apply_control_request(&ControlRequest::StatusFollowStart { interval_secs: 5 })
            .await;

        assert!(result.is_ok());
        assert_eq!(app.status_dump_interval_override_secs, Some(5));
        assert!(app.next_status_dump_at.is_some());

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn enqueue_watch_command_spills_to_pending_queue_when_channel_is_full() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");

        for idx in 0..11 {
            let path = std::env::temp_dir().join(format!("queued-{idx}.magnet"));
            app.enqueue_watch_command(
                AppCommand::AddMagnetFromFile(path),
                Duration::from_millis(0),
            )
            .await;
        }

        assert_eq!(app.app_state.pending_watch_commands.len(), 1);

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn add_magnet_torrent_rejects_hashless_magnet_without_panicking() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");

        let result = app
            .add_magnet_torrent(
                "Fetching name...".to_string(),
                "magnet:?dn=SampleNoHash".to_string(),
                None,
                false,
                TorrentControlState::Running,
                HashMap::new(),
                None,
            )
            .await;

        assert_eq!(
            result,
            CommandIngestResult::Invalid {
                info_hash: None,
                torrent_name: None,
                message: "Magnet link is missing both btih and btmh hashes".to_string(),
            }
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn healthy_probe_for_available_torrent_does_not_request_recovery_again() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let info_hash = b"already_healthy_probe_hash".to_vec();

        let mut display = TorrentDisplayState::default();
        display.latest_state.info_hash = info_hash.clone();
        display.latest_state.torrent_name = "steady healthy torrent".to_string();
        display.latest_state.torrent_control_state = TorrentControlState::Running;
        display.latest_state.data_available = true;
        app.app_state.torrents.insert(info_hash.clone(), display);
        app.integrity_scheduler
            .sync_torrents(app.current_integrity_snapshots());

        let (manager_tx, mut manager_rx) = mpsc::channel(4);
        app.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_tx);

        app.handle_manager_event(ManagerEvent::FileProbeBatchResult {
            info_hash,
            result: FileProbeBatchResult {
                epoch: 0,
                scanned_files: 1,
                next_file_index: 0,
                reached_end_of_manifest: true,
                pending_metadata: false,
                problem_files: Vec::new(),
            },
        });

        assert!(matches!(
            manager_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn stale_healthy_probe_does_not_request_manager_recovery() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let info_hash = b"stale_recovery_probe_hash".to_vec();

        let mut display = TorrentDisplayState::default();
        display.latest_state.info_hash = info_hash.clone();
        display.latest_state.torrent_name = "stale recovery probe torrent".to_string();
        display.latest_state.torrent_control_state = TorrentControlState::Running;
        display.latest_state.data_available = false;
        app.app_state.torrents.insert(info_hash.clone(), display);
        app.integrity_scheduler
            .sync_torrents(app.current_integrity_snapshots());
        app.integrity_scheduler
            .on_data_availability_fault(&info_hash);

        let (manager_tx, mut manager_rx) = mpsc::channel(4);
        app.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_tx);

        app.handle_manager_event(ManagerEvent::FileProbeBatchResult {
            info_hash: info_hash.clone(),
            result: FileProbeBatchResult {
                epoch: 0,
                scanned_files: 1,
                next_file_index: 0,
                reached_end_of_manifest: true,
                pending_metadata: false,
                problem_files: Vec::new(),
            },
        });

        let command = manager_rx.recv().await.expect("expected replacement probe");
        assert!(matches!(command, ManagerCommand::ProbeFileBatch { .. }));
        assert!(matches!(
            manager_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        let _ = app.shutdown_tx.send(());
    }

    #[test]
    fn build_persist_payload_preserves_validation_when_data_is_unavailable() {
        let mut settings = crate::config::Settings::default();
        let mut app_state = AppState::default();
        let info_hash = b"persist_probe_hash".to_vec();

        let mut display = TorrentDisplayState::default();
        display.latest_state.info_hash = info_hash.clone();
        display.latest_state.torrent_or_magnet = "sample.torrent".to_string();
        display.latest_state.torrent_name = "sample".to_string();
        display.latest_state.data_available = false;
        display.latest_state.number_of_pieces_total = 4;
        display.latest_state.number_of_pieces_completed = 4;

        app_state.torrents.insert(info_hash.clone(), display);
        app_state.torrent_list_order.push(info_hash);

        let payload = build_persist_payload(&mut settings, &mut app_state, &VecDeque::new());
        assert_eq!(payload.settings.torrents.len(), 1);
        assert!(payload.settings.torrents[0].validation_status);
    }

    #[test]
    fn ui_telemetry_metrics_refresh_updates_data_availability_flag() {
        let mut app_state = AppState::default();
        let info_hash = b"telemetry_probe_hash".to_vec();

        let mut display = TorrentDisplayState::default();
        display.latest_state.info_hash = info_hash.clone();
        display.latest_state.data_available = false;
        app_state.torrents.insert(info_hash.clone(), display);

        let message = TorrentMetrics {
            info_hash: info_hash.clone(),
            torrent_name: "sample".to_string(),
            data_available: true,
            download_speed_bps: 123,
            ..Default::default()
        };

        UiTelemetry::on_metrics(&mut app_state, message);

        let torrent = app_state
            .torrents
            .get(&info_hash)
            .expect("torrent display should exist");
        assert!(torrent.latest_state.data_available);
        assert_eq!(torrent.latest_state.download_speed_bps, 123);
    }

    #[test]
    fn network_history_interval_persistence_only_when_dirty() {
        let mut app_state = AppState {
            network_history_dirty: false,
            ..Default::default()
        };
        assert!(!should_persist_network_history_on_interval(&app_state));

        app_state.network_history_dirty = true;
        assert!(should_persist_network_history_on_interval(&app_state));
    }

    #[test]
    fn build_persist_payload_skips_network_history_while_restore_is_pending() {
        let mut settings = crate::config::Settings::default();
        let mut app_state = AppState {
            network_history_restore_pending: true,
            ..Default::default()
        };
        app_state.network_history_state.tiers.second_1s.push(
            crate::persistence::network_history::NetworkHistoryPoint {
                ts_unix: 41,
                download_bps: 1000,
                upload_bps: 100,
                backoff_ms_max: 0,
            },
        );

        let payload = build_persist_payload(&mut settings, &mut app_state, &VecDeque::new());

        assert!(payload.network_history.is_none());
        assert_eq!(app_state.network_history_state.updated_at_unix, 0);
        assert_eq!(app_state.next_network_history_persist_request_id, 0);
    }

    #[test]
    fn build_persist_payload_syncs_rollup_snapshot_into_network_history_state() {
        let mut settings = crate::config::Settings::default();
        let snapshot = crate::persistence::network_history::NetworkHistoryRollupSnapshot {
            second_to_minute: crate::persistence::network_history::PersistedRollupAccumulator {
                count: 7,
                dl_sum: 7_000,
                ul_sum: 700,
                backoff_max: 9,
            },
            ..Default::default()
        };
        let mut app_state = AppState {
            network_history_rollups:
                crate::persistence::network_history::NetworkHistoryRollupState::from_snapshot(
                    &snapshot,
                ),
            ..Default::default()
        };

        let payload = build_persist_payload(&mut settings, &mut app_state, &VecDeque::new());
        let network_history = payload
            .network_history
            .expect("network history payload should be present");

        assert_eq!(network_history.state.rollups, snapshot);
        assert_eq!(app_state.network_history_state.rollups, snapshot);
    }

    #[test]
    fn apply_network_history_persist_result_clears_dirty_only_for_latest_success() {
        let mut app_state = AppState {
            network_history_dirty: true,
            pending_network_history_persist_request_id: Some(2),
            ..Default::default()
        };

        apply_network_history_persist_result(&mut app_state, 1, true);
        assert!(app_state.network_history_dirty);
        assert_eq!(
            app_state.pending_network_history_persist_request_id,
            Some(2)
        );

        apply_network_history_persist_result(&mut app_state, 2, false);
        assert!(app_state.network_history_dirty);
        assert_eq!(
            app_state.pending_network_history_persist_request_id,
            Some(2)
        );

        apply_network_history_persist_result(&mut app_state, 2, true);
        assert!(!app_state.network_history_dirty);
        assert_eq!(app_state.pending_network_history_persist_request_id, None);
    }

    #[tokio::test]
    async fn queue_persistence_payload_carries_network_history_state() {
        let (tx, mut rx) = tokio::sync::watch::channel::<Option<PersistPayload>>(None);
        let mut network_history_state =
            crate::persistence::network_history::NetworkHistoryPersistedState {
                updated_at_unix: 42,
                ..Default::default()
            };
        network_history_state.tiers.second_1s.push(
            crate::persistence::network_history::NetworkHistoryPoint {
                ts_unix: 41,
                download_bps: 1000,
                upload_bps: 100,
                backoff_ms_max: 0,
            },
        );

        let payload = PersistPayload {
            settings: crate::config::Settings::default(),
            rss_state: crate::persistence::rss::RssPersistedState::default(),
            network_history: Some(super::NetworkHistoryPersistRequest {
                request_id: 7,
                state: network_history_state.clone(),
            }),
            activity_history: None,
            event_journal_state: EventJournalState::default(),
        };

        assert!(queue_persistence_payload(Some(&tx), payload).is_ok());
        assert!(rx.changed().await.is_ok());

        let received = rx.borrow().clone().expect("payload should be present");
        let network_history = received
            .network_history
            .expect("network history payload should be present");
        assert_eq!(network_history.request_id, 7);
        assert_eq!(
            network_history.state.updated_at_unix,
            network_history_state.updated_at_unix
        );
        assert_eq!(
            network_history.state.tiers.second_1s,
            network_history_state.tiers.second_1s
        );
    }

    #[tokio::test]
    async fn flush_persistence_writer_parts_drops_sender_and_joins_task() {
        let (tx, mut rx) = tokio::sync::watch::channel::<Option<PersistPayload>>(None);
        let task = tokio::spawn(async move { while rx.changed().await.is_ok() {} });

        let mut tx_opt = Some(tx);
        let mut task_opt = Some(task);
        flush_persistence_writer_parts(&mut tx_opt, &mut task_opt).await;

        assert!(tx_opt.is_none());
        assert!(task_opt.is_none());
    }

    #[tokio::test]
    async fn listener_set_bind_keeps_ipv6_listener_when_ipv4_port_is_already_in_use() {
        let ipv6_supported =
            TcpListener::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0))
                .await
                .is_ok();
        let occupied = tokio::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .expect("bind occupied IPv4 port");
        let port = occupied.local_addr().expect("occupied local addr").port();
        let ipv6_can_bind_alongside_ipv4 = if ipv6_supported {
            match TcpListener::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port)).await
            {
                Ok(listener) => {
                    drop(listener);
                    true
                }
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => false,
                Err(error) => panic!("probe IPv6 bind with occupied IPv4 port: {error}"),
            }
        } else {
            false
        };

        match ListenerSet::bind(port).await {
            Ok(listener_set) => {
                assert!(
                    ipv6_can_bind_alongside_ipv4,
                    "expected full bind failure when IPv4 occupancy also blocks IPv6"
                );
                assert!(listener_set.ipv6.is_some());
                assert!(listener_set.ipv4.is_none());
                assert_eq!(listener_set.local_port(), Some(port));
            }
            Err(error) => {
                assert!(
                    !ipv6_can_bind_alongside_ipv4,
                    "expected degraded IPv6-only bind, got {error}"
                );
                assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
            }
        }
    }

    #[tokio::test]
    async fn listener_set_bind_keeps_ipv4_listener_when_ipv6_port_is_already_in_use() {
        let occupied =
            match TcpListener::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)).await {
                Ok(listener) => listener,
                Err(_) => return,
            };
        let port = occupied.local_addr().expect("occupied local addr").port();

        match ListenerSet::bind(port).await {
            Ok(listener_set) => {
                assert!(listener_set.ipv4.is_some());
                assert!(listener_set.ipv6.is_none());
                assert_eq!(listener_set.local_port(), Some(port));
            }
            Err(error) => {
                assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
            }
        }
    }
}
