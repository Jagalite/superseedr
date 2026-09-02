// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

#![cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]

mod reducer;
pub(crate) use reducer::{finalize_manager_metrics_batch, reduce_app_action, AppAction, AppEffect};
pub(crate) mod torrent_manager_protocol;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use std::collections::VecDeque;

use magnet_url::Magnet;

use fuzzy_matcher::FuzzyMatcher;

use rand::RngExt;

use self::torrent_manager_protocol::DiskIoOperation;

use crate::config::{
    FeedSyncError, PeerSortColumn, RssFilterMode, RssHistoryEntry, Settings, SortDirection,
    TorrentMetadataEntry, TorrentMetadataFileEntry, TorrentSettings, TorrentSortColumn,
    UiLayoutMode,
};
use crate::dht_model::{DhtStatus, DhtWaveTelemetry};
use crate::peer_manager::{PeerManagerView, PeerPolicy};
use crate::persistence::activity_history::{
    ActivityHistoryPersistedState, ActivityHistoryRollupState,
};
use crate::persistence::event_journal::{
    append_event_journal_entry, ControlOrigin, EventCategory, EventDetails, EventJournalEntry,
    EventJournalState, EventScope, EventType, IngestKind, IngestOrigin,
};
use crate::persistence::network_history::{
    NetworkHistoryPersistedState, NetworkHistoryRollupState,
};
use crate::persistence::rss::RssPersistedState;

use crate::token_bucket::{rate_limit_bps_to_bucket_bytes_per_sec, TokenBucket};

use crate::tui::events::PasteBurst;
use crate::tui::layout::common::{ColumnId, PeerColumnId};
use crate::tui::layout::normal::{
    calculate_layout, LayoutContext, DEFAULT_SIDEBAR_PERCENT, PEER_STREAM_MIN_HEIGHT,
    PEER_STREAM_MIN_WIDTH,
};
use crate::tui::render::compute_effects_activity_speed_multiplier;
use crate::tui::render::draw;
use crate::tui::screens::browser::{
    build_filesystem_filter, calculate_list_height, focused_pane, preview_content_for_selection,
};
use crate::tui::tree;
use crate::tui::tree::RawNode;
use crate::tui::tree::TreeProjection;
use crate::tui::tree::TreeViewState;

#[cfg(test)]
pub use crate::tui::state::ConfigNetworkInterfaceInventory;
pub(crate) use crate::tui::state::AWAITING_MAGNET_METADATA_LABEL;
pub use crate::tui::state::{
    AppMode, BrowserPane, ConfigEditState, ConfigItem, ConfigPane, ConfigUiState,
    DownloadSelectionTarget, FileBrowserMode, FilePriority, RssPreviewItem, TorrentControlState,
    TorrentPreviewPayload,
};

use crate::resource::ResourceType;
use crate::theme::Theme;

use self::torrent_manager_protocol::data_availability_from_file_probe_result;
use self::torrent_manager_protocol::FileActivityUpdate;
use self::torrent_manager_protocol::ManagerCommand;
use self::torrent_manager_protocol::ManagerEvent;
use self::torrent_manager_protocol::TorrentFileProbeStatus;
use crate::integrations::control::{ControlFilePriorityOverride, ControlRequest};
use crate::networking::PeerTransportKind;
use crate::networking::{NetworkInterfaceInfo, NetworkScopeId};
use crate::torrent_identity::info_hash_from_torrent_source;

#[cfg(test)]
thread_local! {
    static TEST_PERSISTENCE_WRITER_ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn test_persistence_writer_enabled() -> bool {
    TEST_PERSISTENCE_WRITER_ENABLED.get()
}

#[cfg(test)]
fn set_test_persistence_writer_enabled(enabled: bool) {
    TEST_PERSISTENCE_WRITER_ENABLED.set(enabled);
}

use std::collections::{HashMap, HashSet};
use tokio::sync::watch;

use std::sync::Arc;
use web_time::{Instant, SystemTime, UNIX_EPOCH};

use sha1::Digest;
use sha2::Sha256;

use serde::{Deserialize, Serialize};
use std::time::Duration;

use ratatui::prelude::Rect;

use tracing::{event as tracing_event, Level};

pub const RSS_MAX_TORRENT_DOWNLOAD_BYTES: usize = 10 * 1024 * 1024;
const NETWORK_HISTORY_PERSIST_INTERVAL_SECS: u64 = 15 * 60;
const SHARED_RECOVERY_BACKUP_REFRESH_INTERVAL_SECS: u64 = 15 * 60;
const WATCH_FOLDER_RESCAN_INTERVAL_SECS: u64 = 5;
const SHARED_ROLE_RETRY_INTERVAL_SECS: u64 = 2;
const STARTUP_ROLLING_BATCH_INTERVAL_SECS: u64 = 1;
const STARTUP_ROLLING_LOADS_PER_INTERVAL: usize = 1;
const REPEATED_HEALTH_LOG_INTERVAL: Duration = Duration::from_secs(60);

const SHUTDOWN_TIMEOUT_SECS: u64 = 20;
const INCOMING_HANDSHAKE_TIMEOUT_SECS: u64 = 10;
// DHT owns a one-second transport-drain budget during reconfiguration. Keep
// the app-level liveness bound comfortably outside that healthy inner path.
const PORT_REBIND_DHT_TIMEOUT: Duration = Duration::from_secs(3);
const INCOMING_PEER_HANDSHAKE_QUEUE_SIZE: usize = 1024;
const PORT_FAMILY_HIGHLIGHT_DURATION: Duration = Duration::from_millis(450);
const DUAL_STACK_EPHEMERAL_BIND_ATTEMPTS: usize = 16;
const UI_FPS_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const UI_RESPONSIVENESS_EMA_ALPHA: f64 = 0.35;
const WAKE_LAG_PEER_THROTTLE_BAD_RATIO: f64 = 0.25;
const WAKE_LAG_PEER_THROTTLE_BAD_MIN_DELAY: Duration = Duration::from_millis(20);
const WAKE_LAG_PEER_THROTTLE_GOOD_RATIO: f64 = 0.12;
const WAKE_LAG_PEER_THROTTLE_GOOD_TICKS: u8 = 3;
const WAKE_LAG_PEER_THROTTLE_ADDITIVE_STEP_PEERS: usize = 256;
const WAKE_LAG_PEER_THROTTLE_ADDITIVE_STEP_PERCENT: usize = 10;
const WAKE_LAG_PEER_THROTTLE_RECOVERY_HEADROOM_PEERS: usize = 512;
const WAKE_LAG_PEER_THROTTLE_MIN_PEERS: usize = 8;
const WAKE_LAG_PEER_THROTTLE_DOWNLOAD_FLOOR_PERCENT: usize = 25;
const NORMAL_IDLE_FRAME_CHECK_INTERVAL: Duration = Duration::from_millis(100);
const NORMAL_ANIMATION_RECENT_BLOCK_ROWS: usize = 64;
const NORMAL_ANIMATION_RECENT_PEER_EVENTS: usize = 120;
const NORMAL_ANIMATION_FILE_ACTIVITY_WINDOW: Duration = Duration::from_secs(4);
const SWARM_AVAILABILITY_FLASH_DURATION: Duration = Duration::from_millis(350);
const DISK_WRITE_THROTTLE_START_BYTES_PER_SEC: f64 = 1_000_000_000.0 / 8.0;
const DISK_WRITE_THROTTLE_MIN_BYTES_PER_SEC: f64 = 1_000_000.0 / 8.0;
const DISK_WRITE_THROTTLE_WINDOW_TICKS: u8 = 5;
const DISK_WRITE_THROTTLE_STEP_MIN: f64 = 0.80;
const DISK_WRITE_THROTTLE_STEP_MAX: f64 = 1.20;
const DISK_WRITE_THROTTLE_BURST_SECS: f64 = 1.0;
const DISK_WRITE_THROTTLE_TARGET_LATENCY_SECS: f64 = 2.0;
const BITTORRENT_PROTOCOL_STR: &[u8] = b"BitTorrent protocol";

#[derive(serde::Deserialize)]
struct CratesResponse {
    #[serde(rename = "crate")]
    krate: CrateInfo,
}

#[derive(serde::Deserialize)]
struct CrateInfo {
    max_version: String,
}

type VersionCheckError = Box<dyn std::error::Error + Send + Sync>;

struct TorrentPreviewFileEntry {
    parts: Vec<String>,
    file_index: usize,
    size: u64,
}

pub(crate) fn merge_file_browser_mode_for_fetch(
    current: &FileBrowserMode,
    incoming: FileBrowserMode,
) -> FileBrowserMode {
    match (current, incoming) {
        (
            FileBrowserMode::DownloadLocSelection {
                target: current_target,
                torrent_files: current_torrent_files,
                container_name: current_container_name,
                use_container: current_use_container,
                is_editing_name: current_is_editing_name,
                focused_pane: current_focused_pane,
                preview_tree: current_preview_tree,
                preview_state: current_preview_state,
                cursor_pos: current_cursor_pos,
                original_name_backup: current_original_name_backup,
            },
            FileBrowserMode::DownloadLocSelection {
                target,
                torrent_files,
                container_name,
                use_container,
                is_editing_name,
                focused_pane,
                preview_tree,
                preview_state,
                cursor_pos,
                original_name_backup,
            },
        ) => {
            if current_target == &target {
                FileBrowserMode::DownloadLocSelection {
                    target: current_target.clone(),
                    torrent_files: current_torrent_files.clone(),
                    container_name: current_container_name.clone(),
                    use_container: *current_use_container,
                    is_editing_name: *current_is_editing_name,
                    focused_pane: current_focused_pane.clone(),
                    preview_tree: current_preview_tree.clone(),
                    preview_state: current_preview_state.clone(),
                    cursor_pos: *current_cursor_pos,
                    original_name_backup: current_original_name_backup.clone(),
                }
            } else {
                FileBrowserMode::DownloadLocSelection {
                    target,
                    torrent_files,
                    container_name,
                    use_container,
                    is_editing_name,
                    focused_pane,
                    preview_tree,
                    preview_state,
                    cursor_pos,
                    original_name_backup,
                }
            }
        }
        (_, incoming) => incoming,
    }
}

#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub size: u64,
    pub modified: std::time::SystemTime,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TorrentFilePreview {
    pub name: String,
    pub protocol_version: String,
    pub total_size: u64,
    pub tree: Vec<RawNode<TorrentPreviewPayload>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum TorrentFilePreviewState {
    #[default]
    Idle,
    Loading {
        path: PathBuf,
        request_id: u64,
    },
    Ready {
        path: PathBuf,
        preview: TorrentFilePreview,
    },
    Error {
        path: PathBuf,
        message: String,
    },
}

impl TorrentFilePreviewState {
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Idle => None,
            Self::Loading { path, .. } | Self::Ready { path, .. } | Self::Error { path, .. } => {
                Some(path)
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
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

    pub fn fps_label(self) -> &'static str {
        match self {
            DataRate::RateQuarter => "0.25",
            DataRate::RateHalf => "0.5",
            DataRate::Rate1s => "1",
            DataRate::Rate2s => "2",
            DataRate::Rate4s => "4",
            DataRate::Rate10s => "10",
            DataRate::Rate20s => "20",
            DataRate::Rate30s => "30",
            DataRate::Rate60s => "60",
        }
    }

    pub fn target_fps(self) -> f64 {
        match self {
            DataRate::RateQuarter => 0.25,
            DataRate::RateHalf => 0.5,
            DataRate::Rate1s => 1.0,
            DataRate::Rate2s => 2.0,
            DataRate::Rate4s => 4.0,
            DataRate::Rate10s => 10.0,
            DataRate::Rate20s => 20.0,
            DataRate::Rate30s => 30.0,
            DataRate::Rate60s => 60.0,
        }
    }

    pub fn frame_interval(self) -> Duration {
        Duration::from_secs_f64(1.0 / self.target_fps())
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

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct CalculatedLimits {
    pub reserve_permits: usize,
    pub max_connected_peers: usize,
    pub disk_read_permits: usize,
    pub disk_write_permits: usize,
}
impl CalculatedLimits {
    pub fn into_map_with_peer_queue(
        self,
        peer_connection_queue_size: usize,
    ) -> HashMap<ResourceType, (usize, usize)> {
        let mut map = HashMap::new();
        map.insert(ResourceType::Reserve, (self.reserve_permits, 0));
        map.insert(
            ResourceType::PeerConnection,
            (self.max_connected_peers, peer_connection_queue_size),
        );
        map.insert(
            ResourceType::DiskRead,
            (
                self.disk_read_permits,
                self.disk_read_permits.saturating_mul(2),
            ),
        );
        map.insert(
            ResourceType::DiskWrite,
            (
                self.disk_write_permits,
                self.disk_write_permits.saturating_mul(2),
            ),
        );
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
    Torrent(ColumnId),
    Peer(PeerColumnId),
}
impl Default for SelectedHeader {
    fn default() -> Self {
        SelectedHeader::Torrent(ColumnId::Name)
    }
}

fn torrent_sort_header(column: TorrentSortColumn) -> ColumnId {
    match column {
        TorrentSortColumn::Name => ColumnId::Name,
        TorrentSortColumn::Down => ColumnId::DownSpeed,
        TorrentSortColumn::Up => ColumnId::UpSpeed,
        TorrentSortColumn::Progress => ColumnId::Status,
    }
}

pub enum AppCommand {
    AddTorrentFromFile(PathBuf),
    AddTorrentFromPathFile(PathBuf),
    AddMagnetFromFile(PathBuf),
    MarkPortOpen {
        peer_addr: SocketAddr,
        transport: PeerTransportKind,
        scope_id: NetworkScopeId,
    },
    ReloadClusterState(PathBuf),
    SubmitControlRequest(ControlRequest),
    SubmitManualAddRequest {
        request: ControlRequest,
        pending_ingest: Option<PendingManualIngest>,
    },
    ControlRequest {
        path: PathBuf,
        request: ControlRequest,
    },
    ClientShutdown(PathBuf),
    PortFileChanged(PathBuf),
    FetchFileTree {
        browser_generation: u64,
        path: PathBuf,
        browser_mode: FileBrowserMode,
        preserve_browser_mode: bool,
        highlight_path: Option<PathBuf>,
    },
    UpdateFileBrowserData {
        request_id: u64,
        path: PathBuf,
        data: Vec<tree::RawNode<FileMetadata>>,
        highlight_path: Option<PathBuf>,
    },
    FileBrowserFetchFailed {
        request_id: u64,
        path: PathBuf,
        message: String,
    },
    UpdateTorrentFilePreview {
        browser_generation: u64,
        request_id: u64,
        path: PathBuf,
        result: Result<TorrentFilePreview, String>,
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
    ConfigNetworkInterfacesDiscovered {
        request_id: u64,
        result: Result<Vec<NetworkInterfaceInfo>, String>,
    },
    RefreshConfigNetworkInterfaces,
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
pub struct PendingManualIngest {
    source: IngestSource,
    path: PathBuf,
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
    #[serde(default)]
    pub connection_count: u64,
    #[serde(default)]
    pub disconnect_count: u64,
    pub last_action: String,
}

pub fn swarm_availability_counts(peers: &[PeerInfo], total_pieces: u32) -> Vec<u32> {
    let total_pieces_usize = total_pieces as usize;
    let mut availability = vec![0; total_pieces_usize];

    for peer in peers {
        for (i, has_piece) in peer.bitfield.iter().enumerate().take(total_pieces_usize) {
            if *has_piece {
                availability[i] += 1;
            }
        }
    }

    availability
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
    #[serde(default)]
    pub tcp_peer_count: usize,
    #[serde(default)]
    pub utp_peer_count: usize,
    #[serde(default)]
    pub beneficial_tcp_peer_count: usize,
    #[serde(default)]
    pub beneficial_utp_peer_count: usize,
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
    /// Recently departed peers retained long enough for background consumers to observe
    /// their final cumulative transfer counters. UI telemetry does not display these rows.
    #[serde(skip)]
    pub departed_peers: Vec<PeerInfo>,
    /// Cumulative reconnects after an IP has no remaining active peer for this manager lifetime.
    #[serde(skip)]
    pub peer_reconnect_counts: HashMap<IpAddr, u64>,
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
            tcp_peer_count: 0,
            utp_peer_count: 0,
            beneficial_tcp_peer_count: 0,
            beneficial_utp_peer_count: 0,
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
            departed_peers: Vec::new(),
            peer_reconnect_counts: HashMap::new(),
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
    pub added_at_unix_secs: Option<u64>,
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

#[derive(Debug, Clone, Default)]
pub struct SwarmAvailabilityFlashState {
    pub info_hash: Vec<u8>,
    pub previous_availability: Vec<u32>,
    pub flash_start: Vec<Option<Instant>>,
    pub flash_until: Vec<Option<Instant>>,
    active_flash_pieces: Vec<usize>,
    previous_peer_bitfields: HashMap<String, Vec<bool>>,
}

impl SwarmAvailabilityFlashState {
    #[cfg(test)]
    pub fn update(
        &mut self,
        info_hash: &[u8],
        current_availability: Vec<u32>,
        now: Instant,
        flash_duration: Duration,
    ) {
        self.previous_peer_bitfields.clear();
        self.update_from_availability(
            info_hash,
            current_availability.clone(),
            current_availability,
            now,
            flash_duration,
        );
    }

    #[cfg(test)]
    pub fn update_from_peers(
        &mut self,
        info_hash: &[u8],
        peers: &[PeerInfo],
        total_pieces: u32,
        now: Instant,
        flash_duration: Duration,
    ) {
        let current_availability = swarm_availability_counts(peers, total_pieces);
        let current_peer_bitfields =
            swarm_availability_peer_bitfields(peers, current_availability.len());
        self.update_from_peer_availability(
            info_hash,
            current_availability,
            current_peer_bitfields,
            now,
            flash_duration,
        );
    }

    fn update_from_peer_availability(
        &mut self,
        info_hash: &[u8],
        current_availability: Vec<u32>,
        current_peer_bitfields: HashMap<String, Vec<bool>>,
        now: Instant,
        flash_duration: Duration,
    ) {
        if self.info_hash.as_slice() != info_hash
            || self.previous_availability.len() != current_availability.len()
        {
            self.info_hash = info_hash.to_vec();
            self.previous_availability = current_availability;
            self.flash_start = vec![None; self.previous_availability.len()];
            self.flash_until = vec![None; self.previous_availability.len()];
            self.active_flash_pieces.clear();
            self.previous_peer_bitfields = current_peer_bitfields;
            return;
        }

        let mut known_peer_availability = vec![0; current_availability.len()];
        for (peer_key, bitfield) in &current_peer_bitfields {
            if !self.previous_peer_bitfields.contains_key(peer_key) {
                continue;
            }

            for (idx, has_piece) in bitfield.iter().enumerate() {
                if *has_piece {
                    known_peer_availability[idx] += 1;
                }
            }
        }

        self.update_from_availability(
            info_hash,
            current_availability,
            known_peer_availability,
            now,
            flash_duration,
        );
        self.previous_peer_bitfields = current_peer_bitfields;
    }

    fn update_from_availability(
        &mut self,
        info_hash: &[u8],
        current_availability: Vec<u32>,
        flashable_availability: Vec<u32>,
        now: Instant,
        flash_duration: Duration,
    ) {
        if self.info_hash.as_slice() != info_hash
            || self.previous_availability.len() != current_availability.len()
        {
            self.info_hash = info_hash.to_vec();
            self.previous_availability = current_availability;
            self.flash_start = vec![None; self.previous_availability.len()];
            self.flash_until = vec![None; self.previous_availability.len()];
            self.active_flash_pieces.clear();
            self.previous_peer_bitfields.clear();
            return;
        }

        if self.flash_start.len() != current_availability.len() {
            self.flash_start.resize(current_availability.len(), None);
        }
        if self.flash_until.len() != current_availability.len() {
            self.flash_until.resize(current_availability.len(), None);
        }

        let increased_count = self
            .previous_availability
            .iter()
            .zip(flashable_availability.iter())
            .filter(|&(&previous, &current)| current > previous)
            .count();
        let suppress_full_map_flash =
            !flashable_availability.is_empty() && increased_count == flashable_availability.len();

        let mut rank = 0usize;
        for (idx, (&previous, &current)) in self
            .previous_availability
            .iter()
            .zip(flashable_availability.iter())
            .enumerate()
        {
            if current > previous && !suppress_full_map_flash {
                let delay =
                    swarm_availability_flash_rollout_delay(rank, increased_count, flash_duration);
                let start = now + delay;
                self.flash_start[idx] = Some(start);
                self.flash_until[idx] = Some(start + flash_duration);
                if !self.active_flash_pieces.contains(&idx) {
                    self.active_flash_pieces.push(idx);
                }
                rank += 1;
            }
        }

        self.previous_availability = current_availability;
        self.clear_expired(now);
    }

    pub fn is_piece_flashing(&self, info_hash: &[u8], piece_index: usize, now: Instant) -> bool {
        self.info_hash.as_slice() == info_hash
            && self
                .flash_start
                .get(piece_index)
                .copied()
                .flatten()
                .is_some_and(|start| start <= now)
            && self
                .flash_until
                .get(piece_index)
                .copied()
                .flatten()
                .is_some_and(|deadline| deadline > now)
    }

    pub fn has_active_flash(&self, now: Instant) -> bool {
        self.active_flash_pieces.iter().any(|&piece_index| {
            self.flash_until
                .get(piece_index)
                .copied()
                .flatten()
                .is_some_and(|deadline| deadline > now)
        })
    }

    pub fn active_flash_piece_indices(&self, info_hash: &[u8], now: Instant) -> Vec<usize> {
        if self.info_hash.as_slice() != info_hash {
            return Vec::new();
        }

        self.active_flash_pieces
            .iter()
            .copied()
            .filter(|&piece_index| self.is_piece_flashing(info_hash, piece_index, now))
            .collect()
    }

    fn clear_expired(&mut self, now: Instant) {
        self.active_flash_pieces.retain(|&idx| {
            if self.flash_until[idx].is_some_and(|deadline| deadline <= now) {
                self.flash_until[idx] = None;
                if let Some(start) = self.flash_start.get_mut(idx) {
                    *start = None;
                }
                false
            } else {
                true
            }
        });
    }
}

fn swarm_availability_flash_rollout_delay(
    rank: usize,
    flash_count: usize,
    flash_duration: Duration,
) -> Duration {
    if rank == 0 || flash_count <= 1 || flash_duration.is_zero() {
        return Duration::ZERO;
    }

    let steps = flash_count.saturating_sub(1) as u128;
    let delay_nanos = flash_duration
        .as_nanos()
        .saturating_mul(rank as u128)
        .checked_div(steps)
        .unwrap_or(0);
    Duration::from_nanos(delay_nanos.min(u64::MAX as u128) as u64)
}

fn swarm_availability_peer_bitfields(
    peers: &[PeerInfo],
    total_pieces: usize,
) -> HashMap<String, Vec<bool>> {
    let mut bitfields = HashMap::with_capacity(peers.len());
    for (idx, peer) in peers.iter().enumerate() {
        let mut bitfield = vec![false; total_pieces];
        for (piece_idx, has_piece) in peer.bitfield.iter().enumerate().take(total_pieces) {
            bitfield[piece_idx] = *has_piece;
        }
        bitfields.insert(swarm_availability_peer_key(peer, idx), bitfield);
    }
    bitfields
}

fn swarm_availability_peer_key(peer: &PeerInfo, fallback_index: usize) -> String {
    if !peer.address.is_empty() {
        return format!("addr:{}", peer.address);
    }

    if !peer.peer_id.is_empty() {
        return format!("peer:{}", hex::encode(&peer.peer_id));
    }

    format!("slot:{fallback_index}")
}

#[derive(Debug, Clone, Default)]
pub struct DhtWaveUiState {
    pub phase: f64,
    pub amplitude: f64,
    pub harmonic_amplitude: f64,
    pub frequency: f64,
    pub phase_speed: f64,
    pub crest_bias: f64,
    pub bootstrap_ratio: f64,
    pub discovery_boost: f64,
    pub query_load: f64,
    pub query_surge: f64,
    pub initialized: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum VisualizationFocusPanel {
    TorrentList,
    TorrentDetails,
    PeerFiles,
    #[default]
    Chart,
    PeerStream,
    BlockStream,
    DhtWave,
    DiskHealth,
    Statistics,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerStreamVisualization {
    #[default]
    #[serde(alias = "prism_split")]
    Classic,
    HelixExchange,
}

impl PeerStreamVisualization {
    pub const ALL: [Self; 2] = [Self::Classic, Self::HelixExchange];

    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|view| *view == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    pub fn previous(self) -> Self {
        let index = Self::ALL.iter().position(|view| *view == self).unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskHealthVisualization {
    #[default]
    #[serde(
        alias = "io_braid",
        alias = "pressure_fan",
        alias = "load_balance",
        alias = "track_drum",
        alias = "disk_platter",
        alias = "spool_drive",
        alias = "head_sweep",
        alias = "read_head",
        alias = "sector_compass",
        alias = "sector_rack",
        alias = "sector_fan",
        alias = "io_crankshaft",
        alias = "io_equalizer",
        alias = "io_spindle",
        alias = "queue_escapement",
        alias = "queue_conveyor",
        alias = "queue_stack",
        alias = "cache_membrane",
        alias = "cache_reservoir",
        alias = "throughput_rails",
        alias = "pressure_clamp",
        alias = "pressure_bellows",
        alias = "pressure_gauge",
        alias = "block_press",
        alias = "block_well",
        alias = "block_cascade",
        alias = "transfer_pulley",
        alias = "transfer_seismograph",
        alias = "seek_radar",
        alias = "write_stamp",
        alias = "write_anvil",
        alias = "write_fountain",
        alias = "read_fork",
        alias = "read_calipers",
        alias = "read_ribbon",
        alias = "buffer_carousel",
        alias = "buffer_capsules",
        alias = "latency_canyon",
        alias = "flush_sluice",
        alias = "flush_chute",
        alias = "buffer_tower",
        alias = "latency_metronome",
        alias = "latency_spring",
        alias = "transfer_bridge",
        alias = "transfer_cam",
        alias = "transfer_ratchet",
        alias = "load_prism",
        alias = "wear_micrometer",
        alias = "wear_strata",
        alias = "flush_vortex",
        alias = "load_governor",
        alias = "load_piston",
        alias = "sector_bloom",
        alias = "head_shuttle",
        alias = "head_comb",
        alias = "head_ladder"
    )]
    Classic,
    #[serde(alias = "cache_lattice")]
    SeekPendulum,
    #[serde(alias = "circuit_board")]
    StorageDial,
}

impl DiskHealthVisualization {
    pub const ALL: [Self; 3] = [Self::Classic, Self::SeekPendulum, Self::StorageDial];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::SeekPendulum => "Seek Pendulum",
            Self::StorageDial => "Storage Dial",
        }
    }

    pub const fn compact_label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::SeekPendulum => "Pendulum",
            Self::StorageDial => "Dial",
        }
    }

    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|view| *view == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    pub fn previous(self) -> Self {
        let index = Self::ALL.iter().position(|view| *view == self).unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DhtVisualization {
    #[default]
    #[serde(
        alias = "lookup_core",
        alias = "signal_spire",
        alias = "routing_loom",
        alias = "search_beacon",
        alias = "peer_constellation",
        alias = "hash_cascade",
        alias = "query_canyon",
        alias = "packet_lantern",
        alias = "yield_fountain",
        alias = "bootstrap_bridge",
        alias = "demand_prism",
        alias = "signal_ladder",
        alias = "routing_crown",
        alias = "query_hourglass",
        alias = "yield_delta",
        alias = "hash_circuit",
        alias = "query_tide",
        alias = "node_web",
        alias = "query_pulse",
        alias = "query_wings"
    )]
    Classic,
    RelayRibbon,
    PulseGrid,
    LookupVortex,
    PeerBloom,
}

impl DhtVisualization {
    pub const ALL: [Self; 5] = [
        Self::Classic,
        Self::RelayRibbon,
        Self::PulseGrid,
        Self::LookupVortex,
        Self::PeerBloom,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::RelayRibbon => "Relay Ribbon",
            Self::PulseGrid => "Pulse Grid",
            Self::LookupVortex => "Lookup Vortex",
            Self::PeerBloom => "Peer Bloom",
        }
    }

    pub const fn compact_label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::RelayRibbon => "Ribbon",
            Self::PulseGrid => "Grid",
            Self::LookupVortex => "Vortex",
            Self::PeerBloom => "Bloom",
        }
    }

    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|view| *view == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    pub fn previous(self) -> Self {
        let index = Self::ALL.iter().position(|view| *view == self).unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VisualizationFocusState {
    pub active: bool,
    pub selected: VisualizationFocusPanel,
    pub peer_stream: PeerStreamVisualization,
    pub disk_health: DiskHealthVisualization,
    pub dht: DhtVisualization,
}

impl VisualizationFocusState {
    fn from_settings(settings: &Settings) -> Self {
        Self {
            peer_stream: settings.peer_stream_visualization,
            disk_health: settings.disk_health_visualization,
            dht: settings.dht_visualization,
            ..Self::default()
        }
    }

    fn apply_settings(&mut self, settings: &Settings) {
        self.peer_stream = settings.peer_stream_visualization;
        self.disk_health = settings.disk_health_visualization;
        self.dht = settings.dht_visualization;
    }
}

#[derive(Default)]
pub struct UiState {
    pub needs_redraw: bool,
    pub effects_phase_time: f64,
    pub effects_last_wall_time: f64,
    pub effects_speed_multiplier: f64,
    pub measured_fps: Option<f64>,
    pub fps_sample_started_at: Option<Instant>,
    pub fps_sample_frames: u32,
    pub frame_wake_lag_ratio_ema: Option<f64>,
    pub frame_wake_lag_secs_ema: Option<f64>,
    pub frame_draw_ratio_ema: Option<f64>,
    pub file_activity_download_phase: f64,
    pub file_activity_upload_phase: f64,
    pub swarm_availability_flash: SwarmAvailabilityFlashState,
    pub dht_wave: DhtWaveUiState,
    pub visualization_focus: VisualizationFocusState,
    pub selected_header: SelectedHeader,
    pub selected_torrent_index: usize,
    pub selected_peer_index: usize,
    pub is_searching: bool,
    pub search_query: String,
    pub config: ConfigUiState,
    pub delete_confirm: DeleteConfirmUiState,
    pub file_browser: FileBrowserUiState,
    pub help: HelpUiState,
    pub journal: JournalUiState,
    pub peer_management: PeerManagementUiState,
    pub torrent_management: TorrentManagementUiState,
    pub normal_paste_burst: PasteBurst,
    #[allow(dead_code)]
    pub rss: RssUiState,
}

impl UiState {
    fn record_drawn_frame(&mut self, now: Instant) {
        let Some(sample_started_at) = self.fps_sample_started_at else {
            self.fps_sample_started_at = Some(now);
            self.fps_sample_frames = 0;
            return;
        };

        self.fps_sample_frames = self.fps_sample_frames.saturating_add(1);
        let elapsed = now.saturating_duration_since(sample_started_at);
        if elapsed < UI_FPS_SAMPLE_INTERVAL {
            return;
        }

        let elapsed_secs = elapsed.as_secs_f64();
        if elapsed_secs > 0.0 {
            self.measured_fps = Some(self.fps_sample_frames as f64 / elapsed_secs);
        }
        self.fps_sample_started_at = Some(now);
        self.fps_sample_frames = 0;
    }

    fn update_responsiveness_ema(target: &mut Option<f64>, sample: f64) {
        *target = Some(match *target {
            Some(previous) => {
                (sample * UI_RESPONSIVENESS_EMA_ALPHA)
                    + (previous * (1.0 - UI_RESPONSIVENESS_EMA_ALPHA))
            }
            None => sample,
        });
    }

    fn record_frame_wake(
        &mut self,
        scheduled_at: Instant,
        woke_at: Instant,
        target_frame_interval: Duration,
    ) {
        let wake_lag = woke_at.saturating_duration_since(scheduled_at);
        Self::update_responsiveness_ema(&mut self.frame_wake_lag_secs_ema, wake_lag.as_secs_f64());
        let target_secs = target_frame_interval.as_secs_f64();
        if target_secs > 0.0 {
            Self::update_responsiveness_ema(
                &mut self.frame_wake_lag_ratio_ema,
                wake_lag.as_secs_f64() / target_secs,
            );
        }
    }

    fn record_draw_duration(&mut self, draw_duration: Duration, target_frame_interval: Duration) {
        let target_secs = target_frame_interval.as_secs_f64();
        if target_secs > 0.0 {
            Self::update_responsiveness_ema(
                &mut self.frame_draw_ratio_ema,
                draw_duration.as_secs_f64() / target_secs,
            );
        }
    }
}

#[derive(Default)]
pub struct DeleteConfirmUiState {
    pub info_hash: Vec<u8>,
    pub with_files: bool,
}

pub struct FileBrowserUiState {
    pub state: TreeViewState,
    pub data: Vec<RawNode<FileMetadata>>,
    pub browser_mode: FileBrowserMode,
    pub search_state: BrowserSearchState,
    pub search_query: String,
    pub search_mode: SearchMode,
    pub fetch_request_id: u64,
    pub fetch_pending: bool,
    pub fetch_error: Option<String>,
    pub browser_generation: u64,
    pub torrent_preview_request_id: u64,
    pub torrent_file_preview: TorrentFilePreviewState,
    pub return_to_torrent_management_on_close: bool,
}

impl Default for FileBrowserUiState {
    fn default() -> Self {
        Self {
            state: TreeViewState::default(),
            data: Vec::new(),
            browser_mode: FileBrowserMode::default(),
            search_state: BrowserSearchState::default(),
            search_query: String::new(),
            search_mode: SearchMode::Regex,
            fetch_request_id: 0,
            fetch_pending: false,
            fetch_error: None,
            browser_generation: 0,
            torrent_preview_request_id: 0,
            torrent_file_preview: TorrentFilePreviewState::Idle,
            return_to_torrent_management_on_close: false,
        }
    }
}

impl FileBrowserUiState {
    pub fn next_browser_generation(&mut self) -> u64 {
        self.browser_generation = self.browser_generation.wrapping_add(1);
        self.browser_generation
    }

    pub fn invalidate_browser_generation(&mut self) {
        let _ = self.next_browser_generation();
        self.fetch_request_id = self.fetch_request_id.wrapping_add(1);
        self.fetch_pending = false;
        self.fetch_error = None;
        self.torrent_preview_request_id = self.torrent_preview_request_id.wrapping_add(1);
        self.torrent_file_preview = TorrentFilePreviewState::Idle;
    }
}

fn reconcile_file_browser_cursor_after_fetch(
    file_browser: &mut FileBrowserUiState,
    highlight_path: Option<PathBuf>,
    screen_area: Rect,
    pending_torrent_path: bool,
    pending_torrent_link: bool,
) {
    file_browser.state.top_most_offset = 0;

    // Preview-pane searches apply to the torrent tree, not the filesystem list.
    // Match the renderer's filter selection when choosing a post-fetch cursor.
    let filesystem_search_query = if matches!(
        focused_pane(&file_browser.browser_mode),
        BrowserPane::FileSystem
    ) {
        file_browser.search_query.as_str()
    } else {
        ""
    };
    let visible_paths: Vec<PathBuf> = TreeProjection::new(
        &file_browser.data,
        &file_browser.state,
        build_filesystem_filter(
            &file_browser.browser_mode,
            filesystem_search_query,
            file_browser.search_mode,
        ),
        usize::MAX,
    )
    .visible_window()
    .iter()
    .map(|item| item.path.clone())
    .collect();

    let cursor_path = highlight_path
        .filter(|target| visible_paths.iter().any(|path| path == target))
        .or_else(|| visible_paths.first().cloned());
    file_browser.state.cursor_path = cursor_path.clone();

    let has_preview = preview_content_for_selection(
        &file_browser.browser_mode,
        pending_torrent_path,
        pending_torrent_link,
        &file_browser.state,
        &file_browser.data,
    );
    let pane = focused_pane(&file_browser.browser_mode);
    let list_height = calculate_list_height(
        screen_area,
        has_preview,
        file_browser.search_state.is_visible(),
        &pane,
    )
    .max(1);

    if let Some(index) = cursor_path
        .as_ref()
        .and_then(|path| visible_paths.iter().position(|candidate| candidate == path))
    {
        if index >= list_height {
            file_browser.state.top_most_offset = index.saturating_sub(list_height / 2);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HelpSection {
    #[default]
    General,
    Torrents,
    Graphs,
    Legends,
    Screens,
    Paths,
    Build,
}

pub struct HelpUiState {
    pub active_section: HelpSection,
    pub scroll_offset: usize,
    pub is_searching: bool,
    pub search_query: String,
    pub search_mode: SearchMode,
}

impl Default for HelpUiState {
    fn default() -> Self {
        Self {
            active_section: HelpSection::default(),
            scroll_offset: 0,
            is_searching: false,
            search_query: String::new(),
            search_mode: SearchMode::Regex,
        }
    }
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

    let mut tree = RawNode::from_path_list(None, preview_payloads);
    refresh_torrent_preview_directory_priorities(&mut tree);
    tracing::debug!(
        target: "superseedr",
        file_count,
        tree_roots = tree.len(),
        "Built torrent preview tree"
    );
    tree
}

pub fn refresh_torrent_preview_directory_priorities(nodes: &mut [RawNode<TorrentPreviewPayload>]) {
    for node in nodes {
        refresh_torrent_preview_node_priority(node);
    }
}

pub fn apply_torrent_preview_file_priorities(
    nodes: &mut [RawNode<TorrentPreviewPayload>],
    file_priorities: &HashMap<usize, FilePriority>,
) {
    for node in nodes.iter_mut() {
        if let Some(file_index) = node.payload.file_index {
            node.payload.priority = file_priorities
                .get(&file_index)
                .copied()
                .unwrap_or(FilePriority::Normal);
        }
        apply_torrent_preview_file_priorities(&mut node.children, file_priorities);
    }
    refresh_torrent_preview_directory_priorities(nodes);
}

fn refresh_torrent_preview_node_priority(
    node: &mut RawNode<TorrentPreviewPayload>,
) -> FilePriority {
    if !node.is_dir {
        return node.payload.priority;
    }

    let mut common = None;
    let mut mixed = false;
    for child in &mut node.children {
        let child_priority = refresh_torrent_preview_node_priority(child);
        match common {
            Some(priority) if priority != child_priority => mixed = true,
            Some(_) => {}
            None => common = Some(child_priority),
        }
    }

    node.payload.priority = if mixed {
        FilePriority::Mixed
    } else {
        common.unwrap_or(node.payload.priority)
    };
    node.payload.priority
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
    Network,
}

impl JournalFilter {
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Queue,
            Self::Queue => Self::Commands,
            Self::Commands => Self::Health,
            Self::Health => Self::Network,
            Self::Network => Self::All,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::All => Self::Network,
            Self::Queue => Self::All,
            Self::Commands => Self::Queue,
            Self::Health => Self::Commands,
            Self::Network => Self::Health,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "ALL",
            Self::Queue => "INGEST",
            Self::Commands => "COMMANDS",
            Self::Health => "HEALTH",
            Self::Network => "NETWORK",
        }
    }
}

pub struct JournalUiState {
    pub filter: JournalFilter,
    pub selected_index: usize,
    pub scroll_offset: usize,
    pub status_message: Option<String>,
    pub is_searching: bool,
    pub search_query: String,
    pub search_mode: SearchMode,
}

impl Default for JournalUiState {
    fn default() -> Self {
        Self {
            filter: JournalFilter::default(),
            selected_index: 0,
            scroll_offset: 0,
            status_message: None,
            is_searching: false,
            search_query: String::new(),
            search_mode: SearchMode::Regex,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TorrentManagementReviewCache {
    pub(crate) pause: Vec<String>,
    pub(crate) resume: Vec<String>,
    pub(crate) delete: Vec<String>,
    pub(crate) purge: Vec<String>,
    pub(crate) longest_line_width: usize,
}

pub struct TorrentManagementUiState {
    pub selected_index: usize,
    pub cursor_hash: Option<Vec<u8>>,
    pub selected_hashes: HashSet<Vec<u8>>,
    pub pending_commands: Vec<TorrentManagementPendingCommand>,
    pub is_searching: bool,
    pub search_query: String,
    pub search_mode: SearchMode,
    pub selected_column_index: usize,
    pub sort_column_index: Option<usize>,
    pub sort_direction: SortDirection,
    pub status_message: Option<String>,
    pub confirm_submit: bool,
    pub review_scroll_offset: usize,
    pub input_latch: Option<crate::terminal_event::KeyCode>,
    pub(crate) review_cache: Option<TorrentManagementReviewCache>,
}

impl Default for TorrentManagementUiState {
    fn default() -> Self {
        Self {
            selected_index: 0,
            cursor_hash: None,
            selected_hashes: HashSet::new(),
            pending_commands: Vec::new(),
            is_searching: false,
            search_query: String::new(),
            search_mode: SearchMode::Regex,
            selected_column_index: 1,
            sort_column_index: Some(1),
            sort_direction: SortDirection::Ascending,
            status_message: None,
            confirm_submit: false,
            review_scroll_offset: 0,
            input_latch: None,
            review_cache: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchMode {
    #[default]
    Fuzzy,
    Regex,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PeerManagementFilter {
    #[default]
    All,
    Active,
    Recent,
    Restricted,
}

impl PeerManagementFilter {
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Active,
            Self::Active => Self::Recent,
            Self::Recent => Self::Restricted,
            Self::Restricted => Self::All,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::All => Self::Restricted,
            Self::Active => Self::All,
            Self::Recent => Self::Active,
            Self::Restricted => Self::Recent,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "ALL",
            Self::Active => "ACTIVE",
            Self::Recent => "RECENT",
            Self::Restricted => "RESTRICTED",
        }
    }
}

pub struct PeerManagementUiState {
    pub selected_index: usize,
    pub filter: PeerManagementFilter,
    pub is_searching: bool,
    pub search_query: String,
    pub search_mode: SearchMode,
    pub selected_column_index: usize,
    pub sort_column_index: Option<usize>,
    pub sort_direction: SortDirection,
    pub show_details: bool,
    pub details_peer_ip: Option<IpAddr>,
    pub details_scroll_offset: usize,
    pub details_is_searching: bool,
    pub details_search_query: String,
    pub details_search_mode: SearchMode,
    pub status_message: Option<String>,
}

impl Default for PeerManagementUiState {
    fn default() -> Self {
        Self {
            selected_index: 0,
            filter: PeerManagementFilter::All,
            is_searching: false,
            search_query: String::new(),
            search_mode: SearchMode::Regex,
            selected_column_index: 9,
            sort_column_index: Some(9),
            sort_direction: SortDirection::Descending,
            show_details: false,
            details_peer_ip: None,
            details_scroll_offset: 0,
            details_is_searching: false,
            details_search_query: String::new(),
            details_search_mode: SearchMode::Regex,
            status_message: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BrowserSearchState {
    #[default]
    Closed,
    Editing,
    Applied,
}

impl BrowserSearchState {
    pub fn is_editing(self) -> bool {
        matches!(self, Self::Editing)
    }

    pub fn is_visible(self) -> bool {
        !matches!(self, Self::Closed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TorrentManagementPendingCommand {
    pub info_hash: Vec<u8>,
    pub request: ControlRequest,
    pub state: TorrentControlState,
    pub delete_files: bool,
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

/// Platform-resolved paths exposed to the platform-neutral TUI as inert data.
///
/// Native startup populates these from the host configuration environment. The
/// browser supplies virtual paths from its simulation fixture. Rendering and
/// reducers never query the host filesystem or process environment directly.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct RuntimePathView {
    pub shared_mode: bool,
    pub settings_path: Option<PathBuf>,
    pub log_files_path: Option<PathBuf>,
    pub fallback_watch_path: Option<PathBuf>,
    pub shared_inbox_path: Option<PathBuf>,
}

impl RuntimePathView {
    pub fn resolved_watch_path(&self, settings: &Settings) -> Option<PathBuf> {
        settings
            .watch_folder
            .clone()
            .or_else(|| self.fallback_watch_path.clone())
    }
}

#[derive(Default)]
pub struct AppState {
    pub update_available: Option<String>,
    pub should_quit: bool,
    pub shutdown_progress: f64,
    pub system_warning: Option<String>,
    pub system_error: Option<String>,
    pub network_runtime_status: Option<crate::networking::NetworkRuntimeStatus>,
    pub network_activation_status: Option<crate::networking::NetworkActivationStatus>,
    pub limits: CalculatedLimits,

    pub screen_area: Rect,
    pub mode: AppMode,
    pub externally_accessable_port_v4: bool,
    pub externally_accessable_port_v6: bool,
    pub inbound_peer_transports: InboundPeerTransportStatus,
    pub externally_accessable_port_v4_highlight_until: Option<Instant>,
    pub externally_accessable_port_v6_highlight_until: Option<Instant>,
    pub anonymize_torrent_names: bool,

    pub pending_torrent_path: Option<PathBuf>,
    pub pending_torrent_link: String,
    pub pending_magnet_preview_info_hash: Option<Vec<u8>>,
    pub(crate) pending_manual_ingest: Option<PendingManualIngest>,
    pub torrents: HashMap<Vec<u8>, TorrentDisplayState>,
    pub peer_policy: Arc<PeerPolicy>,
    pub peer_manager_view: Arc<PeerManagerView>,

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
    pub avg_disk_write_completed_bps: u64,
    pub effective_download_limit_bps: u64,
    pub active_peer_limit: Option<usize>,

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
    pub bytes_written_completed_this_tick: u64,
    pub pending_piece_write_start_times: HashMap<(Vec<u8>, u32), Instant>,
    pub recv_to_write_latency_samples: VecDeque<Duration>,
    pub recv_to_write_p95: Duration,
    pub read_iops: u32,
    pub write_iops: u32,

    pub ui: UiState,
    pub(crate) peer_management_derived: crate::tui::screens::peers::PeerManagementDerivedState,
    pub rss_runtime: RssRuntimeState,
    pub rss_derived: RssDerivedState,
    pub data_rate: DataRate,
    pub theme: Theme,

    pub torrent_sort: (TorrentSortColumn, SortDirection),
    pub torrent_sort_pinned: bool,
    pub peer_sort: (PeerSortColumn, SortDirection),
    pub peer_sort_pinned: bool,

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
    pub runtime_paths: RuntimePathView,
}

fn sync_peer_policy_to_app_state(
    app_state: &mut AppState,
    peer_policy_rx: &mut watch::Receiver<Arc<PeerPolicy>>,
) -> usize {
    let policy = peer_policy_rx.borrow_and_update().clone();
    let blocked_ips = policy.restrictions.len();
    app_state.peer_policy = policy;
    app_state.ui.needs_redraw = true;
    blocked_ips
}

fn sync_peer_manager_view_to_app_state(
    app_state: &mut AppState,
    peer_manager_view_rx: &mut watch::Receiver<Arc<PeerManagerView>>,
) -> usize {
    let view = peer_manager_view_rx.borrow_and_update().clone();
    let tracked_peers = view.tracked_peers.len();
    app_state.peer_manager_view = view;
    app_state.ui.needs_redraw = true;
    tracked_peers
}

fn should_sync_peer_manager_view(mode: &AppMode) -> bool {
    matches!(mode, AppMode::PeerManagement)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InboundPeerTransportStatus {
    pub tcp_ipv4_seen: bool,
    pub tcp_ipv6_seen: bool,
    pub utp_ipv4_seen: bool,
    pub utp_ipv6_seen: bool,
}

impl InboundPeerTransportStatus {
    fn mark_seen(&mut self, transport: PeerTransportKind, ipv4: bool) {
        match (transport, ipv4) {
            (PeerTransportKind::Tcp, true) => self.tcp_ipv4_seen = true,
            (PeerTransportKind::Tcp, false) => self.tcp_ipv6_seen = true,
            (PeerTransportKind::Utp, true) => self.utp_ipv4_seen = true,
            (PeerTransportKind::Utp, false) => self.utp_ipv6_seen = true,
            (PeerTransportKind::Quic, _) => {}
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct WakeLagPeerThrottle {
    effective_peer_limit: Option<usize>,
    good_ticks: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WakeLagPeerThrottleChange {
    previous_peer_limit: usize,
    current_peer_limit: usize,
    action: &'static str,
}

impl WakeLagPeerThrottle {
    fn additive_step(base_peer_limit: usize) -> usize {
        base_peer_limit
            .saturating_mul(WAKE_LAG_PEER_THROTTLE_ADDITIVE_STEP_PERCENT)
            .saturating_div(100)
            .clamp(1, WAKE_LAG_PEER_THROTTLE_ADDITIVE_STEP_PEERS)
    }

    fn effective_peer_limit(self, base_peer_limit: usize, floor_peer_limit: usize) -> usize {
        if base_peer_limit == 0 {
            return 0;
        }

        self.effective_peer_limit
            .unwrap_or(base_peer_limit)
            .clamp(floor_peer_limit.min(base_peer_limit), base_peer_limit)
    }

    fn update(
        &mut self,
        wake_lag_frame_ratio: Option<f64>,
        wake_lag_secs: Option<f64>,
        base_peer_limit: usize,
        floor_peer_limit: usize,
        connected_peers: usize,
    ) -> Option<WakeLagPeerThrottleChange> {
        if base_peer_limit == 0 {
            self.effective_peer_limit = None;
            self.good_ticks = 0;
            return None;
        }

        let floor_peer_limit = floor_peer_limit.min(base_peer_limit);
        let previous_peer_limit = self.effective_peer_limit(base_peer_limit, floor_peer_limit);
        self.effective_peer_limit =
            (previous_peer_limit < base_peer_limit).then_some(previous_peer_limit);

        let wake_lag_ratio = wake_lag_frame_ratio.filter(|ratio| ratio.is_finite());
        let wake_lag_secs = wake_lag_secs.filter(|secs| secs.is_finite());
        wake_lag_ratio?;

        let mut current_peer_limit = previous_peer_limit;
        let mut action = None;

        let wake_lag_bad = wake_lag_ratio.is_some_and(|ratio| {
            ratio >= WAKE_LAG_PEER_THROTTLE_BAD_RATIO
                && wake_lag_secs
                    .is_some_and(|secs| secs >= WAKE_LAG_PEER_THROTTLE_BAD_MIN_DELAY.as_secs_f64())
        });
        let wake_lag_good = wake_lag_ratio.is_none_or(|ratio| {
            ratio < WAKE_LAG_PEER_THROTTLE_GOOD_RATIO
                || wake_lag_secs
                    .is_some_and(|secs| secs < WAKE_LAG_PEER_THROTTLE_BAD_MIN_DELAY.as_secs_f64())
        });

        if wake_lag_bad {
            self.good_ticks = 0;
            let pressure_peer_limit = if connected_peers == 0 {
                current_peer_limit
            } else {
                current_peer_limit.min(connected_peers)
            };
            current_peer_limit = pressure_peer_limit.saturating_div(2).max(floor_peer_limit);
            if current_peer_limit < previous_peer_limit {
                action = Some("halve_wake_lag");
            }
        } else if wake_lag_good {
            self.good_ticks = self.good_ticks.saturating_add(1);
            if self.good_ticks >= WAKE_LAG_PEER_THROTTLE_GOOD_TICKS
                && current_peer_limit < base_peer_limit
            {
                current_peer_limit = current_peer_limit
                    .saturating_add(Self::additive_step(base_peer_limit))
                    .min(base_peer_limit);
                if current_peer_limit
                    >= connected_peers
                        .saturating_add(WAKE_LAG_PEER_THROTTLE_RECOVERY_HEADROOM_PEERS)
                {
                    current_peer_limit = base_peer_limit;
                    action = Some("clear");
                } else {
                    action = Some("increase");
                }
            }
        } else {
            self.good_ticks = 0;
        }

        self.effective_peer_limit =
            (current_peer_limit < base_peer_limit).then_some(current_peer_limit);

        if current_peer_limit != previous_peer_limit {
            Some(WakeLagPeerThrottleChange {
                previous_peer_limit,
                current_peer_limit,
                action: action.unwrap_or("adjust"),
            })
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
struct DiskBackpressureDownloadThrottle {
    active: bool,
    rate_bytes_per_sec: f64,
    accepted_rate_bytes_per_sec: f64,
    last_score: Option<f64>,
    window_score_total: f64,
    window_ticks: u8,
}

#[derive(Debug, Clone, Copy)]
struct DiskBackpressureSample {
    is_leeching: bool,
    configured_download_limit_bps: u64,
    download_bps: u64,
    disk_write_completed_bps: u64,
    recv_to_write_p95: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum DiskBackpressureDecision {
    Disabled,
    Limited {
        rate_bytes_per_sec: f64,
        capacity_bytes: f64,
    },
}

impl DiskBackpressureDownloadThrottle {
    fn new(configured_download_limit_bps: u64) -> Self {
        let initial_rate = initial_disk_throttle_rate(configured_download_limit_bps);
        Self {
            active: false,
            rate_bytes_per_sec: initial_rate,
            accepted_rate_bytes_per_sec: initial_rate,
            last_score: None,
            window_score_total: 0.0,
            window_ticks: 0,
        }
    }

    fn reset(&mut self, configured_download_limit_bps: u64) {
        let initial_rate = initial_disk_throttle_rate(configured_download_limit_bps);
        self.active = false;
        self.rate_bytes_per_sec = initial_rate;
        self.accepted_rate_bytes_per_sec = initial_rate;
        self.last_score = None;
        self.window_score_total = 0.0;
        self.window_ticks = 0;
    }

    fn update(&mut self, sample: DiskBackpressureSample) -> DiskBackpressureDecision {
        self.update_with_step_factor(sample, random_disk_throttle_step_factor())
    }

    fn update_with_step_factor(
        &mut self,
        sample: DiskBackpressureSample,
        step_factor: f64,
    ) -> DiskBackpressureDecision {
        if !sample.is_leeching || sample.download_bps == 0 {
            self.reset(sample.configured_download_limit_bps);
            return DiskBackpressureDecision::Disabled;
        }

        let ceiling =
            configured_download_ceiling_bytes_per_sec(sample.configured_download_limit_bps);
        self.rate_bytes_per_sec = clamp_disk_throttle_rate(self.rate_bytes_per_sec, ceiling);
        self.accepted_rate_bytes_per_sec =
            clamp_disk_throttle_rate(self.accepted_rate_bytes_per_sec, ceiling);

        if !disk_backpressure_has_signal(sample) {
            self.reset(sample.configured_download_limit_bps);
            return DiskBackpressureDecision::Disabled;
        }

        if !self.active {
            self.active = true;
        }

        self.window_score_total += disk_backpressure_score(sample);
        self.window_ticks = self.window_ticks.saturating_add(1);
        if self.window_ticks >= DISK_WRITE_THROTTLE_WINDOW_TICKS {
            let score = self.window_score_total / f64::from(self.window_ticks);
            self.finish_score_window(score, step_factor, ceiling);
        }

        DiskBackpressureDecision::Limited {
            rate_bytes_per_sec: self.rate_bytes_per_sec,
            capacity_bytes: disk_throttle_capacity_for_rate(self.rate_bytes_per_sec),
        }
    }

    fn finish_score_window(&mut self, score: f64, step_factor: f64, ceiling: f64) {
        match self.last_score {
            Some(last_score) if score < last_score => {
                self.rate_bytes_per_sec = self.accepted_rate_bytes_per_sec;
            }
            _ => {
                self.accepted_rate_bytes_per_sec = self.rate_bytes_per_sec;
                self.last_score = Some(score);
            }
        }

        let next_rate =
            self.accepted_rate_bytes_per_sec * normalize_disk_throttle_step(step_factor);
        self.rate_bytes_per_sec = clamp_disk_throttle_rate(next_rate, ceiling);
        self.window_score_total = 0.0;
        self.window_ticks = 0;
    }
}

fn initial_disk_throttle_rate(configured_download_limit_bps: u64) -> f64 {
    let ceiling = configured_download_ceiling_bytes_per_sec(configured_download_limit_bps);
    clamp_disk_throttle_rate(DISK_WRITE_THROTTLE_START_BYTES_PER_SEC, ceiling)
}

fn configured_download_ceiling_bytes_per_sec(configured_download_limit_bps: u64) -> f64 {
    if crate::config::is_unlimited_rate_limit_bps(configured_download_limit_bps) {
        f64::INFINITY
    } else {
        configured_download_limit_bps as f64 / 8.0
    }
}

fn configured_download_bucket_rate(configured_download_limit_bps: u64) -> f64 {
    rate_limit_bps_to_bucket_bytes_per_sec(configured_download_limit_bps)
}

fn configured_upload_bucket_rate(configured_upload_limit_bps: u64) -> f64 {
    rate_limit_bps_to_bucket_bytes_per_sec(configured_upload_limit_bps)
}

fn random_disk_throttle_step_factor() -> f64 {
    rand::rng().random_range(DISK_WRITE_THROTTLE_STEP_MIN..=DISK_WRITE_THROTTLE_STEP_MAX)
}

fn normalize_disk_throttle_step(step_factor: f64) -> f64 {
    if step_factor.is_finite() && step_factor > 0.0 {
        step_factor.clamp(DISK_WRITE_THROTTLE_STEP_MIN, DISK_WRITE_THROTTLE_STEP_MAX)
    } else {
        1.0
    }
}

fn disk_backpressure_score(sample: DiskBackpressureSample) -> f64 {
    let recv_to_write_seconds = sample.recv_to_write_p95.as_secs_f64();
    sample.disk_write_completed_bps as f64 * DISK_WRITE_THROTTLE_TARGET_LATENCY_SECS
        / recv_to_write_seconds.max(DISK_WRITE_THROTTLE_TARGET_LATENCY_SECS)
}

fn disk_backpressure_has_signal(sample: DiskBackpressureSample) -> bool {
    sample.disk_write_completed_bps > 0 && sample.recv_to_write_p95 > Duration::ZERO
}

fn effective_download_limit_bps(
    configured_download_limit_bps: u64,
    adaptive_bps: Option<u64>,
) -> u64 {
    match adaptive_bps.filter(|bps| *bps > 0) {
        Some(adaptive_bps)
            if !crate::config::is_unlimited_rate_limit_bps(configured_download_limit_bps) =>
        {
            configured_download_limit_bps.min(adaptive_bps)
        }
        Some(adaptive_bps) => adaptive_bps,
        None => configured_download_limit_bps,
    }
}

fn bytes_per_sec_to_bps(bytes_per_sec: f64) -> u64 {
    if !bytes_per_sec.is_finite() || bytes_per_sec <= 0.0 {
        return 0;
    }

    (bytes_per_sec * 8.0).round().min(u64::MAX as f64) as u64
}

fn clamp_disk_throttle_rate(rate_bytes_per_sec: f64, ceiling_bytes_per_sec: f64) -> f64 {
    let minimum = if ceiling_bytes_per_sec.is_finite() {
        DISK_WRITE_THROTTLE_MIN_BYTES_PER_SEC.min(ceiling_bytes_per_sec)
    } else {
        DISK_WRITE_THROTTLE_MIN_BYTES_PER_SEC
    };
    let clamped = rate_bytes_per_sec.max(minimum);
    if ceiling_bytes_per_sec.is_finite() {
        clamped.min(ceiling_bytes_per_sec)
    } else {
        clamped
    }
}

fn disk_throttle_capacity_for_rate(rate_bytes_per_sec: f64) -> f64 {
    if rate_bytes_per_sec > 0.0 && rate_bytes_per_sec.is_finite() {
        (rate_bytes_per_sec * DISK_WRITE_THROTTLE_BURST_SECS).max(1.0)
    } else {
        rate_bytes_per_sec
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::App;

use crate::tui::animation::{advance_dht_wave_state, dht_wave_targets};
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
    if metrics.number_of_pieces_total == 0 {
        return !metrics.is_complete;
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

fn validate_runtime_control_request(request: &ControlRequest) -> Result<(), String> {
    if matches!(request, ControlRequest::MoveTorrent { .. }) {
        return Err(
            "The move command is CLI-only and requires the superseedr client to be stopped."
                .to_string(),
        );
    }
    Ok(())
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

fn torrent_piece_count(torrent: &crate::torrent_file::Torrent) -> u32 {
    if !torrent.info.pieces.is_empty() {
        return (torrent.info.pieces.len() / 20) as u32;
    }

    let total_len = torrent.info.total_length();
    if torrent.info.piece_length > 0 {
        ((total_len as f64) / (torrent.info.piece_length as f64)).ceil() as u32
    } else {
        0
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

fn extract_magnet_exact_length(magnet_link: &str) -> Option<u64> {
    for raw_part in magnet_link.split('&') {
        let part = raw_part.strip_prefix("magnet:?").unwrap_or(raw_part);
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if key.eq_ignore_ascii_case("xl") {
            return value.parse::<u64>().ok();
        }
    }
    None
}

fn normalize_magnet_metadata_path(name: &str) -> String {
    name.replace('\\', "/")
        .split('/')
        .filter(|segment| {
            let segment = segment.trim();
            !segment.is_empty() && segment != "." && segment != ".."
        })
        .collect::<Vec<_>>()
        .join("/")
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

/// Advances production Normal-screen effects from platform-neutral service snapshots.
pub(crate) fn advance_ui_effects_for_frame(
    app_state: &mut AppState,
    settings: &Settings,
    dht_status: &DhtStatus,
    dht_wave_telemetry: &DhtWaveTelemetry,
) {
    let frame_wall_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    if app_state.ui.effects_last_wall_time <= 0.0 {
        app_state.ui.effects_last_wall_time = frame_wall_time;
    }
    let frame_dt = (frame_wall_time - app_state.ui.effects_last_wall_time).clamp(0.0, 0.25);
    app_state.ui.effects_last_wall_time = frame_wall_time;
    advance_ui_effects_for_elapsed(
        app_state,
        settings,
        dht_status,
        dht_wave_telemetry,
        frame_dt,
    );
}

pub(crate) fn advance_ui_effects_for_elapsed(
    app_state: &mut AppState,
    settings: &Settings,
    dht_status: &DhtStatus,
    dht_wave_telemetry: &DhtWaveTelemetry,
    frame_dt: f64,
) {
    let now = Instant::now();
    let mut cleared_port_highlight = false;
    if app_state
        .externally_accessable_port_v4_highlight_until
        .is_some_and(|deadline| deadline <= now)
    {
        app_state.externally_accessable_port_v4_highlight_until = None;
        cleared_port_highlight = true;
    }
    if app_state
        .externally_accessable_port_v6_highlight_until
        .is_some_and(|deadline| deadline <= now)
    {
        app_state.externally_accessable_port_v6_highlight_until = None;
        cleared_port_highlight = true;
    }
    if cleared_port_highlight {
        app_state.ui.needs_redraw = true;
    }

    let frame_dt = frame_dt.clamp(0.0, 0.25);
    let activity_speed_multiplier = compute_effects_activity_speed_multiplier(app_state, settings);
    app_state.ui.effects_speed_multiplier = activity_speed_multiplier;
    app_state.ui.effects_phase_time += frame_dt * activity_speed_multiplier;

    let (target_discovery_boost, download_steps_per_second, upload_steps_per_second) = app_state
        .torrent_list_order
        .get(app_state.ui.selected_torrent_index)
        .and_then(|info_hash| app_state.torrents.get(info_hash))
        .map(|torrent| {
            (
                (torrent.peers_discovered_this_tick as f64 / 10.0).clamp(0.0, 1.0) * 0.18,
                file_activity_wave_steps_per_second(torrent.smoothed_download_speed_bps),
                file_activity_wave_steps_per_second(torrent.smoothed_upload_speed_bps),
            )
        })
        .unwrap_or_else(|| {
            (
                0.0,
                file_activity_wave_steps_per_second(0),
                file_activity_wave_steps_per_second(0),
            )
        });

    let target_wave = dht_wave_targets(dht_status, dht_wave_telemetry);
    advance_dht_wave_state(
        &mut app_state.ui.dht_wave,
        target_wave,
        target_discovery_boost,
        frame_dt,
    );
    app_state.ui.file_activity_download_phase += frame_dt * download_steps_per_second;
    app_state.ui.file_activity_upload_phase += frame_dt * upload_steps_per_second;
    update_swarm_availability_flash_state(app_state, now);

    let disk_phase_speed = crate::tui::animation::disk_health_phase_speed(app_state);
    app_state.disk_health_phase = (app_state.disk_health_phase + frame_dt * disk_phase_speed)
        .rem_euclid(std::f64::consts::TAU);
}

fn update_swarm_availability_flash_state(app_state: &mut AppState, now: Instant) {
    let selected = app_state
        .torrent_list_order
        .get(app_state.ui.selected_torrent_index)
        .and_then(|info_hash| {
            app_state.torrents.get(info_hash).map(|torrent| {
                let current_availability = swarm_availability_counts(
                    &torrent.latest_state.peers,
                    torrent.latest_state.number_of_pieces_total,
                );
                let current_peer_bitfields = swarm_availability_peer_bitfields(
                    &torrent.latest_state.peers,
                    current_availability.len(),
                );
                (
                    info_hash.clone(),
                    current_availability,
                    current_peer_bitfields,
                )
            })
        });

    let Some((info_hash, current_availability, current_peer_bitfields)) = selected else {
        app_state.ui.swarm_availability_flash = SwarmAvailabilityFlashState::default();
        return;
    };

    app_state
        .ui
        .swarm_availability_flash
        .update_from_peer_availability(
            &info_hash,
            current_availability,
            current_peer_bitfields,
            now,
            SWARM_AVAILABILITY_FLASH_DURATION,
        );
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

        if !app_state.torrent_sort_pinned {
            let availability_ordering = a_torrent
                .latest_state
                .data_available
                .cmp(&b_torrent.latest_state.data_available);
            if availability_ordering != std::cmp::Ordering::Equal {
                return availability_ordering;
            }
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

        let default_direction = sort_by.default_direction();
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

fn has_effectively_incomplete_torrents(app_state: &AppState) -> bool {
    app_state
        .torrents
        .values()
        .any(|torrent| torrent_is_effectively_incomplete(&torrent.latest_state))
}

fn clear_finished_progress_priority_pin(app_state: &mut AppState) -> bool {
    let is_progress_priority_pin = app_state.torrent_sort_pinned
        && app_state.torrent_sort == (TorrentSortColumn::Progress, SortDirection::Ascending);
    if !is_progress_priority_pin || app_state.torrents.is_empty() {
        return false;
    }
    if has_effectively_incomplete_torrents(app_state) {
        return false;
    }

    app_state.torrent_sort_pinned = false;
    true
}

pub(crate) fn refresh_autosort_after_stats(
    app_state: &mut AppState,
    previous_torrent_sort: (TorrentSortColumn, SortDirection),
    previous_peer_sort: (PeerSortColumn, SortDirection),
) -> bool {
    let previous_torrent_order = app_state.torrent_list_order.clone();
    let torrent_sort_changed = app_state.torrent_sort != previous_torrent_sort;
    let progress_priority_pin_cleared = clear_finished_progress_priority_pin(app_state);
    if progress_priority_pin_cleared {
        align_unpinned_sort_with_visible_activity(app_state);
    }

    if torrent_sort_changed || progress_priority_pin_cleared || !app_state.torrent_sort_pinned {
        sort_and_filter_torrent_list_state(app_state);
    }

    let peer_sort_changed = app_state.peer_sort != previous_peer_sort;

    torrent_sort_changed
        || progress_priority_pin_cleared
        || app_state.torrent_list_order != previous_torrent_order
        || peer_sort_changed
}

fn set_torrent_sort_to_column(app_state: &mut AppState, column: TorrentSortColumn) {
    app_state.torrent_sort = (column, column.default_direction());
}

fn set_peer_sort_to_column(app_state: &mut AppState, column: PeerSortColumn) {
    app_state.peer_sort = (column, column.default_direction());
}

pub(crate) fn align_unpinned_sort_with_visible_activity(app_state: &mut AppState) {
    if !app_state.torrent_sort_pinned {
        let has_download_activity = app_state
            .torrents
            .values()
            .any(|torrent| torrent.smoothed_download_speed_bps > 0);
        let has_upload_activity = app_state
            .torrents
            .values()
            .any(|torrent| torrent.smoothed_upload_speed_bps > 0);
        let has_incomplete = has_effectively_incomplete_torrents(app_state);

        let target = if has_download_activity && (!app_state.is_seeding || !has_upload_activity) {
            TorrentSortColumn::Down
        } else if has_upload_activity {
            TorrentSortColumn::Up
        } else if has_incomplete {
            TorrentSortColumn::Progress
        } else {
            app_state.torrent_sort.0
        };

        if app_state.torrent_sort.0 != target {
            set_torrent_sort_to_column(app_state, target);
        }
    }

    if !app_state.peer_sort_pinned {
        let selected_torrent = app_state
            .torrent_list_order
            .get(app_state.ui.selected_torrent_index)
            .and_then(|info_hash| app_state.torrents.get(info_hash));
        let has_download_activity = selected_torrent.is_some_and(|torrent| {
            torrent
                .latest_state
                .peers
                .iter()
                .any(|peer| peer.download_speed_bps > 0)
        });
        let has_upload_activity = selected_torrent.is_some_and(|torrent| {
            torrent
                .latest_state
                .peers
                .iter()
                .any(|peer| peer.upload_speed_bps > 0)
        });

        let target = if has_download_activity && (!app_state.is_seeding || !has_upload_activity) {
            PeerSortColumn::DL
        } else if has_upload_activity || app_state.is_seeding {
            PeerSortColumn::UL
        } else {
            PeerSortColumn::DL
        };

        if app_state.peer_sort.0 != target {
            set_peer_sort_to_column(app_state, target);
        }
    }
}
