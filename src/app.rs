// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::fs::File;
use std::future::Future;
use std::io::{self, ErrorKind, Stdout};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use std::collections::VecDeque;

use magnet_url::Magnet;

use fuzzy_matcher::FuzzyMatcher;

use rand::RngExt;

use strum_macros::EnumIter;

use crate::torrent_manager::DiskIoOperation;

use crate::config::{
    classify_shared_mode_settings_change, host_watch_paths, load_torrent_metadata,
    refresh_shared_config_recovery_backup_now, runtime_watch_paths, save_settings, shared_host_id,
    shared_inbox_path, shared_root_path, upsert_torrent_metadata, FeedSyncError, PeerSortColumn,
    RssFilterMode, RssHistoryEntry, Settings, SettingsChangeScope, SortDirection,
    TorrentMetadataEntry, TorrentMetadataFileEntry, TorrentSettings, TorrentSortColumn,
    UiLayoutMode,
};
use crate::control_service::{
    control_event_details, online_control_success_message, plan_control_request,
    ControlExecutionPlan,
};
use crate::dht_service::{DhtService, DhtServiceConfig, DhtStatus, DhtWaveTelemetry};
use crate::peer_manager::{PeerManagerService, PeerManagerView, PeerPolicy};
use crate::persistence::activity_history::{
    load_activity_history_state, save_activity_history_state, ActivityHistoryPersistedState,
    ActivityHistoryRollupState,
};
use crate::persistence::event_journal::{
    append_event_journal_entry, load_event_journal_state, save_event_journal_state,
    save_host_event_journal_state, ControlOrigin, EventCategory, EventDetails, EventJournalEntry,
    EventJournalState, EventScope, EventType, IngestKind, IngestOrigin,
};
use crate::persistence::network_history::{
    load_network_history_state, save_network_history_state, NetworkHistoryPersistedState,
    NetworkHistoryRollupState,
};
use crate::persistence::rss::{load_rss_state, save_rss_state, RssPersistedState};

use crate::token_bucket::{rate_limit_bps_to_bucket_bytes_per_sec, TokenBucket};

use crate::tui::effects::compute_effects_activity_speed_multiplier;
use crate::tui::events;
use crate::tui::layout::common::{ColumnId, PeerColumnId};
use crate::tui::layout::normal::{
    calculate_layout, LayoutContext, DEFAULT_SIDEBAR_PERCENT, PEER_STREAM_MIN_HEIGHT,
    PEER_STREAM_MIN_WIDTH,
};
use crate::tui::paste_burst::PasteBurst;
use crate::tui::screens::browser::{
    build_filesystem_filter, calculate_list_height, focused_pane, preview_content_for_selection,
};
use crate::tui::tree;
use crate::tui::tree::RawNode;
use crate::tui::tree::TreeProjection;
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

use crate::integrations::status::AppOutputState;
use crate::integrations::{
    control::{write_control_request, ControlFilePriorityOverride, ControlRequest},
    rss_ingest, rss_service, status, watcher,
};
use crate::integrity_scheduler::{
    IntegrityScheduler, ProbeBatchOutcome, TorrentIntegritySnapshot,
    INTEGRITY_SCHEDULER_TICK_INTERVAL,
};
use crate::networking::transport::PeerTransportKind;
use crate::networking::{
    available_network_interfaces, NetworkActivationHandle, NetworkActivationPublisher,
    NetworkActivationStatus, NetworkBindingConfig, NetworkHandle, NetworkInterfaceInfo,
    NetworkLease, NetworkScope, NetworkScopeId, NetworkState, NetworkSupervisor, PeerConnection,
    TcpPeerTransport, UtpListenerSet, UtpPeerTransport,
};
use crate::torrent_file::parser::from_bytes;
use crate::torrent_identity::info_hash_from_torrent_source;
use crate::torrent_manager::data_availability_from_file_probe_result;
use crate::torrent_manager::FileActivityUpdate;
use crate::torrent_manager::ManagerCommand;
use crate::torrent_manager::ManagerEvent;
use crate::torrent_manager::TorrentFileProbeStatus;
use crate::torrent_manager::TorrentManager;
use crate::torrent_manager::TorrentParameters;
use crate::watch_inbox::{archive_watch_file, relay_watch_file_to_shared_inbox};

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
use ratatui::{
    backend::{Backend, CrosstermBackend},
    Terminal,
};

use sysinfo::System;

use tracing::{event as tracing_event, Level};

use crate::resource_manager::{
    PermitGuard, ResourceManager, ResourceManagerClient, ResourceManagerError,
};
use tokio::net::TcpListener;
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
const DISK_IDLE_WOBBLE_PHASE_SPEED: f64 = 0.45;
const DISK_MIN_TRANSFER_PHASE_SPEED: f64 = 0.80;
const DISK_MAX_TRANSFER_PHASE_SPEED: f64 = 5.20;
const DISK_PHASE_RATE_MIDPOINT_BPS: f64 = 64.0 * 1024.0 * 1024.0;
const DISK_WRITE_THROTTLE_START_BYTES_PER_SEC: f64 = 1_000_000_000.0 / 8.0;
const DISK_WRITE_THROTTLE_MIN_BYTES_PER_SEC: f64 = 1_000_000.0 / 8.0;
const DISK_WRITE_THROTTLE_WINDOW_TICKS: u8 = 5;
const DISK_WRITE_THROTTLE_STEP_MIN: f64 = 0.80;
const DISK_WRITE_THROTTLE_STEP_MAX: f64 = 1.20;
const DISK_WRITE_THROTTLE_BURST_SECS: f64 = 1.0;
const DISK_WRITE_THROTTLE_TARGET_LATENCY_SECS: f64 = 2.0;
const BITTORRENT_PROTOCOL_STR: &[u8] = b"BitTorrent protocol";

pub struct ListenerSet {
    accept_rx: tokio::sync::Mutex<mpsc::Receiver<io::Result<PeerConnection>>>,
    accept_task: Option<tokio::task::JoinHandle<()>>,
    local_port: u16,
    #[cfg(test)]
    ipv4_bound: bool,
    #[cfg(test)]
    ipv6_bound: bool,
    #[cfg(test)]
    utp_bound: bool,
}

const PEER_TRANSPORT_ENV: &str = "SUPERSEEDR_PEER_TRANSPORT";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerListenerTransportMode {
    Tcp,
    Utp,
    All,
}

fn tcp_peer_listener_enabled_from_env() -> bool {
    tcp_peer_listener_enabled(peer_listener_transport_mode_from_env())
}

fn tcp_peer_listener_enabled(mode: PeerListenerTransportMode) -> bool {
    matches!(
        mode,
        PeerListenerTransportMode::Tcp | PeerListenerTransportMode::All
    )
}

fn utp_peer_listener_enabled_from_env() -> bool {
    matches!(
        peer_listener_transport_mode_from_env(),
        PeerListenerTransportMode::Utp | PeerListenerTransportMode::All
    )
}

fn peer_listener_transport_mode_from_env() -> PeerListenerTransportMode {
    match std::env::var(PEER_TRANSPORT_ENV) {
        Ok(value) => peer_listener_transport_mode(&value),
        Err(_) => PeerListenerTransportMode::All,
    }
}

fn peer_listener_transport_mode(value: &str) -> PeerListenerTransportMode {
    match value.to_ascii_lowercase().as_str() {
        "tcp" => PeerListenerTransportMode::Tcp,
        "utp" => PeerListenerTransportMode::Utp,
        "all" => PeerListenerTransportMode::All,
        _ => PeerListenerTransportMode::All,
    }
}

#[cfg(test)]
async fn bind_peer_listener(
    network_handle: &NetworkHandle,
    port: u16,
) -> io::Result<Option<ListenerSet>> {
    let lease = network_handle.try_lease().map_err(io::Error::other)?;
    bind_peer_listener_with_lease(&lease, port).await
}

async fn bind_peer_listener_with_lease(
    lease: &NetworkLease,
    port: u16,
) -> io::Result<Option<ListenerSet>> {
    lease.ensure_valid().map_err(io::Error::other)?;
    let tcp_enabled = tcp_peer_listener_enabled_from_env();
    let utp_enabled = utp_peer_listener_enabled_from_env();
    if !tcp_enabled && !utp_enabled {
        tracing_event!(
            Level::INFO,
            "Peer listener disabled because TCP is disabled and uTP is not enabled"
        );
        return Ok(None);
    }

    let listener = ListenerSet::bind(lease, port, tcp_enabled, utp_enabled).await?;
    lease.ensure_valid().map_err(io::Error::other)?;
    Ok(Some(listener))
}

impl ListenerSet {
    async fn bind(
        network_lease: &NetworkLease,
        port: u16,
        tcp_enabled: bool,
        utp_enabled: bool,
    ) -> io::Result<Self> {
        let (ipv4, ipv6) = if tcp_enabled {
            bind_tcp_peer_listeners(network_lease, port).await?
        } else {
            tracing_event!(
                Level::INFO,
                "TCP peer listener disabled by peer transport mode"
            );
            (None, None)
        };

        let udp_port = match (
            port,
            ipv4.as_ref()
                .or(ipv6.as_ref())
                .and_then(|listener| listener.local_addr().ok().map(|addr| addr.port())),
        ) {
            (0, Some(bound_port)) => bound_port,
            _ => port,
        };
        let utp = if utp_enabled {
            match UtpPeerTransport::bind_listener(network_lease, udp_port).await {
                Ok(listener) => Some(listener),
                Err(error) if ipv4.is_some() || ipv6.is_some() => {
                    tracing_event!(
                        Level::WARN,
                        error = %error,
                        "uTP listener bind failed; continuing with TCP listener only."
                    );
                    None
                }
                Err(error) => return Err(error),
            }
        } else {
            None
        };

        if ipv4.is_none() && ipv6.is_none() && utp.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "failed to bind any peer listener",
            ));
        }

        let local_port = ipv4
            .as_ref()
            .or(ipv6.as_ref())
            .and_then(|listener| listener.local_addr().ok())
            .map(|addr| addr.port())
            .or_else(|| utp.as_ref().and_then(UtpListenerSet::local_port))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    "listener has no local port",
                )
            })?;
        #[cfg(test)]
        let ipv4_bound = ipv4.is_some();
        #[cfg(test)]
        let ipv6_bound = ipv6.is_some();
        #[cfg(test)]
        let utp_bound = utp.is_some();
        let (accept_tx, accept_rx) = mpsc::channel(64);
        let network_lease = network_lease.clone();
        let accept_task = tokio::spawn(async move {
            let mut invalidation_rx = network_lease.subscribe_invalidation();
            loop {
                if *invalidation_rx.borrow() {
                    break;
                }
                let result = tokio::select! {
                    biased;
                    _ = accept_tx.closed() => break,
                    _ = invalidation_rx.changed() => break,
                    result = accept_peer_transport(&ipv4, &ipv6, utp.as_ref()) => result,
                };
                let result = result.map(|connection| connection.with_network_lease(&network_lease));
                let delivered = tokio::select! {
                    biased;
                    _ = accept_tx.closed() => false,
                    _ = invalidation_rx.changed() => false,
                    result = accept_tx.send(result) => result.is_ok(),
                };
                if !delivered {
                    break;
                }
            }
        });

        Ok(Self {
            accept_rx: tokio::sync::Mutex::new(accept_rx),
            accept_task: Some(accept_task),
            local_port,
            #[cfg(test)]
            ipv4_bound,
            #[cfg(test)]
            ipv6_bound,
            #[cfg(test)]
            utp_bound,
        })
    }

    async fn accept(&self) -> io::Result<PeerConnection> {
        self.accept_rx
            .lock()
            .await
            .recv()
            .await
            .unwrap_or_else(|| Err(network_listener_invalidated()))
    }

    fn local_port(&self) -> Option<u16> {
        Some(self.local_port)
    }

    async fn shutdown(mut self) {
        if let Some(accept_task) = self.accept_task.take() {
            accept_task.abort();
            let _ = accept_task.await;
        }
    }
}

impl Drop for ListenerSet {
    fn drop(&mut self) {
        if let Some(accept_task) = self.accept_task.take() {
            accept_task.abort();
        }
    }
}

async fn accept_peer_transport(
    ipv4: &Option<TcpListener>,
    ipv6: &Option<TcpListener>,
    utp: Option<&UtpListenerSet>,
) -> io::Result<PeerConnection> {
    let tcp_enabled = ipv4.is_some() || ipv6.is_some();
    match (tcp_enabled, utp) {
        (true, Some(utp)) => tokio::select! {
            result = accept_tcp_peer(ipv4, ipv6) => result,
            result = utp.accept() => result,
        },
        (true, None) => accept_tcp_peer(ipv4, ipv6).await,
        (false, Some(utp)) => utp.accept().await,
        (false, None) => Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "no listener is currently bound",
        )),
    }
}

async fn accept_tcp_peer(
    ipv4: &Option<TcpListener>,
    ipv6: &Option<TcpListener>,
) -> io::Result<PeerConnection> {
    let (stream, remote_addr) = match (ipv4, ipv6) {
        (Some(ipv4), Some(ipv6)) => tokio::select! {
            result = ipv4.accept() => result,
            result = ipv6.accept() => result,
        },
        (Some(ipv4), None) => ipv4.accept().await,
        (None, Some(ipv6)) => ipv6.accept().await,
        (None, None) => Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "no TCP listener is currently bound",
        )),
    }?;
    Ok(TcpPeerTransport::incoming(stream, remote_addr))
}

fn network_listener_invalidated() -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "peer listener network generation was invalidated",
    )
}

async fn wait_for_peer_network_invalidation(invalidation_rx: &mut Option<watch::Receiver<bool>>) {
    let Some(invalidation_rx) = invalidation_rx.as_mut() else {
        std::future::pending::<()>().await;
        return;
    };
    if *invalidation_rx.borrow() {
        return;
    }
    let _ = invalidation_rx.changed().await;
}

fn network_policy_warning(config: &crate::networking::NetworkBindingConfig) -> Option<String> {
    (config.mode != crate::networking::NetworkBindingMode::Any
        && config.dns_policy == crate::networking::DnsPolicy::System)
        .then(|| {
            "Strict network binding is active, but DNS uses the system resolver and is outside the application-level leak guarantee."
                .to_string()
        })
}

async fn bind_tcp_peer_listeners(
    network_lease: &NetworkLease,
    port: u16,
) -> io::Result<(Option<TcpListener>, Option<TcpListener>)> {
    let coordinate_ephemeral_dual_stack = port == 0
        && network_lease.ipv4_enabled()
        && network_lease.ipv6_enabled()
        && !network_lease.uses_tokio_tcp_backend();
    let attempts = if coordinate_ephemeral_dual_stack {
        DUAL_STACK_EPHEMERAL_BIND_ATTEMPTS
    } else {
        1
    };

    for attempt in 1..=attempts {
        let ipv6 = match network_lease
            .bind_tcp_listener(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port))
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

        let ipv4 = match network_lease
            .bind_tcp_listener(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                ipv4_port,
            ))
            .await
        {
            Ok(listener) => Some(listener),
            Err(error)
                if ipv6.is_some()
                    && coordinate_ephemeral_dual_stack
                    && error.kind() == io::ErrorKind::AddrInUse
                    && attempt < attempts =>
            {
                tracing_event!(
                    Level::DEBUG,
                    port = ipv4_port,
                    attempt,
                    "Ephemeral TCP port was unavailable to IPv4; retrying dual-stack allocation."
                );
                continue;
            }
            Err(error)
                if ipv6.is_some()
                    && coordinate_ephemeral_dual_stack
                    && error.kind() == io::ErrorKind::AddrInUse =>
            {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!(
                        "failed to allocate a shared IPv4/IPv6 ephemeral TCP port after {attempts} attempts: {error}"
                    ),
                ));
            }
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

        return Ok((ipv4, ipv6));
    }

    unreachable!("dual-stack bind loop always returns or exhausts with an error")
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

type VersionCheckError = Box<dyn std::error::Error + Send + Sync>;

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
pub enum DownloadSelectionTarget {
    #[default]
    PendingAdd,
    ExistingTorrent {
        info_hash: Vec<u8>,
    },
}

pub(crate) const AWAITING_MAGNET_METADATA_LABEL: &str = "awaiting magnet metadata...";

#[derive(Default, Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum FileBrowserMode {
    #[default]
    Directory, // User must pick a folder (e.g. Download Location)
    File(Vec<String>), // User must pick a file matching these extensions (e.g. vec!["torrent"])
    // Future proofing: You could add 'AnyFile' or 'FileOrFolder' here later
    DownloadLocSelection {
        target: DownloadSelectionTarget,
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

fn merge_file_browser_mode_for_fetch(
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

struct IncomingPeerHandshake {
    connection: PeerConnection,
    buffer: Vec<u8>,
    permit: PermitGuard,
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
    NetworkBindingMode,
    NetworkInterface,
    NetworkIpv4Enabled,
    NetworkIpv6Enabled,
    NetworkIpv4Address,
    NetworkIpv6Address,
    NetworkDnsPolicy,
    NetworkDnsServers,
    DefaultDownloadFolder,
    WatchFolder,
    UiLayoutMode,
    AlwaysShowAddLocationPrompt,
    GlobalDownloadLimit,
    GlobalUploadLimit,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConfigPane {
    #[default]
    Settings,
    Details,
}

#[derive(Default)]
#[allow(clippy::large_enum_variant)]
pub enum AppMode {
    Welcome,
    #[default]
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

#[derive(Default, Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
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
pub struct ConfigNetworkInterfaceInventory {
    pub interfaces: Vec<NetworkInterfaceInfo>,
    pub loading: bool,
    pub error: Option<String>,
    request_id: u64,
}

impl ConfigNetworkInterfaceInventory {
    fn begin_refresh(&mut self) -> u64 {
        self.request_id = self.request_id.wrapping_add(1);
        self.interfaces.clear();
        self.loading = true;
        self.error = None;
        self.request_id
    }

    fn finish_refresh(
        &mut self,
        request_id: u64,
        result: Result<Vec<NetworkInterfaceInfo>, String>,
    ) -> bool {
        if request_id != self.request_id {
            return false;
        }
        self.loading = false;
        match result {
            Ok(interfaces) => {
                self.interfaces = interfaces;
                self.error = None;
            }
            Err(error) => {
                self.interfaces.clear();
                self.error = Some(error);
            }
        }
        true
    }
}

#[derive(Default)]
pub struct ConfigUiState {
    pub settings_edit: Box<Settings>,
    pub selected_index: usize,
    pub items: Vec<ConfigItem>,
    pub active_pane: ConfigPane,
    pub editing: Option<ConfigEditState>,
    pub reset_confirmation: Option<ConfigItem>,
    pub network_interface_selection_pending: bool,
    pub network_interface_inventory: ConfigNetworkInterfaceInventory,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConfigEditState {
    pub item: ConfigItem,
    pub buffer: String,
    pub cursor: usize,
    pub select_all: bool,
    pub network_binding_on_cancel: Option<crate::networking::NetworkBindingConfig>,
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

fn load_torrent_file_preview(path: &Path) -> Result<TorrentFilePreview, String> {
    let file_bytes = fs::read(path).map_err(|error| {
        format_filesystem_path_error("Failed to read torrent preview", path, &error)
    })?;
    let torrent = from_bytes(&file_bytes)
        .map_err(|error| format!("Failed to parse torrent preview: {error}"))?;
    let protocol_version = match torrent.info.meta_version {
        Some(2) if !torrent.info.pieces.is_empty() => "BitTorrent v2 (Hybrid)",
        Some(2) => "BitTorrent v2 (Pure)",
        _ => "BitTorrent v1",
    }
    .to_string();

    Ok(TorrentFilePreview {
        name: torrent.info.name.clone(),
        protocol_version,
        total_size: torrent.info.total_length().max(0) as u64,
        tree: build_torrent_preview_tree(torrent.file_list(), &HashMap::new()),
    })
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
    pub input_latch: Option<ratatui::crossterm::event::KeyCode>,
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

pub struct App {
    pub app_state: AppState,
    pub client_configs: Settings,
    pub runtime_mode: AppRuntimeMode,
    pub shared_mode_enabled: bool,
    pub current_cluster_role: Option<AppClusterRole>,
    pub watched_paths: Vec<PathBuf>,
    pub base_system_warning: Option<String>,
    pub network_warning: Option<String>,

    pub listener: Option<ListenerSet>,
    pub network_handle: NetworkHandle,
    pub network_state_rx: watch::Receiver<NetworkState>,
    pub network_activation: NetworkActivationHandle,
    network_activation_publisher: NetworkActivationPublisher,

    pub torrent_manager_incoming_peer_txs:
        HashMap<Vec<u8>, Sender<crate::torrent_manager::IncomingPeerSession>>,
    pub torrent_manager_command_txs: HashMap<Vec<u8>, Sender<ManagerCommand>>,
    incoming_peer_handshake_tx: mpsc::Sender<IncomingPeerHandshake>,
    incoming_peer_handshake_rx: mpsc::Receiver<IncomingPeerHandshake>,
    pub dht_service: DhtService,
    pub dht_status_rx: watch::Receiver<DhtStatus>,
    pub peer_manager: PeerManagerService,
    pub peer_policy_rx: watch::Receiver<Arc<PeerPolicy>>,
    peer_policy_open: bool,
    pub peer_manager_view_rx: watch::Receiver<Arc<PeerManagerView>>,
    peer_manager_view_open: bool,
    pub resource_manager: ResourceManagerClient,
    wake_lag_peer_throttle: WakeLagPeerThrottle,
    last_applied_resource_limits: Option<CalculatedLimits>,
    last_applied_peer_queue_size: Option<usize>,
    pub global_dl_bucket: Arc<TokenBucket>,
    pub global_ul_bucket: Arc<TokenBucket>,
    disk_write_download_throttle: DiskBackpressureDownloadThrottle,

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
    peer_manager_shutdown_tx: broadcast::Sender<()>,
    pub persistence_tx: Option<watch::Sender<Option<PersistPayload>>>,
    pub persistence_task: Option<tokio::task::JoinHandle<()>>,
    event_journal_persistence_tx: Option<watch::Sender<Option<EventJournalPersistRequest>>>,
    event_journal_persistence_task: Option<tokio::task::JoinHandle<()>>,
    shared_recovery_backup_tx: Option<mpsc::Sender<()>>,
    shared_recovery_backup_task: Option<tokio::task::JoinHandle<()>>,
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
    persisted_network_binding_override: Option<NetworkBindingConfig>,
    pub leader_status_snapshot: Option<AppOutputState>,
    pub startup_completion_suppressed_hashes: HashSet<Vec<u8>>,
    pub startup_deferred_load_queue: VecDeque<Vec<u8>>,
    pub startup_loaded_torrent_count: usize,
    pub startup_load_summary_logged: bool,
    pub next_startup_load_at: Option<time::Instant>,
    pub last_dht_peer_slot_usage: Option<(usize, usize)>,
    persisted_torrent_metadata_cache: HashMap<Vec<u8>, TorrentMetadataEntry>,
    data_availability_fault_log_cooldowns: HashMap<Vec<u8>, LogCooldown>,
    probe_available_log_cooldowns: HashMap<Vec<u8>, LogCooldown>,
}

#[cfg(test)]
impl Drop for App {
    fn drop(&mut self) {
        if let Some(task) = self.persistence_task.take() {
            task.abort();
        }
        if let Some(task) = self.event_journal_persistence_task.take() {
            task.abort();
        }
        if let Some(task) = self.shared_recovery_backup_task.take() {
            task.abort();
        }
    }
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
}

#[derive(Clone)]
struct EventJournalPersistRequest {
    state: EventJournalState,
    can_write_shared_state: bool,
}

#[derive(Debug, Clone, Default)]
struct LogCooldown {
    last_logged_at: Option<Instant>,
}

impl LogCooldown {
    fn should_log(&mut self, now: Instant, interval: Duration) -> bool {
        if self
            .last_logged_at
            .is_some_and(|last_logged_at| now.duration_since(last_logged_at) < interval)
        {
            return false;
        }

        self.last_logged_at = Some(now);
        true
    }
}

fn initial_cluster_role_for_runtime_mode(runtime_mode: AppRuntimeMode) -> Option<AppClusterRole> {
    match runtime_mode {
        AppRuntimeMode::Normal => None,
        AppRuntimeMode::SharedLeader => Some(AppClusterRole::Leader),
        AppRuntimeMode::SharedFollower => Some(AppClusterRole::Follower),
    }
}

#[derive(Debug, Clone, Copy)]
struct DhtWaveTargets {
    amplitude: f64,
    harmonic_amplitude: f64,
    frequency: f64,
    phase_speed: f64,
    crest_bias: f64,
    bootstrap_ratio: f64,
    query_load: f64,
}

fn dht_wave_query_load_signal(telemetry: &DhtWaveTelemetry) -> f64 {
    let total_queries = (telemetry.inflight_ipv4_queries + telemetry.inflight_ipv6_queries) as f64;

    if total_queries <= 0.0 {
        0.0
    } else {
        (total_queries / (total_queries + 40.0)).clamp(0.0, 1.0)
    }
}

fn dht_wave_query_pressure_signal(telemetry: &DhtWaveTelemetry) -> f64 {
    let total_queries = (telemetry.inflight_ipv4_queries + telemetry.inflight_ipv6_queries) as f64;
    let unique_peers_found_last_10s = telemetry.unique_peers_found_last_10s as f64;

    if total_queries <= 0.0 {
        0.0
    } else if unique_peers_found_last_10s <= 0.0 {
        (total_queries / (total_queries + 32.0)).clamp(0.0, 1.0)
    } else {
        (total_queries / (total_queries + unique_peers_found_last_10s * 3.0)).clamp(0.0, 1.0)
    }
}

fn dht_wave_targets(status: &DhtStatus, telemetry: &DhtWaveTelemetry) -> DhtWaveTargets {
    let health = &status.health;
    let routes = (health.cached_ipv4_routes + health.cached_ipv6_routes) as f64;
    let bootstrap_total = (health.ipv4_bootstrap_nodes + health.ipv6_bootstrap_nodes) as f64;
    let responsive_total =
        (health.responsive_ipv4_bootstrap_nodes + health.responsive_ipv6_bootstrap_nodes) as f64;

    let route_energy = (routes / 2_048.0).clamp(0.0, 1.0);
    let query_load = dht_wave_query_load_signal(telemetry);
    let pressure_signal = dht_wave_query_pressure_signal(telemetry);
    let bootstrap_ratio = if bootstrap_total > 0.0 {
        (responsive_total / bootstrap_total).clamp(0.0, 1.0)
    } else if health.enabled {
        0.0
    } else {
        1.0
    };
    let enabled_factor = if health.enabled { 1.0 } else { 0.0 };
    let firewalled_factor = match health.firewalled {
        Some(true) => 0.72,
        Some(false) => 1.0,
        None => 0.88,
    };
    let warning_boost = f64::from(status.warning.is_some() || health.recovery_pending);
    let activity_energy = query_load
        .max(pressure_signal * 0.72)
        .max((warning_boost * 0.55).clamp(0.0, 1.0));

    let amplitude = ((0.01
        + query_load * (0.08 + route_energy * 0.12)
        + pressure_signal * 0.13
        + warning_boost * 0.04)
        * firewalled_factor
        * enabled_factor)
        .clamp(0.0, 0.52);
    let harmonic_amplitude = ((0.004
        + query_load * 0.055
        + pressure_signal * 0.075
        + activity_energy * ((1.0 - bootstrap_ratio) * 0.04 + warning_boost * 0.04))
        * enabled_factor)
        .clamp(0.0, 0.20);
    let frequency = (0.08
        + query_load * 0.15
        + pressure_signal * 0.07
        + activity_energy * ((1.0 - bootstrap_ratio) * 0.04 + warning_boost * 0.03))
        .clamp(0.06, 0.38);
    let phase_speed = ((0.03
        + query_load * (0.35 + query_load * 0.85)
        + pressure_signal * 0.48
        + warning_boost * 0.35)
        * enabled_factor)
        .clamp(0.0, 2.0);
    let crest_bias = match health.firewalled {
        Some(true) => -0.10,
        Some(false) => 0.06,
        None => 0.0,
    } + ((route_energy - 0.5) * 0.08 * activity_energy)
        + ((query_load - 0.5) * 0.05 * pressure_signal);

    DhtWaveTargets {
        amplitude,
        harmonic_amplitude,
        frequency,
        phase_speed,
        crest_bias: crest_bias.clamp(-0.22, 0.22),
        bootstrap_ratio,
        query_load,
    }
}

fn dht_wave_smoothing_factor(frame_dt: f64, rate: f64) -> f64 {
    1.0 - (-frame_dt * rate).exp()
}

fn smooth_dht_wave_component(current: &mut f64, target: f64, factor: f64) {
    *current += (target - *current) * factor;
}

const DHT_WAVE_PHASE_WRAP_PERIOD: f64 = std::f64::consts::TAU * 25.0;

fn advance_dht_wave_state(
    wave: &mut DhtWaveUiState,
    target_wave: DhtWaveTargets,
    target_discovery_boost: f64,
    frame_dt: f64,
) {
    if !wave.initialized {
        wave.amplitude = target_wave.amplitude;
        wave.harmonic_amplitude = target_wave.harmonic_amplitude;
        wave.frequency = target_wave.frequency;
        wave.phase_speed = target_wave.phase_speed;
        wave.crest_bias = target_wave.crest_bias;
        wave.bootstrap_ratio = target_wave.bootstrap_ratio;
        wave.discovery_boost = target_discovery_boost;
        wave.query_load = target_wave.query_load;
        wave.query_surge = 0.0;
        wave.initialized = true;
    } else {
        let profile_blend = dht_wave_smoothing_factor(frame_dt, 9.0);
        let phase_speed_blend = dht_wave_smoothing_factor(frame_dt, 14.0);
        let discovery_blend = dht_wave_smoothing_factor(frame_dt, 12.0);
        let query_blend = dht_wave_smoothing_factor(frame_dt, 16.0);
        let query_load_delta = (target_wave.query_load - wave.query_load).abs();
        let target_query_surge = (query_load_delta * 0.32).clamp(0.0, 0.18);
        let query_surge_blend = if target_query_surge > wave.query_surge {
            dht_wave_smoothing_factor(frame_dt, 22.0)
        } else {
            dht_wave_smoothing_factor(frame_dt, 6.0)
        };
        smooth_dht_wave_component(&mut wave.amplitude, target_wave.amplitude, profile_blend);
        smooth_dht_wave_component(
            &mut wave.harmonic_amplitude,
            target_wave.harmonic_amplitude,
            profile_blend,
        );
        smooth_dht_wave_component(&mut wave.frequency, target_wave.frequency, profile_blend);
        smooth_dht_wave_component(
            &mut wave.phase_speed,
            target_wave.phase_speed,
            phase_speed_blend,
        );
        smooth_dht_wave_component(&mut wave.crest_bias, target_wave.crest_bias, profile_blend);
        smooth_dht_wave_component(
            &mut wave.bootstrap_ratio,
            target_wave.bootstrap_ratio,
            profile_blend,
        );
        smooth_dht_wave_component(
            &mut wave.discovery_boost,
            target_discovery_boost,
            discovery_blend,
        );
        smooth_dht_wave_component(&mut wave.query_load, target_wave.query_load, query_blend);
        smooth_dht_wave_component(&mut wave.query_surge, target_query_surge, query_surge_blend);
    }
    wave.phase = (wave.phase + frame_dt * (wave.phase_speed + wave.query_surge * 1.3))
        .rem_euclid(DHT_WAVE_PHASE_WRAP_PERIOD);
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
        let mut persistence_error_log_cooldowns: HashMap<String, LogCooldown> = HashMap::new();
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
                    if persistence_error_log_cooldowns
                        .entry(e.clone())
                        .or_default()
                        .should_log(Instant::now(), REPEATED_HEALTH_LOG_INTERVAL)
                    {
                        tracing_event!(Level::ERROR, "{}", e);
                    }
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

fn spawn_event_journal_persistence_writer() -> (
    watch::Sender<Option<EventJournalPersistRequest>>,
    tokio::task::JoinHandle<()>,
) {
    let (persistence_tx, mut persistence_rx) =
        watch::channel::<Option<EventJournalPersistRequest>>(None);
    let persistence_task = tokio::spawn(async move {
        while persistence_rx.changed().await.is_ok() {
            let Some(request) = persistence_rx.borrow().clone() else {
                continue;
            };
            let write_result = tokio::task::spawn_blocking(move || {
                if request.can_write_shared_state {
                    save_event_journal_state(&request.state)
                } else {
                    save_host_event_journal_state(&request.state)
                }
            })
            .await;

            match write_result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing_event!(Level::ERROR, %error, "Failed to auto-save event journal state");
                }
                Err(error) => {
                    tracing_event!(Level::ERROR, %error, "Event journal writer failed");
                }
            }
        }
    });
    (persistence_tx, persistence_task)
}

fn spawn_shared_recovery_backup_worker() -> (mpsc::Sender<()>, tokio::task::JoinHandle<()>) {
    let (request_tx, mut request_rx) = mpsc::channel::<()>(1);
    let task = tokio::spawn(async move {
        while request_rx.recv().await.is_some() {
            let result =
                tokio::task::spawn_blocking(refresh_shared_config_recovery_backup_now).await;

            match result {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    tracing_event!(
                        Level::WARN,
                        error = %error,
                        "Failed to refresh scheduled shared config recovery backup"
                    );
                }
                Err(error) => {
                    tracing_event!(
                        Level::ERROR,
                        error = %error,
                        "Scheduled shared config recovery backup worker failed"
                    );
                }
            }
        }
    });
    (request_tx, task)
}

fn build_app_dht_service_config(client_configs: &Settings) -> DhtServiceConfig {
    let config = DhtServiceConfig::from_settings(client_configs);
    #[cfg(test)]
    {
        let mut config = config;
        if client_configs.client_port == 0 {
            config.preferred_backend = crate::dht_service::DhtBackendKind::Disabled;
        }
        config
    }
    #[cfg(not(test))]
    {
        config
    }
}

fn requested_listener_port(settings: &Settings) -> u16 {
    if settings.randomize_client_port {
        0
    } else {
        settings.client_port
    }
}

fn listener_bind_error_is_transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::AddrInUse
            | io::ErrorKind::AddrNotAvailable
            | io::ErrorKind::Interrupted
            | io::ErrorKind::TimedOut
            | io::ErrorKind::WouldBlock
    )
}

impl App {
    #[cfg(test)]
    fn active_network_generation_id(&self) -> Option<u64> {
        self.network_activation
            .try_active()
            .ok()
            .map(|active| active.scope().id().generation_id())
    }

    #[cfg(test)]
    pub async fn new(
        client_configs: Settings,
        runtime_mode: AppRuntimeMode,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_with_lock(client_configs, runtime_mode, None).await
    }

    pub async fn new_with_lock(
        client_configs: Settings,
        runtime_mode: AppRuntimeMode,
        app_lock_handle: Option<File>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_with_lock_and_network_persistence_override(
            client_configs,
            runtime_mode,
            app_lock_handle,
            None,
        )
        .await
    }

    pub async fn new_with_lock_and_network_persistence_override(
        mut client_configs: Settings,
        runtime_mode: AppRuntimeMode,
        app_lock_handle: Option<File>,
        persisted_network_binding_override: Option<NetworkBindingConfig>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let requested_port = requested_listener_port(&client_configs);
        let (network_handle, _network_supervisor_task) =
            NetworkSupervisor::spawn_with_config(&client_configs.network_binding);
        let mut network_state_rx = network_handle.subscribe();
        let initial_network_state = network_state_rx.borrow().clone();
        let (mut network_activation_publisher, network_activation) =
            NetworkActivationPublisher::channel();
        let (listener, network_warning) = match initial_network_state {
            NetworkState::Ready(generation) => {
                let lease = network_handle
                    .try_lease_generation(generation.id())
                    .map_err(io::Error::other)?;
                let scope = network_activation_publisher
                    .prepare(lease)
                    .map_err(io::Error::other)?;
                match bind_peer_listener_with_lease(scope.lease(), requested_port).await {
                    Ok(listener) => {
                        let bound_port = listener
                            .as_ref()
                            .and_then(ListenerSet::local_port)
                            .unwrap_or(requested_port);
                        if requested_port == 0 {
                            client_configs.client_port = bound_port;
                        }
                        network_activation_publisher
                            .activate_prepared(scope, bound_port)
                            .map_err(io::Error::other)?;
                        (
                            listener,
                            network_policy_warning(&client_configs.network_binding),
                        )
                    }
                    Err(error) => {
                        let retry_binding = listener_bind_error_is_transient(&error);
                        let reason = format!("initial listener preflight failed: {error}");
                        network_activation_publisher.block(reason.clone());
                        network_handle
                            .block_generation_with_retry(
                                generation.id(),
                                reason.clone(),
                                retry_binding,
                            )
                            .await
                            .map_err(io::Error::other)?;
                        while network_state_rx
                            .borrow()
                            .generation_id()
                            .is_some_and(|generation_id| generation_id == generation.id())
                        {
                            network_state_rx.changed().await.map_err(io::Error::other)?;
                        }
                        (None, Some(format!("Networking blocked: {reason}")))
                    }
                }
            }
            NetworkState::Blocked(reason) => {
                network_activation_publisher.block(reason.to_string());
                (None, Some(format!("Networking blocked: {reason}")))
            }
        };
        let initial_network_runtime_status = network_state_rx
            .borrow()
            .runtime_status(&client_configs.network_binding);
        let initial_network_activation_status = network_activation.status();
        if requested_port == 0 {
            client_configs.client_port = listener
                .as_ref()
                .and_then(ListenerSet::local_port)
                .unwrap_or(0);
        }

        let (manager_event_tx, manager_event_rx) = mpsc::channel::<ManagerEvent>(1000);
        let (app_command_tx, app_command_rx) = mpsc::channel::<AppCommand>(10);
        let (incoming_peer_handshake_tx, incoming_peer_handshake_rx) =
            mpsc::channel::<IncomingPeerHandshake>(INCOMING_PEER_HANDSHAKE_QUEUE_SIZE);
        let (rss_sync_tx, rss_sync_rx) = mpsc::channel::<()>(8);
        let (rss_downloaded_entry_tx, rss_downloaded_entry_rx) =
            mpsc::channel::<RssHistoryEntry>(64);
        let (rss_settings_tx, rss_settings_rx) = watch::channel(client_configs.clone());
        let (tui_event_tx, tui_event_rx) = mpsc::channel::<CrosstermEvent>(100);
        let (shutdown_tx, _) = broadcast::channel(1);
        let (peer_manager_shutdown_tx, _) = broadcast::channel(1);
        let peer_manager = PeerManagerService::new(peer_manager_shutdown_tx.subscribe());
        let peer_policy_rx = peer_manager.handle().subscribe_policy();
        let peer_manager_view_rx = peer_manager.handle().subscribe_view();
        let shared_mode_enabled = runtime_mode.is_shared();
        let current_cluster_role = initial_cluster_role_for_runtime_mode(runtime_mode);
        // Only tests holding the shared-environment guard own an isolated persistence path.
        // Other App tests must not leave background writers targeting that process-global path.
        #[cfg(test)]
        let persistence_writer_enabled = test_persistence_writer_enabled();
        #[cfg(not(test))]
        let persistence_writer_enabled = true;
        let (persistence_tx, persistence_task) = if !persistence_writer_enabled
            || (shared_mode_enabled
                && matches!(current_cluster_role, Some(AppClusterRole::Follower)))
        {
            (None, None)
        } else {
            let (persistence_tx, persistence_task) =
                spawn_persistence_writer(app_command_tx.clone());
            (Some(persistence_tx), Some(persistence_task))
        };
        let (event_journal_persistence_tx, event_journal_persistence_task) =
            if persistence_writer_enabled {
                let (tx, task) = spawn_event_journal_persistence_writer();
                (Some(tx), Some(task))
            } else {
                (None, None)
            };
        let (shared_recovery_backup_tx, shared_recovery_backup_task) = if shared_mode_enabled {
            let (tx, task) = spawn_shared_recovery_backup_worker();
            (Some(tx), Some(task))
        } else {
            (None, None)
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
            ResourceType::PeerConnection,
            (limits.max_connected_peers, limits.max_connected_peers * 2),
        );
        rm_limits.insert(
            ResourceType::DiskRead,
            (limits.disk_read_permits, limits.disk_read_permits * 2),
        );
        rm_limits.insert(
            ResourceType::DiskWrite,
            (limits.disk_write_permits, limits.disk_write_permits * 2),
        );
        let (resource_manager, resource_manager_client) =
            ResourceManager::new(rm_limits, shutdown_tx.clone());
        tokio::spawn(resource_manager.run());

        let dht_service = DhtService::new(
            network_activation.clone(),
            build_app_dht_service_config(&client_configs),
            shutdown_tx.subscribe(),
        )
        .await
        .map_err(io::Error::other)?;
        let dht_status_rx = dht_service.subscribe_status();

        let dl_limit = configured_download_bucket_rate(client_configs.global_download_limit_bps);
        let ul_limit = configured_upload_bucket_rate(client_configs.global_upload_limit_bps);
        let global_dl_bucket = Arc::new(TokenBucket::new(dl_limit, dl_limit));
        let global_ul_bucket = Arc::new(TokenBucket::new(ul_limit, ul_limit));
        let _ = crate::config::ensure_watch_directories(&client_configs);
        let persisted_rss_state = load_rss_state();
        let persisted_event_journal_state = load_event_journal_state();

        let tuning_controller = TuningController::new_adaptive(limits.clone());
        let tuning_state = tuning_controller.state().clone();
        let torrent_sort_direction = if client_configs.torrent_sort_pinned {
            client_configs.torrent_sort_direction
        } else {
            client_configs.torrent_sort_column.default_direction()
        };
        let peer_sort_direction = if client_configs.peer_sort_pinned {
            client_configs.peer_sort_direction
        } else {
            client_configs.peer_sort_column.default_direction()
        };
        let app_state = AppState {
            system_warning: None,
            system_error: None,
            network_runtime_status: Some(initial_network_runtime_status),
            network_activation_status: Some(initial_network_activation_status),
            limits: limits.clone(),
            ui: UiState {
                needs_redraw: true,
                visualization_focus: VisualizationFocusState::from_settings(&client_configs),
                selected_header: if client_configs.torrent_sort_pinned {
                    SelectedHeader::Torrent(torrent_sort_header(client_configs.torrent_sort_column))
                } else {
                    SelectedHeader::default()
                },
                ..Default::default()
            },
            theme: Theme::builtin(client_configs.ui_theme),
            torrent_sort: (client_configs.torrent_sort_column, torrent_sort_direction),
            peer_sort: (client_configs.peer_sort_column, peer_sort_direction),
            torrent_sort_pinned: client_configs.torrent_sort_pinned,
            peer_sort_pinned: client_configs.peer_sort_pinned,
            data_rate: client_configs.ui_refresh_rate,
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
            effective_download_limit_bps: client_configs.global_download_limit_bps,
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
        let persisted_torrent_metadata_cache = load_torrent_metadata()
            .map(|metadata| {
                metadata
                    .torrents
                    .into_iter()
                    .filter_map(|entry| {
                        hex::decode(&entry.info_hash_hex)
                            .ok()
                            .map(|info_hash| (info_hash, entry))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut app = Self {
            app_state,
            client_configs: client_configs.clone(),
            runtime_mode,
            shared_mode_enabled,
            current_cluster_role,
            watched_paths,
            base_system_warning: system_warning,
            network_warning,
            listener,
            network_handle,
            network_state_rx,
            network_activation,
            network_activation_publisher,
            torrent_manager_incoming_peer_txs: HashMap::new(),
            torrent_manager_command_txs: HashMap::new(),
            incoming_peer_handshake_tx,
            incoming_peer_handshake_rx,
            dht_service,
            dht_status_rx,
            peer_manager,
            peer_policy_rx,
            peer_policy_open: true,
            peer_manager_view_rx,
            peer_manager_view_open: true,
            resource_manager: resource_manager_client,
            wake_lag_peer_throttle: WakeLagPeerThrottle::default(),
            last_applied_resource_limits: Some(limits.clone()),
            last_applied_peer_queue_size: Some(limits.max_connected_peers.saturating_mul(2)),
            global_dl_bucket,
            global_ul_bucket,
            disk_write_download_throttle: DiskBackpressureDownloadThrottle::new(
                client_configs.global_download_limit_bps,
            ),
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
            peer_manager_shutdown_tx,
            persistence_tx,
            persistence_task,
            event_journal_persistence_tx,
            event_journal_persistence_task,
            shared_recovery_backup_tx,
            shared_recovery_backup_task,
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
            persisted_network_binding_override,
            leader_status_snapshot: None,
            startup_completion_suppressed_hashes: HashSet::new(),
            startup_deferred_load_queue: VecDeque::new(),
            startup_loaded_torrent_count: 0,
            startup_load_summary_logged: false,
            next_startup_load_at: None,
            last_dht_peer_slot_usage: None,
            persisted_torrent_metadata_cache,
            data_availability_fault_log_cooldowns: HashMap::new(),
            probe_available_log_cooldowns: HashMap::new(),
        };
        sync_peer_policy_to_app_state(&mut app.app_state, &mut app.peer_policy_rx);
        sync_peer_manager_view_to_app_state(&mut app.app_state, &mut app.peer_manager_view_rx);
        app.sync_cluster_role_label();
        app.refresh_system_warning();

        app.ensure_leader_services_running();

        let mut torrents_to_load = app.client_configs.torrents.clone();
        torrents_to_load.sort_by_key(|t| !t.validation_status);
        let mut running_torrents_started = 0usize;
        for torrent_config in torrents_to_load {
            let is_running = matches!(
                torrent_config.torrent_control_state,
                TorrentControlState::Running
            );
            let should_roll_running_torrent =
                is_running && !app.should_suppress_follower_runtime_for_torrent(&torrent_config);
            let should_defer_running_torrent = should_roll_running_torrent
                && running_torrents_started >= STARTUP_ROLLING_LOADS_PER_INTERVAL;

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
                    if app.load_runtime_torrent_from_settings(torrent_config).await {
                        app.startup_loaded_torrent_count =
                            app.startup_loaded_torrent_count.saturating_add(1);
                    }
                }
            } else {
                if app.load_runtime_torrent_from_settings(torrent_config).await {
                    if should_roll_running_torrent {
                        running_torrents_started = running_torrents_started.saturating_add(1);
                    }
                    app.startup_loaded_torrent_count =
                        app.startup_loaded_torrent_count.saturating_add(1);
                }
            }
        }
        app.reschedule_startup_load_deadline();
        app.maybe_log_startup_load_summary();

        if app.app_state.torrents.is_empty()
            && app.startup_deferred_load_queue.is_empty()
            && app.app_state.lifetime_downloaded_from_config == 0
        {
            app.app_state.mode = AppMode::Welcome;
        }

        let is_leeching = app.app_state.torrents.values().any(|t| {
            t.latest_state.number_of_pieces_completed < t.latest_state.number_of_pieces_total
        });
        app.app_state.is_seeding = !is_leeching;
        app.refresh_rss_derived();
        app.refresh_follower_read_model();
        if matches!(
            app.network_activation.status(),
            NetworkActivationStatus::Blocked { .. }
        ) {
            let status = app.network_activation.status();
            app.record_network_activation_status_in_journal(status);
        }

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
            added_at_unix_secs: torrent.added_at_unix_secs,
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
        let mut running_torrents_started = 0usize;
        let mut deferred_torrent_added = false;

        for torrent in self.client_configs.torrents.clone() {
            let Some(info_hash) = info_hash_from_torrent_source(&torrent.torrent_or_magnet) else {
                continue;
            };
            if self.has_live_runtime_for_torrent(&info_hash) {
                continue;
            }
            if self
                .startup_deferred_load_queue
                .iter()
                .any(|queued_hash| queued_hash == &info_hash)
            {
                continue;
            }
            if self.should_suppress_follower_runtime_for_torrent(&torrent) {
                self.ensure_display_only_torrent_from_settings(&torrent);
                continue;
            }
            let is_running = matches!(torrent.torrent_control_state, TorrentControlState::Running);
            if is_running
                && (running_torrents_started >= STARTUP_ROLLING_LOADS_PER_INTERVAL
                    || !self.startup_deferred_load_queue.is_empty())
            {
                self.startup_deferred_load_queue.push_back(info_hash);
                deferred_torrent_added = true;
                continue;
            }

            if self.load_runtime_torrent_from_settings(torrent).await {
                if is_running {
                    running_torrents_started = running_torrents_started.saturating_add(1);
                }
                self.startup_loaded_torrent_count =
                    self.startup_loaded_torrent_count.saturating_add(1);
            }
        }

        if deferred_torrent_added {
            self.reschedule_startup_load_deadline();
        }
        self.maybe_log_startup_load_summary();
    }

    pub fn is_shared_mode_enabled(&self) -> bool {
        self.shared_mode_enabled
    }

    pub fn is_current_shared_leader(&self) -> bool {
        matches!(self.current_cluster_role, Some(AppClusterRole::Leader))
    }

    fn refresh_shared_recovery_backup_on_interval(&self) {
        let Some(tx) = &self.shared_recovery_backup_tx else {
            return;
        };
        match tx.try_send(()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
            Err(mpsc::error::TrySendError::Closed(())) => {
                tracing_event!(
                    Level::WARN,
                    "Scheduled shared config recovery backup worker is unavailable"
                );
            }
        }
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

        #[cfg(test)]
        let persistence_writer_enabled = test_persistence_writer_enabled();
        #[cfg(not(test))]
        let persistence_writer_enabled = true;
        if self.persistence_tx.is_none() && persistence_writer_enabled {
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
                self.network_activation.clone(),
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
            validation_status: false,
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
            validation_status: false,
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

        if self.is_current_shared_follower() && !is_shared_inbox_path {
            let Some(default_download_folder) = self.client_configs.default_download_folder.clone()
            else {
                return AddIngressAction::Fail {
                    message: "Follower add ingest requires a default download folder so the leader can apply the torrent without local manual UI.".to_string(),
                };
            };
            return match self
                .control_request_for_add_payload(&payload, Some(default_download_folder))
            {
                Ok(request) => AddIngressAction::QueueControlRequest(request),
                Err(message) => AddIngressAction::Fail { message },
            };
        }

        if self.client_configs.always_show_add_location_prompt
            && !is_host_watch_path
            && (!is_shared_inbox_path || matches!(self.runtime_mode, AppRuntimeMode::SharedLeader))
        {
            return AddIngressAction::OpenManualBrowser { payload };
        }

        if let Some(download_path) = self.client_configs.default_download_folder.clone() {
            AddIngressAction::ApplyDirectly {
                payload,
                download_path,
            }
        } else if self.is_current_shared_follower() {
            AddIngressAction::Fail {
                message: "Follower add ingest requires a default download folder so the leader can apply the torrent without local manual UI.".to_string(),
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

        let mut updated = false;
        for entry in self.app_state.event_journal_state.entries.iter_mut().rev() {
            if entry.category != EventCategory::Ingest {
                continue;
            }
            if entry.correlation_id.as_deref() != Some(correlation_id.as_str()) {
                continue;
            }
            entry.source_path = Some(final_path.clone());
            updated = true;
            if entry.event_type == EventType::IngestQueued {
                break;
            }
        }
        if updated {
            self.persist_event_journal();
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

    fn open_manual_browser_for_torrent_file_with_archive(
        &mut self,
        path: PathBuf,
        archive_watched_input: bool,
    ) -> Result<(), String> {
        let buffer = fs::read(&path).map_err(|error| {
            format_filesystem_path_error("Failed to read torrent file", &path, &error)
        })?;
        let torrent = from_bytes(&buffer)
            .map_err(|_| "Failed to parse torrent file for preview.".to_string())?;

        let final_path = if archive_watched_input
            && (self.is_host_watch_path(&path) || self.is_shared_inbox_path(&path))
        {
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

        self.cleanup_pending_magnet_preview_runtime();
        self.app_state.pending_torrent_link.clear();
        self.app_state.pending_torrent_path = Some(final_path);
        let initial_path = self.get_initial_destination_path();
        let initial_pane = self.initial_download_selection_pane();
        let browser_generation = self.app_state.ui.file_browser.next_browser_generation();

        self.start_file_browser_fetch(
            browser_generation,
            initial_path,
            FileBrowserMode::DownloadLocSelection {
                target: DownloadSelectionTarget::PendingAdd,
                torrent_files: vec![],
                container_name: default_container_name.clone(),
                use_container: should_enclose,
                is_editing_name: false,
                preview_tree,
                preview_state,
                focused_pane: initial_pane,
                cursor_pos: 0,
                original_name_backup: default_container_name,
            },
            false,
            None,
        );
        Ok(())
    }

    async fn open_manual_browser_for_payload(
        &mut self,
        source: IngestSource,
        payload: ResolvedAddPayload,
    ) -> Result<(), String> {
        match payload {
            ResolvedAddPayload::TorrentFile { source_path } => {
                if matches!(source, IngestSource::TorrentFile) {
                    let archive_watched_input = !self.is_shared_inbox_path(&source_path);
                    self.open_manual_browser_for_torrent_file_with_archive(
                        source_path,
                        archive_watched_input,
                    )
                } else {
                    self.cleanup_pending_magnet_preview_runtime();
                    self.app_state.pending_torrent_link.clear();
                    self.app_state.pending_torrent_path = Some(source_path);
                    let initial_path = self.get_initial_destination_path();
                    let initial_pane = self.initial_download_selection_pane();
                    let browser_generation =
                        self.app_state.ui.file_browser.next_browser_generation();
                    self.start_file_browser_fetch(
                        browser_generation,
                        initial_path,
                        FileBrowserMode::DownloadLocSelection {
                            target: DownloadSelectionTarget::PendingAdd,
                            torrent_files: vec![],
                            container_name: "New Torrent".to_string(),
                            use_container: true,
                            is_editing_name: false,
                            preview_tree: Vec::new(),
                            preview_state: TreeViewState::default(),
                            focused_pane: initial_pane,
                            cursor_pos: 0,
                            original_name_backup: "New Torrent".to_string(),
                        },
                        false,
                        None,
                    );
                    Ok(())
                }
            }
            ResolvedAddPayload::MagnetLink { magnet_link } => {
                self.cleanup_pending_magnet_preview_runtime();
                self.app_state.pending_torrent_path = None;
                self.app_state.pending_torrent_link = magnet_link.clone();
                let (btih, btmh) = parse_hybrid_hashes(&magnet_link);
                let pending_info_hash = btih.or(btmh);
                let initial_path = self.get_initial_destination_path();
                let initial_pane = self.initial_download_selection_pane();
                let browser_generation = self.app_state.ui.file_browser.next_browser_generation();
                let (container_name, use_container) = if self.is_current_shared_follower() {
                    (String::new(), false)
                } else {
                    (AWAITING_MAGNET_METADATA_LABEL.to_string(), true)
                };
                self.start_file_browser_fetch(
                    browser_generation,
                    initial_path,
                    FileBrowserMode::DownloadLocSelection {
                        target: DownloadSelectionTarget::PendingAdd,
                        torrent_files: vec![],
                        container_name: container_name.clone(),
                        use_container,
                        is_editing_name: false,
                        preview_tree: Vec::new(),
                        preview_state: TreeViewState::default(),
                        focused_pane: initial_pane,
                        cursor_pos: 0,
                        original_name_backup: container_name,
                    },
                    false,
                    None,
                );
                if !self.is_current_shared_follower() {
                    let ingest_result = self
                        .add_magnet_torrent(
                            "Fetching name...".to_string(),
                            magnet_link,
                            None,
                            false,
                            TorrentControlState::Running,
                            HashMap::new(),
                            None,
                        )
                        .await;
                    match ingest_result {
                        CommandIngestResult::Added { info_hash, .. } => {
                            let info_hash = info_hash.or_else(|| pending_info_hash.clone());
                            if let Some(info_hash) = info_hash {
                                self.app_state.pending_magnet_preview_info_hash =
                                    Some(info_hash.clone());
                                self.hydrate_pending_magnet_browser_from_display(&info_hash);
                            }
                        }
                        CommandIngestResult::Duplicate { info_hash, .. } => {
                            let info_hash = info_hash.or_else(|| pending_info_hash.clone());
                            if let Some(info_hash) = info_hash {
                                self.hydrate_pending_magnet_browser_from_display(&info_hash);
                            }
                        }
                        CommandIngestResult::Failed { message, .. }
                        | CommandIngestResult::Invalid { message, .. } => {
                            self.app_state.system_error = Some(message);
                        }
                    }
                }
                Ok(())
            }
        }
    }

    pub(crate) async fn open_manual_magnet_browser(
        &mut self,
        magnet_link: String,
    ) -> Result<(), String> {
        self.open_manual_browser_for_payload(
            IngestSource::MagnetFile,
            ResolvedAddPayload::MagnetLink { magnet_link },
        )
        .await
    }

    pub(crate) fn open_add_torrent_file_browser(&mut self) {
        let initial_path = self.get_initial_source_path();
        let browser_generation = self.app_state.ui.file_browser.next_browser_generation();
        self.app_state
            .ui
            .file_browser
            .return_to_torrent_management_on_close = false;
        self.start_file_browser_fetch(
            browser_generation,
            initial_path,
            FileBrowserMode::File(vec![".torrent".to_string()]),
            false,
            None,
        );
    }

    pub(crate) fn open_existing_torrent_file_browser(&mut self, info_hash: Vec<u8>) {
        let Some(display) = self.app_state.torrents.get(&info_hash) else {
            return;
        };
        let return_to_torrent_management_on_close =
            matches!(self.app_state.mode, AppMode::TorrentManagement);
        let metrics = display.latest_state.clone();
        let mut preview_tree = display.file_preview_tree.clone();
        if preview_tree.is_empty() {
            if let Some(metadata) = self.persisted_torrent_metadata_cache.get(&info_hash) {
                let files = metadata
                    .files
                    .iter()
                    .map(|file| {
                        (
                            file.relative_path
                                .split('/')
                                .filter(|segment| !segment.is_empty())
                                .map(|segment| segment.to_string())
                                .collect::<Vec<_>>(),
                            file.length,
                        )
                    })
                    .collect();
                preview_tree = build_torrent_preview_tree(files, &metrics.file_priorities);
            }
        }

        let mut preview_state = TreeViewState::new();
        for node in &preview_tree {
            node.expand_all(&mut preview_state);
        }
        preview_state.cursor_path = preview_tree.first().map(|node| node.full_path.clone());

        let initial_path = metrics
            .download_path
            .clone()
            .or_else(|| self.client_configs.default_download_folder.clone())
            .unwrap_or_else(|| self.get_initial_destination_path());

        let should_abandon_pending_magnet_preview = !self.app_state.pending_torrent_link.is_empty();
        self.app_state.pending_torrent_path = None;
        self.app_state.pending_torrent_link.clear();
        if should_abandon_pending_magnet_preview {
            self.cleanup_pending_magnet_preview_runtime();
        }
        self.app_state
            .ui
            .file_browser
            .invalidate_browser_generation();
        self.app_state.ui.file_browser.state = TreeViewState {
            current_path: initial_path,
            ..TreeViewState::default()
        };
        self.app_state.ui.file_browser.data.clear();
        self.app_state.ui.file_browser.search_state = BrowserSearchState::Closed;
        self.app_state.ui.file_browser.search_query.clear();
        self.app_state
            .ui
            .file_browser
            .return_to_torrent_management_on_close = return_to_torrent_management_on_close;
        self.app_state.ui.file_browser.browser_mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::ExistingTorrent { info_hash },
            torrent_files: vec![],
            container_name: String::new(),
            use_container: false,
            is_editing_name: false,
            preview_tree,
            preview_state,
            focused_pane: BrowserPane::TorrentPreview,
            cursor_pos: 0,
            original_name_backup: String::new(),
        };
        self.app_state.mode = AppMode::FileBrowser;
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
                self.clear_pending_magnet_preview_if_applied(&ingest_result);
                self.record_ingest_result(&path, &ingest_result);
                self.save_state_to_disk();
                self.archive_processed_ingest(source, &path);
            }
            AddIngressAction::OpenManualBrowser { payload } => {
                let should_defer_archive = self.is_shared_inbox_path(&path);
                if let Err(message) = self.open_manual_browser_for_payload(source, payload).await {
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
                } else if should_defer_archive {
                    self.app_state.pending_manual_ingest = Some(PendingManualIngest {
                        source,
                        path: path.clone(),
                    });
                } else {
                    self.app_state.pending_manual_ingest = None;
                }
                if !matches!(source, IngestSource::TorrentFile) && !should_defer_archive {
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
        self.dispatch_cluster_control_request_with_ingest_result(request, origin)
            .await
            .map(|(message, _)| message)
    }

    async fn dispatch_cluster_control_request_with_ingest_result(
        &mut self,
        request: ControlRequest,
        origin: ControlOrigin,
    ) -> Result<(String, Option<CommandIngestResult>), String> {
        if self.is_current_shared_follower() {
            self.queue_control_request_for_leader(request, origin)
                .map(|message| (message, None))
        } else {
            self.apply_control_request_with_ingest_result(&request)
                .await
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

    fn clear_pending_magnet_preview_if_applied(&mut self, result: &CommandIngestResult) {
        let applied_info_hash = match result {
            CommandIngestResult::Added {
                info_hash: Some(info_hash),
                ..
            }
            | CommandIngestResult::Duplicate {
                info_hash: Some(info_hash),
                ..
            } => info_hash,
            _ => return,
        };

        if self.app_state.pending_magnet_preview_info_hash.as_deref()
            == Some(applied_info_hash.as_slice())
        {
            self.app_state.pending_magnet_preview_info_hash = None;
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
                self.apply_reloaded_settings(new_settings).await;
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

    fn wake_lag_peer_throttle_floor(&self, base_peer_limit: usize) -> usize {
        if base_peer_limit == 0 {
            return 0;
        }

        let minimum_floor = WAKE_LAG_PEER_THROTTLE_MIN_PEERS.min(base_peer_limit);
        if self.peer_limiter_download_activity_active() {
            let download_floor = base_peer_limit
                .saturating_mul(WAKE_LAG_PEER_THROTTLE_DOWNLOAD_FLOOR_PERCENT)
                .saturating_div(100)
                .clamp(1, base_peer_limit);
            minimum_floor.max(download_floor)
        } else {
            minimum_floor
        }
    }

    fn peer_limiter_download_activity_active(&self) -> bool {
        self.app_state
            .avg_download_history
            .last()
            .copied()
            .unwrap_or(0)
            > 0
            || self.app_state.torrents.values().any(|torrent| {
                torrent.latest_state.torrent_control_state == TorrentControlState::Running
                    && !torrent.latest_state.is_complete
            })
    }

    fn effective_resource_limits(&self) -> CalculatedLimits {
        let mut limits = self.app_state.limits.clone();
        let floor_peer_limit = self.wake_lag_peer_throttle_floor(limits.max_connected_peers);
        limits.max_connected_peers = self
            .wake_lag_peer_throttle
            .effective_peer_limit(limits.max_connected_peers, floor_peer_limit);
        limits
    }

    fn peer_admission_stress_active_for(&self, effective_limits: &CalculatedLimits) -> bool {
        effective_limits.max_connected_peers < self.app_state.limits.max_connected_peers
    }

    fn peer_admission_stress_active(&self) -> bool {
        self.peer_admission_stress_active_for(&self.effective_resource_limits())
    }

    fn effective_peer_queue_size(&self, effective_limits: &CalculatedLimits) -> usize {
        if self.peer_admission_stress_active_for(effective_limits) {
            0
        } else {
            self.app_state.limits.max_connected_peers.saturating_mul(2)
        }
    }

    async fn apply_effective_resource_limits(&mut self) {
        let effective_limits = self.effective_resource_limits();
        let peer_queue_size = self.effective_peer_queue_size(&effective_limits);
        self.app_state.active_peer_limit = (effective_limits.max_connected_peers
            < self.app_state.limits.max_connected_peers)
            .then_some(effective_limits.max_connected_peers);
        if self.last_applied_resource_limits.as_ref() == Some(&effective_limits)
            && self.last_applied_peer_queue_size == Some(peer_queue_size)
        {
            return;
        }

        self.last_applied_resource_limits = Some(effective_limits.clone());
        self.last_applied_peer_queue_size = Some(peer_queue_size);
        self.last_dht_peer_slot_usage = None;
        self.sync_dht_peer_slot_usage();
        let _ = self
            .resource_manager
            .update_limits_and_queue_sizes(
                effective_limits.into_map_with_peer_queue(peer_queue_size),
            )
            .await;
    }

    fn update_wake_lag_peer_throttle(&mut self) {
        let wake_lag_frame_ratio = self.app_state.ui.frame_wake_lag_ratio_ema;
        let wake_lag_secs = self.app_state.ui.frame_wake_lag_secs_ema;
        let base_peer_limit = self.app_state.limits.max_connected_peers;
        let floor_peer_limit = self.wake_lag_peer_throttle_floor(base_peer_limit);
        let connected_peers = self.total_successfully_connected_peers();
        let change = self.wake_lag_peer_throttle.update(
            wake_lag_frame_ratio,
            wake_lag_secs,
            base_peer_limit,
            floor_peer_limit,
            connected_peers,
        );
        let effective_peer_limit = self
            .wake_lag_peer_throttle
            .effective_peer_limit(base_peer_limit, floor_peer_limit);

        if let Some(change) = change {
            tracing_event!(
                target: "superseedr::wake_lag_peer_throttle",
                Level::INFO,
                wake_lag_frame_ratio = ?wake_lag_frame_ratio,
                wake_lag_secs = ?wake_lag_secs,
                action = change.action,
                previous_peer_limit = change.previous_peer_limit,
                current_peer_limit = change.current_peer_limit,
                base_peer_limit,
                floor_peer_limit,
                effective_peer_limit,
                connected_peers,
                good_ticks = self.wake_lag_peer_throttle.good_ticks,
                "wake_lag_peer_throttle"
            );
        }
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
        self.startup_version_checker();

        let mut sys = System::new();

        let mut stats_interval = time::interval(Duration::from_secs(1));
        let mut network_history_persist_interval =
            time::interval(Duration::from_secs(NETWORK_HISTORY_PERSIST_INTERVAL_SECS));
        let shared_recovery_backup_period =
            Duration::from_secs(SHARED_RECOVERY_BACKUP_REFRESH_INTERVAL_SECS);
        let mut shared_recovery_backup_interval = time::interval_at(
            time::Instant::now() + shared_recovery_backup_period,
            shared_recovery_backup_period,
        );
        let mut watch_folder_rescan_interval =
            time::interval(Duration::from_secs(WATCH_FOLDER_RESCAN_INTERVAL_SECS));
        let mut shared_role_retry_interval =
            time::interval(Duration::from_secs(SHARED_ROLE_RETRY_INTERVAL_SECS));
        let mut integrity_scheduler_interval = time::interval(INTEGRITY_SCHEDULER_TICK_INTERVAL);
        self.reschedule_tuning_deadline();
        network_history_persist_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        shared_recovery_backup_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
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
                AppMode::Welcome => DataRate::Rate60s.frame_interval(), // Force 60 FPS for animation
                AppMode::PowerSaving => Duration::from_secs(1),         // Force 1 FPS for Zen mode
                _ => self.app_state.data_rate.frame_interval(),         // User-defined FPS
            };
            let next_tuning_at = self.next_tuning_at;
            let next_paste_flush_at = self.app_state.ui.normal_paste_burst.next_deadline();
            let next_status_dump_at = self.next_status_dump_at;
            let next_startup_load_at = self.next_startup_load_at;

            tokio::select! {
                _ = signal::ctrl_c() => {
                    self.app_state.should_quit = true;
                }
                Ok(Ok(connection)) = async {
                    match &self.listener {
                        Some(listener) => tokio::time::timeout(Duration::from_secs(2), listener.accept()).await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Ok(active_network) = self.network_activation.try_active() {
                        self.handle_incoming_peer(
                            connection.with_network_lease(active_network.scope().lease()),
                        ).await;
                    }

                }
                Some(incoming) = self.incoming_peer_handshake_rx.recv() => {
                    self.route_incoming_peer_handshake(incoming);
                }
                Some(event) = self.manager_event_rx.recv() => {
                    self.handle_manager_event(event);
                    self.app_state.ui.needs_redraw = true;
                }
                status_changed = self.dht_status_rx.changed() => {
                    if status_changed.is_ok() {
                        self.handle_dht_status_changed();
                    }
                }
                network_changed = self.network_state_rx.changed() => {
                    if network_changed.is_ok() {
                        self.handle_network_state_changed().await;
                    }
                }
                policy_changed = self.peer_policy_rx.changed(), if self.peer_policy_open => {
                    if policy_changed.is_ok() {
                        let blocked_ips = sync_peer_policy_to_app_state(
                            &mut self.app_state,
                            &mut self.peer_policy_rx,
                        );
                        tracing::debug!(
                            blocked_ips,
                            "App received peer policy"
                        );
                        if matches!(self.app_state.mode, AppMode::PeerManagement) {
                            self.refresh_peer_management_derived(SystemTime::now());
                        }
                    } else {
                        self.peer_policy_open = false;
                    }
                }
                view_changed = self.peer_manager_view_rx.changed(), if self.peer_manager_view_open && should_sync_peer_manager_view(&self.app_state.mode) => {
                    if view_changed.is_ok() {
                        let tracked_peers = sync_peer_manager_view_to_app_state(
                            &mut self.app_state,
                            &mut self.peer_manager_view_rx,
                        );
                        tracing::debug!(
                            tracked_peers,
                            "App received peer manager view"
                        );
                        self.refresh_peer_management_derived(SystemTime::now());
                    } else {
                        self.peer_manager_view_open = false;
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
                    self.calculate_stats(&mut sys).await;
                    if matches!(self.app_state.mode, AppMode::PeerManagement) {
                        crate::tui::screens::peers::refresh_peer_management_expiries(
                            &mut self.app_state,
                            SystemTime::now(),
                        );
                    }
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
                _ = shared_recovery_backup_interval.tick() => {
                    self.refresh_shared_recovery_backup_on_interval();
                }
                _ = integrity_scheduler_interval.tick() => {
                    self.advance_integrity_scheduler(INTEGRITY_SCHEDULER_TICK_INTERVAL);
                }
                _ = time::sleep_until(next_draw_time.into()) => {
                    let scheduled_frame_time = next_draw_time;
                    let frame_started_at = Instant::now();
                    self.app_state.ui.record_frame_wake(
                        scheduled_frame_time,
                        frame_started_at,
                        current_target_framerate,
                    );
                    Self::advance_next_draw_time(
                        &mut next_draw_time,
                        frame_started_at,
                        current_target_framerate,
                    );
                    self.drain_latest_torrent_metrics();
                    self.sync_dht_peer_slot_usage();
                    self.sync_network_activation_status_to_journal();
                    let normal_animation_active = if matches!(self.app_state.mode, AppMode::Normal)
                    {
                        let dht_wave_telemetry = self.dht_service.current_wave_telemetry();
                        Self::normal_mode_animation_active(
                            &self.app_state,
                            self.client_configs.ui_layout_mode,
                            Some(&dht_wave_telemetry),
                            frame_started_at,
                        )
                    } else {
                        false
                    };
                    let should_draw = Self::should_draw_this_frame(
                        &self.app_state.mode,
                        self.app_state.ui.needs_redraw,
                        normal_animation_active,
                    );
                    if should_draw {
                        self.app_state.ui.record_drawn_frame(frame_started_at);
                        self.tick_ui_effects_clock();
                        let dht_status = self.dht_service.current_status();
                        let dht_wave_telemetry = self.dht_service.current_wave_telemetry();
                        let draw_started_at = Instant::now();
                        terminal.draw(|f| {
                            draw(
                                f,
                                &self.app_state,
                                &dht_status,
                                &dht_wave_telemetry,
                                &self.client_configs,
                            );
                        })?;
                        self.app_state.ui.record_draw_duration(
                            draw_started_at.elapsed(),
                            current_target_framerate,
                        );
                        self.app_state.ui.needs_redraw = false;
                    } else if matches!(self.app_state.mode, AppMode::Normal) {
                        next_draw_time = frame_started_at
                            + Self::normal_idle_frame_check_interval(current_target_framerate);
                    }
                }
            }
        }

        self.save_state_to_disk();

        self.shutdown_sequence(terminal).await;
        self.flush_shared_recovery_backup_worker().await;
        self.flush_persistence_writer().await;

        Ok(())
    }

    fn should_draw_this_frame(
        mode: &AppMode,
        ui_needs_redraw: bool,
        normal_animation_active: bool,
    ) -> bool {
        match mode {
            AppMode::PowerSaving => ui_needs_redraw,
            // The one-second stats tick dirties these time-based screens often enough to
            // refresh live ages and restriction countdowns without continuously redrawing.
            AppMode::Journal | AppMode::PeerManagement => ui_needs_redraw,
            AppMode::Normal => ui_needs_redraw || normal_animation_active,
            _ => true,
        }
    }

    fn normal_mode_animation_active(
        app_state: &AppState,
        layout_mode: UiLayoutMode,
        dht_wave_telemetry: Option<&DhtWaveTelemetry>,
        now: Instant,
    ) -> bool {
        if app_state.theme.effects.enabled() {
            return true;
        }

        if Self::disk_health_has_current_signal(app_state) {
            return true;
        }

        if Self::dht_wave_animation_active(&app_state.ui.dht_wave, dht_wave_telemetry) {
            return true;
        }

        if app_state.ui.swarm_availability_flash.has_active_flash(now) {
            return true;
        }

        app_state
            .torrent_list_order
            .get(app_state.ui.selected_torrent_index)
            .and_then(|info_hash| app_state.torrents.get(info_hash))
            .is_some_and(|torrent| {
                Self::selected_torrent_animation_active(torrent, now)
                    || (app_state.ui.visualization_focus.peer_stream
                        != PeerStreamVisualization::Classic
                        && (torrent.latest_state.number_of_successfully_connected_peers > 0
                            || !torrent.latest_state.peers.is_empty())
                        && Self::peer_stream_panel_visible(app_state, layout_mode))
            })
    }

    fn peer_stream_panel_visible(app_state: &AppState, layout_mode: UiLayoutMode) -> bool {
        let layout_ctx = LayoutContext::new(
            app_state.screen_area,
            app_state,
            layout_mode,
            DEFAULT_SIDEBAR_PERCENT,
        );
        calculate_layout(app_state.screen_area, &layout_ctx)
            .peer_stream
            .is_some_and(|area| {
                area.width >= PEER_STREAM_MIN_WIDTH && area.height >= PEER_STREAM_MIN_HEIGHT
            })
    }

    fn disk_health_has_current_signal(app_state: &AppState) -> bool {
        app_state.avg_disk_read_bps > 0
            || app_state.avg_disk_write_bps > 0
            || app_state.read_iops > 0
            || app_state.write_iops > 0
            || app_state.max_disk_backoff_this_tick_ms > 0
    }

    fn disk_health_phase_speed(app_state: &AppState) -> f64 {
        match app_state.ui.visualization_focus.disk_health {
            DiskHealthVisualization::Classic => Self::classic_disk_health_phase_speed(app_state),
            DiskHealthVisualization::SeekPendulum | DiskHealthVisualization::StorageDial => {
                Self::alternate_disk_health_phase_speed(app_state)
            }
        }
    }

    fn classic_disk_health_phase_speed(app_state: &AppState) -> f64 {
        let download_bps = app_state.avg_download_history.last().copied().unwrap_or(0) as f64;
        let upload_bps = app_state.avg_upload_history.last().copied().unwrap_or(0) as f64;
        let total_bps = download_bps + upload_bps;

        if total_bps <= 0.0 {
            return DISK_IDLE_WOBBLE_PHASE_SPEED;
        }

        let transfer_signal = (total_bps / 50_000_000.0).clamp(0.0, 1.0).sqrt();
        let balance = ((download_bps - upload_bps) / total_bps).clamp(-1.0, 1.0);
        let direction = if balance < -0.05 { -1.0 } else { 1.0 };
        let dominance = balance.abs();
        let disk_pressure = app_state
            .disk_health_ema
            .max(app_state.disk_health_peak_hold)
            .clamp(0.0, 1.0);
        let speed = (DISK_MIN_TRANSFER_PHASE_SPEED
            + 1.60 * transfer_signal
            + 1.40 * dominance
            + 1.40 * disk_pressure)
            .min(DISK_MAX_TRANSFER_PHASE_SPEED);

        direction * speed
    }

    fn alternate_disk_health_phase_speed(app_state: &AppState) -> f64 {
        let disk_bps = app_state
            .avg_disk_read_bps
            .saturating_add(app_state.avg_disk_write_bps) as f64;
        let rate_signal = disk_bps / (disk_bps + DISK_PHASE_RATE_MIDPOINT_BPS);

        rate_signal * DISK_MAX_TRANSFER_PHASE_SPEED
    }

    fn dht_wave_animation_active(
        wave: &DhtWaveUiState,
        telemetry: Option<&DhtWaveTelemetry>,
    ) -> bool {
        if telemetry.is_some_and(|telemetry| {
            telemetry.active_lookups > 0
                || telemetry.active_user_lookups > 0
                || telemetry.inflight_ipv4_queries > 0
                || telemetry.inflight_ipv6_queries > 0
                || telemetry.unique_peers_found_last_10s > 0
        }) {
            return true;
        }

        wave.query_load > 0.01
            || wave.discovery_boost > 0.01
            || wave.query_surge > 0.01
            || (wave.phase_speed > 0.05
                && (wave.amplitude > 0.02 || wave.harmonic_amplitude > 0.01))
    }

    fn selected_torrent_animation_active(torrent: &TorrentDisplayState, now: Instant) -> bool {
        if torrent.smoothed_download_speed_bps > 0
            || torrent.smoothed_upload_speed_bps > 0
            || torrent.disk_read_speed_bps > 0
            || torrent.disk_write_speed_bps > 0
            || torrent.peers_discovered_this_tick > 0
            || torrent.peers_connected_this_tick > 0
            || torrent.peers_disconnected_this_tick > 0
        {
            return true;
        }

        let metrics = &torrent.latest_state;
        if metrics.blocks_in_this_tick > 0
            || metrics.blocks_out_this_tick > 0
            || metrics
                .blocks_in_history
                .iter()
                .rev()
                .take(NORMAL_ANIMATION_RECENT_BLOCK_ROWS)
                .any(|&blocks| blocks > 0)
            || metrics
                .blocks_out_history
                .iter()
                .rev()
                .take(NORMAL_ANIMATION_RECENT_BLOCK_ROWS)
                .any(|&blocks| blocks > 0)
        {
            return true;
        }

        if torrent
            .peer_discovery_history
            .iter()
            .chain(torrent.peer_connection_history.iter())
            .chain(torrent.peer_disconnect_history.iter())
            .rev()
            .take(NORMAL_ANIMATION_RECENT_PEER_EVENTS)
            .any(|&events| events > 0)
        {
            return true;
        }

        torrent.recent_file_activity.values().any(|activity| {
            [activity.download_at, activity.upload_at]
                .into_iter()
                .flatten()
                .any(|seen_at| {
                    now.saturating_duration_since(seen_at) <= NORMAL_ANIMATION_FILE_ACTIVITY_WINDOW
                })
        })
    }

    fn normal_idle_frame_check_interval(target_frame_interval: Duration) -> Duration {
        target_frame_interval.max(NORMAL_IDLE_FRAME_CHECK_INTERVAL)
    }

    fn advance_next_draw_time(
        next_draw_time: &mut Instant,
        frame_started_at: Instant,
        target_frame_interval: Duration,
    ) {
        *next_draw_time += target_frame_interval;
        while *next_draw_time <= frame_started_at {
            *next_draw_time += target_frame_interval;
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
        let dht_status = self.dht_service.current_status();
        let dht_wave_telemetry = self.dht_service.current_wave_telemetry();
        let target_wave = dht_wave_targets(&dht_status, &dht_wave_telemetry);
        let target_discovery_boost = selected_torrent
            .map(|torrent| {
                (torrent.peers_discovered_this_tick as f64 / 10.0).clamp(0.0, 1.0) * 0.18
            })
            .unwrap_or_default();
        let wave = &mut self.app_state.ui.dht_wave;
        advance_dht_wave_state(wave, target_wave, target_discovery_boost, frame_dt);
        let download_steps_per_second = selected_torrent
            .map(|torrent| file_activity_wave_steps_per_second(torrent.smoothed_download_speed_bps))
            .unwrap_or_else(|| file_activity_wave_steps_per_second(0));
        let upload_steps_per_second = selected_torrent
            .map(|torrent| file_activity_wave_steps_per_second(torrent.smoothed_upload_speed_bps))
            .unwrap_or_else(|| file_activity_wave_steps_per_second(0));
        self.app_state.ui.file_activity_download_phase += frame_dt * download_steps_per_second;
        self.app_state.ui.file_activity_upload_phase += frame_dt * upload_steps_per_second;
        self.update_swarm_availability_flash(now);

        let disk_phase_speed = Self::disk_health_phase_speed(&self.app_state);
        self.app_state.disk_health_phase = (self.app_state.disk_health_phase
            + frame_dt * disk_phase_speed)
            .rem_euclid(std::f64::consts::TAU);
    }

    fn update_swarm_availability_flash(&mut self, now: Instant) {
        let selected = self
            .app_state
            .torrent_list_order
            .get(self.app_state.ui.selected_torrent_index)
            .and_then(|info_hash| {
                self.app_state.torrents.get(info_hash).map(|torrent| {
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
            self.app_state.ui.swarm_availability_flash = SwarmAvailabilityFlashState::default();
            return;
        };

        self.app_state
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

    fn refresh_system_warning(&mut self) {
        let dht_warning = self.dht_service.current_warning();
        let base_and_network = compose_system_warning(
            self.base_system_warning.as_deref(),
            self.network_warning.as_deref(),
        );
        self.app_state.system_warning =
            compose_system_warning(base_and_network.as_deref(), dht_warning.as_deref());
    }

    async fn handle_network_state_changed(&mut self) {
        let state = self.network_state_rx.borrow().clone();
        self.app_state.network_runtime_status =
            Some(state.runtime_status(&self.client_configs.network_binding));
        let valid_generation_id = self
            .network_activation
            .try_active()
            .ok()
            .map(|active| active.scope().id().generation_id());
        if matches!(&state, NetworkState::Ready(generation) if valid_generation_id == Some(generation.id()))
        {
            return;
        }
        let previous_generation_id = self
            .network_activation_publisher
            .active_scope_id()
            .map(NetworkScopeId::generation_id);
        self.publish_network_activation_pending(state.generation_id());
        match state {
            NetworkState::Blocked(reason) => {
                if let Some(listener) = self.listener.take() {
                    listener.shutdown().await;
                }
                self.clear_network_generation_reachability();
                let mut suspended_dht = build_app_dht_service_config(&self.client_configs);
                suspended_dht.preferred_backend = crate::dht_service::DhtBackendKind::Disabled;
                if let Some(previous_generation_id) = previous_generation_id {
                    if let Err(error) = self.dht_service.reconfigure_and_wait(suspended_dht).await {
                        tracing_event!(
                            Level::WARN,
                            %error,
                            "DHT teardown did not acknowledge blocked network generation"
                        );
                    }
                    crate::networking::utp::shutdown_udp_generation(previous_generation_id).await;
                }
                self.block_network_activation(reason.to_string());
                self.network_warning = Some(format!("Networking blocked: {reason}"));
                tracing_event!(Level::WARN, %reason, "network generation blocked");
                self.refresh_system_warning();
                self.app_state.ui.needs_redraw = true;
            }
            NetworkState::Ready(generation) => {
                if let Some(listener) = self.listener.take() {
                    listener.shutdown().await;
                }
                self.clear_network_generation_reachability();
                if let Some(previous_generation_id) = previous_generation_id {
                    let mut suspended_dht = build_app_dht_service_config(&self.client_configs);
                    suspended_dht.preferred_backend = crate::dht_service::DhtBackendKind::Disabled;
                    let dht_teardown = self.dht_service.reconfigure_and_wait(suspended_dht).await;
                    crate::networking::utp::shutdown_udp_generation(previous_generation_id).await;
                    if let Err(error) = dht_teardown {
                        self.block_network_activation(format!(
                            "old DHT transport did not stop before rebind: {error}"
                        ));
                        self.network_warning = Some(format!(
                            "Networking blocked: old DHT transport did not stop before rebind: {error}"
                        ));
                        let _ = self
                            .network_handle
                            .block_generation(
                                generation.id(),
                                format!("old DHT transport did not stop before rebind: {error}"),
                            )
                            .await;
                        self.refresh_system_warning();
                        self.app_state.ui.needs_redraw = true;
                        return;
                    }
                }
                let requested_port = requested_listener_port(&self.client_configs);
                let lease = match self.network_handle.try_lease_generation(generation.id()) {
                    Ok(lease) => lease,
                    Err(error) => {
                        self.block_network_activation(error.to_string());
                        return;
                    }
                };
                let scope = match self.prepare_network_activation(lease) {
                    Ok(scope) => scope,
                    Err(error) => {
                        self.block_network_activation(error.to_string());
                        return;
                    }
                };
                match bind_peer_listener_with_lease(scope.lease(), requested_port).await {
                    Ok(listener) => {
                        if requested_port == 0 {
                            if let Some(bound_port) =
                                listener.as_ref().and_then(ListenerSet::local_port)
                            {
                                self.client_configs.client_port = bound_port;
                            }
                        }
                        self.listener = listener;
                        self.network_warning =
                            network_policy_warning(&self.client_configs.network_binding);
                        let bound_port = self
                            .listener
                            .as_ref()
                            .and_then(ListenerSet::local_port)
                            .unwrap_or(self.client_configs.client_port);
                        if let Err(error) = self.activate_network_scope(scope, bound_port) {
                            self.block_network_activation(error.to_string());
                            return;
                        }
                        if let Err(error) = self
                            .dht_service
                            .reconfigure_and_wait(build_app_dht_service_config(
                                &self.client_configs,
                            ))
                            .await
                        {
                            tracing_event!(
                                Level::WARN,
                                %error,
                                "DHT did not acknowledge the replacement network generation"
                            );
                        }
                        tracing_event!(
                            Level::INFO,
                            generation_id = generation.id(),
                            config_epoch = generation.config_epoch(),
                            "network generation activated"
                        );
                    }
                    Err(error) => {
                        let retry_binding = listener_bind_error_is_transient(&error);
                        self.block_network_activation(format!(
                            "replacement listener preflight failed: {error}"
                        ));
                        self.network_warning = Some(format!(
                            "Networking blocked: replacement listener preflight failed: {error}"
                        ));
                        let _ = self
                            .network_handle
                            .block_generation_with_retry(
                                generation.id(),
                                format!("replacement listener preflight failed: {error}"),
                                retry_binding,
                            )
                            .await;
                    }
                }
                self.refresh_system_warning();
                self.app_state.ui.needs_redraw = true;
            }
        }
    }

    fn clear_network_generation_reachability(&mut self) {
        self.app_state.externally_accessable_port_v4 = false;
        self.app_state.externally_accessable_port_v6 = false;
        self.app_state.inbound_peer_transports = InboundPeerTransportStatus::default();
        self.app_state.externally_accessable_port_v4_highlight_until = None;
        self.app_state.externally_accessable_port_v6_highlight_until = None;
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
                        tokio::select! {
                            send_result = tui_event_tx_clone.send(e) => {
                                if send_result.is_err() {
                                    break;
                                }
                            }
                            _ = tui_shutdown_rx.recv() => break,
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
        self.persist_event_journal();
        flush_persistence_writer_parts(&mut self.persistence_tx, &mut self.persistence_task).await;
        self.event_journal_persistence_tx = None;
        if let Some(handle) = self.event_journal_persistence_task.take() {
            if let Err(error) = handle.await {
                tracing_event!(Level::ERROR, %error, "Error joining event journal persistence task");
            }
        }
    }

    async fn flush_shared_recovery_backup_worker(&mut self) {
        self.shared_recovery_backup_tx = None;
        if let Some(handle) = self.shared_recovery_backup_task.take() {
            if let Err(error) = handle.await {
                tracing_event!(Level::ERROR, %error, "Error joining recovery backup worker");
            }
        }
    }

    async fn shutdown_sequence<B: Backend>(&mut self, terminal: &mut Terminal<B>) {
        // Stop producers that feed the app loop before it stops draining their channels.
        // The peer manager has a dedicated signal so it can remain alive for the final
        // torrent-manager metrics flush below.
        let _ = self.shutdown_tx.send(());

        if let Some(handle) = self.tui_task.take() {
            tracing::info!("Waiting for TUI event listener to finish...");
            if let Err(error) = handle.await {
                tracing::error!(%error, "Error joining TUI task");
            }
        }

        let total_managers_to_shut_down = self.torrent_manager_command_txs.len();
        let mut managers_shut_down = 0;

        for manager_tx in self.torrent_manager_command_txs.values() {
            let _ = manager_tx.try_send(ManagerCommand::Shutdown);
        }

        if total_managers_to_shut_down > 0 {
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
                let dht_status = self.dht_service.current_status();
                let dht_wave_telemetry = self.dht_service.current_wave_telemetry();
                let _ = terminal.draw(|f| {
                    draw(
                        f,
                        &self.app_state,
                        &dht_status,
                        &dht_wave_telemetry,
                        &self.client_configs,
                    );
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

        if !self.peer_manager.handle().flush().await {
            tracing::warn!("Peer manager stopped before final torrent metrics were flushed");
        }

        let _ = self.peer_manager_shutdown_tx.send(());
        self.peer_manager.wait_for_shutdown().await;
    }

    fn route_incoming_peer_handshake(&mut self, incoming: IncomingPeerHandshake) {
        let IncomingPeerHandshake {
            connection,
            buffer,
            permit,
        } = incoming;
        if buffer.len() < 48 {
            return;
        }

        let peer_addr = connection.remote_addr;
        let transport = connection.endpoint.kind;
        let network_scope_id = connection.network_scope_id();
        let peer_info_hash = buffer[28..48].to_vec();
        let peer_info_hash_hex = hex::encode(&peer_info_hash);

        if self
            .peer_policy_rx
            .borrow()
            .blocks_ip(peer_addr.ip(), SystemTime::now())
        {
            tracing::trace!(
                peer_ip = %peer_addr,
                "Rejected inbound connection from blocked peer"
            );
            return;
        }

        let Some(torrent_manager_tx) = self.torrent_manager_incoming_peer_txs.get(&peer_info_hash)
        else {
            tracing::trace!(
                "ROUTING FAIL: No manager registered for hash: {}",
                peer_info_hash_hex
            );
            return;
        };

        let torrent_manager_tx = torrent_manager_tx.clone();
        let app_command_tx = self.app_command_tx.clone();
        tokio::spawn(async move {
            let mut network_invalidation_rx = connection.subscribe_network_invalidation();
            let send_result = tokio::select! {
                biased;
                _ = wait_for_peer_network_invalidation(&mut network_invalidation_rx) => return,
                result = torrent_manager_tx.send((connection, buffer, permit)) => result,
            };
            match send_result {
                Ok(()) => {
                    if let Some(scope_id) = network_scope_id {
                        let _ = app_command_tx.try_send(AppCommand::MarkPortOpen {
                            peer_addr,
                            transport,
                            scope_id,
                        });
                    }
                }
                Err(_) => {
                    tracing::trace!(
                        "ROUTING FAIL: Manager channel closed for hash: {}",
                        peer_info_hash_hex
                    );
                }
            }
        });
    }

    async fn handle_incoming_peer(&mut self, mut connection: PeerConnection) {
        if self
            .peer_policy_rx
            .borrow()
            .blocks_ip(connection.remote_addr.ip(), SystemTime::now())
        {
            tracing::trace!(
                peer_ip = %connection.remote_addr,
                "Rejected inbound connection from blocked peer before handshake"
            );
            return;
        }

        let resource_manager_clone = self.resource_manager.clone();
        let incoming_peer_handshake_tx = self.incoming_peer_handshake_tx.clone();
        let mut permit_shutdown_rx = self.shutdown_tx.subscribe();
        tokio::spawn(async move {
            let mut network_invalidation_rx = connection.subscribe_network_invalidation();
            let session_permit = tokio::select! {
                biased;
                _ = wait_for_peer_network_invalidation(&mut network_invalidation_rx) => None,
                permit_result = resource_manager_clone.acquire_peer_connection() => {
                    match permit_result {
                        Ok(permit) => Some(permit),
                        Err(ResourceManagerError::QueueFull) => {
                            tracing_event!(
                                Level::DEBUG,
                                peer_ip = %connection.remote_addr,
                                "Incoming peer dropped because peer permit capacity is saturated."
                            );
                            None
                        }
                        Err(ResourceManagerError::ManagerShutdown) => {
                            tracing_event!(Level::DEBUG, "Failed to acquire permit. Manager shut down?");
                            None
                        }
                    }
                }
                _ = permit_shutdown_rx.recv() => None
            };
            let Some(permit) = session_permit else {
                return;
            };
            let peer_addr = connection.remote_addr;
            let mut buffer = vec![0u8; 68];
            let read_ok = tokio::select! {
                biased;
                _ = wait_for_peer_network_invalidation(&mut network_invalidation_rx) => false,
                result = time::timeout(
                    Duration::from_secs(INCOMING_HANDSHAKE_TIMEOUT_SECS),
                    connection.stream.read_exact(&mut buffer)
                ) => matches!(result, Ok(Ok(_))),
            };
            if !read_ok {
                return;
            }

            if !is_valid_incoming_bittorrent_handshake(&buffer) {
                tracing::trace!(
                    "Rejected inbound TCP connection with invalid BitTorrent handshake."
                );
                return;
            }

            let incoming = IncomingPeerHandshake {
                connection,
                buffer,
                permit,
            };
            let send_result = tokio::select! {
                biased;
                _ = wait_for_peer_network_invalidation(&mut network_invalidation_rx) => return,
                result = incoming_peer_handshake_tx.send(incoming) => result,
            };
            if send_result.is_err() {
                tracing_event!(
                    Level::DEBUG,
                    peer_ip = %peer_addr,
                    "Incoming peer routing queue closed; dropping connection."
                );
            }
        });
    }

    fn refresh_rss_derived(&mut self) {
        crate::tui::screens::rss::recompute_rss_derived(&mut self.app_state, &self.client_configs);
    }

    fn refresh_peer_management_derived(&mut self, now: SystemTime) {
        crate::tui::screens::peers::recompute_peer_management_derived(&mut self.app_state, now);
    }

    pub(crate) fn refresh_peer_management_screen(&mut self) {
        sync_peer_policy_to_app_state(&mut self.app_state, &mut self.peer_policy_rx);
        sync_peer_manager_view_to_app_state(&mut self.app_state, &mut self.peer_manager_view_rx);
        self.refresh_peer_management_derived(SystemTime::now());
    }

    fn active_running_torrents_for_dht_announce(&self) -> Vec<Vec<u8>> {
        self.app_state
            .torrents
            .iter()
            .filter(|(info_hash, display)| {
                display.latest_state.torrent_control_state == TorrentControlState::Running
                    && display.latest_state.number_of_pieces_total > 0
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
            let should_announce = self
                .app_state
                .torrents
                .get(&info_hash)
                .is_some_and(|display| display.latest_state.number_of_pieces_total > 0);
            if !should_announce {
                continue;
            }
            let dht_handle = dht_handle.clone();
            tokio::spawn(async move {
                let _ = dht_handle.announce_peer(info_hash, Some(port)).await;
            });
        }
    }

    fn remove_torrent_runtime(&mut self, info_hash: &[u8]) {
        self.app_state.torrents.remove(info_hash);
        self.startup_completion_suppressed_hashes.remove(info_hash);
        self.torrent_manager_command_txs.remove(info_hash);
        self.torrent_manager_incoming_peer_txs.remove(info_hash);
        self.torrent_metric_watch_rxs.remove(info_hash);
        let _ = self
            .peer_manager
            .handle()
            .unregister_torrent(info_hash.to_vec());
        self.integrity_scheduler.remove_torrent(info_hash);
        self.app_state
            .torrent_list_order
            .retain(|candidate| candidate.as_slice() != info_hash);
        clamp_selected_indices_in_state(&mut self.app_state);
        self.refresh_rss_derived();
        self.dispatch_integrity_probe_batches();
    }

    pub(crate) fn cleanup_pending_magnet_preview_runtime(&mut self) {
        let Some(info_hash) = self.app_state.pending_magnet_preview_info_hash.take() else {
            return;
        };

        if let Some(manager_tx) = self.torrent_manager_command_txs.get(&info_hash).cloned() {
            let mut shutdown_rx = self.shutdown_tx.subscribe();
            tokio::spawn(async move {
                tokio::select! {
                    result = manager_tx.send(ManagerCommand::Shutdown) => {
                        if let Err(error) = result {
                            tracing::error!("Failed to send Shutdown to cancelled preview manager: {}", error);
                        }
                    }
                    shutdown = shutdown_rx.recv() => {
                        match shutdown {
                            Ok(())
                            | Err(broadcast::error::RecvError::Closed)
                            | Err(broadcast::error::RecvError::Lagged(_)) => {}
                        }
                    }
                }
            });
        }

        self.remove_torrent_runtime(&info_hash);
        self.save_state_to_disk();
        self.app_state.ui.needs_redraw = true;
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
            Level::DEBUG,
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
                torrent_config.torrent_control_state.clone(),
                torrent_config.file_priorities.clone(),
                torrent_config.container_name.clone(),
            )
            .await
        } else {
            self.add_torrent_from_file(
                PathBuf::from(&torrent_config.torrent_or_magnet),
                torrent_config.download_path.clone(),
                torrent_config.validation_status,
                torrent_config.torrent_control_state.clone(),
                torrent_config.file_priorities.clone(),
                torrent_config.container_name.clone(),
            )
            .await
        };

        let restored = matches!(
            ingest_result,
            CommandIngestResult::Added { .. } | CommandIngestResult::Duplicate { .. }
        );
        if restored {
            preserve_restored_added_at(&mut self.app_state, &torrent_config);
        }
        restored
    }

    async fn sync_runtime_torrents_from_settings(
        &mut self,
        old_settings: &Settings,
        new_settings: &Settings,
    ) {
        if !runtime_torrent_settings_changed(old_settings, new_settings) {
            return;
        }

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
                runtime.added_at_unix_secs = torrent.added_at_unix_secs;
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
                    self.send_manager_command_until_shutdown(manager_tx, ManagerCommand::Shutdown)
                        .await;
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
                        self.send_manager_command_until_shutdown(manager_tx, command)
                            .await;
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
                        self.send_manager_command_until_shutdown(
                            manager_tx,
                            ManagerCommand::SetUserTorrentConfig {
                                torrent_data_path,
                                file_priorities: torrent.file_priorities.clone(),
                                container_name: torrent.container_name.clone(),
                            },
                        )
                        .await;
                    }
                }
            }
        }

        for info_hash in old_by_hash.keys() {
            if new_by_hash.contains_key(info_hash) {
                continue;
            }

            if let Some(manager_tx) = self.torrent_manager_command_txs.get(info_hash) {
                self.send_manager_command_until_shutdown(manager_tx, ManagerCommand::Shutdown)
                    .await;
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

    async fn send_manager_command_until_shutdown(
        &self,
        manager_tx: &mpsc::Sender<ManagerCommand>,
        command: ManagerCommand,
    ) {
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        tokio::select! {
            result = manager_tx.send(command) => {
                if result.is_err() {
                    tracing_event!(Level::WARN, "Torrent manager command channel closed");
                }
            }
            shutdown = shutdown_rx.recv() => {
                match shutdown {
                    Ok(())
                    | Err(tokio::sync::broadcast::error::RecvError::Closed)
                    | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                }
            }
        }
    }

    async fn apply_reloaded_settings(&mut self, mut new_settings: Settings) {
        if self.persisted_network_binding_override.is_some() {
            self.persisted_network_binding_override = Some(new_settings.network_binding.clone());
            new_settings.network_binding = self.client_configs.network_binding.clone();
        }
        if new_settings != self.client_configs {
            self.apply_settings_update(new_settings, false).await;
        }
    }

    async fn apply_settings_update(&mut self, mut new_settings: Settings, persist: bool) {
        let old_settings = self.client_configs.clone();
        preserve_bound_random_client_port(&old_settings, &mut new_settings);
        let rss_changed = rss_settings_changed(&old_settings, &new_settings);
        let network_binding_changed = new_settings.network_binding != old_settings.network_binding;
        let mut config_error = None;

        if network_binding_changed {
            self.persisted_network_binding_override = None;
            tracing::info!("Config update: Network binding policy changed.");
            if self
                .network_handle
                .reconfigure(new_settings.network_binding.clone())
                .await
                .is_err()
            {
                config_error =
                    Some("Could not submit the updated network binding policy.".to_string());
            }
        }

        self.client_configs = new_settings.clone();
        let _ = self.rss_settings_tx.send(self.client_configs.clone());
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
        if new_settings.ui_refresh_rate != old_settings.ui_refresh_rate {
            self.app_state.data_rate = new_settings.ui_refresh_rate;
            for manager_tx in self.torrent_manager_command_txs.values() {
                let _ = manager_tx.try_send(ManagerCommand::SetDataRate(
                    new_settings.ui_refresh_rate.as_ms(),
                ));
            }
        }

        let port_changed = new_settings.randomize_client_port != old_settings.randomize_client_port
            || (!new_settings.randomize_client_port
                && new_settings.client_port != old_settings.client_port);
        let pinning_current_random_port = old_settings.randomize_client_port
            && !new_settings.randomize_client_port
            && self.listener.as_ref().and_then(ListenerSet::local_port)
                == Some(new_settings.client_port);
        let bootstrap_changed = new_settings.bootstrap_nodes != old_settings.bootstrap_nodes;

        if !network_binding_changed {
            if pinning_current_random_port {
                tracing::info!(
                    "Config update: Pinned current random listen port {} without rebinding",
                    new_settings.client_port
                );
            }

            if port_changed && !pinning_current_random_port {
                let requested_port = requested_listener_port(&new_settings);
                tracing::info!(
                    "Config update: Port changed to {}",
                    if requested_port == 0 {
                        "RANDOM".to_string()
                    } else {
                        requested_port.to_string()
                    }
                );
                if matches!(*self.network_state_rx.borrow(), NetworkState::Blocked(_)) {
                    tracing::info!(
                        "Deferred listen port {} until networking recovers.",
                        if requested_port == 0 {
                            "RANDOM".to_string()
                        } else {
                            requested_port.to_string()
                        }
                    );
                    if bootstrap_changed {
                        tracing::info!("Config update: DHT bootstrap nodes changed.");
                        self.dht_service
                            .reconfigure(DhtServiceConfig::from_settings(&self.client_configs));
                    }
                    if self.network_handle.retry_binding().await.is_err() {
                        config_error = Some(
                            "Could not retry networking with the updated listen port.".to_string(),
                        );
                    }
                } else if !self.rebind_listener(requested_port).await {
                    config_error = Some(format!(
                        "Could not activate listen port {}. Networking is blocked; configured port {} was restored.",
                        if requested_port == 0 {
                            "RANDOM".to_string()
                        } else {
                            requested_port.to_string()
                        },
                        old_settings.client_port
                    ));
                    self.client_configs.client_port = old_settings.client_port;
                    self.client_configs.randomize_client_port = old_settings.randomize_client_port;
                    let _ = self.rss_settings_tx.send(self.client_configs.clone());
                    if bootstrap_changed {
                        tracing::info!("Config update: DHT bootstrap nodes changed.");
                        self.dht_service
                            .reconfigure(DhtServiceConfig::from_settings(&self.client_configs));
                    }
                    if self.network_handle.retry_binding().await.is_err() {
                        config_error = Some(
                            "Could not retry networking with the restored listen port.".to_string(),
                        );
                    }
                }
            } else if bootstrap_changed {
                tracing::info!("Config update: DHT bootstrap nodes changed.");
                self.dht_service
                    .reconfigure(DhtServiceConfig::from_settings(&self.client_configs));
            }
        }

        if new_settings.global_download_limit_bps != old_settings.global_download_limit_bps {
            self.disk_write_download_throttle
                .reset(new_settings.global_download_limit_bps);
            self.app_state.effective_download_limit_bps = new_settings.global_download_limit_bps;
            self.global_dl_bucket
                .set_rate(configured_download_bucket_rate(
                    new_settings.global_download_limit_bps,
                ));
        }
        if new_settings.global_upload_limit_bps != old_settings.global_upload_limit_bps {
            self.global_ul_bucket
                .set_rate(configured_upload_bucket_rate(
                    new_settings.global_upload_limit_bps,
                ));
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

        self.app_state.system_error = config_error;
        self.app_state.ui.needs_redraw = true;
    }

    fn mark_peer_port_open(&mut self, peer_addr: SocketAddr, transport: PeerTransportKind) {
        let highlight_until = Some(Instant::now() + PORT_FAMILY_HIGHLIGHT_DURATION);
        let ipv4 = match peer_addr {
            SocketAddr::V4(_) => {
                self.app_state.externally_accessable_port_v4_highlight_until = highlight_until;
                true
            }
            SocketAddr::V6(addr) if addr.ip().to_ipv4_mapped().is_some() => {
                self.app_state.externally_accessable_port_v4_highlight_until = highlight_until;
                true
            }
            SocketAddr::V6(_) => {
                self.app_state.externally_accessable_port_v6_highlight_until = highlight_until;
                false
            }
        };
        self.app_state
            .inbound_peer_transports
            .mark_seen(transport, ipv4);
        let open_flag = if ipv4 {
            &mut self.app_state.externally_accessable_port_v4
        } else {
            &mut self.app_state.externally_accessable_port_v6
        };
        let just_opened = !*open_flag;
        if just_opened {
            *open_flag = true;
            let info_hashes = self.active_running_torrents_for_dht_announce();
            self.announce_torrents_to_dht(info_hashes);
        }
        self.app_state.ui.needs_redraw = true;
    }

    async fn handle_submit_control_request(
        &mut self,
        request: ControlRequest,
        pending_manual_ingest: Option<PendingManualIngest>,
    ) {
        let pending_manual_ingest = pending_manual_ingest.filter(|_| {
            matches!(
                &request,
                ControlRequest::AddTorrentFile { .. } | ControlRequest::AddMagnet { .. }
            )
        });

        match self
            .dispatch_cluster_control_request_with_ingest_result(request, ControlOrigin::CliOnline)
            .await
        {
            Ok((_message, ingest_result)) => {
                if let (Some(pending), Some(ingest_result)) = (pending_manual_ingest, ingest_result)
                {
                    self.archive_processed_ingest(pending.source, &pending.path);
                    self.record_ingest_result(&pending.path, &ingest_result);
                    self.save_state_to_disk();
                }
            }
            Err(error) => {
                self.app_state.system_error = Some(error);
                self.app_state.ui.needs_redraw = true;
            }
        }
    }

    pub(crate) async fn apply_config_update_from_ui(&mut self, settings: Settings) {
        self.handle_app_command(AppCommand::UpdateConfig(settings))
            .await;
    }

    pub(crate) fn refresh_config_network_interfaces(&mut self) {
        let inventory = &mut self.app_state.ui.config.network_interface_inventory;
        let request_id = inventory.begin_refresh();
        self.app_state.ui.needs_redraw = true;

        let app_command_tx = self.app_command_tx.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(|| {
                available_network_interfaces().map(|interfaces| {
                    interfaces
                        .into_iter()
                        .filter(NetworkInterfaceInfo::is_selectable)
                        .collect()
                })
            })
            .await
            .map_err(|error| format!("interface discovery task failed: {error}"))
            .and_then(|result| result.map_err(|error| error.to_string()));

            let _ = app_command_tx
                .send(AppCommand::ConfigNetworkInterfacesDiscovered { request_id, result })
                .await;
        });
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
            AppCommand::MarkPortOpen {
                peer_addr,
                transport,
                scope_id,
            } => {
                if self.network_activation.is_current(scope_id) {
                    self.mark_peer_port_open(peer_addr, transport);
                } else {
                    let active_scope_id = self
                        .network_activation
                        .try_active()
                        .ok()
                        .map(|active| active.scope().id());
                    tracing::trace!(
                        ?scope_id,
                        ?active_scope_id,
                        "ignoring reachability observed on an inactive network activation"
                    );
                }
            }
            AppCommand::SubmitControlRequest(request) => {
                self.handle_submit_control_request(request, None).await;
            }
            AppCommand::SubmitManualAddRequest {
                request,
                pending_ingest,
            } => {
                self.handle_submit_control_request(request, pending_ingest)
                    .await;
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
                browser_generation,
                path,
                browser_mode,
                preserve_browser_mode,
                highlight_path,
            } => {
                self.start_file_browser_fetch(
                    browser_generation,
                    path,
                    browser_mode,
                    preserve_browser_mode,
                    highlight_path,
                );
            }

            AppCommand::UpdateFileBrowserData {
                request_id,
                path,
                mut data,
                highlight_path,
            } => {
                if matches!(self.app_state.mode, AppMode::FileBrowser) {
                    if request_id != self.app_state.ui.file_browser.fetch_request_id
                        || path != self.app_state.ui.file_browser.state.current_path
                    {
                        tracing::debug!(
                            target: "superseedr",
                            request_id,
                            ?path,
                            current_request_id = self.app_state.ui.file_browser.fetch_request_id,
                            current_path = ?self.app_state.ui.file_browser.state.current_path,
                            "Ignoring stale file browser data"
                        );
                        return;
                    }

                    let screen_area = self.app_state.screen_area;
                    let pending_torrent_path = self.app_state.pending_torrent_path.is_some();
                    let pending_torrent_link = !self.app_state.pending_torrent_link.is_empty();
                    let file_browser = &mut self.app_state.ui.file_browser;
                    file_browser.fetch_pending = false;
                    file_browser.fetch_error = None;
                    // --- 1. Apply Dynamic Sorting ---
                    if let FileBrowserMode::File(extensions) = &file_browser.browser_mode {
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
                    file_browser.data = data;

                    // --- 3. Select and scroll within the rows the screen actually renders ---
                    reconcile_file_browser_cursor_after_fetch(
                        file_browser,
                        highlight_path,
                        screen_area,
                        pending_torrent_path,
                        pending_torrent_link,
                    );

                    self.app_state.ui.needs_redraw = true;
                    self.sync_torrent_file_preview();
                }
            }
            AppCommand::FileBrowserFetchFailed {
                request_id,
                path,
                message,
            } => {
                if matches!(self.app_state.mode, AppMode::FileBrowser)
                    && request_id == self.app_state.ui.file_browser.fetch_request_id
                    && path == self.app_state.ui.file_browser.state.current_path
                {
                    let file_browser = &mut self.app_state.ui.file_browser;
                    file_browser.fetch_pending = false;
                    file_browser.data.clear();
                    file_browser.state.cursor_path = None;
                    file_browser.state.top_most_offset = 0;
                    file_browser.torrent_file_preview = TorrentFilePreviewState::Idle;
                    file_browser.fetch_error = Some(message);
                    self.app_state.ui.needs_redraw = true;
                }
            }
            AppCommand::UpdateTorrentFilePreview {
                browser_generation,
                request_id,
                path,
                result,
            } => {
                let file_browser = &mut self.app_state.ui.file_browser;
                if matches!(self.app_state.mode, AppMode::FileBrowser)
                    && browser_generation == file_browser.browser_generation
                    && request_id == file_browser.torrent_preview_request_id
                    && file_browser.state.cursor_path.as_ref() == Some(&path)
                {
                    file_browser.torrent_file_preview = match result {
                        Ok(preview) => TorrentFilePreviewState::Ready { path, preview },
                        Err(message) => TorrentFilePreviewState::Error { path, message },
                    };
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
            AppCommand::ConfigNetworkInterfacesDiscovered { request_id, result } => {
                let inventory = &mut self.app_state.ui.config.network_interface_inventory;
                let error = result.as_ref().err().cloned();
                if inventory.finish_refresh(request_id, result) {
                    if let Some(error) = error {
                        tracing_event!(
                            Level::WARN,
                            %error,
                            "Config interface discovery failed"
                        );
                    }
                    self.app_state.ui.needs_redraw = true;
                }
            }
            AppCommand::RefreshConfigNetworkInterfaces => {
                self.refresh_config_network_interfaces();
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
            AppCommand::ReloadClusterState(_path) => {
                if self.is_current_shared_leader() {
                    return;
                }
                match crate::config::load_settings() {
                    Ok(new_settings) => {
                        self.apply_reloaded_settings(new_settings).await;
                    }
                    Err(error) => {
                        tracing_event!(
                            Level::ERROR,
                            "Failed to reload shared cluster state: {}",
                            error
                        );
                    }
                }
            }
            AppCommand::UpdateVersionAvailable(latest_version) => {
                self.app_state.update_available = Some(latest_version);
            }
        }
    }

    fn handle_manager_event(&mut self, event: ManagerEvent) {
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
                self.torrent_metric_watch_rxs.remove(&info_hash);
                let _ = self
                    .peer_manager
                    .handle()
                    .unregister_torrent(info_hash.clone());
                self.integrity_scheduler.remove_torrent(&info_hash);
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

                let should_log_fault = self
                    .data_availability_fault_log_cooldowns
                    .entry(info_hash.clone())
                    .or_default()
                    .should_log(Instant::now(), REPEATED_HEALTH_LOG_INTERVAL);
                if should_log_fault {
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
                        let should_log_available = self
                            .probe_available_log_cooldowns
                            .entry(info_hash.clone())
                            .or_default()
                            .should_log(Instant::now(), REPEATED_HEALTH_LOG_INTERVAL);
                        if should_log_available {
                            tracing_event!(
                                Level::INFO,
                                info_hash = %hex::encode(&info_hash),
                                torrent = %torrent_name,
                                saved_location = ?saved_location,
                                "Torrent probe found data available; awaiting manager metrics confirmation"
                            );
                        }
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

                let pending_torrent_link = self.app_state.pending_torrent_link.clone();
                if let FileBrowserMode::DownloadLocSelection {
                    target,
                    preview_tree,
                    preview_state,
                    container_name,
                    original_name_backup,
                    use_container,
                    ..
                } = &mut self.app_state.ui.file_browser.browser_mode
                {
                    let should_hydrate_active_browser = match target {
                        DownloadSelectionTarget::PendingAdd => {
                            let (v1_hash, v2_hash) = parse_hybrid_hashes(&pending_torrent_link);
                            v1_hash.as_deref() == Some(info_hash.as_slice())
                                || v2_hash.as_deref() == Some(info_hash.as_slice())
                        }
                        DownloadSelectionTarget::ExistingTorrent {
                            info_hash: target_hash,
                        } => target_hash.as_slice() == info_hash.as_slice(),
                    };

                    if !should_hydrate_active_browser {
                        return;
                    }

                    // 1. REDUNDANCY GUARD: Check if metadata was already processed
                    // If the tree is already populated, ignore subsequent peer metadata arrivals
                    if !preview_tree.is_empty() {
                        tracing::debug!(target: "superseedr", "Metadata already hydrated for {:?}, ignoring redundant peer update", hex::encode(&info_hash));
                        return;
                    }

                    // 2. Hydrate the tree structure
                    let file_list = torrent.file_list();
                    let has_multiple_files = file_list.len() > 1;
                    let hydrated_file_priorities = match target {
                        DownloadSelectionTarget::ExistingTorrent { .. } => file_priorities.clone(),
                        DownloadSelectionTarget::PendingAdd => HashMap::new(),
                    };
                    *preview_tree =
                        build_torrent_preview_tree(file_list, &hydrated_file_priorities);

                    // 3. Update Display Name and State
                    let info_hash_hex = hex::encode(&info_hash);
                    let name = format!("{} [{}]", torrent.info.name, &info_hash_hex);
                    *container_name = name.clone();
                    *original_name_backup = name;
                    *use_container = has_multiple_files;

                    // 4. INITIALIZE UI STATE: Set the initial cursor
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

    fn start_file_browser_fetch(
        &mut self,
        browser_generation: u64,
        path: PathBuf,
        browser_mode: FileBrowserMode,
        preserve_browser_mode: bool,
        highlight_path: Option<PathBuf>,
    ) {
        if browser_generation != self.app_state.ui.file_browser.browser_generation {
            tracing::debug!(
                target: "superseedr",
                browser_generation,
                current_browser_generation = self.app_state.ui.file_browser.browser_generation,
                ?path,
                "Ignoring stale file browser fetch"
            );
            return;
        }

        let tx = self.app_command_tx.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let path_clone = path.clone();
        let highlight_clone = highlight_path.clone();
        let request_id = self
            .app_state
            .ui
            .file_browser
            .fetch_request_id
            .wrapping_add(1);
        {
            let file_browser = &mut self.app_state.ui.file_browser;
            file_browser.fetch_request_id = request_id;
            file_browser.fetch_pending = true;
            file_browser.fetch_error = None;
            file_browser.torrent_preview_request_id =
                file_browser.torrent_preview_request_id.wrapping_add(1);
            file_browser.torrent_file_preview = TorrentFilePreviewState::Idle;
            if !preserve_browser_mode {
                file_browser.search_state = BrowserSearchState::Closed;
                file_browser.search_query.clear();
            }
        }

        if matches!(self.app_state.mode, AppMode::FileBrowser) {
            let file_browser = &mut self.app_state.ui.file_browser;
            file_browser.state.current_path = path.clone();
            file_browser.state.cursor_path = None;
            file_browser.state.top_most_offset = 0;
            file_browser.state.expanded_paths.clear();
            file_browser.state.selected_paths.clear();
            file_browser.data.clear();
            file_browser.browser_mode = if preserve_browser_mode {
                merge_file_browser_mode_for_fetch(&file_browser.browser_mode, browser_mode)
            } else {
                browser_mode
            };
        } else {
            let mut tree_state = crate::tui::tree::TreeViewState::new();
            tree_state.current_path = path.clone();
            self.app_state.ui.file_browser.state = tree_state;
            self.app_state.ui.file_browser.data = Vec::new();
            self.app_state.ui.file_browser.browser_mode = browser_mode;
            self.app_state.mode = AppMode::FileBrowser;
        }

        tokio::spawn(async move {
            tokio::select! {
                result = build_fs_tree(&path_clone, 0) => {
                    let command = match result {
                        Ok(nodes) => AppCommand::UpdateFileBrowserData {
                            request_id,
                            path: path_clone,
                            data: nodes,
                            highlight_path: highlight_clone,
                        },
                        Err(error) => AppCommand::FileBrowserFetchFailed {
                            request_id,
                            message: format_filesystem_path_error(
                                "Failed to open file browser directory",
                                &path_clone,
                                &error,
                            ),
                            path: path_clone,
                        },
                    };
                    let _ = tx.send(command).await;
                }
                _ = shutdown_rx.recv() => {
                    tracing::debug!("Aborting FileBrowser crawl due to shutdown");
                }
            }
        });
    }

    pub(crate) fn sync_torrent_file_preview(&mut self) {
        let selected_path = if matches!(self.app_state.mode, AppMode::FileBrowser)
            && matches!(
                &self.app_state.ui.file_browser.browser_mode,
                FileBrowserMode::File(_)
            )
            && !self.app_state.ui.file_browser.fetch_pending
        {
            self.app_state
                .ui
                .file_browser
                .state
                .cursor_path
                .as_ref()
                .filter(|path| {
                    self.app_state.ui.file_browser.data.iter().any(|node| {
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
            let file_browser = &mut self.app_state.ui.file_browser;
            if !matches!(
                file_browser.torrent_file_preview,
                TorrentFilePreviewState::Idle
            ) {
                file_browser.torrent_preview_request_id =
                    file_browser.torrent_preview_request_id.wrapping_add(1);
                file_browser.torrent_file_preview = TorrentFilePreviewState::Idle;
                self.app_state.ui.needs_redraw = true;
            }
            return;
        };

        if self.app_state.ui.file_browser.torrent_file_preview.path() == Some(path.as_path()) {
            return;
        }

        let file_browser = &mut self.app_state.ui.file_browser;
        file_browser.torrent_preview_request_id =
            file_browser.torrent_preview_request_id.wrapping_add(1);
        let request_id = file_browser.torrent_preview_request_id;
        let browser_generation = file_browser.browser_generation;
        file_browser.torrent_file_preview = TorrentFilePreviewState::Loading {
            path: path.clone(),
            request_id,
        };
        self.app_state.ui.needs_redraw = true;

        let tx = self.app_command_tx.clone();
        tokio::spawn(async move {
            let load_path = path.clone();
            let result =
                tokio::task::spawn_blocking(move || load_torrent_file_preview(load_path.as_path()))
                    .await
                    .map_err(|error| format!("Torrent preview task failed: {error}"))
                    .and_then(|result| result);
            let _ = tx
                .send(AppCommand::UpdateTorrentFilePreview {
                    browser_generation,
                    request_id,
                    path,
                    result,
                })
                .await;
        });
    }

    fn hydrate_pending_magnet_browser_from_display(&mut self, info_hash: &[u8]) {
        let Some(display) = self.app_state.torrents.get(info_hash) else {
            return;
        };
        if display.file_preview_tree.is_empty() {
            return;
        }

        let FileBrowserMode::DownloadLocSelection {
            target,
            preview_tree,
            preview_state,
            container_name,
            original_name_backup,
            use_container,
            ..
        } = &mut self.app_state.ui.file_browser.browser_mode
        else {
            return;
        };
        if !matches!(target, DownloadSelectionTarget::PendingAdd) || !preview_tree.is_empty() {
            return;
        }

        let info_hash_hex = hex::encode(info_hash);
        let name = format!("{} [{}]", display.latest_state.torrent_name, info_hash_hex);
        *container_name = name.clone();
        *original_name_backup = name;
        *use_container = display.latest_state.file_count.unwrap_or(1) > 1;
        *preview_tree = display.file_preview_tree.clone();
        if let Some(first) = preview_tree.first() {
            preview_state.cursor_path = Some(std::path::PathBuf::from(&first.name));
        }
        for node in preview_tree.iter_mut() {
            node.expand_all(preview_state);
        }
        self.app_state.ui.needs_redraw = true;
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

                    if matches!(*self.network_state_rx.borrow(), NetworkState::Blocked(_)) {
                        self.client_configs.client_port = new_port;
                        self.client_configs.randomize_client_port = false;
                        let _ = self.rss_settings_tx.send(self.client_configs.clone());
                        self.save_state_to_disk();
                        if self.network_handle.retry_binding().await.is_err() {
                            tracing_event!(
                                Level::ERROR,
                                "Could not retry networking with forwarded port {}.",
                                new_port
                            );
                        } else {
                            tracing_event!(
                                Level::INFO,
                                "Retrying networking with forwarded port {}.",
                                new_port
                            );
                        }
                    } else if self.rebind_listener(new_port).await {
                        self.client_configs.randomize_client_port = false;
                        self.save_state_to_disk();
                    } else {
                        tracing_event!(
                            Level::ERROR,
                            "Failed to bind to new port {}. Retaining old listener.",
                            new_port
                        );
                    }
                } else if new_port == self.client_configs.client_port {
                    if new_port > 0 && self.client_configs.randomize_client_port {
                        self.client_configs.randomize_client_port = false;
                        let _ = self.rss_settings_tx.send(self.client_configs.clone());
                        self.save_state_to_disk();
                        tracing_event!(
                            Level::INFO,
                            "Forwarded port {} matches the active listener; pinned it for future starts.",
                            new_port
                        );
                    } else {
                        tracing_event!(
                            Level::DEBUG,
                            "Port file updated, but port is unchanged ({}).",
                            new_port
                        );
                    }
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

    async fn calculate_stats(&mut self, sys: &mut System) {
        let was_seeding = self.app_state.is_seeding;
        let previous_torrent_sort = self.app_state.torrent_sort;
        let previous_peer_sort = self.app_state.peer_sort;
        UiTelemetry::on_second_tick(&mut self.app_state, sys);
        self.update_disk_backpressure_download_throttle();
        align_unpinned_sort_with_visible_activity(&mut self.app_state);
        if refresh_autosort_after_stats(
            &mut self.app_state,
            previous_torrent_sort,
            previous_peer_sort,
        ) {
            self.app_state.ui.needs_redraw = true;
        }
        NetworkHistoryTelemetry::on_second_tick(&mut self.app_state);
        self.tuning_controller.on_second_tick();
        self.app_state.tuning_countdown = self.tuning_controller.countdown_secs();
        self.update_wake_lag_peer_throttle();
        if was_seeding != self.app_state.is_seeding {
            self.reset_tuning_for_objective_change();
        }
        self.apply_effective_resource_limits().await;

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

    fn update_disk_backpressure_download_throttle(&mut self) {
        let sample = DiskBackpressureSample {
            is_leeching: !self.app_state.is_seeding,
            configured_download_limit_bps: self.client_configs.global_download_limit_bps,
            download_bps: self
                .app_state
                .avg_download_history
                .last()
                .copied()
                .unwrap_or(0),
            disk_write_completed_bps: self.app_state.avg_disk_write_completed_bps,
            recv_to_write_p95: self.app_state.recv_to_write_p95,
        };

        match self.disk_write_download_throttle.update(sample) {
            DiskBackpressureDecision::Disabled => {
                self.app_state.effective_download_limit_bps = effective_download_limit_bps(
                    self.client_configs.global_download_limit_bps,
                    None,
                );
                self.global_dl_bucket
                    .set_rate_preserving_tokens(configured_download_bucket_rate(
                        self.client_configs.global_download_limit_bps,
                    ));
            }
            DiskBackpressureDecision::Limited {
                rate_bytes_per_sec,
                capacity_bytes,
            } => {
                let adaptive_limit_bps = bytes_per_sec_to_bps(rate_bytes_per_sec);
                self.app_state.effective_download_limit_bps = effective_download_limit_bps(
                    self.client_configs.global_download_limit_bps,
                    Some(adaptive_limit_bps),
                );
                self.global_dl_bucket
                    .set_rate_with_capacity_preserving_tokens(rate_bytes_per_sec, capacity_bytes);
            }
        }
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
            let _ = self.peer_manager.handle().unregister_torrent(info_hash);
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

    fn total_successfully_connected_peers(&self) -> usize {
        self.app_state
            .torrents
            .values()
            .map(|torrent| torrent.latest_state.number_of_successfully_connected_peers)
            .sum()
    }

    fn sync_dht_peer_slot_usage(&mut self) {
        let total_peers = self.total_successfully_connected_peers();
        let max_connected_peers = self.effective_resource_limits().max_connected_peers;
        let usage = (total_peers, max_connected_peers);
        if self.last_dht_peer_slot_usage == Some(usage) {
            return;
        }

        self.last_dht_peer_slot_usage = Some(usage);
        self.dht_service
            .update_peer_slot_usage(total_peers, max_connected_peers);
    }

    fn handle_dht_status_changed(&mut self) {
        self.refresh_system_warning();
        // ResetDemandPlanner is followed by a DHT status publish; resend peer pressure
        // because the planner-side cap may have been reset while usage stayed unchanged.
        self.last_dht_peer_slot_usage = None;
        self.sync_dht_peer_slot_usage();
        self.app_state.ui.needs_redraw = true;
    }

    async fn tuning_resource_limits(&mut self) {
        if self.peer_admission_stress_active() {
            tracing_event!(
                Level::DEBUG,
                base_peer_limit = self.app_state.limits.max_connected_peers,
                effective_peer_limit = self.effective_resource_limits().max_connected_peers,
                "Self-Tune: paused while wake-lag peer throttle is active"
            );
            self.apply_effective_resource_limits().await;
            return;
        }

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

            self.apply_effective_resource_limits().await;
        }

        let (next_limits, desc) =
            make_random_adjustment(self.app_state.limits.clone(), self.app_state.is_seeding);
        self.app_state.limits = next_limits;

        tracing_event!(Level::DEBUG, "Self-Tune: Trying next change... {}", desc);
        self.apply_effective_resource_limits().await;
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

        let mut payload = build_persist_payload(
            &mut self.client_configs,
            &mut self.app_state,
            &self.startup_deferred_load_queue,
        );
        if let Some(network_binding) = &self.persisted_network_binding_override {
            payload.settings.network_binding = network_binding.clone();
        }
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
        self.client_configs
            .default_download_folder
            .clone()
            .or_else(|| self.find_most_common_download_path())
            .or_else(|| UserDirs::new().and_then(|ud| ud.download_dir().map(|p| p.to_path_buf())))
            .or_else(|| UserDirs::new().map(|ud| ud.home_dir().to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    fn initial_download_selection_pane(&self) -> BrowserPane {
        if self.client_configs.default_download_folder.is_some() {
            BrowserPane::TorrentPreview
        } else {
            BrowserPane::FileSystem
        }
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
                let should_apply_duplicate_config =
                    self.app_state.pending_magnet_preview_info_hash.as_deref()
                        == Some(info_hash.as_slice());
                let mut applied_duplicate_config = false;
                if should_apply_duplicate_config {
                    if let Some(path) = download_path {
                        applied_duplicate_config = true;
                        if let Some(display) = self.app_state.torrents.get_mut(&info_hash) {
                            display.latest_state.download_path = Some(path.clone());
                            display.latest_state.container_name = container_name.clone();
                            display.latest_state.file_priorities = file_priorities.clone();
                            apply_torrent_preview_file_priorities(
                                &mut display.file_preview_tree,
                                &file_priorities,
                            );
                        }
                        if let Some(manager_tx) = self.torrent_manager_command_txs.get(&info_hash) {
                            self.send_manager_command_until_shutdown(
                                manager_tx,
                                ManagerCommand::SetUserTorrentConfig {
                                    torrent_data_path: path,
                                    file_priorities: file_priorities.clone(),
                                    container_name,
                                },
                            )
                            .await;
                        }
                    }
                }
                let message = if applied_duplicate_config {
                    format!(
                        "Updated path for existing torrent from file: {}",
                        torrent.info.name
                    )
                } else {
                    format!("Ignoring already present torrent: {}", torrent.info.name)
                };
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
        let number_of_pieces_total = torrent_piece_count(&torrent);
        let added_at_unix_secs = current_unix_secs();

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
            added_at_unix_secs: Some(added_at_unix_secs),
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

        let (incoming_peer_tx, incoming_peer_rx) =
            mpsc::channel::<crate::torrent_manager::IncomingPeerSession>(100);
        self.torrent_manager_incoming_peer_txs
            .insert(info_hash.clone(), incoming_peer_tx);
        let (manager_command_tx, manager_command_rx) = mpsc::channel::<ManagerCommand>(100);
        self.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_command_tx);

        let (torrent_metrics_tx, torrent_metrics_rx) = watch::channel(TorrentMetrics::default());
        self.torrent_metric_watch_rxs
            .insert(info_hash.clone(), torrent_metrics_rx.clone());
        let manager_event_tx_clone = self.manager_event_tx.clone();
        let resource_manager_clone = self.resource_manager.clone();
        let global_dl_bucket_clone = self.global_dl_bucket.clone();
        let global_ul_bucket_clone = self.global_ul_bucket.clone();

        let dht_handle = self.dht_service.handle();

        let torrent_params = TorrentParameters {
            network_activation: self.network_activation.clone(),
            dht_handle,
            incoming_peer_rx,
            metrics_tx: torrent_metrics_tx,
            peer_policy_rx: self.peer_manager.handle().subscribe_policy(),
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
                if !self
                    .peer_manager
                    .handle()
                    .register_torrent(info_hash.clone(), torrent_metrics_rx)
                {
                    tracing_event!(
                        Level::WARN,
                        info_hash = %hex::encode(&info_hash),
                        "Peer manager was unavailable while registering torrent metrics"
                    );
                }
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
        self.persist_magnet_metadata_snapshot(
            &info_hash,
            &magnet_link,
            &resolved_torrent_name,
            &file_priorities,
        );

        if self.app_state.torrents.contains_key(&info_hash) {
            if !self.has_live_runtime_for_torrent(&info_hash) {
                self.clear_display_only_torrent(&info_hash);
            } else {
                if let Some(path) = download_path {
                    if let Some(display) = self.app_state.torrents.get_mut(&info_hash) {
                        display.latest_state.download_path = Some(path.clone());
                        display.latest_state.container_name = container_name.clone();
                        display.latest_state.file_priorities = file_priorities.clone();
                        apply_torrent_preview_file_priorities(
                            &mut display.file_preview_tree,
                            &file_priorities,
                        );
                    }
                    if let Some(manager_tx) = self.torrent_manager_command_txs.get(&info_hash) {
                        self.send_manager_command_until_shutdown(
                            manager_tx,
                            ManagerCommand::SetUserTorrentConfig {
                                torrent_data_path: path,
                                file_priorities: file_priorities.clone(),
                                container_name,
                            },
                        )
                        .await;
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
            added_at_unix_secs: Some(current_unix_secs()),
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

        let (incoming_peer_tx, incoming_peer_rx) =
            mpsc::channel::<crate::torrent_manager::IncomingPeerSession>(100);
        self.torrent_manager_incoming_peer_txs
            .insert(info_hash.clone(), incoming_peer_tx);
        let (manager_command_tx, manager_command_rx) = mpsc::channel::<ManagerCommand>(100);
        self.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_command_tx);

        let dht_handle = self.dht_service.handle();
        let (torrent_metrics_tx, torrent_metrics_rx) = watch::channel(TorrentMetrics::default());
        self.torrent_metric_watch_rxs
            .insert(info_hash.clone(), torrent_metrics_rx.clone());
        let manager_event_tx_clone = self.manager_event_tx.clone();
        let resource_manager_clone = self.resource_manager.clone();
        let global_dl_bucket_clone = self.global_dl_bucket.clone();
        let global_ul_bucket_clone = self.global_ul_bucket.clone();
        let torrent_params = TorrentParameters {
            network_activation: self.network_activation.clone(),
            dht_handle,
            incoming_peer_rx,
            metrics_tx: torrent_metrics_tx,
            peer_policy_rx: self.peer_manager.handle().subscribe_policy(),
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
                if !self
                    .peer_manager
                    .handle()
                    .register_torrent(info_hash.clone(), torrent_metrics_rx)
                {
                    tracing_event!(
                        Level::WARN,
                        info_hash = %hex::encode(&info_hash),
                        "Peer manager was unavailable while registering torrent metrics"
                    );
                }
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
        host_watch_paths(&self.client_configs)
            .iter()
            .any(|host_watch| watched_parent_matches(path, host_watch))
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
        self.persist_event_journal();
    }

    fn persist_event_journal(&self) {
        let Some(tx) = self.event_journal_persistence_tx.as_ref() else {
            return;
        };
        tx.send_replace(Some(EventJournalPersistRequest {
            state: self.app_state.event_journal_state.clone(),
            can_write_shared_state: self.can_write_shared_state(),
        }));
        if tx.is_closed() {
            tracing_event!(
                Level::ERROR,
                "Failed to queue event journal persistence: writer unavailable"
            );
        }
    }

    fn network_journal_interface(&self) -> Option<String> {
        self.app_state
            .network_runtime_status
            .as_ref()
            .and_then(|status| {
                status
                    .interface_display_name
                    .as_ref()
                    .or(status.interface.as_ref())
            })
            .cloned()
    }

    fn sync_network_activation_status_to_journal(&mut self) {
        let status = self.network_activation.status();
        if self.app_state.network_activation_status.as_ref() == Some(&status) {
            return;
        }

        self.record_network_activation_status_in_journal(status);
    }

    fn record_network_activation_status_in_journal(&mut self, status: NetworkActivationStatus) {
        let interface = self.network_journal_interface();
        let (event_type, generation_id, listen_port, message) = match &status {
            NetworkActivationStatus::Pending { generation_id } => (
                EventType::NetworkRebinding,
                *generation_id,
                None,
                interface.as_deref().map_or_else(
                    || "Rebinding network".to_string(),
                    |interface| format!("Rebinding network to {interface}"),
                ),
            ),
            NetworkActivationStatus::Blocked { reason } => {
                (EventType::NetworkBlocked, None, None, reason.to_string())
            }
            NetworkActivationStatus::Active {
                generation_id,
                listen_port,
            } => (
                EventType::NetworkRestored,
                Some(*generation_id),
                Some(*listen_port),
                interface.as_deref().map_or_else(
                    || format!("Network restored on port {listen_port}"),
                    |interface| {
                        format!("Network restored on {interface}, listening on port {listen_port}")
                    },
                ),
            ),
        };

        self.app_state.network_activation_status = Some(status);
        self.append_event_journal_entry(EventJournalEntry {
            host_id: self.event_journal_host_id.clone(),
            ts_iso: chrono::Utc::now().to_rfc3339(),
            category: EventCategory::Network,
            event_type,
            message: Some(message),
            details: EventDetails::Network {
                interface,
                generation_id,
                listen_port,
            },
            ..Default::default()
        });
        self.app_state.ui.needs_redraw = true;
    }

    fn publish_network_activation_pending(&mut self, generation_id: Option<u64>) {
        self.network_activation_publisher.pending(generation_id);
        self.sync_network_activation_status_to_journal();
    }

    fn prepare_network_activation(
        &mut self,
        lease: NetworkLease,
    ) -> Result<NetworkScope, crate::networking::runtime::NetworkLeaseError> {
        let result = self.network_activation_publisher.prepare(lease);
        self.sync_network_activation_status_to_journal();
        result
    }

    fn activate_network_scope(
        &mut self,
        scope: NetworkScope,
        listen_port: u16,
    ) -> Result<
        Arc<crate::networking::activation::ActiveNetwork>,
        crate::networking::runtime::NetworkLeaseError,
    > {
        let result = self
            .network_activation_publisher
            .activate_prepared(scope, listen_port);
        self.sync_network_activation_status_to_journal();
        result
    }

    fn block_network_activation(&mut self, reason: impl Into<Arc<str>>) {
        self.network_activation_publisher.block(reason);
        self.sync_network_activation_status_to_journal();
    }

    fn control_event_scope(&self) -> EventScope {
        if crate::config::is_shared_config_mode() {
            EventScope::Shared
        } else {
            EventScope::Host
        }
    }

    fn persist_torrent_metadata_snapshot(
        &mut self,
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

        if self
            .persisted_torrent_metadata_cache
            .get(info_hash)
            .is_some_and(|persisted| persisted == &entry)
        {
            return;
        }

        if let Err(error) = upsert_torrent_metadata(entry.clone()) {
            tracing_event!(
                Level::WARN,
                "Failed to persist torrent metadata snapshot: {}",
                error
            );
            return;
        }

        self.persisted_torrent_metadata_cache
            .insert(info_hash.to_vec(), entry);
    }

    fn persist_magnet_metadata_snapshot(
        &mut self,
        info_hash: &[u8],
        magnet_link: &str,
        torrent_name: &str,
        file_priorities: &HashMap<usize, FilePriority>,
    ) {
        let Some(length) = extract_magnet_exact_length(magnet_link) else {
            return;
        };
        let Some(file_name) = extract_magnet_display_name(magnet_link) else {
            return;
        };
        let entry = TorrentMetadataEntry {
            info_hash_hex: hex::encode(info_hash),
            torrent_name: torrent_name.to_string(),
            total_size: length,
            is_multi_file: false,
            files: vec![TorrentMetadataFileEntry {
                relative_path: normalize_magnet_metadata_path(&file_name),
                length,
            }],
            file_priorities: file_priorities.clone(),
        };

        if self
            .persisted_torrent_metadata_cache
            .get(info_hash)
            .is_some_and(|persisted| persisted == &entry)
        {
            return;
        }

        if let Err(error) = upsert_torrent_metadata(entry.clone()) {
            tracing_event!(
                Level::WARN,
                "Failed to persist magnet metadata snapshot: {}",
                error
            );
            return;
        }

        self.persisted_torrent_metadata_cache
            .insert(info_hash.to_vec(), entry);
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
                download_path: None,
                container_name: None,
                payload_path: None,
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
        let (download_path, container_name, payload_path) = info_hash_hex
            .as_deref()
            .and_then(|hash| hex::decode(hash).ok())
            .and_then(|info_hash| self.app_state.torrents.get(&info_hash))
            .map(|torrent| {
                (
                    torrent.latest_state.download_path.clone(),
                    torrent.latest_state.container_name.clone(),
                    Self::torrent_saved_location(&torrent.latest_state),
                )
            })
            .unwrap_or_default();

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
                download_path,
                container_name,
                payload_path,
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
        self.apply_control_request_with_ingest_result(request)
            .await
            .map(|(message, _)| message)
    }

    async fn apply_control_request_with_ingest_result(
        &mut self,
        request: &ControlRequest,
    ) -> Result<(String, Option<CommandIngestResult>), String> {
        validate_runtime_control_request(request)?;

        match plan_control_request(&self.client_configs, request)? {
            ControlExecutionPlan::StatusNow => {
                self.trigger_status_dump_now();
                Ok(("Wrote fresh status snapshot".to_string(), None))
            }
            ControlExecutionPlan::StatusFollowStart { interval_secs } => {
                self.set_runtime_status_dump_interval_override(Some(interval_secs));
                self.trigger_status_dump_now();
                Ok((
                    format!(
                        "Enabled runtime status dumps every {} seconds",
                        interval_secs
                    ),
                    None,
                ))
            }
            ControlExecutionPlan::StatusFollowStop => {
                self.set_runtime_status_dump_interval_override(Some(0));
                Ok(("Stopped runtime status dumps".to_string(), None))
            }
            ControlExecutionPlan::ApplySettings {
                next_settings,
                success_message,
            } => {
                self.apply_settings_update(next_settings, true).await;
                self.trigger_status_dump_after_successful_cluster_mutation();
                Ok((success_message, None))
            }
            ControlExecutionPlan::AddTorrentFile {
                source_path,
                download_path,
                container_name,
                validation_status,
                file_priorities,
            } => {
                let has_applied_download_path = download_path.is_some();
                let ingest_result = self
                    .add_torrent_from_file(
                        source_path.clone(),
                        download_path,
                        validation_status,
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
                    if has_applied_download_path {
                        self.clear_pending_magnet_preview_if_applied(&ingest_result);
                    }
                    self.save_state_to_disk();
                    self.trigger_status_dump_after_successful_cluster_mutation();
                }
                let response = Self::map_add_result_to_control_response(ingest_result.clone())?;
                Ok((response, Some(ingest_result)))
            }
            ControlExecutionPlan::AddMagnet {
                magnet_link,
                download_path,
                container_name,
                validation_status,
                file_priorities,
            } => {
                let has_applied_download_path = download_path.is_some();
                let ingest_result = self
                    .add_magnet_torrent(
                        "Fetching name...".to_string(),
                        magnet_link,
                        download_path,
                        validation_status,
                        TorrentControlState::Running,
                        file_priorities,
                        container_name,
                    )
                    .await;
                if matches!(
                    ingest_result,
                    CommandIngestResult::Added { .. } | CommandIngestResult::Duplicate { .. }
                ) {
                    if has_applied_download_path {
                        self.clear_pending_magnet_preview_if_applied(&ingest_result);
                    }
                    self.save_state_to_disk();
                    self.trigger_status_dump_after_successful_cluster_mutation();
                }
                let response = Self::map_add_result_to_control_response(ingest_result.clone())?;
                Ok((response, Some(ingest_result)))
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
        self.rebind_listener_with_dht_timeout(new_port, PORT_REBIND_DHT_TIMEOUT)
            .await
    }

    async fn rebind_listener_with_dht_timeout(
        &mut self,
        new_port: u16,
        dht_timeout: Duration,
    ) -> bool {
        let previous_bound_port = self.listener.as_ref().and_then(ListenerSet::local_port);
        let active_network = match self.network_activation.try_active() {
            Ok(active) => active,
            Err(error) => {
                tracing_event!(Level::WARN, %error, "Cannot rebind listener without an active network scope");
                return false;
            }
        };
        let generation_id = active_network.scope().id().generation_id();
        let lease = match self.network_handle.try_lease_generation(generation_id) {
            Ok(lease) => lease,
            Err(error) => {
                self.block_network_activation(error.to_string());
                return false;
            }
        };
        let scope = match self.prepare_network_activation(lease) {
            Ok(scope) => scope,
            Err(error) => {
                self.block_network_activation(error.to_string());
                return false;
            }
        };
        if let Some(old_listener) = self.listener.take() {
            old_listener.shutdown().await;
        }
        if let Some(old_port) = previous_bound_port {
            crate::networking::utp::shutdown_udp_port(generation_id, old_port).await;
        }
        let first_attempt = bind_peer_listener_with_lease(scope.lease(), new_port).await;
        let bind_result = match first_attempt {
            Err(_) => {
                // A listener driver aborted by its owner is released on the next
                // Tokio scheduling turn. Give that cleanup one bounded chance
                // before treating the requested port as externally occupied.
                tokio::task::yield_now().await;
                bind_peer_listener_with_lease(scope.lease(), new_port).await
            }
            result => result,
        };
        match bind_result {
            Ok(new_listener) => {
                self.listener = new_listener;
                // Note: client_configs.client_port is likely already updated by the caller (UpdateConfig)
                // but we ensure consistency here just in case.
                let bound_port = self
                    .listener
                    .as_ref()
                    .and_then(ListenerSet::local_port)
                    .unwrap_or(new_port);
                self.client_configs.client_port = bound_port;
                let bound_port_changed = previous_bound_port != Some(bound_port);

                if let Err(error) = self.activate_network_scope(scope, bound_port) {
                    self.block_network_activation(error.to_string());
                    return false;
                }

                tracing_event!(
                    Level::INFO,
                    "Successfully rebound listener to port {}",
                    bound_port
                );

                // Listener bind is the success criterion: a failed or wedged DHT
                // must not roll back the replacement listener or port persistence.
                match time::timeout(
                    dht_timeout,
                    self.dht_service
                        .reconfigure_and_wait(DhtServiceConfig::from_settings(&self.client_configs)),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing_event!(
                        Level::WARN,
                        %error,
                        "DHT rejected the replacement listen port; TCP/uTP remain on the replacement port"
                    ),
                    Err(_) => tracing_event!(
                        Level::WARN,
                        timeout_ms = dht_timeout.as_millis(),
                        "DHT timed out on the replacement listen port; TCP/uTP remain on the replacement port"
                    ),
                }

                if self.app_state.externally_accessable_port_v4
                    || self.app_state.externally_accessable_port_v6
                {
                    let info_hashes = self.active_running_torrents_for_dht_announce();
                    self.announce_torrents_to_dht(info_hashes);
                }

                if bound_port_changed {
                    self.app_state.externally_accessable_port_v4 = false;
                    self.app_state.externally_accessable_port_v6 = false;
                    self.app_state.externally_accessable_port_v4_highlight_until = None;
                    self.app_state.externally_accessable_port_v6_highlight_until = None;
                    self.app_state.inbound_peer_transports = InboundPeerTransportStatus::default();
                }

                true
            }
            Err(e) => {
                let retry_binding = listener_bind_error_is_transient(&e);
                self.block_network_activation(format!(
                    "replacement listener preflight failed: {e}"
                ));
                let _ = self
                    .network_handle
                    .block_generation_with_retry(
                        generation_id,
                        format!("replacement listener preflight failed: {e}"),
                        retry_binding,
                    )
                    .await;
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
        let active_network = match self.network_activation.try_active() {
            Ok(active) => active,
            Err(error) => {
                tracing_event!(Level::WARN, %error, "RSS manual download deferred while networking is unavailable");
                return (false, None, None);
            }
        };
        let network_scope = active_network.scope().clone();

        let client = match network_scope.lease().rss_http_client() {
            Ok(client) => client,
            Err(error) => {
                tracing_event!(Level::WARN, %error, "RSS manual download deferred because its HTTP client is unavailable");
                return (false, None, None);
            }
        };

        let request = match client.get(url) {
            Ok(request) => request,
            Err(error) => {
                tracing_event!(Level::WARN, %error, "RSS manual download blocked by network policy");
                return (false, None, None);
            }
        };
        let response = match network_scope.run(request.send()).await {
            Ok(Ok(resp)) => resp,
            Err(error) => {
                tracing_event!(Level::WARN, %error, "RSS manual download canceled after network invalidation");
                return (false, None, None);
            }
            Ok(Err(e)) => {
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

        let bytes = match network_scope.run(response.bytes()).await {
            Ok(Ok(b)) => b,
            Err(error) => {
                tracing_event!(Level::WARN, %error, "RSS manual download body canceled after network invalidation");
                return (false, None, None);
            }
            Ok(Err(e)) => {
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

    fn startup_version_checker(&self) {
        let current_version = env!("CARGO_PKG_VERSION");
        let tx = self.app_command_tx.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let network_activation = self.network_activation.clone();
        let mut network_state_rx = network_activation.subscribe();

        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(24 * 60 * 60));
            interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = shutdown_rx.recv() => {
                        tracing::debug!("Version checker stopped due to shutdown");
                        return;
                    }
                }

                let latest = match App::fetch_latest_version_when_network_ready(
                    &network_activation,
                    &mut network_state_rx,
                    &mut shutdown_rx,
                    |network_scope| async move { App::fetch_latest_version(&network_scope).await },
                )
                .await
                {
                    Ok(latest) => latest,
                    Err(()) => {
                        tracing::debug!("Version check aborted due to shutdown");
                        return;
                    }
                };

                let Some(latest) = latest else {
                    continue;
                };
                if latest != current_version {
                    tracing::info!(
                        "New version found! Current: {} - Latest: {}",
                        current_version,
                        latest
                    );
                    let _ = tx.send(AppCommand::UpdateVersionAvailable(latest)).await;
                } else {
                    tracing::info!(
                        "Current version is latest! Current: {} - Latest: {}",
                        current_version,
                        latest
                    );
                }
            }
        });
    }

    async fn fetch_latest_version_when_network_ready<F, Fut>(
        network_activation: &NetworkActivationHandle,
        network_state_rx: &mut watch::Receiver<crate::networking::NetworkActivationState>,
        shutdown_rx: &mut broadcast::Receiver<()>,
        mut fetch: F,
    ) -> Result<Option<String>, ()>
    where
        F: FnMut(crate::networking::NetworkScope) -> Fut,
        Fut: Future<Output = Result<String, VersionCheckError>>,
    {
        loop {
            let network_scope = loop {
                if let Ok(active_network) = network_activation.try_active() {
                    break active_network.scope().clone();
                }
                tokio::select! {
                    changed = network_state_rx.changed() => {
                        if changed.is_err() {
                            return Err(());
                        }
                    }
                    _ = shutdown_rx.recv() => return Err(()),
                }
            };

            let result = tokio::select! {
                result = network_scope.run(fetch(network_scope.clone())) => result,
                _ = shutdown_rx.recv() => return Err(()),
            };
            match result {
                Ok(Ok(latest)) => return Ok(Some(latest)),
                Err(error) => {
                    tracing::debug!(
                        generation_id = network_scope.id().generation_id(),
                        %error,
                        "Retrying version check after network activation invalidation"
                    );
                }
                Ok(Err(error)) => {
                    tracing::debug!(%error, "Version check failed");
                    return Ok(None);
                }
            }
        }
    }

    async fn fetch_latest_version(
        network_scope: &crate::networking::NetworkScope,
    ) -> Result<String, VersionCheckError> {
        let client = network_scope.lease().general_http_client()?;

        let url = "https://crates.io/api/v1/crates/superseedr";
        let request = client.get(url)?;
        let resp = network_scope.run(request.send()).await??;
        let resp: CratesResponse = network_scope.run(resp.json()).await??;

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
            network: Some(
                self.network_state_rx
                    .borrow()
                    .runtime_status(&self.client_configs.network_binding),
            ),
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
        if configured_interval == 0 && self.is_shared_mode_enabled() {
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
        self.reschedule_startup_load_deadline_after(Duration::from_secs(
            STARTUP_ROLLING_BATCH_INTERVAL_SECS,
        ));
    }

    fn reschedule_startup_load_deadline_after(&mut self, delay: Duration) {
        self.next_startup_load_at = if self.startup_deferred_load_queue.is_empty() {
            None
        } else {
            Some(time::Instant::now() + delay)
        };
    }

    fn maybe_log_startup_load_summary(&mut self) {
        if self.startup_load_summary_logged || !self.startup_deferred_load_queue.is_empty() {
            return;
        }
        if self.startup_loaded_torrent_count == 0 && self.client_configs.torrents.is_empty() {
            return;
        }

        self.startup_load_summary_logged = true;
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

        self.startup_loaded_torrent_count = self
            .startup_loaded_torrent_count
            .saturating_add(loaded_count);
        self.reschedule_startup_load_deadline();

        if loaded_count > 0 {
            self.app_state.ui.needs_redraw = true;
            self.save_state_to_disk();
        }
        self.maybe_log_startup_load_summary();
    }
}

fn preserve_bound_random_client_port(old_settings: &Settings, new_settings: &mut Settings) {
    if old_settings.randomize_client_port && new_settings.randomize_client_port {
        new_settings.client_port = old_settings.client_port;
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

fn rss_settings_changed(old_settings: &Settings, new_settings: &Settings) -> bool {
    new_settings.rss != old_settings.rss
}

fn runtime_torrent_settings_changed(old_settings: &Settings, new_settings: &Settings) -> bool {
    old_settings.torrents != new_settings.torrents
        || old_settings.default_download_folder != new_settings.default_download_folder
}

fn should_load_persisted_torrent(torrent_settings: &TorrentSettings) -> bool {
    torrent_settings.torrent_control_state != TorrentControlState::Deleting
}

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn preserve_restored_added_at(app_state: &mut AppState, torrent_config: &TorrentSettings) {
    let Some(added_at_unix_secs) = torrent_config.added_at_unix_secs else {
        return;
    };
    let Some(info_hash) = info_hash_from_torrent_source(&torrent_config.torrent_or_magnet) else {
        return;
    };
    if let Some(runtime) = app_state.torrents.get_mut(&info_hash) {
        runtime.added_at_unix_secs = Some(added_at_unix_secs);
    }
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
    client_configs.torrent_sort_pinned = app_state.torrent_sort_pinned;
    client_configs.peer_sort_column = app_state.peer_sort.0;
    client_configs.peer_sort_direction = app_state.peer_sort.1;
    client_configs.peer_sort_pinned = app_state.peer_sort_pinned;
    client_configs.ui_refresh_rate = app_state.data_rate;
    client_configs.peer_stream_visualization = app_state.ui.visualization_focus.peer_stream;
    client_configs.disk_health_visualization = app_state.ui.visualization_focus.disk_health;
    client_configs.dht_visualization = app_state.ui.visualization_focus.dht;
    let old_validation_statuses: HashMap<String, bool> = client_configs
        .torrents
        .iter()
        .map(|cfg| (cfg.torrent_or_magnet.clone(), cfg.validation_status))
        .collect();
    let old_added_at_unix_secs: HashMap<String, Option<u64>> = client_configs
        .torrents
        .iter()
        .map(|cfg| (cfg.torrent_or_magnet.clone(), cfg.added_at_unix_secs))
        .collect();
    let previous_torrents = client_configs.torrents.clone();
    let deferred_hashes: HashSet<Vec<u8>> = startup_deferred_load_queue.iter().cloned().collect();
    let pending_preview_info_hash = app_state.pending_magnet_preview_info_hash.clone();
    let is_pending_preview =
        |info_hash: &[u8]| pending_preview_info_hash.as_deref() == Some(info_hash);
    let mut persisted_info_hashes: HashSet<Vec<u8>> = app_state
        .torrents
        .keys()
        .filter(|info_hash| !is_pending_preview(info_hash.as_slice()))
        .cloned()
        .collect();

    let mut persisted_torrents: Vec<TorrentSettings> = app_state
        .torrents
        .iter()
        .filter_map(|(info_hash, torrent)| {
            if is_pending_preview(info_hash) {
                return None;
            }

            let torrent_state = &torrent.latest_state;
            let previous_validation_status = old_validation_statuses
                .get(&torrent_state.torrent_or_magnet)
                .copied()
                .unwrap_or(false);

            let final_validation_status =
                persisted_validation_status_from_metrics(torrent_state, previous_validation_status);

            Some(TorrentSettings {
                torrent_or_magnet: torrent_state.torrent_or_magnet.clone(),
                name: torrent_state.torrent_name.clone(),
                added_at_unix_secs: torrent.added_at_unix_secs.or_else(|| {
                    old_added_at_unix_secs
                        .get(&torrent_state.torrent_or_magnet)
                        .copied()
                        .flatten()
                }),
                validation_status: final_validation_status,
                download_path: torrent_state.download_path.clone(),
                container_name: torrent_state.container_name.clone(),
                torrent_control_state: torrent_state.torrent_control_state.clone(),
                delete_files: torrent_state.delete_files,
                file_priorities: torrent_state.file_priorities.clone(),
            })
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

    fn activation_listen_port(
        rx: &tokio::sync::watch::Receiver<crate::networking::NetworkActivationState>,
    ) -> Option<u16> {
        match &*rx.borrow() {
            crate::networking::NetworkActivationState::Active(active) => Some(active.listen_port()),
            _ => None,
        }
    }

    fn install_test_network_activation(app: &mut App, generation_id: u64) -> NetworkScopeId {
        let (network_handle, lease) =
            crate::networking::runtime::test_network_lease_with_generation(generation_id);
        let (mut publisher, activation) = crate::networking::NetworkActivationPublisher::channel();
        let scope_id = publisher
            .activate(lease, 6681)
            .expect("activate test network")
            .scope()
            .id();
        app.network_handle = network_handle;
        app.network_activation_publisher = publisher;
        app.network_activation = activation;
        scope_id
    }

    use super::{
        advance_dht_wave_state, align_unpinned_sort_with_visible_activity,
        apply_network_history_persist_result, build_app_dht_service_config, build_persist_payload,
        build_torrent_preview_tree, bytes_per_sec_to_bps, clamp_selected_indices_in_state,
        compose_system_warning, configured_download_bucket_rate,
        configured_download_ceiling_bytes_per_sec, configured_upload_bucket_rate, dht_wave_targets,
        disk_backpressure_score, effective_download_limit_bps, extract_magnet_display_name,
        flush_persistence_writer_parts, format_filesystem_path_error, initial_disk_throttle_rate,
        is_valid_incoming_bittorrent_handshake, load_torrent_file_preview,
        move_file_with_fallback_impl, network_policy_warning, parse_hybrid_hashes,
        persisted_validation_status_from_metrics, preserve_restored_added_at,
        prune_rss_feed_errors, queue_persistence_payload, refresh_autosort_after_stats,
        resolve_magnet_torrent_name, rss_settings_changed, runtime_torrent_settings_changed,
        set_test_persistence_writer_enabled, should_load_persisted_torrent,
        should_persist_network_history_on_interval, sort_and_filter_torrent_list_state,
        swarm_availability_counts, tcp_peer_listener_enabled, torrent_completion_percent,
        torrent_is_effectively_incomplete, App, AppClusterRole, AppCommand, AppMode,
        AppRuntimeMode, AppState, BrowserPane, BrowserSearchState, ColumnId, CommandIngestResult,
        ConfigNetworkInterfaceInventory, DataRate, DhtVisualization, DhtWaveTargets,
        DhtWaveUiState, DiskBackpressureDecision, DiskBackpressureDownloadThrottle,
        DiskBackpressureSample, DiskHealthVisualization, DownloadSelectionTarget, FileBrowserMode,
        FileMetadata, FilePriority, InboundPeerTransportStatus, IngestSource, ListenerSet,
        LogCooldown, PeerInfo, PeerListenerTransportMode, PeerSortColumn, PeerStreamVisualization,
        PendingManualIngest, PersistPayload, ResolvedAddPayload, SearchMode, SelectedHeader,
        SortDirection, SwarmAvailabilityFlashState, TorrentControlState, TorrentDisplayState,
        TorrentIntegritySnapshot, TorrentMetrics, TorrentPreviewPayload, TorrentSortColumn,
        UiState, VisualizationFocusPanel, VisualizationFocusState, WakeLagPeerThrottle,
        AWAITING_MAGNET_METADATA_LABEL, BITTORRENT_PROTOCOL_STR, DHT_WAVE_PHASE_WRAP_PERIOD,
        DISK_WRITE_THROTTLE_MIN_BYTES_PER_SEC, DISK_WRITE_THROTTLE_START_BYTES_PER_SEC,
        DISK_WRITE_THROTTLE_STEP_MAX, DISK_WRITE_THROTTLE_STEP_MIN,
        DISK_WRITE_THROTTLE_TARGET_LATENCY_SECS, DISK_WRITE_THROTTLE_WINDOW_TICKS,
        SWARM_AVAILABILITY_FLASH_DURATION,
    };
    use crate::config::{
        clear_shared_config_state_for_tests, set_app_paths_override_for_tests, Settings,
        TorrentSettings, UiLayoutMode,
    };
    use crate::control_service::control_event_details;
    #[cfg(feature = "dht")]
    use crate::dht_service::{DhtBackendKind, DhtServiceConfig};
    use crate::dht_service::{DhtService, DhtStatus, DhtWaveTelemetry, TestDhtRecorder};
    use crate::errors::StorageError;
    use crate::integrations::control::{read_control_request, ControlRequest};
    use crate::integrations::status::{self, AppOutputState};
    use crate::networking::transport::PeerTransportKind;
    use crate::networking::{
        NetworkHandle, NetworkLease, NetworkScopeId, NetworkState, NetworkSupervisor,
        UtpPeerTransport,
    };
    use crate::persistence::event_journal::{
        ControlOrigin, EventDetails, EventType, IngestKind, IngestOrigin,
    };
    use crate::persistence::event_journal::{EventCategory, EventJournalEntry};
    use crate::telemetry::ui_telemetry::UiTelemetry;
    use crate::torrent_identity::{info_hash_from_torrent_bytes, info_hash_from_torrent_source};
    use crate::torrent_manager::{
        FileProbeBatchResult, FileProbeEntry, ManagerCommand, ManagerEvent, TorrentFileProbeStatus,
    };
    use crate::tui::layout::normal::{
        calculate_layout, LayoutContext, DEFAULT_SIDEBAR_PERCENT, PEER_STREAM_MIN_HEIGHT,
        PEER_STREAM_MIN_WIDTH,
    };
    use crate::tui::screens::browser::{
        build_download_confirm_payload, execute_browser_dialog_effects, execute_confirm_decision,
        reduce_browser_dialog_action, BrowserDialogAction, BrowserDialogEffect,
    };
    use crate::tui::tree::{RawNode, TreeViewState};
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    use std::collections::{HashMap, VecDeque};
    use std::env;
    use std::io;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime};
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream, UdpSocket};
    use tokio::sync::mpsc;
    use tokio::sync::watch;
    use tokio::sync::Notify;
    use tokio::time;

    fn unrestricted_network_lease() -> (NetworkHandle, NetworkLease) {
        let (handle, _task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let lease = handle.try_lease().unwrap();
        (handle, lease)
    }

    #[test]
    fn config_interface_inventory_ignores_stale_discovery_results() {
        let mut inventory = ConfigNetworkInterfaceInventory::default();
        inventory
            .interfaces
            .push(crate::networking::NetworkInterfaceInfo {
                identity: "interface-test0".to_string(),
                display_name: "Interface Test 0".to_string(),
                ipv4_index: Some(7),
                ipv6_index: None,
                is_up: true,
                is_loopback: false,
                ipv4_addresses: vec![std::net::Ipv4Addr::new(192, 0, 2, 7)],
                ipv6_addresses: Vec::new(),
            });
        let request_id = inventory.begin_refresh();

        assert!(inventory.loading);
        assert!(inventory.interfaces.is_empty());
        assert!(
            !inventory.finish_refresh(request_id.wrapping_sub(1), Err("stale failure".to_string()))
        );
        assert!(inventory.loading);
        assert!(inventory.error.is_none());

        assert!(inventory.finish_refresh(request_id, Ok(Vec::new())));
        assert!(!inventory.loading);
        assert!(inventory.error.is_none());
    }

    #[test]
    fn utp_only_mode_disables_tcp_peer_listener() {
        assert!(!tcp_peer_listener_enabled(PeerListenerTransportMode::Utp));
        assert!(tcp_peer_listener_enabled(PeerListenerTransportMode::Tcp));
        assert!(tcp_peer_listener_enabled(PeerListenerTransportMode::All));
    }

    #[test]
    fn log_cooldown_allows_first_event_and_then_only_after_interval() {
        let now = Instant::now();
        let mut cooldown = LogCooldown::default();

        assert!(cooldown.should_log(now, Duration::from_secs(60)));
        assert!(!cooldown.should_log(now + Duration::from_secs(59), Duration::from_secs(60)));
        assert!(cooldown.should_log(now + Duration::from_secs(60), Duration::from_secs(60)));
    }

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

    struct SharedEnvTestGuard {
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for SharedEnvTestGuard {
        fn drop(&mut self) {
            set_test_persistence_writer_enabled(false);
        }
    }

    fn lock_shared_env() -> SharedEnvTestGuard {
        let guard = shared_env_guard()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        set_test_persistence_writer_enabled(true);
        SharedEnvTestGuard { _guard: guard }
    }

    fn disk_backpressure_sample(
        download_bps: u64,
        disk_write_completed_bps: u64,
    ) -> DiskBackpressureSample {
        DiskBackpressureSample {
            is_leeching: true,
            configured_download_limit_bps: 0,
            download_bps,
            disk_write_completed_bps,
            recv_to_write_p95: Duration::from_secs(1),
        }
    }

    fn set_disk_throttle_rate(throttle: &mut DiskBackpressureDownloadThrottle, rate_bps: u64) {
        let rate_bytes_per_sec = rate_bps as f64 / 8.0;
        throttle.active = true;
        throttle.rate_bytes_per_sec = rate_bytes_per_sec;
        throttle.accepted_rate_bytes_per_sec = rate_bytes_per_sec;
        throttle.last_score = None;
        throttle.window_score_total = 0.0;
        throttle.window_ticks = 0;
    }

    fn completed_bps_for_cap(rate_bytes_per_sec: f64, disk_capacity_bps: u64) -> u64 {
        bytes_per_sec_to_bps(rate_bytes_per_sec).min(disk_capacity_bps)
    }

    fn run_disk_throttle_window(
        throttle: &mut DiskBackpressureDownloadThrottle,
        disk_capacity_bps: u64,
        step_factor: f64,
    ) {
        let completed_bps = completed_bps_for_cap(throttle.rate_bytes_per_sec, disk_capacity_bps);
        let download_bps = bytes_per_sec_to_bps(throttle.rate_bytes_per_sec).max(1);
        let sample = disk_backpressure_sample(download_bps, completed_bps);
        for _ in 0..DISK_WRITE_THROTTLE_WINDOW_TICKS {
            throttle.update_with_step_factor(sample, step_factor);
        }
    }

    fn latency_limited_disk_sample(
        rate_bytes_per_sec: f64,
        disk_capacity_bps: u64,
    ) -> DiskBackpressureSample {
        let attempted_bps = bytes_per_sec_to_bps(rate_bytes_per_sec).max(1);
        let completed_bps = attempted_bps.min(disk_capacity_bps);
        let latency_seconds = if attempted_bps <= disk_capacity_bps {
            DISK_WRITE_THROTTLE_TARGET_LATENCY_SECS
        } else {
            DISK_WRITE_THROTTLE_TARGET_LATENCY_SECS * attempted_bps as f64
                / disk_capacity_bps as f64
        };

        DiskBackpressureSample {
            recv_to_write_p95: Duration::from_secs_f64(latency_seconds),
            ..disk_backpressure_sample(attempted_bps, completed_bps)
        }
    }

    fn run_latency_limited_disk_window(
        throttle: &mut DiskBackpressureDownloadThrottle,
        disk_capacity_bps: u64,
        step_factor: f64,
    ) {
        let sample = latency_limited_disk_sample(throttle.rate_bytes_per_sec, disk_capacity_bps);
        for _ in 0..DISK_WRITE_THROTTLE_WINDOW_TICKS {
            throttle.update_with_step_factor(sample, step_factor);
        }
    }

    #[test]
    fn disk_backpressure_hill_climber_converges_up_from_low_cap() {
        let mut throttle = DiskBackpressureDownloadThrottle::new(0);
        set_disk_throttle_rate(&mut throttle, 100_000_000);

        for _ in 0..8 {
            run_disk_throttle_window(&mut throttle, 1_000_000_000, DISK_WRITE_THROTTLE_STEP_MAX);
        }

        assert!(bytes_per_sec_to_bps(throttle.accepted_rate_bytes_per_sec) > 300_000_000);
        assert!(throttle.last_score.unwrap_or_default() > 250_000_000.0);
    }

    #[test]
    fn disk_backpressure_hill_climber_converges_down_from_high_cap() {
        let mut throttle = DiskBackpressureDownloadThrottle::new(0);
        set_disk_throttle_rate(&mut throttle, 2_000_000_000);

        for _ in 0..8 {
            run_disk_throttle_window(&mut throttle, 500_000_000, DISK_WRITE_THROTTLE_STEP_MIN);
        }

        let accepted_bps = bytes_per_sec_to_bps(throttle.accepted_rate_bytes_per_sec);
        assert!(accepted_bps >= 500_000_000);
        assert!(accepted_bps <= 700_000_000);
        assert_eq!(throttle.last_score, Some(500_000_000.0));
    }

    #[test]
    fn disk_backpressure_hill_climber_rejects_candidate_that_lowers_completed_speed() {
        let mut throttle = DiskBackpressureDownloadThrottle::new(0);
        set_disk_throttle_rate(&mut throttle, 600_000_000);

        run_disk_throttle_window(&mut throttle, 500_000_000, DISK_WRITE_THROTTLE_STEP_MIN);
        run_disk_throttle_window(&mut throttle, 500_000_000, DISK_WRITE_THROTTLE_STEP_MIN);

        assert_eq!(
            bytes_per_sec_to_bps(throttle.accepted_rate_bytes_per_sec),
            600_000_000
        );
        assert_eq!(throttle.last_score, Some(500_000_000.0));
    }

    #[test]
    fn disk_backpressure_hill_climber_converges_up_to_latency_limited_disk() {
        let disk_capacity_bps = 500_000_000;
        let mut throttle = DiskBackpressureDownloadThrottle::new(0);
        set_disk_throttle_rate(&mut throttle, 100_000_000);

        let steps = [1.18, 0.93, 1.14, 1.09, 0.86, 1.20, 0.91, 1.11];
        for step in steps.into_iter().cycle().take(80) {
            run_latency_limited_disk_window(&mut throttle, disk_capacity_bps, step);
        }

        let accepted_bps = bytes_per_sec_to_bps(throttle.accepted_rate_bytes_per_sec);
        let accepted_score = disk_backpressure_score(latency_limited_disk_sample(
            throttle.accepted_rate_bytes_per_sec,
            disk_capacity_bps,
        ));

        assert!(
            (350_000_000..=650_000_000).contains(&accepted_bps),
            "accepted_bps={accepted_bps}"
        );
        assert!(
            accepted_score >= disk_capacity_bps as f64 * 0.90,
            "accepted_score={accepted_score}"
        );
    }

    #[test]
    fn disk_backpressure_hill_climber_converges_down_to_latency_limited_disk() {
        let disk_capacity_bps = 500_000_000;
        let mut throttle = DiskBackpressureDownloadThrottle::new(0);
        set_disk_throttle_rate(&mut throttle, 2_000_000_000);

        let steps = [0.82, 1.12, 0.88, 0.91, 1.19, 0.84, 1.08, 0.90];
        for step in steps.into_iter().cycle().take(80) {
            run_latency_limited_disk_window(&mut throttle, disk_capacity_bps, step);
        }

        let accepted_bps = bytes_per_sec_to_bps(throttle.accepted_rate_bytes_per_sec);
        let accepted_score = disk_backpressure_score(latency_limited_disk_sample(
            throttle.accepted_rate_bytes_per_sec,
            disk_capacity_bps,
        ));

        assert!(
            (350_000_000..=650_000_000).contains(&accepted_bps),
            "accepted_bps={accepted_bps}"
        );
        assert!(
            accepted_score >= disk_capacity_bps as f64 * 0.90,
            "accepted_score={accepted_score}"
        );
    }

    #[test]
    fn disk_backpressure_hill_climber_converges_down_from_100mbps_to_30mbps_disk() {
        let disk_capacity_bps = 30_000_000;
        let mut throttle = DiskBackpressureDownloadThrottle::new(0);
        set_disk_throttle_rate(&mut throttle, 100_000_000);

        let steps = [0.82, 1.14, 0.88, 0.91, 1.18, 0.84, 1.08, 0.90];
        for step in steps.into_iter().cycle().take(120) {
            run_latency_limited_disk_window(&mut throttle, disk_capacity_bps, step);
        }

        let accepted_bps = bytes_per_sec_to_bps(throttle.accepted_rate_bytes_per_sec);
        let accepted_score = disk_backpressure_score(latency_limited_disk_sample(
            throttle.accepted_rate_bytes_per_sec,
            disk_capacity_bps,
        ));

        assert!(
            (25_000_000..=40_000_000).contains(&accepted_bps),
            "accepted_bps={accepted_bps}"
        );
        assert!(
            accepted_score >= disk_capacity_bps as f64 * 0.85,
            "accepted_score={accepted_score}"
        );
    }

    #[test]
    fn disk_backpressure_hill_climber_climbs_after_disk_capacity_recovers() {
        let slow_disk_capacity_bps = 30_000_000;
        let recovered_disk_capacity_bps = 120_000_000;
        let mut throttle = DiskBackpressureDownloadThrottle::new(0);
        set_disk_throttle_rate(&mut throttle, 100_000_000);

        let steps = [0.82, 1.14, 0.88, 0.91, 1.18, 0.84, 1.08, 0.90];
        for step in steps.into_iter().cycle().take(120) {
            run_latency_limited_disk_window(&mut throttle, slow_disk_capacity_bps, step);
        }

        let slow_accepted_bps = bytes_per_sec_to_bps(throttle.accepted_rate_bytes_per_sec);
        assert!(
            (25_000_000..=40_000_000).contains(&slow_accepted_bps),
            "slow_accepted_bps={slow_accepted_bps}"
        );

        for step in steps.into_iter().cycle().take(120) {
            run_latency_limited_disk_window(&mut throttle, recovered_disk_capacity_bps, step);
        }

        let recovered_accepted_bps = bytes_per_sec_to_bps(throttle.accepted_rate_bytes_per_sec);
        let recovered_score = disk_backpressure_score(latency_limited_disk_sample(
            throttle.accepted_rate_bytes_per_sec,
            recovered_disk_capacity_bps,
        ));

        assert!(
            (90_000_000..=150_000_000).contains(&recovered_accepted_bps),
            "recovered_accepted_bps={recovered_accepted_bps}"
        );
        assert!(
            recovered_score >= recovered_disk_capacity_bps as f64 * 0.90,
            "recovered_score={recovered_score}"
        );
    }

    #[test]
    fn disk_backpressure_score_penalizes_only_above_target_receive_to_write_latency() {
        let fast = DiskBackpressureSample {
            recv_to_write_p95: Duration::from_millis(500),
            ..disk_backpressure_sample(1_000_000_000, 1_000_000_000)
        };
        let target = DiskBackpressureSample {
            recv_to_write_p95: Duration::from_secs(2),
            ..disk_backpressure_sample(1_000_000_000, 1_000_000_000)
        };
        let slow = DiskBackpressureSample {
            recv_to_write_p95: Duration::from_secs(4),
            ..disk_backpressure_sample(1_000_000_000, 1_000_000_000)
        };

        assert_eq!(disk_backpressure_score(fast), 1_000_000_000.0);
        assert_eq!(disk_backpressure_score(target), 1_000_000_000.0);
        assert_eq!(disk_backpressure_score(slow), 500_000_000.0);
    }

    #[test]
    fn disk_backpressure_throttle_waits_for_disk_write_signal() {
        let mut throttle = DiskBackpressureDownloadThrottle::new(0);
        let mut sample = disk_backpressure_sample(100_000_000, 0);
        sample.recv_to_write_p95 = Duration::ZERO;

        for _ in 0..DISK_WRITE_THROTTLE_WINDOW_TICKS {
            assert_eq!(
                throttle.update_with_step_factor(sample, DISK_WRITE_THROTTLE_STEP_MIN),
                DiskBackpressureDecision::Disabled
            );
        }

        assert!(!throttle.active);
        assert_eq!(throttle.window_ticks, 0);
        assert_eq!(throttle.last_score, None);
    }

    #[test]
    fn disk_backpressure_throttle_disables_when_signal_disappears() {
        let mut throttle = DiskBackpressureDownloadThrottle::new(0);
        set_disk_throttle_rate(&mut throttle, 30_000_000);

        let mut sample = disk_backpressure_sample(100_000_000, 0);
        sample.recv_to_write_p95 = Duration::ZERO;

        assert_eq!(
            throttle.update_with_step_factor(sample, DISK_WRITE_THROTTLE_STEP_MIN),
            DiskBackpressureDecision::Disabled
        );
        assert!(!throttle.active);
        assert_eq!(
            throttle.rate_bytes_per_sec,
            initial_disk_throttle_rate(sample.configured_download_limit_bps)
        );
        assert_eq!(throttle.window_ticks, 0);
        assert_eq!(throttle.last_score, None);
    }

    #[test]
    fn configured_rate_limit_buckets_use_bytes_per_second() {
        assert_eq!(configured_download_bucket_rate(8_000), 1_000.0);
        assert_eq!(configured_upload_bucket_rate(16_000), 2_000.0);
        assert!(configured_download_bucket_rate(0).is_infinite());
        assert!(configured_upload_bucket_rate(0).is_infinite());
        assert!(
            configured_download_bucket_rate(crate::config::UNLIMITED_RATE_LIMIT_BPS).is_infinite()
        );
        assert!(
            configured_upload_bucket_rate(crate::config::UNLIMITED_RATE_LIMIT_BPS).is_infinite()
        );
        assert!(configured_download_ceiling_bytes_per_sec(0).is_infinite());
        assert!(
            configured_download_ceiling_bytes_per_sec(crate::config::UNLIMITED_RATE_LIMIT_BPS)
                .is_infinite()
        );
    }

    #[test]
    fn disk_backpressure_throttle_clamps_to_one_mbps_floor() {
        let mut throttle = DiskBackpressureDownloadThrottle::new(0);
        set_disk_throttle_rate(&mut throttle, 1_100_000);

        run_disk_throttle_window(&mut throttle, 10_000_000, DISK_WRITE_THROTTLE_STEP_MIN);

        assert_eq!(
            throttle.rate_bytes_per_sec,
            DISK_WRITE_THROTTLE_MIN_BYTES_PER_SEC
        );
    }

    #[test]
    fn disk_backpressure_throttle_disables_when_seeding() {
        let mut throttle = DiskBackpressureDownloadThrottle::new(0);
        let mut sample = disk_backpressure_sample(1_000_000_000, 100_000_000);
        sample.is_leeching = false;
        assert_eq!(throttle.update(sample), DiskBackpressureDecision::Disabled);
    }

    #[test]
    fn effective_download_limit_uses_lower_configured_or_adaptive_limit() {
        assert_eq!(effective_download_limit_bps(0, None), 0);
        assert_eq!(effective_download_limit_bps(800_000_000, None), 800_000_000);
        assert_eq!(
            effective_download_limit_bps(0, Some(500_000_000)),
            500_000_000
        );
        assert_eq!(
            effective_download_limit_bps(
                crate::config::UNLIMITED_RATE_LIMIT_BPS,
                Some(500_000_000)
            ),
            500_000_000
        );
        assert_eq!(
            effective_download_limit_bps(800_000_000, Some(500_000_000)),
            500_000_000
        );
        assert_eq!(
            effective_download_limit_bps(300_000_000, Some(500_000_000)),
            300_000_000
        );
    }

    #[tokio::test]
    async fn app_disk_backpressure_update_changes_live_download_bucket() {
        let _guard = lock_shared_env();
        let _temp_paths = configure_temp_app_paths_for_test();
        let settings = crate::config::Settings {
            client_port: 0,
            global_download_limit_bps: crate::config::UNLIMITED_RATE_LIMIT_BPS,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");

        app.app_state.is_seeding = false;
        app.app_state.avg_download_history.push(1_000_000_000);
        app.app_state.avg_disk_write_bps = 1_000_000_000;
        app.app_state.avg_disk_write_completed_bps = 400_000_000;
        app.app_state.avg_disk_write_latency = Duration::from_millis(1);
        app.app_state.recv_to_write_p95 = Duration::from_secs(1);

        assert!(app.global_dl_bucket.get_fill_rate().is_infinite());

        for _ in 0..DISK_WRITE_THROTTLE_WINDOW_TICKS {
            app.update_disk_backpressure_download_throttle();
        }

        let fill_rate = app.global_dl_bucket.get_fill_rate();
        assert!(
            fill_rate >= DISK_WRITE_THROTTLE_START_BYTES_PER_SEC * DISK_WRITE_THROTTLE_STEP_MIN
        );
        assert!(
            fill_rate <= DISK_WRITE_THROTTLE_START_BYTES_PER_SEC * DISK_WRITE_THROTTLE_STEP_MAX
        );
        assert_eq!(app.global_dl_bucket.get_capacity(), fill_rate);
        assert_eq!(
            app.app_state.effective_download_limit_bps,
            (fill_rate * 8.0).round() as u64
        );

        let _ = app.shutdown_tx.send(());
    }

    struct TempAppPaths {
        dir: tempfile::TempDir,
    }

    impl TempAppPaths {
        fn path(&self) -> &std::path::Path {
            self.dir.path()
        }
    }

    impl Drop for TempAppPaths {
        fn drop(&mut self) {
            set_app_paths_override_for_tests(None);
        }
    }

    fn configure_temp_app_paths_for_test() -> TempAppPaths {
        let dir = tempfile::tempdir().expect("create tempdir");
        let config_dir = dir.path().join("config");
        let data_dir = dir.path().join("data");
        set_app_paths_override_for_tests(Some((config_dir, data_dir)));
        TempAppPaths { dir }
    }

    fn mark_startup_roll_in_responsiveness_ready(app: &mut App) {
        app.app_state.ui.frame_wake_lag_ratio_ema = Some(0.0);
        app.app_state.ui.frame_draw_ratio_ema = Some(0.0);
    }

    async fn wait_for_peer_slot_usages(
        recorder: &TestDhtRecorder,
        expected_len: usize,
    ) -> Vec<(usize, usize)> {
        time::timeout(Duration::from_secs(1), async {
            loop {
                let recorded = recorder.recorded_peer_slot_usages();
                if recorded.len() >= expected_len {
                    return recorded;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("DHT peer slot usage should be recorded")
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
                    added_at_unix_secs: Some(1_700_000_000),
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
                    torrent_or_magnet: loaded_magnet.clone(),
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
        assert!(payload.settings.torrents.iter().any(|torrent| {
            torrent.torrent_or_magnet == loaded_magnet
                && torrent.added_at_unix_secs == Some(1_700_000_000)
        }));
    }

    #[test]
    fn build_persist_payload_skips_pending_magnet_preview_runtime() {
        let info_hash = vec![0x55; 20];
        let magnet = "magnet:?xt=urn:btih:5555555555555555555555555555555555555555".to_string();
        let mut settings = crate::config::Settings::default();
        let mut app_state = AppState {
            pending_magnet_preview_info_hash: Some(info_hash.clone()),
            ..Default::default()
        };
        app_state.torrents.insert(
            info_hash.clone(),
            TorrentDisplayState {
                latest_state: TorrentMetrics {
                    info_hash: info_hash.clone(),
                    torrent_or_magnet: magnet,
                    torrent_name: "sample-preview".to_string(),
                    torrent_control_state: TorrentControlState::Running,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        app_state.torrent_list_order.push(info_hash.clone());

        let payload = build_persist_payload(&mut settings, &mut app_state, &VecDeque::new());

        assert!(payload.settings.torrents.is_empty());
        assert!(app_state.torrents.contains_key(&info_hash));
    }

    #[test]
    fn build_persist_payload_captures_visualization_selections() {
        let mut settings = crate::config::Settings::default();
        let mut app_state = AppState::default();
        app_state.ui.visualization_focus.active = true;
        app_state.ui.visualization_focus.selected = VisualizationFocusPanel::DhtWave;
        app_state.ui.visualization_focus.peer_stream = PeerStreamVisualization::HelixExchange;
        app_state.ui.visualization_focus.disk_health = DiskHealthVisualization::SeekPendulum;
        app_state.ui.visualization_focus.dht = DhtVisualization::PeerBloom;

        let payload = build_persist_payload(&mut settings, &mut app_state, &VecDeque::new());

        assert_eq!(
            payload.settings.peer_stream_visualization,
            PeerStreamVisualization::HelixExchange
        );
        assert_eq!(
            payload.settings.disk_health_visualization,
            DiskHealthVisualization::SeekPendulum
        );
        assert_eq!(
            payload.settings.dht_visualization,
            DhtVisualization::PeerBloom
        );
    }

    #[test]
    fn preserve_restored_added_at_keeps_original_added_date() {
        let original_added_at = 1_700_000_000;
        let restored_runtime_added_at = 1_800_000_000;
        let magnet = "magnet:?xt=urn:btih:7777777777777777777777777777777777777777".to_string();
        let info_hash = vec![0x77; 20];
        let torrent_config = TorrentSettings {
            torrent_or_magnet: magnet.clone(),
            name: "sample-restored".to_string(),
            added_at_unix_secs: Some(original_added_at),
            torrent_control_state: TorrentControlState::Running,
            ..Default::default()
        };
        let mut app_state = AppState::default();
        app_state.torrents.insert(
            info_hash.clone(),
            TorrentDisplayState {
                latest_state: TorrentMetrics {
                    info_hash: info_hash.clone(),
                    torrent_or_magnet: magnet,
                    torrent_name: "sample-restored".to_string(),
                    torrent_control_state: TorrentControlState::Running,
                    ..Default::default()
                },
                added_at_unix_secs: Some(restored_runtime_added_at),
                ..Default::default()
            },
        );

        preserve_restored_added_at(&mut app_state, &torrent_config);

        assert_eq!(
            app_state
                .torrents
                .get(&info_hash)
                .and_then(|torrent| torrent.added_at_unix_secs),
            Some(original_added_at)
        );
    }

    #[test]
    fn should_draw_normal_mode_when_dirty_or_animating() {
        assert!(!App::should_draw_this_frame(&AppMode::Normal, false, false));
        assert!(App::should_draw_this_frame(&AppMode::Normal, true, false));
        assert!(App::should_draw_this_frame(&AppMode::Normal, false, true));
    }

    #[test]
    fn swarm_availability_counts_pieces_across_peers() {
        let peers = vec![
            PeerInfo {
                bitfield: vec![true, false, true],
                ..Default::default()
            },
            PeerInfo {
                bitfield: vec![false, true, true, true],
                ..Default::default()
            },
        ];

        assert_eq!(swarm_availability_counts(&peers, 3), vec![1, 1, 2]);
    }

    #[test]
    fn swarm_availability_flash_tracks_newly_added_pieces() {
        let now = Instant::now();
        let duration = Duration::from_millis(350);
        let mut state = SwarmAvailabilityFlashState::default();

        state.update(b"torrent-a", vec![0, 1, 0], now, duration);

        assert!(!state.is_piece_flashing(b"torrent-a", 1, now));
        assert!(!state.has_active_flash(now));

        let next = now + Duration::from_millis(10);
        state.update(b"torrent-a", vec![1, 1, 2], next, duration);

        assert!(state.is_piece_flashing(b"torrent-a", 0, next));
        assert!(!state.is_piece_flashing(b"torrent-a", 1, next));
        assert!(!state.is_piece_flashing(b"torrent-a", 2, next));
        assert_eq!(
            state.active_flash_piece_indices(b"torrent-a", next),
            vec![0]
        );
        assert!(state.has_active_flash(next));
        assert!(!state.is_piece_flashing(b"torrent-a", 0, next + duration));
        assert!(state.is_piece_flashing(b"torrent-a", 2, next + duration));
        assert!(!state.has_active_flash(next + duration * 2 + Duration::from_millis(1)));
    }

    #[test]
    fn swarm_availability_flash_rolls_batch_by_piece_index() {
        let now = Instant::now();
        let duration = Duration::from_millis(300);
        let mut state = SwarmAvailabilityFlashState::default();

        state.update(b"torrent-a", vec![0, 0, 0, 0], now, duration);

        let next = now + Duration::from_millis(10);
        state.update(b"torrent-a", vec![1, 1, 0, 1], next, duration);

        assert!(state.is_piece_flashing(b"torrent-a", 0, next));
        assert!(!state.is_piece_flashing(b"torrent-a", 1, next));
        assert!(!state.is_piece_flashing(b"torrent-a", 3, next));

        let second_start = next + Duration::from_millis(150);
        assert!(state.is_piece_flashing(b"torrent-a", 1, second_start));
        assert!(!state.is_piece_flashing(b"torrent-a", 3, second_start));

        let third_start = next + duration;
        assert!(!state.is_piece_flashing(b"torrent-a", 0, third_start));
        assert!(state.is_piece_flashing(b"torrent-a", 3, third_start));
    }

    #[test]
    fn swarm_availability_flash_suppresses_full_map_increase() {
        let now = Instant::now();
        let duration = Duration::from_millis(350);
        let mut state = SwarmAvailabilityFlashState::default();

        state.update(b"torrent-a", vec![0, 0, 0], now, duration);
        state.update(
            b"torrent-a",
            vec![1, 1, 1],
            now + Duration::from_millis(10),
            duration,
        );

        assert!(!state.has_active_flash(now + Duration::from_millis(10)));
        assert!(!state.is_piece_flashing(b"torrent-a", 0, now + Duration::from_millis(10)));
        assert!(!state.is_piece_flashing(b"torrent-a", 1, now + Duration::from_millis(10)));
        assert!(!state.is_piece_flashing(b"torrent-a", 2, now + Duration::from_millis(10)));
    }

    #[test]
    fn swarm_availability_flash_keeps_partial_increase_after_complete_baseline() {
        let now = Instant::now();
        let duration = Duration::from_millis(350);
        let mut state = SwarmAvailabilityFlashState::default();

        state.update(b"torrent-a", vec![4, 4, 4], now, duration);
        state.update(
            b"torrent-a",
            vec![5, 4, 4],
            now + Duration::from_millis(10),
            duration,
        );

        assert!(state.is_piece_flashing(b"torrent-a", 0, now + Duration::from_millis(10)));
        assert!(!state.is_piece_flashing(b"torrent-a", 1, now + Duration::from_millis(10)));
        assert!(!state.is_piece_flashing(b"torrent-a", 2, now + Duration::from_millis(10)));
    }

    #[test]
    fn swarm_availability_flash_suppresses_new_peer_initial_bitfield() {
        let now = Instant::now();
        let duration = Duration::from_millis(350);
        let mut state = SwarmAvailabilityFlashState::default();

        state.update_from_peers(b"torrent-a", &[], 3, now, duration);

        let peers = vec![PeerInfo {
            address: "127.0.0.1:7001".to_string(),
            bitfield: vec![true, false, true],
            ..Default::default()
        }];
        let next = now + Duration::from_millis(10);
        state.update_from_peers(b"torrent-a", &peers, 3, next, duration);

        assert!(!state.has_active_flash(next));
        assert!(!state.is_piece_flashing(b"torrent-a", 0, next));
        assert!(!state.is_piece_flashing(b"torrent-a", 2, next));
    }

    #[test]
    fn swarm_availability_flash_tracks_known_peer_new_piece() {
        let now = Instant::now();
        let duration = Duration::from_millis(350);
        let mut state = SwarmAvailabilityFlashState::default();

        let peers = vec![PeerInfo {
            address: "127.0.0.1:7001".to_string(),
            bitfield: vec![true, false, false],
            ..Default::default()
        }];
        state.update_from_peers(b"torrent-a", &peers, 3, now, duration);

        let peers = vec![PeerInfo {
            address: "127.0.0.1:7001".to_string(),
            bitfield: vec![true, true, false],
            ..Default::default()
        }];
        let next = now + Duration::from_millis(10);
        state.update_from_peers(b"torrent-a", &peers, 3, next, duration);

        assert!(!state.is_piece_flashing(b"torrent-a", 0, next));
        assert!(state.is_piece_flashing(b"torrent-a", 1, next));
        assert!(!state.is_piece_flashing(b"torrent-a", 2, next));
    }

    #[test]
    fn swarm_availability_flash_ignores_later_new_peer_bitfield() {
        let now = Instant::now();
        let duration = Duration::from_millis(350);
        let mut state = SwarmAvailabilityFlashState::default();

        let peers = vec![PeerInfo {
            address: "127.0.0.1:7001".to_string(),
            bitfield: vec![false, false, false],
            ..Default::default()
        }];
        state.update_from_peers(b"torrent-a", &peers, 3, now, duration);

        let peers = vec![
            PeerInfo {
                address: "127.0.0.1:7001".to_string(),
                bitfield: vec![false, false, false],
                ..Default::default()
            },
            PeerInfo {
                address: "127.0.0.1:7002".to_string(),
                bitfield: vec![true, true, false],
                ..Default::default()
            },
        ];
        let next = now + Duration::from_millis(10);
        state.update_from_peers(b"torrent-a", &peers, 3, next, duration);

        assert!(!state.has_active_flash(next));
    }

    #[test]
    fn should_draw_every_frame_in_welcome_mode() {
        assert!(App::should_draw_this_frame(&AppMode::Welcome, false, false));
        assert!(App::should_draw_this_frame(&AppMode::Welcome, true, false));
    }

    #[test]
    fn should_only_draw_dirty_in_power_saving_mode() {
        assert!(!App::should_draw_this_frame(
            &AppMode::PowerSaving,
            false,
            true
        ));
        assert!(App::should_draw_this_frame(
            &AppMode::PowerSaving,
            true,
            false
        ));
    }

    #[test]
    fn should_only_draw_dirty_in_peer_management_mode() {
        assert!(!App::should_draw_this_frame(
            &AppMode::PeerManagement,
            false,
            true
        ));
        assert!(App::should_draw_this_frame(
            &AppMode::PeerManagement,
            true,
            false
        ));
    }

    #[test]
    fn should_only_draw_dirty_in_journal_mode() {
        assert!(!App::should_draw_this_frame(&AppMode::Journal, false, true));
        assert!(App::should_draw_this_frame(&AppMode::Journal, true, false));
    }

    #[test]
    fn normal_animation_gate_is_idle_for_static_state() {
        let app_state = AppState::default();

        assert!(!App::normal_mode_animation_active(
            &app_state,
            UiLayoutMode::Auto,
            None,
            Instant::now()
        ));
    }

    #[test]
    fn normal_animation_gate_detects_active_swarm_availability_flash() {
        let now = Instant::now();
        let mut app_state = AppState::default();
        app_state.ui.swarm_availability_flash.update(
            b"torrent-a",
            vec![0, 0],
            now,
            SWARM_AVAILABILITY_FLASH_DURATION,
        );
        app_state.ui.swarm_availability_flash.update(
            b"torrent-a",
            vec![1, 0],
            now + Duration::from_millis(1),
            SWARM_AVAILABILITY_FLASH_DURATION,
        );

        assert!(App::normal_mode_animation_active(
            &app_state,
            UiLayoutMode::Auto,
            None,
            now + Duration::from_millis(2)
        ));
    }

    #[test]
    fn normal_animation_gate_ignores_held_disk_health_when_disk_is_idle() {
        let app_state = AppState {
            disk_health_state_level: 1,
            disk_health_ema: 0.55,
            disk_health_peak_hold: 0.70,
            ..Default::default()
        };

        assert!(!App::normal_mode_animation_active(
            &app_state,
            UiLayoutMode::Auto,
            None,
            Instant::now()
        ));
    }

    #[test]
    fn normal_animation_gate_detects_current_disk_activity() {
        let app_state = AppState {
            avg_disk_read_bps: 1,
            ..Default::default()
        };

        assert!(App::normal_mode_animation_active(
            &app_state,
            UiLayoutMode::Auto,
            None,
            Instant::now()
        ));
    }

    #[test]
    fn classic_disk_health_phase_keeps_its_idle_wobble() {
        let app_state = AppState::default();

        assert_eq!(
            App::disk_health_phase_speed(&app_state),
            super::DISK_IDLE_WOBBLE_PHASE_SPEED
        );
    }

    #[test]
    fn classic_disk_health_phase_keeps_transfer_direction() {
        let download_dominant = AppState {
            avg_download_history: vec![90_000_000],
            avg_upload_history: vec![10_000_000],
            ..Default::default()
        };
        let upload_dominant = AppState {
            avg_download_history: vec![10_000_000],
            avg_upload_history: vec![90_000_000],
            ..Default::default()
        };

        assert!(App::disk_health_phase_speed(&download_dominant) > 0.0);
        assert!(App::disk_health_phase_speed(&upload_dominant) < 0.0);
    }

    #[test]
    fn alternate_disk_health_phase_stops_without_disk_io() {
        let mut app_state = AppState::default();
        app_state.ui.visualization_focus.disk_health = DiskHealthVisualization::SeekPendulum;

        assert_eq!(App::disk_health_phase_speed(&app_state), 0.0);
    }

    #[test]
    fn alternate_disk_health_phase_uses_disk_throughput_not_network_direction() {
        let mut read_activity = AppState {
            avg_disk_read_bps: 90_000_000,
            avg_download_history: vec![0],
            avg_upload_history: vec![500_000_000],
            ..Default::default()
        };
        read_activity.ui.visualization_focus.disk_health = DiskHealthVisualization::SeekPendulum;
        let mut write_activity = AppState {
            avg_disk_write_bps: 90_000_000,
            avg_download_history: vec![500_000_000],
            avg_upload_history: vec![0],
            ..Default::default()
        };
        write_activity.ui.visualization_focus.disk_health = DiskHealthVisualization::StorageDial;

        assert_eq!(
            App::disk_health_phase_speed(&read_activity),
            App::disk_health_phase_speed(&write_activity)
        );
        assert!(App::disk_health_phase_speed(&read_activity) > 0.0);
    }

    #[test]
    fn alternate_disk_health_phase_increases_with_disk_throughput() {
        let mut slow = AppState {
            avg_disk_read_bps: 8_000_000,
            ..Default::default()
        };
        slow.ui.visualization_focus.disk_health = DiskHealthVisualization::StorageDial;
        let mut fast = AppState {
            avg_disk_read_bps: 256_000_000,
            ..Default::default()
        };
        fast.ui.visualization_focus.disk_health = DiskHealthVisualization::StorageDial;

        assert!(App::disk_health_phase_speed(&fast) > App::disk_health_phase_speed(&slow));
        assert!(App::disk_health_phase_speed(&fast) <= super::DISK_MAX_TRANSFER_PHASE_SPEED);
    }

    #[test]
    fn normal_animation_gate_detects_selected_torrent_activity() {
        let mut app_state = AppState::default();
        let info_hash = b"active_hash".to_vec();
        let mut torrent = TorrentDisplayState::default();
        torrent.latest_state.blocks_in_history = vec![0, 0, 1];
        app_state.torrents.insert(info_hash.clone(), torrent);
        app_state.torrent_list_order.push(info_hash);

        assert!(App::normal_mode_animation_active(
            &app_state,
            UiLayoutMode::Auto,
            None,
            Instant::now()
        ));
    }

    #[test]
    fn normal_animation_gate_keeps_alternate_peer_stream_live() {
        let mut app_state = AppState {
            screen_area: Rect::new(0, 0, 200, 60),
            ..Default::default()
        };
        let info_hash = b"peer_stream_hash".to_vec();
        let mut torrent = TorrentDisplayState::default();
        torrent.latest_state.number_of_successfully_connected_peers = 1;
        app_state.torrents.insert(info_hash.clone(), torrent);
        app_state.torrent_list_order.push(info_hash);
        app_state.ui.visualization_focus.peer_stream = PeerStreamVisualization::HelixExchange;

        assert!(App::normal_mode_animation_active(
            &app_state,
            UiLayoutMode::Horizontal,
            None,
            Instant::now()
        ));
    }

    #[test]
    fn normal_animation_gate_stops_alternate_peer_stream_when_hidden() {
        let mut app_state = AppState {
            screen_area: Rect::new(0, 0, 80, 60),
            ..Default::default()
        };
        let info_hash = b"hidden_peer_stream_hash".to_vec();
        let mut torrent = TorrentDisplayState::default();
        torrent.latest_state.number_of_successfully_connected_peers = 1;
        app_state.torrents.insert(info_hash.clone(), torrent);
        app_state.torrent_list_order.push(info_hash);
        app_state.ui.visualization_focus.peer_stream = PeerStreamVisualization::HelixExchange;

        assert!(!App::normal_mode_animation_active(
            &app_state,
            UiLayoutMode::Vertical,
            None,
            Instant::now()
        ));
    }

    #[test]
    fn normal_animation_gate_stops_alternate_peer_stream_when_too_narrow_to_draw() {
        let mut app_state = AppState {
            screen_area: Rect::new(0, 0, 70, 60),
            ..Default::default()
        };
        let info_hash = b"narrow_peer_stream_hash".to_vec();
        let mut torrent = TorrentDisplayState::default();
        torrent.latest_state.number_of_successfully_connected_peers = 1;
        app_state.torrents.insert(info_hash.clone(), torrent);
        app_state.torrent_list_order.push(info_hash);
        app_state.ui.visualization_focus.peer_stream = PeerStreamVisualization::HelixExchange;

        let layout_ctx = LayoutContext::new(
            app_state.screen_area,
            &app_state,
            UiLayoutMode::Horizontal,
            DEFAULT_SIDEBAR_PERCENT,
        );
        let peer_stream = calculate_layout(app_state.screen_area, &layout_ctx)
            .peer_stream
            .expect("forced horizontal layout should include peer stream");
        assert!((2..PEER_STREAM_MIN_WIDTH).contains(&peer_stream.width));
        assert!(peer_stream.height >= PEER_STREAM_MIN_HEIGHT);

        assert!(!App::normal_mode_animation_active(
            &app_state,
            UiLayoutMode::Horizontal,
            None,
            Instant::now()
        ));
    }

    #[test]
    fn visualization_selections_restore_from_settings() {
        let settings = Settings {
            peer_stream_visualization: PeerStreamVisualization::HelixExchange,
            disk_health_visualization: DiskHealthVisualization::StorageDial,
            dht_visualization: DhtVisualization::RelayRibbon,
            ..Default::default()
        };

        let restored = VisualizationFocusState::from_settings(&settings);

        assert!(!restored.active);
        assert_eq!(restored.selected, VisualizationFocusPanel::Chart);
        assert_eq!(restored.peer_stream, PeerStreamVisualization::HelixExchange);
        assert_eq!(restored.disk_health, DiskHealthVisualization::StorageDial);
        assert_eq!(restored.dht, DhtVisualization::RelayRibbon);
    }

    #[test]
    fn normal_animation_gate_detects_dht_query_activity() {
        let app_state = AppState::default();
        let telemetry = DhtWaveTelemetry {
            inflight_ipv4_queries: 1,
            ..Default::default()
        };

        assert!(App::normal_mode_animation_active(
            &app_state,
            UiLayoutMode::Auto,
            Some(&telemetry),
            Instant::now()
        ));
    }

    #[test]
    fn normal_idle_check_uses_light_polling_cadence_for_fast_targets() {
        assert_eq!(
            App::normal_idle_frame_check_interval(DataRate::Rate60s.frame_interval()),
            super::NORMAL_IDLE_FRAME_CHECK_INTERVAL
        );
    }

    #[test]
    fn normal_idle_check_preserves_slower_targets() {
        assert_eq!(
            App::normal_idle_frame_check_interval(DataRate::Rate1s.frame_interval()),
            DataRate::Rate1s.frame_interval()
        );
    }

    #[test]
    fn data_rate_sixty_uses_precise_frame_interval() {
        assert!(
            (DataRate::Rate60s.frame_interval().as_secs_f64() - (1.0 / 60.0)).abs() < 0.000_001
        );
    }

    #[test]
    fn draw_scheduler_recovers_from_late_timer_wakeups() {
        let start = Instant::now();
        let interval = DataRate::Rate60s.frame_interval();
        let mut next_draw_time = start;

        App::advance_next_draw_time(
            &mut next_draw_time,
            start + Duration::from_millis(2),
            interval,
        );

        assert!(next_draw_time < start + interval + Duration::from_millis(1));
    }

    #[test]
    fn ui_fps_counter_measures_drawn_frames_per_second() {
        let mut ui = UiState::default();
        let start = Instant::now();

        ui.record_drawn_frame(start);
        for frame in 1..=44 {
            ui.record_drawn_frame(start + Duration::from_secs_f64(frame as f64 / 44.0));
        }

        assert_eq!(ui.measured_fps, Some(44.0));
    }

    #[test]
    fn ui_responsiveness_metrics_measure_wake_lag_and_draw_cost() {
        let mut ui = UiState::default();
        let start = Instant::now();
        let frame_interval = Duration::from_millis(20);

        ui.record_frame_wake(start, start + Duration::from_millis(5), frame_interval);
        ui.record_draw_duration(Duration::from_millis(10), frame_interval);

        assert_eq!(ui.frame_wake_lag_ratio_ema, Some(0.25));
        assert_eq!(ui.frame_wake_lag_secs_ema, Some(0.005));
        assert_eq!(ui.frame_draw_ratio_ema, Some(0.5));
    }

    #[test]
    fn wake_lag_peer_throttle_does_not_reduce_below_minimum() {
        let mut throttle = WakeLagPeerThrottle::default();

        let change = throttle
            .update(
                Some(super::WAKE_LAG_PEER_THROTTLE_BAD_RATIO),
                Some(super::WAKE_LAG_PEER_THROTTLE_BAD_MIN_DELAY.as_secs_f64()),
                65,
                super::WAKE_LAG_PEER_THROTTLE_MIN_PEERS,
                10,
            )
            .expect("throttle should reduce under bad wake lag");

        assert_eq!(change.previous_peer_limit, 65);
        assert_eq!(
            change.current_peer_limit,
            super::WAKE_LAG_PEER_THROTTLE_MIN_PEERS
        );
        assert_eq!(
            throttle.effective_peer_limit(65, super::WAKE_LAG_PEER_THROTTLE_MIN_PEERS),
            super::WAKE_LAG_PEER_THROTTLE_MIN_PEERS
        );
    }

    #[test]
    fn wake_lag_peer_throttle_ignores_high_ratio_with_small_absolute_delay() {
        let mut throttle = WakeLagPeerThrottle::default();

        let change = throttle.update(
            Some(super::WAKE_LAG_PEER_THROTTLE_BAD_RATIO),
            Some(super::WAKE_LAG_PEER_THROTTLE_BAD_MIN_DELAY.as_secs_f64() / 2.0),
            65,
            super::WAKE_LAG_PEER_THROTTLE_MIN_PEERS,
            10,
        );

        assert_eq!(change, None);
        assert_eq!(throttle.effective_peer_limit(65, 8), 65);
    }

    #[test]
    fn wake_lag_peer_throttle_uses_download_floor_when_provided() {
        let mut throttle = WakeLagPeerThrottle::default();
        let base_peer_limit: usize = 100;
        let download_floor = base_peer_limit
            .saturating_mul(super::WAKE_LAG_PEER_THROTTLE_DOWNLOAD_FLOOR_PERCENT)
            .saturating_div(100);

        let change = throttle
            .update(
                Some(super::WAKE_LAG_PEER_THROTTLE_BAD_RATIO),
                Some(super::WAKE_LAG_PEER_THROTTLE_BAD_MIN_DELAY.as_secs_f64()),
                base_peer_limit,
                download_floor,
                10,
            )
            .expect("throttle should reduce under bad wake lag");

        assert_eq!(change.previous_peer_limit, base_peer_limit);
        assert_eq!(change.current_peer_limit, download_floor);
        assert_eq!(
            throttle.effective_peer_limit(base_peer_limit, download_floor),
            download_floor
        );
    }

    fn test_dht_wave_targets(
        amplitude: f64,
        harmonic_amplitude: f64,
        frequency: f64,
        phase_speed: f64,
        crest_bias: f64,
        bootstrap_ratio: f64,
    ) -> DhtWaveTargets {
        DhtWaveTargets {
            amplitude,
            harmonic_amplitude,
            frequency,
            phase_speed,
            crest_bias,
            bootstrap_ratio,
            query_load: 0.0,
        }
    }

    fn test_dht_wave_signal_at(wave: &DhtWaveUiState, x: f64) -> f64 {
        let theta = x * wave.frequency;
        let envelope = 0.84 + 0.16 * (theta * 0.33 + wave.phase * 0.28).sin();
        let dht_amplitude =
            (wave.amplitude + wave.discovery_boost + wave.query_surge).clamp(0.05, 0.78);
        let carrier = wave.crest_bias * 0.35
            + envelope * dht_amplitude * (theta + wave.phase).sin()
            + wave.harmonic_amplitude * ((theta * 2.35) - wave.phase * 0.72).sin();
        carrier.clamp(-1.1, 1.1)
    }

    #[test]
    fn dht_wave_targets_remain_reactive_above_ten_queries() {
        let mut status = DhtStatus::default();
        status.health.enabled = true;
        status.health.firewalled = Some(false);
        status.health.cached_ipv4_routes = 900;

        let q10 = dht_wave_targets(
            &status,
            &DhtWaveTelemetry {
                inflight_ipv4_queries: 10,
                ..Default::default()
            },
        );
        let q48 = dht_wave_targets(
            &status,
            &DhtWaveTelemetry {
                inflight_ipv4_queries: 48,
                ..Default::default()
            },
        );
        let q96 = dht_wave_targets(
            &status,
            &DhtWaveTelemetry {
                inflight_ipv4_queries: 96,
                ..Default::default()
            },
        );

        assert!(q10.query_load < 0.30);
        assert!(q48.query_load > q10.query_load);
        assert!(q96.query_load > q48.query_load);
        assert!(q48.amplitude > q10.amplitude);
        assert!(q48.harmonic_amplitude > q10.harmonic_amplitude);
        assert!(q48.frequency > q10.frequency);
        assert!(q48.phase_speed > q10.phase_speed);
    }

    #[test]
    fn dht_wave_state_smooths_60fps_target_transition() {
        let frame_dt = 1.0 / 60.0;
        let idle = test_dht_wave_targets(0.01, 0.004, 0.08, 0.03, 0.0, 1.0);
        let busy = test_dht_wave_targets(0.36, 0.12, 0.24, 1.2, 0.10, 1.0);
        let busy = DhtWaveTargets {
            query_load: 0.75,
            ..busy
        };
        let mut wave = DhtWaveUiState::default();

        advance_dht_wave_state(&mut wave, idle, 0.0, frame_dt);

        let mut previous = wave.clone();
        let mut max_amplitude_delta: f64 = 0.0;
        let mut max_frequency_delta: f64 = 0.0;
        let mut max_discovery_delta: f64 = 0.0;
        let mut max_sample_delta: f64 = 0.0;

        for frame in 0..120 {
            let (target, discovery_boost) = if frame < 60 {
                (idle, 0.0)
            } else {
                (busy, 0.18)
            };
            advance_dht_wave_state(&mut wave, target, discovery_boost, frame_dt);

            max_amplitude_delta =
                max_amplitude_delta.max((wave.amplitude - previous.amplitude).abs());
            max_frequency_delta =
                max_frequency_delta.max((wave.frequency - previous.frequency).abs());
            max_discovery_delta =
                max_discovery_delta.max((wave.discovery_boost - previous.discovery_boost).abs());

            let previous_sample = test_dht_wave_signal_at(&previous, 18.0);
            let sample = test_dht_wave_signal_at(&wave, 18.0);
            max_sample_delta = max_sample_delta.max((sample - previous_sample).abs());

            previous = wave.clone();
        }

        assert!(
            max_amplitude_delta < 0.06,
            "amplitude delta too large at 60fps: {max_amplitude_delta}"
        );
        assert!(
            max_frequency_delta < 0.03,
            "frequency delta too large at 60fps: {max_frequency_delta}"
        );
        assert!(
            max_discovery_delta < 0.04,
            "discovery delta too large at 60fps: {max_discovery_delta}"
        );
        assert!(
            max_sample_delta < 0.12,
            "signal delta too large at 60fps: {max_sample_delta}"
        );
    }

    #[test]
    fn dht_wave_state_stays_continuous_across_phase_wrap() {
        let frame_dt = 1.0 / 60.0;
        let target = test_dht_wave_targets(0.34, 0.11, 0.22, 2.0, 0.08, 1.0);
        let phase_step = frame_dt * target.phase_speed;
        let mut wave = DhtWaveUiState {
            phase: DHT_WAVE_PHASE_WRAP_PERIOD - (phase_step * 0.5),
            amplitude: target.amplitude,
            harmonic_amplitude: target.harmonic_amplitude,
            frequency: target.frequency,
            phase_speed: target.phase_speed,
            crest_bias: target.crest_bias,
            bootstrap_ratio: target.bootstrap_ratio,
            discovery_boost: 0.0,
            query_load: target.query_load,
            query_surge: 0.0,
            initialized: true,
        };

        let before = test_dht_wave_signal_at(&wave, 18.0);
        advance_dht_wave_state(&mut wave, target, 0.0, frame_dt);
        let after = test_dht_wave_signal_at(&wave, 18.0);

        assert!(
            (after - before).abs() < 0.08,
            "wave jumped too much across wrap: {}",
            (after - before).abs()
        );
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
    fn completion_helper_marks_metadata_pending_incomplete() {
        let metrics = TorrentMetrics::default();

        assert!(torrent_is_effectively_incomplete(&metrics));
        assert_eq!(torrent_completion_percent(&metrics), 0.0);
    }

    #[test]
    fn completion_helper_marks_zero_piece_complete_when_metrics_say_complete() {
        let metrics = TorrentMetrics {
            is_complete: true,
            ..Default::default()
        };

        assert!(!torrent_is_effectively_incomplete(&metrics));
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
                selected_header: SelectedHeader::Torrent(ColumnId::Name),
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
    fn sort_and_filter_respects_pinned_sort_over_availability_priority() {
        let mut app_state = AppState {
            torrent_sort: (TorrentSortColumn::Name, SortDirection::Ascending),
            torrent_sort_pinned: true,
            ..Default::default()
        };

        let unavailable_hash = b"unavailable_hash".to_vec();
        let available_hash = b"available_hash".to_vec();

        let mut unavailable = mock_display("zeta-sample.iso", 0);
        unavailable.latest_state.data_available = false;

        let available = mock_display("alpha-sample.iso", 0);

        app_state
            .torrents
            .insert(unavailable_hash.clone(), unavailable);
        app_state.torrents.insert(available_hash.clone(), available);

        sort_and_filter_torrent_list_state(&mut app_state);

        assert_eq!(
            app_state.torrent_list_order,
            vec![available_hash, unavailable_hash]
        );
    }

    #[test]
    fn sort_and_filter_progress_descending_puts_most_complete_first() {
        let mut app_state = AppState {
            torrent_sort: (TorrentSortColumn::Progress, SortDirection::Descending),
            torrent_sort_pinned: true,
            ..Default::default()
        };

        let lower_hash = b"lower_hash".to_vec();
        let higher_hash = b"higher_hash".to_vec();

        let mut lower = mock_display("sample-lower.iso", 0);
        lower.latest_state.number_of_pieces_total = 10;
        lower.latest_state.number_of_pieces_completed = 2;

        let mut higher = mock_display("sample-higher.iso", 0);
        higher.latest_state.number_of_pieces_total = 10;
        higher.latest_state.number_of_pieces_completed = 8;

        app_state.torrents.insert(lower_hash.clone(), lower);
        app_state.torrents.insert(higher_hash.clone(), higher);

        sort_and_filter_torrent_list_state(&mut app_state);

        assert_eq!(app_state.torrent_list_order, vec![higher_hash, lower_hash]);
    }

    #[test]
    fn sort_and_filter_progress_ascending_puts_zero_progress_first() {
        let mut app_state = AppState {
            torrent_sort: (TorrentSortColumn::Progress, SortDirection::Ascending),
            torrent_sort_pinned: true,
            ..Default::default()
        };

        let zero_hash = b"zero_hash".to_vec();
        let partial_hash = b"partial_hash".to_vec();

        let mut zero = mock_display("sample-zero.iso", 0);
        zero.latest_state.number_of_pieces_total = 10;
        zero.latest_state.number_of_pieces_completed = 0;

        let mut partial = mock_display("sample-partial.iso", 0);
        partial.latest_state.number_of_pieces_total = 10;
        partial.latest_state.number_of_pieces_completed = 5;

        app_state.torrents.insert(zero_hash.clone(), zero);
        app_state.torrents.insert(partial_hash.clone(), partial);

        sort_and_filter_torrent_list_state(&mut app_state);

        assert_eq!(app_state.torrent_list_order, vec![zero_hash, partial_hash]);
    }

    #[test]
    fn stats_autosort_refresh_reorders_torrents_when_sort_mode_changes() {
        let mut app_state = AppState {
            torrent_sort: (TorrentSortColumn::Up, SortDirection::Descending),
            peer_sort: (PeerSortColumn::UL, SortDirection::Descending),
            ..Default::default()
        };
        let slow_hash = b"slow_hash".to_vec();
        let fast_hash = b"fast_hash".to_vec();

        let mut slow = mock_display("sample-slow.iso", 0);
        slow.latest_state.data_available = true;
        slow.smoothed_upload_speed_bps = 10;

        let mut fast = mock_display("sample-fast.iso", 0);
        fast.latest_state.data_available = true;
        fast.smoothed_upload_speed_bps = 10_000;

        app_state.torrents.insert(slow_hash.clone(), slow);
        app_state.torrents.insert(fast_hash.clone(), fast);
        app_state.torrent_list_order = vec![slow_hash.clone(), fast_hash.clone()];

        let changed = refresh_autosort_after_stats(
            &mut app_state,
            (TorrentSortColumn::Down, SortDirection::Descending),
            (PeerSortColumn::DL, SortDirection::Descending),
        );

        assert!(changed);
        assert_eq!(app_state.torrent_list_order, vec![fast_hash, slow_hash]);
    }

    #[test]
    fn stats_autosort_refresh_reorders_unpinned_torrents_when_speeds_change() {
        let mut app_state = AppState {
            torrent_sort: (TorrentSortColumn::Down, SortDirection::Descending),
            torrent_sort_pinned: false,
            peer_sort: (PeerSortColumn::DL, SortDirection::Descending),
            ..Default::default()
        };
        let old_fast_hash = b"old_fast_hash".to_vec();
        let new_fast_hash = b"new_fast_hash".to_vec();

        let mut old_fast = mock_display("sample-old-fast.iso", 0);
        old_fast.latest_state.data_available = true;
        old_fast.smoothed_download_speed_bps = 10;

        let mut new_fast = mock_display("sample-new-fast.iso", 0);
        new_fast.latest_state.data_available = true;
        new_fast.smoothed_download_speed_bps = 10_000;

        app_state.torrents.insert(old_fast_hash.clone(), old_fast);
        app_state.torrents.insert(new_fast_hash.clone(), new_fast);
        app_state.torrent_list_order = vec![old_fast_hash.clone(), new_fast_hash.clone()];

        let changed = refresh_autosort_after_stats(
            &mut app_state,
            (TorrentSortColumn::Down, SortDirection::Descending),
            (PeerSortColumn::DL, SortDirection::Descending),
        );

        assert!(changed);
        assert_eq!(
            app_state.torrent_list_order,
            vec![new_fast_hash, old_fast_hash]
        );
    }

    #[test]
    fn stats_autosort_refresh_preserves_pinned_torrent_order_when_speeds_change() {
        let mut app_state = AppState {
            torrent_sort: (TorrentSortColumn::Down, SortDirection::Descending),
            torrent_sort_pinned: true,
            peer_sort: (PeerSortColumn::DL, SortDirection::Descending),
            ..Default::default()
        };
        let old_fast_hash = b"pinned_old_fast".to_vec();
        let new_fast_hash = b"pinned_new_fast".to_vec();

        let mut old_fast = mock_display("sample-pinned-old.iso", 0);
        old_fast.latest_state.data_available = true;
        old_fast.smoothed_download_speed_bps = 10;

        let mut new_fast = mock_display("sample-pinned-new.iso", 0);
        new_fast.latest_state.data_available = true;
        new_fast.smoothed_download_speed_bps = 10_000;

        app_state.torrents.insert(old_fast_hash.clone(), old_fast);
        app_state.torrents.insert(new_fast_hash.clone(), new_fast);
        app_state.torrent_list_order = vec![old_fast_hash.clone(), new_fast_hash.clone()];

        let changed = refresh_autosort_after_stats(
            &mut app_state,
            (TorrentSortColumn::Down, SortDirection::Descending),
            (PeerSortColumn::DL, SortDirection::Descending),
        );

        assert!(!changed);
        assert_eq!(
            app_state.torrent_list_order,
            vec![old_fast_hash, new_fast_hash]
        );
    }

    #[test]
    fn stats_autosort_refresh_clears_finished_progress_priority_pin() {
        let mut app_state = AppState {
            torrent_sort: (TorrentSortColumn::Progress, SortDirection::Ascending),
            torrent_sort_pinned: true,
            peer_sort: (PeerSortColumn::DL, SortDirection::Descending),
            ..Default::default()
        };
        let complete_hash = b"complete_hash".to_vec();
        let mut complete = mock_display("sample-complete.iso", 0);
        complete.latest_state.data_available = true;
        complete.latest_state.number_of_pieces_total = 10;
        complete.latest_state.number_of_pieces_completed = 10;
        app_state.torrents.insert(complete_hash.clone(), complete);
        app_state.torrent_list_order = vec![complete_hash];

        let changed = refresh_autosort_after_stats(
            &mut app_state,
            (TorrentSortColumn::Progress, SortDirection::Ascending),
            (PeerSortColumn::DL, SortDirection::Descending),
        );

        assert!(changed);
        assert!(!app_state.torrent_sort_pinned);
        assert_eq!(
            app_state.torrent_sort,
            (TorrentSortColumn::Progress, SortDirection::Ascending)
        );
    }

    #[test]
    fn stats_autosort_refresh_keeps_progress_priority_pin_while_unfinished() {
        let mut app_state = AppState {
            torrent_sort: (TorrentSortColumn::Progress, SortDirection::Ascending),
            torrent_sort_pinned: true,
            peer_sort: (PeerSortColumn::DL, SortDirection::Descending),
            ..Default::default()
        };
        let incomplete_hash = b"incomplete_hash".to_vec();
        let mut incomplete = mock_display("sample-incomplete.iso", 0);
        incomplete.latest_state.data_available = true;
        incomplete.latest_state.number_of_pieces_total = 10;
        incomplete.latest_state.number_of_pieces_completed = 4;
        app_state
            .torrents
            .insert(incomplete_hash.clone(), incomplete);
        app_state.torrent_list_order = vec![incomplete_hash];

        let changed = refresh_autosort_after_stats(
            &mut app_state,
            (TorrentSortColumn::Progress, SortDirection::Ascending),
            (PeerSortColumn::DL, SortDirection::Descending),
        );

        assert!(!changed);
        assert!(app_state.torrent_sort_pinned);
        assert_eq!(
            app_state.torrent_sort,
            (TorrentSortColumn::Progress, SortDirection::Ascending)
        );
    }

    #[test]
    fn stats_autosort_refresh_keeps_progress_priority_pin_for_metadata_pending() {
        let mut app_state = AppState {
            torrent_sort: (TorrentSortColumn::Progress, SortDirection::Ascending),
            torrent_sort_pinned: true,
            peer_sort: (PeerSortColumn::DL, SortDirection::Descending),
            ..Default::default()
        };
        let pending_hash = b"metadata_pending_hash".to_vec();
        let mut pending = mock_display("sample-metadata-pending.iso", 0);
        pending.latest_state.data_available = true;
        pending.latest_state.number_of_pieces_total = 0;
        pending.latest_state.number_of_pieces_completed = 0;
        pending.latest_state.is_complete = false;
        app_state.torrents.insert(pending_hash.clone(), pending);
        app_state.torrent_list_order = vec![pending_hash];

        let changed = refresh_autosort_after_stats(
            &mut app_state,
            (TorrentSortColumn::Progress, SortDirection::Ascending),
            (PeerSortColumn::DL, SortDirection::Descending),
        );

        assert!(!changed);
        assert!(app_state.torrent_sort_pinned);
        assert_eq!(
            app_state.torrent_sort,
            (TorrentSortColumn::Progress, SortDirection::Ascending)
        );
    }

    #[test]
    fn stats_autosort_refresh_keeps_non_progress_user_pin_after_completion() {
        let mut app_state = AppState {
            torrent_sort: (TorrentSortColumn::Name, SortDirection::Ascending),
            torrent_sort_pinned: true,
            peer_sort: (PeerSortColumn::DL, SortDirection::Descending),
            ..Default::default()
        };
        let complete_hash = b"user_pin_complete_hash".to_vec();
        let mut complete = mock_display("sample-user-pin-complete.iso", 0);
        complete.latest_state.data_available = true;
        complete.latest_state.number_of_pieces_total = 10;
        complete.latest_state.number_of_pieces_completed = 10;
        app_state.torrents.insert(complete_hash.clone(), complete);
        app_state.torrent_list_order = vec![complete_hash];

        let changed = refresh_autosort_after_stats(
            &mut app_state,
            (TorrentSortColumn::Name, SortDirection::Ascending),
            (PeerSortColumn::DL, SortDirection::Descending),
        );

        assert!(!changed);
        assert!(app_state.torrent_sort_pinned);
        assert_eq!(
            app_state.torrent_sort,
            (TorrentSortColumn::Name, SortDirection::Ascending)
        );
    }

    #[test]
    fn stats_autosort_refresh_clears_progress_pin_for_completed_probe_issue() {
        let mut app_state = AppState {
            torrent_sort: (TorrentSortColumn::Progress, SortDirection::Ascending),
            torrent_sort_pinned: true,
            peer_sort: (PeerSortColumn::DL, SortDirection::Descending),
            ..Default::default()
        };
        let unavailable_hash = b"complete_unavailable_hash".to_vec();
        let available_hash = b"complete_available_hash".to_vec();

        let mut unavailable = mock_display("sample-zeta.iso", 0);
        unavailable.latest_state.data_available = false;
        unavailable.latest_state.number_of_pieces_total = 10;
        unavailable.latest_state.number_of_pieces_completed = 10;

        let mut available = mock_display("sample-alpha.iso", 0);
        available.latest_state.data_available = true;
        available.latest_state.number_of_pieces_total = 10;
        available.latest_state.number_of_pieces_completed = 10;

        app_state
            .torrents
            .insert(unavailable_hash.clone(), unavailable);
        app_state.torrents.insert(available_hash.clone(), available);
        app_state.torrent_list_order = vec![available_hash.clone(), unavailable_hash.clone()];

        let changed = refresh_autosort_after_stats(
            &mut app_state,
            (TorrentSortColumn::Progress, SortDirection::Ascending),
            (PeerSortColumn::DL, SortDirection::Descending),
        );

        assert!(changed);
        assert!(!app_state.torrent_sort_pinned);
        assert_eq!(app_state.torrent_list_order[0], unavailable_hash);
    }

    #[test]
    fn stats_autosort_refresh_marks_change_when_only_peer_sort_changes() {
        let mut app_state = AppState {
            torrent_sort: (TorrentSortColumn::Down, SortDirection::Descending),
            peer_sort: (PeerSortColumn::UL, SortDirection::Descending),
            ..Default::default()
        };

        let changed = refresh_autosort_after_stats(
            &mut app_state,
            (TorrentSortColumn::Down, SortDirection::Descending),
            (PeerSortColumn::DL, SortDirection::Descending),
        );

        assert!(changed);
    }

    #[test]
    fn align_unpinned_sort_uses_upload_when_only_upload_is_visible() {
        let mut app_state = AppState {
            torrent_sort: (TorrentSortColumn::Down, SortDirection::Descending),
            ..Default::default()
        };
        let hash = b"hash_a".to_vec();
        let mut torrent = mock_display("sample-upload.iso", 0);
        torrent.latest_state.data_available = true;
        torrent.smoothed_upload_speed_bps = 4_096;
        app_state.torrents.insert(hash, torrent);

        align_unpinned_sort_with_visible_activity(&mut app_state);

        assert_eq!(
            app_state.torrent_sort,
            (TorrentSortColumn::Up, SortDirection::Descending)
        );
    }

    #[test]
    fn align_unpinned_sort_preserves_current_sort_when_idle_and_complete() {
        let mut app_state = AppState {
            torrent_sort: (TorrentSortColumn::Down, SortDirection::Descending),
            ..Default::default()
        };
        let hash = b"hash_a".to_vec();
        let mut torrent = mock_display("sample-complete.iso", 0);
        torrent.latest_state.data_available = true;
        torrent.latest_state.number_of_pieces_total = 10;
        torrent.latest_state.number_of_pieces_completed = 10;
        app_state.torrents.insert(hash, torrent);

        align_unpinned_sort_with_visible_activity(&mut app_state);

        assert_eq!(
            app_state.torrent_sort,
            (TorrentSortColumn::Down, SortDirection::Descending)
        );
    }

    #[test]
    fn align_unpinned_sort_preserves_pinned_torrent_sort() {
        let mut app_state = AppState {
            torrent_sort: (TorrentSortColumn::Down, SortDirection::Descending),
            torrent_sort_pinned: true,
            ..Default::default()
        };
        let hash = b"hash_a".to_vec();
        let mut torrent = mock_display("sample-upload.iso", 0);
        torrent.latest_state.data_available = true;
        torrent.smoothed_upload_speed_bps = 4_096;
        app_state.torrents.insert(hash, torrent);

        align_unpinned_sort_with_visible_activity(&mut app_state);

        assert_eq!(
            app_state.torrent_sort,
            (TorrentSortColumn::Down, SortDirection::Descending)
        );
    }

    #[test]
    fn align_unpinned_sort_uses_peer_upload_when_only_peer_upload_is_visible() {
        let mut app_state = AppState {
            peer_sort: (PeerSortColumn::DL, SortDirection::Descending),
            ..Default::default()
        };
        let hash = b"hash_a".to_vec();
        let mut torrent = mock_display("sample-peer-upload.iso", 1);
        torrent.latest_state.peers[0].upload_speed_bps = 2_048;
        app_state.torrent_list_order = vec![hash.clone()];
        app_state.torrents.insert(hash, torrent);

        align_unpinned_sort_with_visible_activity(&mut app_state);

        assert_eq!(
            app_state.peer_sort,
            (PeerSortColumn::UL, SortDirection::Descending)
        );
    }

    #[test]
    fn align_unpinned_sort_keeps_peer_speed_sort_when_peer_activity_is_idle() {
        let mut app_state = AppState {
            is_seeding: true,
            peer_sort: (PeerSortColumn::Address, SortDirection::Ascending),
            ..Default::default()
        };
        let hash = b"hash_a".to_vec();
        app_state
            .torrents
            .insert(hash.clone(), mock_display("sample-peer-idle.iso", 1));
        app_state.torrent_list_order = vec![hash];

        align_unpinned_sort_with_visible_activity(&mut app_state);

        assert_eq!(
            app_state.peer_sort,
            (PeerSortColumn::UL, SortDirection::Descending)
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
    fn runtime_torrent_settings_changed_ignores_network_updates() {
        let old = crate::config::Settings::default();
        let mut new = old.clone();
        new.network_binding.mode = crate::networking::NetworkBindingMode::Interface;
        new.network_binding.interface = Some("interface-test0".to_string());

        assert!(!runtime_torrent_settings_changed(&old, &new));
    }

    #[test]
    fn runtime_torrent_settings_changed_detects_runtime_inputs() {
        let old = crate::config::Settings::default();
        let mut torrent_update = old.clone();
        torrent_update
            .torrents
            .push(crate::config::TorrentSettings {
                torrent_or_magnet: "magnet:?xt=urn:btih:5555555555555555555555555555555555555555"
                    .to_string(),
                name: "Sample Hotel".to_string(),
                ..Default::default()
            });
        assert!(runtime_torrent_settings_changed(&old, &torrent_update));

        let mut path_update = old.clone();
        path_update.default_download_folder = Some("/tmp/example-downloads".into());
        assert!(runtime_torrent_settings_changed(&old, &path_update));
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
    fn strict_system_dns_policy_reports_its_reduced_guarantee() {
        let strict_system_dns = crate::networking::NetworkBindingConfig {
            mode: crate::networking::NetworkBindingMode::Interface,
            interface: Some("interface-test".to_string()),
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: None,
            ipv6_address: None,
            dns_policy: crate::networking::DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        assert!(network_policy_warning(&strict_system_dns)
            .is_some_and(|warning| warning.contains("system resolver")));

        let strict_bound_dns = crate::networking::NetworkBindingConfig {
            dns_policy: crate::networking::DnsPolicy::Bound,
            dns_servers: vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 53))],
            ..strict_system_dns
        };
        assert!(network_policy_warning(&strict_bound_dns).is_none());
        assert!(
            network_policy_warning(&crate::networking::NetworkBindingConfig::default()).is_none()
        );
    }

    #[test]
    fn incoming_handshake_validator_accepts_expected_peer_protocol_prefix() {
        let mut handshake = vec![0u8; 68];
        handshake[0] = BITTORRENT_PROTOCOL_STR.len() as u8;
        handshake[1..(1 + BITTORRENT_PROTOCOL_STR.len())].copy_from_slice(BITTORRENT_PROTOCOL_STR);

        assert!(is_valid_incoming_bittorrent_handshake(&handshake));
    }

    #[test]
    fn incoming_handshake_validator_rejects_unexpected_peer_protocol_prefix() {
        let mut handshake = vec![0u8; 68];
        handshake[0] = BITTORRENT_PROTOCOL_STR.len() as u8;
        handshake[1..(1 + BITTORRENT_PROTOCOL_STR.len())].copy_from_slice(b"NotTorrent protocol");

        assert!(!is_valid_incoming_bittorrent_handshake(&handshake));
    }

    #[test]
    fn peer_policy_sync_makes_initial_and_updated_policy_tui_visible() {
        let first_ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10));
        let second_ip = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 20));
        let blocked_until = SystemTime::now() + Duration::from_secs(3_600);
        let initial_policy = Arc::new(crate::peer_manager::PeerPolicy::from_blocked_until(
            HashMap::from([(first_ip, blocked_until)]),
        ));
        let (policy_tx, mut policy_rx) = watch::channel(Arc::clone(&initial_policy));
        let mut app_state = AppState::default();
        app_state.ui.needs_redraw = false;

        assert_eq!(
            super::sync_peer_policy_to_app_state(&mut app_state, &mut policy_rx),
            1
        );
        assert!(Arc::ptr_eq(&app_state.peer_policy, &initial_policy));
        assert!(app_state.ui.needs_redraw);

        let updated_policy = Arc::new(crate::peer_manager::PeerPolicy::from_blocked_until(
            HashMap::from([(first_ip, blocked_until), (second_ip, blocked_until)]),
        ));
        policy_tx.send_replace(Arc::clone(&updated_policy));
        app_state.ui.needs_redraw = false;

        assert_eq!(
            super::sync_peer_policy_to_app_state(&mut app_state, &mut policy_rx),
            2
        );
        assert!(Arc::ptr_eq(&app_state.peer_policy, &updated_policy));
        assert!(app_state.ui.needs_redraw);
    }

    #[test]
    fn peer_manager_view_sync_makes_tracked_peers_tui_visible() {
        let initial_view = Arc::new(crate::peer_manager::PeerManagerView::default());
        let (view_tx, mut view_rx) = watch::channel(Arc::clone(&initial_view));
        let mut app_state = AppState::default();
        app_state.ui.needs_redraw = false;

        assert_eq!(
            super::sync_peer_manager_view_to_app_state(&mut app_state, &mut view_rx),
            0
        );
        assert!(Arc::ptr_eq(&app_state.peer_manager_view, &initial_view));
        assert!(app_state.ui.needs_redraw);

        let updated_view = Arc::new(crate::peer_manager::PeerManagerView {
            registered_torrents: 1,
            metrics_updates: 2,
            tracked_peers: vec![crate::peer_manager::PeerManagerTrackedPeer {
                torrent_info_hash: vec![0x41; 20],
                torrent_name: "Silver Current".to_string(),
                ip: IpAddr::V4(Ipv4Addr::new(203, 0, 113, 90)),
                is_active: true,
                endpoints: Vec::new(),
                downloaded_evidence_bytes: 1_024,
                uploaded_evidence_bytes: 2_048,
                total_downloaded_bytes: 1_024,
                total_uploaded_bytes: 2_048,
                connection_count: 2,
                disconnect_count: 1,
                transfer_threshold_bytes: 256 * 1024 * 1024,
                reconnect_count: 1,
                reconnect_limit: 10,
                reconnect_window_secs: 10,
                last_seen: Some(SystemTime::now()),
                clients: vec!["Unknown (ZZ1234)".to_string()],
            }],
        });
        view_tx.send_replace(Arc::clone(&updated_view));
        app_state.ui.needs_redraw = false;

        assert_eq!(
            super::sync_peer_manager_view_to_app_state(&mut app_state, &mut view_rx),
            1
        );
        assert!(Arc::ptr_eq(&app_state.peer_manager_view, &updated_view));
        assert_eq!(app_state.peer_manager_view.registered_torrents, 1);
        assert_eq!(
            app_state.peer_manager_view.tracked_peers[0].torrent_name,
            "Silver Current"
        );
        assert_eq!(
            app_state.peer_manager_view.tracked_peers[0].clients,
            vec!["Unknown (ZZ1234)".to_string()]
        );
        assert!(app_state.ui.needs_redraw);
    }

    #[test]
    fn peer_manager_view_is_only_adopted_while_its_screen_is_open() {
        assert!(!super::should_sync_peer_manager_view(&AppMode::Normal));
        assert!(super::should_sync_peer_manager_view(
            &AppMode::PeerManagement
        ));
    }

    #[tokio::test]
    async fn shutdown_keeps_peer_manager_alive_until_torrent_managers_quiesce() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let info_hash = vec![9; 20];
        let (manager_tx, mut manager_rx) = mpsc::channel(1);
        app.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_tx);

        let manager_event_tx = app.manager_event_tx.clone();
        let policy_probe = app.peer_policy_rx.clone();
        let manager_task = tokio::spawn(async move {
            let command = time::timeout(Duration::from_secs(1), manager_rx.recv())
                .await
                .expect("torrent manager received shutdown before timeout")
                .expect("torrent manager command channel remained open");
            assert!(matches!(command, ManagerCommand::Shutdown));
            assert!(
                policy_probe.has_changed().is_ok(),
                "peer manager stopped before the torrent manager quiesced"
            );
            manager_event_tx
                .send(ManagerEvent::DeletionComplete(info_hash, Ok(())))
                .await
                .expect("acknowledge torrent manager shutdown");
        });

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        app.shutdown_sequence(&mut terminal).await;
        manager_task.await.expect("join fake torrent manager");
    }

    #[tokio::test]
    async fn blocked_peer_policy_rejects_inbound_handshake_before_manager_routing() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let info_hash = vec![7; 20];
        let (manager_tx, mut manager_rx) = mpsc::channel(1);
        app.torrent_manager_incoming_peer_txs
            .insert(info_hash.clone(), manager_tx);

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind test listener");
        let client = TcpStream::connect(listener.local_addr().expect("listener address"));
        let (client, accepted) = tokio::join!(client, listener.accept());
        let _client = client.expect("connect test peer");
        let (stream, remote_addr) = accepted.expect("accept test peer");
        let connection = crate::networking::TcpPeerTransport::incoming(stream, remote_addr);
        let permit = app
            .resource_manager
            .acquire_peer_connection()
            .await
            .expect("acquire peer permit");

        let (_policy_tx, policy_rx) = watch::channel(Arc::new(
            crate::peer_manager::PeerPolicy::from_blocked_until(HashMap::from([(
                remote_addr.ip(),
                SystemTime::now() + Duration::from_secs(3_600),
            )])),
        ));
        app.peer_policy_rx = policy_rx;

        let mut buffer = vec![0; 68];
        buffer[28..48].copy_from_slice(&info_hash);
        app.route_incoming_peer_handshake(super::IncomingPeerHandshake {
            connection,
            buffer,
            permit,
        });

        assert!(
            time::timeout(Duration::from_millis(50), manager_rx.recv())
                .await
                .is_err(),
            "blocked inbound peer was routed to the torrent manager"
        );
    }

    #[tokio::test]
    async fn blocked_peer_policy_closes_inbound_before_waiting_for_handshake() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind test listener");
        let client = TcpStream::connect(listener.local_addr().expect("listener address"));
        let (client, accepted) = tokio::join!(client, listener.accept());
        let mut client = client.expect("connect test peer");
        let (stream, remote_addr) = accepted.expect("accept test peer");
        let connection = crate::networking::TcpPeerTransport::incoming(stream, remote_addr);
        let (_policy_tx, policy_rx) = watch::channel(Arc::new(
            crate::peer_manager::PeerPolicy::from_blocked_until(HashMap::from([(
                remote_addr.ip(),
                SystemTime::now() + Duration::from_secs(3_600),
            )])),
        ));
        app.peer_policy_rx = policy_rx;

        app.handle_incoming_peer(connection).await;

        let mut byte = [0_u8; 1];
        let bytes_read = time::timeout(Duration::from_millis(100), client.read(&mut byte))
            .await
            .expect("blocked connection should close without waiting for a handshake")
            .expect("read blocked connection closure");
        assert_eq!(bytes_read, 0);
        let _ = app.shutdown_tx.send(());
    }

    #[test]
    fn inbound_peer_transport_status_tracks_the_full_transport_family_matrix() {
        let mut status = InboundPeerTransportStatus::default();

        status.mark_seen(PeerTransportKind::Tcp, true);
        status.mark_seen(PeerTransportKind::Tcp, false);
        status.mark_seen(PeerTransportKind::Utp, true);
        status.mark_seen(PeerTransportKind::Utp, false);

        assert_eq!(
            status,
            InboundPeerTransportStatus {
                tcp_ipv4_seen: true,
                tcp_ipv6_seen: true,
                utp_ipv4_seen: true,
                utp_ipv6_seen: true,
            }
        );
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

        app.mark_peer_port_open(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 6681),
            PeerTransportKind::Tcp,
        );

        assert!(app.app_state.externally_accessable_port_v4);
        assert!(!app.app_state.externally_accessable_port_v6);
        assert!(app.app_state.inbound_peer_transports.tcp_ipv4_seen);
        assert!(!app.app_state.inbound_peer_transports.utp_ipv4_seen);

        app.mark_peer_port_open(
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 6681),
            PeerTransportKind::Utp,
        );

        assert!(app.app_state.externally_accessable_port_v4);
        assert!(app.app_state.externally_accessable_port_v6);
        assert!(app.app_state.inbound_peer_transports.utp_ipv6_seen);
        assert!(!app.app_state.inbound_peer_transports.tcp_ipv6_seen);
    }

    #[tokio::test]
    async fn mark_port_open_command_ignores_stale_network_generation() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let active_scope_id = install_test_network_activation(&mut app, 41);
        let peer_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 6681);

        app.handle_app_command(AppCommand::MarkPortOpen {
            peer_addr,
            transport: PeerTransportKind::Tcp,
            scope_id: NetworkScopeId::for_test(active_scope_id.generation_id().saturating_add(1)),
        })
        .await;

        assert!(!app.app_state.externally_accessable_port_v4);
        assert!(!app.app_state.inbound_peer_transports.tcp_ipv4_seen);

        app.handle_app_command(AppCommand::MarkPortOpen {
            peer_addr,
            transport: PeerTransportKind::Tcp,
            scope_id: active_scope_id,
        })
        .await;

        assert!(app.app_state.externally_accessable_port_v4);
        assert!(app.app_state.inbound_peer_transports.tcp_ipv4_seen);
    }

    #[tokio::test]
    async fn mark_port_open_command_ignores_stale_same_generation_activation() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        install_test_network_activation(&mut app, 43);
        let active_network = app.network_activation.try_active().expect("active network");
        let stale_scope_id = active_network.scope().id();
        let listen_port = active_network.listen_port();
        let lease = app
            .network_handle
            .try_lease_generation(stale_scope_id.generation_id())
            .expect("current generation lease");
        let replacement_scope = app
            .network_activation_publisher
            .prepare(lease)
            .expect("prepare replacement activation");
        let replacement_scope_id = replacement_scope.id();
        app.network_activation_publisher
            .activate_prepared(replacement_scope, listen_port)
            .expect("activate replacement scope");
        let peer_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 6681);

        assert_eq!(
            stale_scope_id.generation_id(),
            replacement_scope_id.generation_id()
        );
        assert_ne!(stale_scope_id, replacement_scope_id);

        app.handle_app_command(AppCommand::MarkPortOpen {
            peer_addr,
            transport: PeerTransportKind::Tcp,
            scope_id: stale_scope_id,
        })
        .await;

        assert!(!app.app_state.externally_accessable_port_v4);
        assert!(!app.app_state.inbound_peer_transports.tcp_ipv4_seen);

        app.handle_app_command(AppCommand::MarkPortOpen {
            peer_addr,
            transport: PeerTransportKind::Tcp,
            scope_id: replacement_scope_id,
        })
        .await;

        assert!(app.app_state.externally_accessable_port_v4);
        assert!(app.app_state.inbound_peer_transports.tcp_ipv4_seen);
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
        app.mark_peer_port_open(mapped_addr, PeerTransportKind::Utp);

        assert!(app.app_state.externally_accessable_port_v4);
        assert!(!app.app_state.externally_accessable_port_v6);
        assert!(app.app_state.inbound_peer_transports.utp_ipv4_seen);
        assert!(!app.app_state.inbound_peer_transports.utp_ipv6_seen);
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
        let mut activation_rx = app.network_activation.subscribe();
        app.app_state.externally_accessable_port_v4 = true;
        app.app_state.externally_accessable_port_v6 = true;
        app.app_state.externally_accessable_port_v4_highlight_until =
            Some(Instant::now() + Duration::from_secs(1));
        app.app_state.externally_accessable_port_v6_highlight_until =
            Some(Instant::now() + Duration::from_secs(1));
        app.app_state.inbound_peer_transports.tcp_ipv4_seen = true;
        app.app_state.inbound_peer_transports.utp_ipv6_seen = true;

        assert!(app.rebind_listener(0).await);

        let bound_port = app.client_configs.client_port;
        assert_ne!(bound_port, 0);

        activation_rx
            .changed()
            .await
            .expect("activation channel should remain open");
        assert_eq!(activation_listen_port(&activation_rx), Some(bound_port));
        assert_eq!(
            app.app_state.inbound_peer_transports,
            InboundPeerTransportStatus::default()
        );
        assert!(!app.app_state.externally_accessable_port_v4);
        assert!(!app.app_state.externally_accessable_port_v6);
        assert!(app
            .app_state
            .externally_accessable_port_v4_highlight_until
            .is_none());
        assert!(app
            .app_state
            .externally_accessable_port_v6_highlight_until
            .is_none());

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn forwarded_port_rebind_closes_old_udp_and_supports_a_b_a_with_live_utp() {
        async fn wait_for_ipv4_udp_release(port: u16) {
            time::timeout(Duration::from_millis(500), async {
                loop {
                    match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).await {
                        Ok(probe) => {
                            drop(probe);
                            return;
                        }
                        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                            time::sleep(Duration::from_millis(1)).await;
                        }
                        Err(error) => panic!("could not probe old IPv4 UDP port {port}: {error}"),
                    }
                }
            })
            .await
            .expect("old IPv4 UDP port must be released promptly");
        }

        let _guard = lock_shared_env();
        let mut app = App::new(
            crate::config::Settings {
                client_port: 0,
                bootstrap_nodes: Vec::new(),
                ..Default::default()
            },
            AppRuntimeMode::Normal,
        )
        .await
        .expect("create app");
        let generation_id = app
            .active_network_generation_id()
            .expect("active network generation");
        let port_a = app
            .listener
            .as_ref()
            .and_then(ListenerSet::local_port)
            .expect("listener A port");
        let network_lease = app.network_handle.try_lease().expect("network lease");
        let live_session_a = time::timeout(
            Duration::from_secs(2),
            UtpPeerTransport::connect_from_port(
                &network_lease,
                SocketAddr::from((Ipv4Addr::LOCALHOST, port_a)),
                port_a,
            ),
        )
        .await
        .expect("listener A uTP connect should not hang")
        .expect("connect to listener A over uTP");

        let mut activation_rx = app.network_activation.subscribe();
        let replacement_probe = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .expect("reserve replacement port");
        let port_b = replacement_probe
            .local_addr()
            .expect("replacement address")
            .port();
        drop(replacement_probe);
        assert_ne!(port_a, port_b);

        assert!(app.rebind_listener(port_b).await);
        assert_eq!(app.active_network_generation_id(), Some(generation_id));
        activation_rx.changed().await.expect("port B publication");
        assert_eq!(activation_listen_port(&activation_rx), Some(port_b));
        wait_for_ipv4_udp_release(port_a).await;
        let session_b = time::timeout(
            Duration::from_secs(2),
            UtpPeerTransport::connect_from_port(
                &network_lease,
                SocketAddr::from((Ipv4Addr::LOCALHOST, port_b)),
                port_b,
            ),
        )
        .await
        .expect("listener B uTP connect should not hang")
        .expect("connect to listener B over uTP");

        assert!(app.rebind_listener(port_a).await);
        assert_eq!(app.active_network_generation_id(), Some(generation_id));
        activation_rx
            .changed()
            .await
            .expect("restored port A publication");
        assert_eq!(activation_listen_port(&activation_rx), Some(port_a));
        wait_for_ipv4_udp_release(port_b).await;
        let restored_session_a = time::timeout(
            Duration::from_secs(2),
            UtpPeerTransport::connect_from_port(
                &network_lease,
                SocketAddr::from((Ipv4Addr::LOCALHOST, port_a)),
                port_a,
            ),
        )
        .await
        .expect("restored listener A uTP connect should not hang")
        .expect("connect to restored listener A over uTP");

        drop((live_session_a, session_b, restored_session_a));
        app.network_handle
            .shutdown()
            .await
            .expect("shutdown network supervisor");
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn forwarded_port_rebind_publishes_activation_before_bounded_dht_wait() {
        let mut app = App::new(
            crate::config::Settings {
                client_port: 0,
                bootstrap_nodes: Vec::new(),
                ..Default::default()
            },
            AppRuntimeMode::Normal,
        )
        .await
        .expect("create app");
        let recorder = TestDhtRecorder::with_blocked_reconfigure();
        app.dht_service = DhtService::from_test_recorder(recorder.clone());
        app.dht_status_rx = app.dht_service.subscribe_status();
        let mut activation_rx = app.network_activation.subscribe();

        let replacement_probe = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .expect("reserve replacement port");
        let replacement_port = replacement_probe
            .local_addr()
            .expect("replacement address")
            .port();
        drop(replacement_probe);

        {
            let rebind =
                app.rebind_listener_with_dht_timeout(replacement_port, Duration::from_millis(100));
            tokio::pin!(rebind);
            tokio::select! {
                biased;
                active = activation_rx.wait_for(|state| matches!(
                    state,
                    crate::networking::NetworkActivationState::Active(active)
                        if active.listen_port() == replacement_port
                )) => {
                    active.expect("activation channel should remain open");
                }
                result = &mut rebind => {
                    panic!("rebind returned before its DHT timeout: {result}")
                }
                _ = time::sleep(Duration::from_millis(40)) => {
                    panic!("replacement activation waited for DHT")
                }
            }
            assert_eq!(
                activation_listen_port(&activation_rx),
                Some(replacement_port)
            );
            assert!(time::timeout(Duration::from_secs(1), &mut rebind)
                .await
                .expect("wedged DHT wait must be bounded"));
        }
        assert_eq!(
            app.listener.as_ref().and_then(ListenerSet::local_port),
            Some(replacement_port)
        );
        assert!(recorder
            .recorded_reconfigures()
            .iter()
            .any(|config| config.port == replacement_port));
        recorder.release_reconfigure();

        app.network_handle
            .shutdown()
            .await
            .expect("shutdown network supervisor");
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn forwarded_port_rebind_allows_a_healthy_slow_dht_reconfigure() {
        let mut app = App::new(
            crate::config::Settings {
                client_port: 0,
                bootstrap_nodes: Vec::new(),
                ..Default::default()
            },
            AppRuntimeMode::Normal,
        )
        .await
        .expect("create app");
        let recorder = TestDhtRecorder::with_blocked_reconfigure();
        app.dht_service = DhtService::from_test_recorder(recorder.clone());
        app.dht_status_rx = app.dht_service.subscribe_status();
        let mut activation_rx = app.network_activation.subscribe();

        let replacement_probe = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .expect("reserve replacement port");
        let replacement_port = replacement_probe
            .local_addr()
            .expect("replacement address")
            .port();
        drop(replacement_probe);

        let release = recorder.clone();
        tokio::spawn(async move {
            time::sleep(Duration::from_millis(75)).await;
            release.release_reconfigure();
        });
        let started = Instant::now();
        assert!(
            app.rebind_listener_with_dht_timeout(replacement_port, Duration::from_millis(500))
                .await
        );
        assert!(started.elapsed() >= Duration::from_millis(50));
        activation_rx
            .changed()
            .await
            .expect("replacement port publication");
        assert_eq!(
            activation_listen_port(&activation_rx),
            Some(replacement_port)
        );

        app.network_handle
            .shutdown()
            .await
            .expect("shutdown network supervisor");
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn randomized_client_port_binds_an_ephemeral_port_and_preserves_the_mode() {
        let settings = crate::config::Settings {
            client_port: 6681,
            randomize_client_port: true,
            ..Default::default()
        };
        let app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");

        assert_ne!(app.client_configs.client_port, 0);
        assert!(app.client_configs.randomize_client_port);

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn unguarded_test_app_does_not_start_a_process_global_persistence_writer() {
        let app = App::new(crate::config::Settings::default(), AppRuntimeMode::Normal)
            .await
            .expect("create app");

        assert!(app.persistence_tx.is_none());
        assert!(app.persistence_task.is_none());
        assert!(app.event_journal_persistence_tx.is_none());
        assert!(app.event_journal_persistence_task.is_none());

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn startup_interface_override_stays_runtime_only_and_initial_block_is_journaled() {
        let _guard = lock_shared_env();
        let _temp_paths = configure_temp_app_paths_for_test();
        let mut runtime_settings = crate::config::Settings::default();
        let persisted_binding = runtime_settings.network_binding.clone();
        runtime_settings.network_binding.mode =
            crate::networking::runtime::NetworkBindingMode::Interface;
        runtime_settings.network_binding.interface =
            Some("missing-startup-interface-test".to_string());
        runtime_settings.network_binding.enable_ipv4 = true;
        runtime_settings.network_binding.enable_ipv6 = false;

        let mut app = App::new_with_lock_and_network_persistence_override(
            runtime_settings,
            AppRuntimeMode::Normal,
            None,
            Some(persisted_binding.clone()),
        )
        .await
        .expect("create blocked app with startup override");

        assert_eq!(
            app.client_configs.network_binding.interface.as_deref(),
            Some("missing-startup-interface-test")
        );
        assert!(matches!(
            app.network_activation.status(),
            crate::networking::NetworkActivationStatus::Blocked { .. }
        ));
        assert!(app
            .app_state
            .event_journal_state
            .entries
            .iter()
            .any(|entry| entry.event_type == EventType::NetworkBlocked));

        app.flush_persistence_writer().await;
        let persisted_journal = crate::persistence::event_journal::load_event_journal_state();
        assert!(persisted_journal
            .entries
            .iter()
            .any(|entry| entry.event_type == EventType::NetworkBlocked));

        let (persistence_tx, persistence_rx) = watch::channel(None);
        app.persistence_tx = Some(persistence_tx);
        app.save_state_to_disk();
        let payload = persistence_rx
            .borrow()
            .clone()
            .expect("startup override should queue a persistence payload");
        assert_eq!(payload.settings.network_binding, persisted_binding);
        assert_eq!(
            app.client_configs.network_binding.interface.as_deref(),
            Some("missing-startup-interface-test")
        );

        let runtime_binding = app.client_configs.network_binding.clone();
        let mut reloaded_persisted_binding = persisted_binding.clone();
        reloaded_persisted_binding.enable_ipv6 = false;
        let mut reloaded_settings = app.client_configs.clone();
        reloaded_settings.network_binding = reloaded_persisted_binding.clone();
        reloaded_settings.output_status_interval += 1;
        app.apply_reloaded_settings(reloaded_settings).await;

        assert_eq!(app.client_configs.network_binding, runtime_binding);
        assert_eq!(
            app.persisted_network_binding_override,
            Some(reloaded_persisted_binding)
        );

        let mut explicit_settings = app.client_configs.clone();
        explicit_settings.network_binding.interface =
            Some("explicit-saved-interface-test".to_string());
        app.apply_settings_update(explicit_settings.clone(), true)
            .await;
        let explicit_payload = persistence_rx
            .borrow()
            .clone()
            .expect("explicit binding update should queue a persistence payload");
        assert_eq!(
            explicit_payload.settings.network_binding,
            explicit_settings.network_binding
        );
        assert!(app.persisted_network_binding_override.is_none());

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn shared_follower_persists_host_network_journal_without_full_state_writer() {
        let _guard = lock_shared_env();
        let _temp_paths = configure_temp_app_paths_for_test();
        let mut app = App::new(
            crate::config::Settings::default(),
            AppRuntimeMode::SharedFollower,
        )
        .await
        .expect("create shared follower");
        assert!(app.persistence_tx.is_none());
        assert!(app.event_journal_persistence_tx.is_some());

        install_test_network_activation(&mut app, 52);
        app.app_state.network_activation_status = Some(app.network_activation.status());
        app.block_network_activation("Interface unavailable");
        app.flush_persistence_writer().await;

        let persisted = crate::persistence::event_journal::load_event_journal_state();
        assert!(persisted.entries.iter().any(|entry| {
            entry.scope == crate::persistence::event_journal::EventScope::Host
                && entry.event_type == EventType::NetworkBlocked
                && entry.message.as_deref() == Some("Interface unavailable")
        }));

        let _ = app.shutdown_tx.send(());
    }

    #[test]
    fn random_client_port_reload_normalization_preserves_the_bound_port() {
        let current_settings = crate::config::Settings {
            client_port: 49152,
            randomize_client_port: true,
            ..Default::default()
        };
        let mut reloaded_settings = current_settings.clone();
        reloaded_settings.client_port = 0;

        super::preserve_bound_random_client_port(&current_settings, &mut reloaded_settings);

        assert_eq!(reloaded_settings.client_port, current_settings.client_port);
        assert!(reloaded_settings.randomize_client_port);
    }

    #[tokio::test]
    async fn random_client_port_reload_preserves_the_bound_port() {
        let settings = crate::config::Settings {
            client_port: 6681,
            randomize_client_port: true,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let bound_port = app.client_configs.client_port;
        let mut reloaded_settings = app.client_configs.clone();
        reloaded_settings.client_port = 0;
        reloaded_settings.output_status_interval += 1;

        app.apply_settings_update(reloaded_settings, false).await;

        assert_eq!(app.client_configs.client_port, bound_port);
        assert!(app.client_configs.randomize_client_port);
        assert_eq!(
            app.listener.as_ref().and_then(ListenerSet::local_port),
            Some(bound_port)
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn pinning_current_random_port_preserves_listener_and_persists_fixed_mode() {
        let _guard = lock_shared_env();
        let _temp_paths = configure_temp_app_paths_for_test();
        let settings = crate::config::Settings {
            client_port: 6681,
            randomize_client_port: true,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        app.flush_persistence_writer().await;
        let (persistence_tx, persistence_rx) = watch::channel(None);
        app.persistence_tx = Some(persistence_tx);
        let bound_port = app
            .listener
            .as_ref()
            .and_then(ListenerSet::local_port)
            .expect("random listener should expose its bound port");
        let mut fixed_settings = app.client_configs.clone();
        fixed_settings.client_port = bound_port;
        fixed_settings.randomize_client_port = false;

        app.apply_settings_update(fixed_settings, true).await;

        assert_eq!(
            app.listener.as_ref().and_then(ListenerSet::local_port),
            Some(bound_port)
        );
        assert_eq!(app.client_configs.client_port, bound_port);
        assert!(!app.client_configs.randomize_client_port);
        assert!(app.app_state.system_error.is_none());

        let persisted = persistence_rx
            .borrow()
            .clone()
            .expect("settings update should queue persistence");
        assert_eq!(persisted.settings.client_port, bound_port);
        assert!(!persisted.settings.randomize_client_port);

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn forwarded_port_hot_reload_clears_random_mode_and_persists_fixed_port() {
        let _guard = lock_shared_env();
        let _temp_paths = configure_temp_app_paths_for_test();
        let settings = crate::config::Settings {
            client_port: 6681,
            randomize_client_port: true,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        app.flush_persistence_writer().await;
        let (persistence_tx, persistence_rx) = watch::channel(None);
        app.persistence_tx = Some(persistence_tx);
        let probe_listener = super::bind_peer_listener(&app.network_handle, 0)
            .await
            .expect("reserve forwarded port");
        let forwarded_port = probe_listener
            .as_ref()
            .and_then(ListenerSet::local_port)
            .expect("forwarded listener should expose its port");
        drop(probe_listener);
        let port_file = _temp_paths.path().join("forwarded-port");
        std::fs::write(&port_file, forwarded_port.to_string()).expect("write forwarded port file");

        app.handle_port_change(port_file).await;

        assert_eq!(app.client_configs.client_port, forwarded_port);
        assert!(!app.client_configs.randomize_client_port);
        assert_eq!(
            app.listener.as_ref().and_then(ListenerSet::local_port),
            Some(forwarded_port)
        );

        let persisted = persistence_rx
            .borrow()
            .clone()
            .expect("forwarded port update should queue persistence");
        assert_eq!(persisted.settings.client_port, forwarded_port);
        assert!(!persisted.settings.randomize_client_port);

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn matching_forwarded_port_pins_current_random_listener() {
        let _guard = lock_shared_env();
        let _temp_paths = configure_temp_app_paths_for_test();
        let settings = crate::config::Settings {
            client_port: 6681,
            randomize_client_port: true,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let bound_port = app
            .listener
            .as_ref()
            .and_then(ListenerSet::local_port)
            .expect("random listener should expose its bound port");
        let port_file = _temp_paths.path().join("matching-forwarded-port");
        std::fs::write(&port_file, bound_port.to_string()).expect("write forwarded port file");

        app.handle_port_change(port_file).await;
        app.flush_persistence_writer().await;

        assert_eq!(app.client_configs.client_port, bound_port);
        assert!(!app.client_configs.randomize_client_port);
        assert_eq!(
            app.listener.as_ref().and_then(ListenerSet::local_port),
            Some(bound_port)
        );

        let persisted = crate::config::load_settings().expect("reload persisted settings");
        assert_eq!(persisted.client_port, bound_port);
        assert!(!persisted.randomize_client_port);

        let _ = app.shutdown_tx.send(());
        set_app_paths_override_for_tests(None);
    }

    #[tokio::test]
    async fn forwarded_port_hot_reload_is_applied_after_blocked_network_recovers() {
        let _guard = lock_shared_env();
        let _temp_paths = configure_temp_app_paths_for_test();
        let settings = crate::config::Settings {
            client_port: 6681,
            randomize_client_port: true,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");

        let mut blocked_settings = app.client_configs.clone();
        blocked_settings.network_binding = crate::networking::NetworkBindingConfig {
            mode: crate::networking::runtime::NetworkBindingMode::Interface,
            interface: Some("missing-forwarded-port-interface-test".to_string()),
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: None,
            ipv6_address: None,
            dns_policy: crate::networking::DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        app.apply_settings_update(blocked_settings, false).await;
        wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Blocked(_)))
            .await;
        app.handle_network_state_changed().await;

        let probe_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("reserve forwarded port");
        let forwarded_port = probe_listener
            .local_addr()
            .expect("forwarded listener address")
            .port();
        drop(probe_listener);
        let port_file = _temp_paths.path().join("forwarded-port-during-recovery");
        std::fs::write(&port_file, forwarded_port.to_string()).expect("write forwarded port file");

        app.handle_port_change(port_file).await;
        app.flush_persistence_writer().await;

        assert!(app.listener.is_none());
        assert_eq!(app.client_configs.client_port, forwarded_port);
        assert!(!app.client_configs.randomize_client_port);
        let persisted = crate::config::load_settings().expect("reload persisted settings");
        assert_eq!(persisted.client_port, forwarded_port);
        assert!(!persisted.randomize_client_port);

        let mut restored_settings = app.client_configs.clone();
        restored_settings.network_binding = crate::networking::NetworkBindingConfig::default();
        app.apply_settings_update(restored_settings, false).await;
        wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Ready(_))).await;
        app.handle_network_state_changed().await;

        assert_eq!(
            app.listener.as_ref().and_then(ListenerSet::local_port),
            Some(forwarded_port)
        );
        assert_eq!(app.client_configs.client_port, forwarded_port);
        assert!(!app.client_configs.randomize_client_port);

        app.network_handle.shutdown().await.unwrap();
        let _ = app.shutdown_tx.send(());
        set_app_paths_override_for_tests(None);
    }

    #[tokio::test]
    async fn network_activation_transitions_are_persisted_in_the_host_journal() {
        let _guard = lock_shared_env();
        let _temp_paths = configure_temp_app_paths_for_test();
        let mut app = App::new(crate::config::Settings::default(), AppRuntimeMode::Normal)
            .await
            .expect("create app");
        install_test_network_activation(&mut app, 41);
        app.app_state.network_activation_status = Some(app.network_activation.status());
        let initial_entry_count = app.app_state.event_journal_state.entries.len();
        let initial_network_event_count = app.app_state.event_journal_state.entries
            [..initial_entry_count]
            .iter()
            .filter(|entry| entry.category == EventCategory::Network)
            .count();
        let active = app
            .network_activation
            .try_active()
            .expect("initial network activation");
        let generation_id = active.scope().id().generation_id();
        let listen_port = active.listen_port();
        drop(active);
        if let Some(status) = app.app_state.network_runtime_status.as_mut() {
            status.interface = Some("interface-test0".to_string());
        }

        app.publish_network_activation_pending(Some(generation_id));
        app.publish_network_activation_pending(Some(generation_id));
        app.block_network_activation(
            "interface interface-test0 was not found: Device not configured (os error 6)",
        );
        app.block_network_activation(
            "interface interface-test0 was not found: Device not configured (os error 6)",
        );

        let blocked_events = &app.app_state.event_journal_state.entries[initial_entry_count..];
        assert_eq!(blocked_events.len(), 2);
        assert_eq!(blocked_events[0].event_type, EventType::NetworkRebinding);
        assert_eq!(blocked_events[1].event_type, EventType::NetworkBlocked);
        assert!(blocked_events[1]
            .message
            .as_deref()
            .is_some_and(|message| message.contains("Device not configured (os error 6)")));

        let lease = app
            .network_handle
            .try_lease_generation(generation_id)
            .expect("current network generation lease");
        let scope = app
            .prepare_network_activation(lease)
            .expect("prepare replacement activation");
        app.activate_network_scope(scope, listen_port)
            .expect("restore network activation");

        let network_events = &app.app_state.event_journal_state.entries[initial_entry_count..];
        assert_eq!(network_events.len(), 4);
        assert_eq!(network_events[2].event_type, EventType::NetworkRebinding);
        assert_eq!(network_events[3].event_type, EventType::NetworkRestored);
        assert!(matches!(
            &network_events[3].details,
            EventDetails::Network {
                generation_id: Some(_),
                listen_port: Some(_),
                ..
            }
        ));

        app.flush_persistence_writer().await;
        let persisted = crate::persistence::event_journal::load_event_journal_state();
        assert_eq!(
            persisted
                .entries
                .iter()
                .filter(|entry| entry.category == EventCategory::Network)
                .count(),
            initial_network_event_count + 4
        );

        let _ = app.shutdown_tx.send(());
        set_app_paths_override_for_tests(None);
    }

    #[tokio::test]
    async fn forwarded_port_hot_reload_retries_a_blocked_listener_generation() {
        let _guard = lock_shared_env();
        let _temp_paths = configure_temp_app_paths_for_test();
        let settings = crate::config::Settings {
            client_port: 0,
            randomize_client_port: true,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let generation_id = app
            .active_network_generation_id()
            .expect("active startup generation");
        app.network_handle
            .block_generation(generation_id, "simulated replacement listener failure")
            .await
            .expect("block active generation");
        wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Blocked(_)))
            .await;
        app.handle_network_state_changed().await;

        let probe_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("reserve replacement forwarded port");
        let forwarded_port = probe_listener
            .local_addr()
            .expect("replacement forwarded listener address")
            .port();
        drop(probe_listener);
        let port_file = _temp_paths.path().join("forwarded-port-retry");
        std::fs::write(&port_file, forwarded_port.to_string()).expect("write forwarded port file");

        app.handle_port_change(port_file).await;
        wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Ready(_))).await;
        app.handle_network_state_changed().await;

        assert_eq!(
            app.listener.as_ref().and_then(ListenerSet::local_port),
            Some(forwarded_port)
        );
        assert_eq!(app.client_configs.client_port, forwarded_port);
        assert!(!app.client_configs.randomize_client_port);

        app.network_handle.shutdown().await.unwrap();
        let _ = app.shutdown_tx.send(());
        set_app_paths_override_for_tests(None);
    }

    #[test]
    fn running_client_rejects_cli_only_move_requests() {
        let error = super::validate_runtime_control_request(&ControlRequest::MoveTorrent {
            info_hash_hex: "1111111111111111111111111111111111111111".to_string(),
            download_path: PathBuf::from("/fictional-downloads"),
        })
        .expect_err("runtime move should be rejected");

        assert!(error.contains("CLI-only"));
        assert!(error.contains("stopped"));
    }

    #[tokio::test]
    async fn rebind_listener_reannounces_running_torrents_on_new_port_when_already_reachable() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let recorder = TestDhtRecorder::default();
        app.dht_service = DhtService::from_test_recorder(recorder.clone());
        app.dht_status_rx = app.dht_service.subscribe_status();
        app.app_state.externally_accessable_port_v4 = true;

        let running_hash = vec![3; 20];
        let (running_tx, _running_rx) = mpsc::channel(1);
        app.torrent_manager_command_txs
            .insert(running_hash.clone(), running_tx);
        let mut running_display = TorrentDisplayState::default();
        running_display.latest_state.info_hash = running_hash.clone();
        running_display.latest_state.torrent_name = "port reannounce sample".to_string();
        running_display.latest_state.torrent_control_state = TorrentControlState::Running;
        running_display.latest_state.number_of_pieces_total = 1;
        app.app_state
            .torrents
            .insert(running_hash.clone(), running_display);

        assert!(app.rebind_listener(0).await);
        tokio::task::yield_now().await;

        let bound_port = app.client_configs.client_port;
        assert_ne!(bound_port, 0);
        assert_eq!(
            recorder.recorded_announces(),
            vec![(running_hash, Some(bound_port))]
        );
        assert!(!app.app_state.externally_accessable_port_v4);
        assert!(!app.app_state.externally_accessable_port_v6);

        let _ = app.shutdown_tx.send(());
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
        running_display.latest_state.number_of_pieces_total = 1;
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

        app.mark_peer_port_open(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 6681),
            PeerTransportKind::Tcp,
        );
        tokio::task::yield_now().await;

        assert_eq!(
            recorder.recorded_announces(),
            vec![(running_hash.clone(), Some(6681))]
        );

        app.mark_peer_port_open(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 6681),
            PeerTransportKind::Utp,
        );
        tokio::task::yield_now().await;

        assert_eq!(
            recorder.recorded_announces(),
            vec![(running_hash.clone(), Some(6681))]
        );

        app.mark_peer_port_open(
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 6681),
            PeerTransportKind::Tcp,
        );
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
    async fn apply_settings_update_restores_previous_port_when_rebind_fails() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let original_port = app.client_configs.client_port;
        let (_occupied_network_handle, occupied_network_lease) = unrestricted_network_lease();
        let (occupied_v4, occupied_v6) = super::bind_tcp_peer_listeners(&occupied_network_lease, 0)
            .await
            .expect("reserve occupied production listener port");
        let occupied_port = occupied_v4
            .as_ref()
            .or(occupied_v6.as_ref())
            .and_then(|listener| listener.local_addr().ok())
            .expect("occupied local addr")
            .port();

        let mut next_settings = app.client_configs.clone();
        next_settings.client_port = occupied_port;

        app.apply_settings_update(next_settings, false).await;

        assert_eq!(app.client_configs.client_port, original_port);
        assert!(app.listener.is_none());
        assert!(matches!(
            &*app.network_activation.subscribe().borrow(),
            crate::networking::NetworkActivationState::Blocked(_)
        ));
        assert!(app
            .app_state
            .system_error
            .as_deref()
            .is_some_and(|message| message.contains("Networking is blocked")));

        wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Ready(_))).await;
        app.handle_network_state_changed().await;
        assert_eq!(
            app.listener.as_ref().and_then(ListenerSet::local_port),
            Some(original_port)
        );
        assert!(matches!(
            &*app.network_activation.subscribe().borrow(),
            crate::networking::NetworkActivationState::Active(active)
                if active.listen_port() == original_port
        ));

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn dht_status_change_resends_cached_peer_slot_usage() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let recorder = TestDhtRecorder::default();
        app.dht_service = DhtService::from_test_recorder(recorder.clone());
        app.dht_status_rx = app.dht_service.subscribe_status();
        app.app_state.limits.max_connected_peers = 10;

        let info_hash = vec![4; 20];
        let mut display = TorrentDisplayState::default();
        display.latest_state.info_hash = info_hash.clone();
        display.latest_state.torrent_name = "peer pressure sample".to_string();
        display.latest_state.number_of_successfully_connected_peers = 9;
        app.app_state.torrents.insert(info_hash, display);

        app.sync_dht_peer_slot_usage();
        assert_eq!(wait_for_peer_slot_usages(&recorder, 1).await, vec![(9, 10)]);

        app.sync_dht_peer_slot_usage();
        tokio::task::yield_now().await;
        assert_eq!(recorder.recorded_peer_slot_usages(), vec![(9, 10)]);

        app.handle_dht_status_changed();
        assert_eq!(
            wait_for_peer_slot_usages(&recorder, 2).await,
            vec![(9, 10), (9, 10)]
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn wake_lag_peer_throttle_floor_is_more_lenient_while_downloading() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let base_peer_limit = 100;

        assert_eq!(
            app.wake_lag_peer_throttle_floor(base_peer_limit),
            super::WAKE_LAG_PEER_THROTTLE_MIN_PEERS
        );

        let info_hash = vec![9; 20];
        let mut display = TorrentDisplayState::default();
        display.latest_state.info_hash = info_hash.clone();
        display.latest_state.torrent_name = "sample download".to_string();
        display.latest_state.torrent_control_state = TorrentControlState::Running;
        display.latest_state.is_complete = false;
        app.app_state.torrents.insert(info_hash, display);

        assert_eq!(
            app.wake_lag_peer_throttle_floor(base_peer_limit),
            base_peer_limit * super::WAKE_LAG_PEER_THROTTLE_DOWNLOAD_FLOOR_PERCENT / 100
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn apply_settings_update_reconfigures_dht_bootstrap_after_failed_port_rebind() {
        let settings = crate::config::Settings {
            client_port: 0,
            bootstrap_nodes: vec!["127.0.0.1:9".to_string()],
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let recorder = TestDhtRecorder::default();
        app.dht_service = DhtService::from_test_recorder(recorder.clone());
        app.dht_status_rx = app.dht_service.subscribe_status();

        let original_port = app.client_configs.client_port;
        let (_occupied_network_handle, occupied_network_lease) = unrestricted_network_lease();
        let (occupied_v4, occupied_v6) = super::bind_tcp_peer_listeners(&occupied_network_lease, 0)
            .await
            .expect("reserve occupied production listener port");
        let occupied_port = occupied_v4
            .as_ref()
            .or(occupied_v6.as_ref())
            .and_then(|listener| listener.local_addr().ok())
            .expect("occupied local addr")
            .port();

        let mut next_settings = app.client_configs.clone();
        next_settings.client_port = occupied_port;
        next_settings.bootstrap_nodes = vec!["127.0.0.1:10".to_string()];

        app.apply_settings_update(next_settings.clone(), false)
            .await;

        let recorded = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let recorded = recorder.recorded_reconfigures();
                if !recorded.is_empty() {
                    break recorded;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("DHT reconfigure should be recorded");
        let config = recorded.last().expect("recorded reconfigure");
        assert_eq!(app.client_configs.client_port, original_port);
        assert_eq!(
            app.client_configs.bootstrap_nodes,
            next_settings.bootstrap_nodes
        );
        assert_eq!(config.port, original_port);
        assert_eq!(config.bootstrap_nodes, next_settings.bootstrap_nodes);

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
    async fn tuning_resource_limits_pauses_while_peer_admission_stress_is_active() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        app.app_state.limits = super::CalculatedLimits {
            reserve_permits: 100,
            max_connected_peers: 10,
            disk_read_permits: 8,
            disk_write_permits: 8,
        };
        app.wake_lag_peer_throttle.effective_peer_limit = Some(4);
        let before_limits = app.app_state.limits.clone();
        let before_tuning = app.tuning_controller.state().clone();

        app.tuning_resource_limits().await;

        assert_eq!(app.app_state.limits, before_limits);
        assert_eq!(app.app_state.active_peer_limit, Some(8));

        let after_tuning = app.tuning_controller.state();
        assert_eq!(
            after_tuning.last_tuning_score,
            before_tuning.last_tuning_score
        );
        assert_eq!(
            after_tuning.current_tuning_score,
            before_tuning.current_tuning_score
        );
        assert_eq!(
            after_tuning.last_tuning_limits,
            before_tuning.last_tuning_limits
        );
        assert_eq!(
            after_tuning.baseline_speed_ema,
            before_tuning.baseline_speed_ema
        );

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
    async fn startup_restore_rolls_running_torrents_after_first() {
        let mut settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        for index in 0..4 {
            let hash_digit = char::from_digit((index + 1) as u32, 16).expect("hex digit");
            settings.torrents.push(TorrentSettings {
                torrent_or_magnet: format!(
                    "magnet:?xt=urn:btih:{}",
                    hash_digit.to_string().repeat(40)
                ),
                name: format!("roll-start-{}", index),
                torrent_control_state: TorrentControlState::Running,
                ..Default::default()
            });
        }

        let app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");

        assert_eq!(app.torrent_manager_command_txs.len(), 1);
        assert_eq!(app.startup_deferred_load_queue.len(), 3);
        assert_eq!(app.startup_loaded_torrent_count, 1);
        assert!(app.next_startup_load_at.is_some());

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn start_missing_runtime_torrents_preserves_startup_rollout() {
        let mut app = App::new(
            crate::config::Settings {
                client_port: 0,
                ..Default::default()
            },
            AppRuntimeMode::Normal,
        )
        .await
        .expect("build app");

        for index in 0..4 {
            let hash_digit = char::from_digit((index + 1) as u32, 16).expect("hex digit");
            app.client_configs.torrents.push(TorrentSettings {
                torrent_or_magnet: format!(
                    "magnet:?xt=urn:btih:{}",
                    hash_digit.to_string().repeat(40)
                ),
                name: format!("missing-roll-{}", index),
                torrent_control_state: TorrentControlState::Running,
                ..Default::default()
            });
        }

        app.start_missing_runtime_torrents_for_current_role().await;

        assert_eq!(app.torrent_manager_command_txs.len(), 1);
        assert_eq!(app.startup_deferred_load_queue.len(), 3);
        assert_eq!(app.startup_loaded_torrent_count, 1);
        assert!(app.next_startup_load_at.is_some());

        app.load_next_startup_batch().await;

        assert_eq!(app.torrent_manager_command_txs.len(), 2);
        assert_eq!(app.startup_deferred_load_queue.len(), 2);
        assert_eq!(app.startup_loaded_torrent_count, 2);

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
        mark_startup_roll_in_responsiveness_ready(&mut app);

        app.load_next_startup_batch().await;

        assert_eq!(app.app_state.torrents.len(), 1);
        assert_eq!(app.startup_deferred_load_queue.len(), 5);
        assert_eq!(app.startup_loaded_torrent_count, 1);
        assert!(!app.startup_load_summary_logged);
        assert!(app.next_startup_load_at.is_some());

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn load_next_startup_batch_keeps_loading_when_effective_peer_limit_is_active() {
        let info_hash_hex = "1".repeat(40);
        let torrent = TorrentSettings {
            torrent_or_magnet: format!("magnet:?xt=urn:btih:{info_hash_hex}"),
            name: "peer-limit-start".to_string(),
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
                .expect("derive info hash")]);
        app.app_state.limits.max_connected_peers = 10;
        app.app_state.active_peer_limit = None;
        app.wake_lag_peer_throttle.effective_peer_limit = Some(4);
        mark_startup_roll_in_responsiveness_ready(&mut app);

        app.load_next_startup_batch().await;

        assert_eq!(app.app_state.torrents.len(), 1);
        assert!(app.startup_deferred_load_queue.is_empty());
        assert_eq!(app.startup_loaded_torrent_count, 1);
        assert!(app.next_startup_load_at.is_none());

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn load_next_startup_batch_records_one_summary_after_queue_drains() {
        let mut settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        for index in 0..2 {
            let hash_digit = char::from_digit((index + 1) as u32, 16).expect("hex digit");
            settings.torrents.push(TorrentSettings {
                torrent_or_magnet: format!(
                    "magnet:?xt=urn:btih:{}",
                    hash_digit.to_string().repeat(40)
                ),
                name: format!("summary-start-{}", index),
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
        mark_startup_roll_in_responsiveness_ready(&mut app);

        app.load_next_startup_batch().await;
        assert_eq!(app.startup_loaded_torrent_count, 1);
        assert!(!app.startup_load_summary_logged);

        app.load_next_startup_batch().await;
        assert_eq!(app.startup_loaded_torrent_count, 2);
        assert!(app.startup_deferred_load_queue.is_empty());
        assert!(app.startup_load_summary_logged);

        app.maybe_log_startup_load_summary();
        assert_eq!(app.startup_loaded_torrent_count, 2);
        assert!(app.startup_load_summary_logged);

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
        mark_startup_roll_in_responsiveness_ready(&mut app);

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
        mark_startup_roll_in_responsiveness_ready(&mut app);

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
        let download_path = std::env::temp_dir().join("event-journal-downloads");
        let info_hash = vec![0x11; 20];
        app.app_state.torrents.insert(
            info_hash.clone(),
            TorrentDisplayState {
                latest_state: TorrentMetrics {
                    info_hash: info_hash.clone(),
                    torrent_name: "Sample Alpha".to_string(),
                    download_path: Some(download_path.clone()),
                    container_name: Some("Sample Alpha".to_string()),
                    is_multi_file: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let initial_entry_count = app.app_state.event_journal_state.entries.len();

        app.record_watch_path_discovered(&queued_path);
        app.record_ingest_result(
            &queued_path,
            &CommandIngestResult::Duplicate {
                info_hash: Some(info_hash),
                torrent_name: Some("Sample Alpha".to_string()),
            },
        );

        let entries = &app.app_state.event_journal_state.entries[initial_entry_count..];
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
                download_path: None,
                container_name: None,
                payload_path: None,
            }
        );
        assert_eq!(
            entries[1].details,
            EventDetails::Ingest {
                origin: IngestOrigin::WatchFolder,
                ingest_kind: IngestKind::MagnetFile,
                download_path: Some(download_path.clone()),
                container_name: Some("Sample Alpha".to_string()),
                payload_path: Some(download_path.join("Sample Alpha")),
            }
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn startup_selected_header_reflects_pinned_torrent_sort() {
        let settings = crate::config::Settings {
            client_port: 0,
            torrent_sort_column: TorrentSortColumn::Progress,
            torrent_sort_pinned: true,
            ..Default::default()
        };
        let app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");

        assert_eq!(
            app.app_state.ui.selected_header,
            SelectedHeader::Torrent(ColumnId::Status)
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
        let initial_entry_count = app.app_state.event_journal_state.entries.len();
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

        let entries = &app.app_state.event_journal_state.entries[initial_entry_count..];
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
        app.open_manual_browser_for_torrent_file_with_archive(watched_path.clone(), true)
            .expect("open manual browser");

        let final_path = processed_dir.join("manual-input.torrent");
        assert_eq!(app.app_state.pending_torrent_path, Some(final_path.clone()));
        assert!(matches!(app.app_state.mode, AppMode::FileBrowser));
        assert!(app.app_state.ui.file_browser.fetch_pending);
        assert!(matches!(
            &app.app_state.ui.file_browser.browser_mode,
            FileBrowserMode::DownloadLocSelection {
                target: DownloadSelectionTarget::PendingAdd,
                preview_tree,
                ..
            } if !preview_tree.is_empty()
        ));
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
        app.open_manual_browser_for_torrent_file_with_archive(watched_path.clone(), true)
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

        assert!(super::watched_parent_matches(
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
    async fn interactive_add_prompt_setting_overrides_default_download_fast_path() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(temp_dir.path().join("downloads")),
            always_show_add_location_prompt: true,
            ..Default::default()
        };
        let app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let torrent_path = temp_dir.path().join("sample-input.torrent");

        let action = app.resolve_add_ingress_action(IngestSource::TorrentFile, &torrent_path);

        assert!(matches!(
            action,
            super::AddIngressAction::OpenManualBrowser { .. }
        ));
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn always_show_prompt_preserves_host_watch_folder_fast_path() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let watch_folder = temp_dir.path().join("watch");
        let download_folder = temp_dir.path().join("downloads");
        std::fs::create_dir_all(&watch_folder).expect("create watch folder");
        let magnet_path = watch_folder.join("automation-input.magnet");
        std::fs::write(
            &magnet_path,
            "magnet:?xt=urn:btih:5555555555555555555555555555555555555555",
        )
        .expect("write magnet");
        let settings = crate::config::Settings {
            client_port: 0,
            watch_folder: Some(watch_folder),
            default_download_folder: Some(download_folder.clone()),
            always_show_add_location_prompt: true,
            ..Default::default()
        };
        let app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");

        let action = app.resolve_add_ingress_action(IngestSource::MagnetFile, &magnet_path);

        assert!(matches!(
            action,
            super::AddIngressAction::ApplyDirectly { download_path, .. }
                if download_path == download_folder
        ));
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn interactive_add_prompt_starts_at_default_download_folder() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let default_download_folder = temp_dir.path().join("downloads");
        std::fs::create_dir_all(&default_download_folder).expect("create default download folder");
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(default_download_folder.clone()),
            always_show_add_location_prompt: true,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}

        app.open_manual_browser_for_payload(
            IngestSource::MagnetFile,
            ResolvedAddPayload::MagnetLink {
                magnet_link: "magnet:?xt=urn:btih:5555555555555555555555555555555555555555"
                    .to_string(),
            },
        )
        .await
        .expect("open manual browser");

        assert_eq!(
            app.app_state.ui.file_browser.state.current_path,
            default_download_folder
        );
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn interactive_add_prompt_starts_on_priority_pane_when_default_download_folder_is_set() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let default_download_folder = temp_dir.path().join("downloads");
        std::fs::create_dir_all(&default_download_folder).expect("create default download folder");
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(default_download_folder),
            always_show_add_location_prompt: true,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}

        app.open_manual_browser_for_payload(
            IngestSource::MagnetFile,
            ResolvedAddPayload::MagnetLink {
                magnet_link: "magnet:?xt=urn:btih:5555555555555555555555555555555555555555"
                    .to_string(),
            },
        )
        .await
        .expect("open manual browser");

        let FileBrowserMode::DownloadLocSelection { focused_pane, .. } =
            &app.app_state.ui.file_browser.browser_mode
        else {
            panic!("expected download location selection browser");
        };
        assert_eq!(focused_pane, &BrowserPane::TorrentPreview);
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn interactive_add_prompt_starts_on_location_pane_without_default_download_folder() {
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: None,
            always_show_add_location_prompt: true,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}

        app.open_manual_browser_for_payload(
            IngestSource::MagnetFile,
            ResolvedAddPayload::MagnetLink {
                magnet_link: "magnet:?xt=urn:btih:5555555555555555555555555555555555555555"
                    .to_string(),
            },
        )
        .await
        .expect("open manual browser");

        let FileBrowserMode::DownloadLocSelection { focused_pane, .. } =
            &app.app_state.ui.file_browser.browser_mode
        else {
            panic!("expected download location selection browser");
        };
        assert_eq!(focused_pane, &BrowserPane::FileSystem);
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn manual_magnet_browser_shows_awaiting_metadata_and_starts_pending_runtime() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(temp_dir.path().join("downloads")),
            always_show_add_location_prompt: true,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}

        let magnet_link = "magnet:?xt=urn:btih:5555555555555555555555555555555555555555";
        app.open_manual_magnet_browser(magnet_link.to_string())
            .await
            .expect("open manual magnet browser");

        let info_hash = vec![0x55; 20];
        assert_eq!(app.app_state.pending_torrent_link, magnet_link);
        assert_eq!(
            app.app_state.pending_magnet_preview_info_hash,
            Some(info_hash.clone())
        );
        assert!(app.app_state.torrents.contains_key(&info_hash));
        assert!(app.torrent_manager_command_txs.contains_key(&info_hash));

        let FileBrowserMode::DownloadLocSelection {
            target,
            container_name,
            original_name_backup,
            preview_tree,
            use_container,
            ..
        } = &app.app_state.ui.file_browser.browser_mode
        else {
            panic!("expected download location selection browser");
        };
        assert!(matches!(app.app_state.mode, AppMode::FileBrowser));
        assert_eq!(target, &DownloadSelectionTarget::PendingAdd);
        assert_eq!(container_name, AWAITING_MAGNET_METADATA_LABEL);
        assert_eq!(original_name_backup, AWAITING_MAGNET_METADATA_LABEL);
        assert!(preview_tree.is_empty());
        assert!(*use_container);

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn second_manual_magnet_replaces_and_cleans_old_pending_preview_runtime() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(temp_dir.path().join("downloads")),
            always_show_add_location_prompt: true,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}

        let first_magnet = "magnet:?xt=urn:btih:5555555555555555555555555555555555555555";
        let first_hash = vec![0x55; 20];
        app.open_manual_magnet_browser(first_magnet.to_string())
            .await
            .expect("open first manual magnet browser");
        assert_eq!(
            app.app_state.pending_magnet_preview_info_hash,
            Some(first_hash.clone())
        );
        assert!(app.app_state.torrents.contains_key(&first_hash));
        assert!(app.torrent_manager_command_txs.contains_key(&first_hash));

        let second_magnet = "magnet:?xt=urn:btih:6666666666666666666666666666666666666666";
        let second_hash = vec![0x66; 20];
        app.open_manual_magnet_browser(second_magnet.to_string())
            .await
            .expect("open second manual magnet browser");

        assert_eq!(app.app_state.pending_torrent_link, second_magnet);
        assert_eq!(
            app.app_state.pending_magnet_preview_info_hash,
            Some(second_hash.clone())
        );
        assert!(!app.app_state.torrents.contains_key(&first_hash));
        assert!(!app.app_state.torrent_list_order.contains(&first_hash));
        assert!(!app.torrent_manager_command_txs.contains_key(&first_hash));
        assert!(app.app_state.torrents.contains_key(&second_hash));

        let payload = build_persist_payload(
            &mut app.client_configs,
            &mut app.app_state,
            &VecDeque::new(),
        );
        assert!(payload
            .settings
            .torrents
            .iter()
            .all(|torrent| !torrent.torrent_or_magnet.contains(first_magnet)));
        assert!(payload
            .settings
            .torrents
            .iter()
            .all(|torrent| !torrent.torrent_or_magnet.contains(second_magnet)));

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn path_add_replacing_pending_magnet_clears_stale_link_and_ignores_late_metadata() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let default_download_folder = temp_dir.path().join("downloads");
        std::fs::create_dir_all(&default_download_folder).expect("create default download folder");
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(default_download_folder),
            always_show_add_location_prompt: true,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}

        let old_magnet = "magnet:?xt=urn:btih:5555555555555555555555555555555555555555";
        let old_info_hash = vec![0x55; 20];
        app.open_manual_magnet_browser(old_magnet.to_string())
            .await
            .expect("open pending magnet browser");
        assert_eq!(app.app_state.pending_torrent_link, old_magnet);
        assert_eq!(
            app.app_state.pending_magnet_preview_info_hash,
            Some(old_info_hash.clone())
        );

        let referenced_torrent_path = temp_dir.path().join("referenced.torrent");
        app.open_manual_browser_for_payload(
            IngestSource::TorrentPathFile,
            ResolvedAddPayload::TorrentFile {
                source_path: referenced_torrent_path.clone(),
            },
        )
        .await
        .expect("replace pending magnet with path add");

        assert!(matches!(app.app_state.mode, AppMode::FileBrowser));
        assert!(app.app_state.ui.file_browser.fetch_pending);
        assert!(app.app_state.ui.file_browser.data.is_empty());

        assert!(app.app_state.pending_torrent_link.is_empty());
        assert_eq!(
            app.app_state.pending_torrent_path,
            Some(referenced_torrent_path)
        );
        assert_eq!(app.app_state.pending_magnet_preview_info_hash, None);

        app.handle_manager_event(ManagerEvent::MetadataLoaded {
            info_hash: old_info_hash,
            torrent: Box::new(crate::torrent_file::Torrent {
                info: crate::torrent_file::Info {
                    name: "Old Magnet Preview".to_string(),
                    files: vec![crate::torrent_file::InfoFile {
                        length: 10,
                        path: vec!["old-preview.bin".to_string()],
                        md5sum: None,
                        attr: None,
                    }],
                    ..Default::default()
                },
                ..Default::default()
            }),
        });

        let FileBrowserMode::DownloadLocSelection {
            preview_tree,
            container_name,
            original_name_backup,
            ..
        } = &app.app_state.ui.file_browser.browser_mode
        else {
            panic!("expected replacement path add browser");
        };
        assert!(preview_tree.is_empty());
        assert_eq!(container_name, "New Torrent");
        assert_eq!(original_name_backup, "New Torrent");

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn magnet_add_replacing_pending_path_clears_stale_path() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let default_download_folder = temp_dir.path().join("downloads");
        std::fs::create_dir_all(&default_download_folder).expect("create default download folder");
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(default_download_folder),
            always_show_add_location_prompt: true,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}

        app.app_state.pending_torrent_path = Some(temp_dir.path().join("stale-input.torrent"));
        let magnet_link = "magnet:?xt=urn:btih:6666666666666666666666666666666666666666";
        app.open_manual_browser_for_payload(
            IngestSource::MagnetFile,
            ResolvedAddPayload::MagnetLink {
                magnet_link: magnet_link.to_string(),
            },
        )
        .await
        .expect("replace pending path add with magnet add");

        assert!(app.app_state.pending_torrent_path.is_none());
        assert_eq!(app.app_state.pending_torrent_link, magnet_link);
        assert_eq!(
            app.app_state.pending_magnet_preview_info_hash,
            Some(vec![0x66; 20])
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn pending_magnet_escape_shuts_down_and_removes_all_preview_runtime_state() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(temp_dir.path().join("downloads")),
            always_show_add_location_prompt: true,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}

        let info_hash = vec![0x55; 20];
        app.app_state.pending_torrent_link =
            "magnet:?xt=urn:btih:5555555555555555555555555555555555555555".to_string();
        app.app_state.pending_magnet_preview_info_hash = Some(info_hash.clone());
        app.app_state.mode = AppMode::FileBrowser;
        app.app_state.ui.file_browser.browser_mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::PendingAdd,
            torrent_files: vec![],
            container_name: AWAITING_MAGNET_METADATA_LABEL.to_string(),
            use_container: true,
            is_editing_name: false,
            preview_tree: Vec::new(),
            preview_state: TreeViewState::default(),
            focused_pane: BrowserPane::FileSystem,
            cursor_pos: 0,
            original_name_backup: AWAITING_MAGNET_METADATA_LABEL.to_string(),
        };

        let (manager_tx, mut manager_rx) = mpsc::channel(1);
        app.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_tx);
        let (incoming_tx, _incoming_rx) =
            mpsc::channel::<crate::torrent_manager::IncomingPeerSession>(1);
        app.torrent_manager_incoming_peer_txs
            .insert(info_hash.clone(), incoming_tx);
        let (_metrics_tx, metrics_rx) = watch::channel(TorrentMetrics::default());
        app.torrent_metric_watch_rxs
            .insert(info_hash.clone(), metrics_rx);
        app.app_state
            .torrents
            .insert(info_hash.clone(), TorrentDisplayState::default());
        app.app_state.torrent_list_order.push(info_hash.clone());
        app.integrity_scheduler
            .sync_torrents([TorrentIntegritySnapshot {
                info_hash: info_hash.clone(),
                data_available: false,
                is_downloading: true,
                file_count: None,
                saved_location: Some(temp_dir.path().join("downloads")),
                download_speed_bps: 0,
                upload_speed_bps: 0,
            }]);
        assert!(app.integrity_scheduler.next_probe_in(&info_hash).is_some());

        let reduced = reduce_browser_dialog_action(
            BrowserDialogAction::Escape,
            &app.app_state.ui.file_browser.state,
            &app.app_state.ui.file_browser.browser_mode,
            true,
        );
        execute_browser_dialog_effects(&mut app, reduced.effects).await;

        let command = time::timeout(Duration::from_secs(1), manager_rx.recv())
            .await
            .expect("manager shutdown command should be sent")
            .expect("manager command channel should remain open until command is received");
        assert!(matches!(command, ManagerCommand::Shutdown));
        assert!(!app.app_state.torrents.contains_key(&info_hash));
        assert!(!app.app_state.torrent_list_order.contains(&info_hash));
        assert!(!app.torrent_manager_command_txs.contains_key(&info_hash));
        assert!(!app
            .torrent_manager_incoming_peer_txs
            .contains_key(&info_hash));
        assert!(!app.torrent_metric_watch_rxs.contains_key(&info_hash));
        assert!(app.integrity_scheduler.next_probe_in(&info_hash).is_none());
        assert!(app.app_state.pending_magnet_preview_info_hash.is_none());

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn pending_magnet_escape_keeps_duplicate_existing_runtime() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(temp_dir.path().join("downloads")),
            always_show_add_location_prompt: true,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}

        let info_hash = vec![0x55; 20];
        let magnet_link = "magnet:?xt=urn:btih:5555555555555555555555555555555555555555";
        app.app_state
            .torrents
            .insert(info_hash.clone(), TorrentDisplayState::default());
        app.app_state.torrent_list_order.push(info_hash.clone());
        let (manager_tx, mut manager_rx) = mpsc::channel(1);
        app.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_tx);

        app.open_manual_magnet_browser(magnet_link.to_string())
            .await
            .expect("open duplicate manual magnet browser");
        assert_eq!(app.app_state.pending_torrent_link, magnet_link);
        assert!(app.app_state.pending_magnet_preview_info_hash.is_none());

        let reduced = reduce_browser_dialog_action(
            BrowserDialogAction::Escape,
            &app.app_state.ui.file_browser.state,
            &app.app_state.ui.file_browser.browser_mode,
            true,
        );
        execute_browser_dialog_effects(&mut app, reduced.effects).await;

        assert!(manager_rx.try_recv().is_err());
        assert!(app.app_state.torrents.contains_key(&info_hash));
        assert!(app.app_state.torrent_list_order.contains(&info_hash));
        assert!(app.torrent_manager_command_txs.contains_key(&info_hash));

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn shared_follower_always_show_prompt_queues_leader_request_without_manual_browser() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let download_folder = temp_dir.path().join("downloads");
        let magnet_path = temp_dir.path().join("manual-input.magnet");
        let magnet_link = "magnet:?xt=urn:btih:5555555555555555555555555555555555555555";
        std::fs::write(&magnet_path, magnet_link).expect("write magnet file");
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(download_folder.clone()),
            always_show_add_location_prompt: true,
            ..Default::default()
        };
        let app = App::new(settings, AppRuntimeMode::SharedFollower)
            .await
            .expect("build app");

        let action = app.resolve_add_ingress_action(IngestSource::MagnetFile, &magnet_path);

        match action {
            super::AddIngressAction::QueueControlRequest(ControlRequest::AddMagnet {
                magnet_link: queued_link,
                download_path,
                container_name,
                ..
            }) => {
                assert_eq!(queued_link, magnet_link);
                assert_eq!(download_path, Some(download_folder));
                assert!(container_name.is_none());
            }
            other => panic!("unexpected follower add action: {:?}", other),
        }
        assert!(!matches!(app.app_state.mode, AppMode::FileBrowser));
        assert!(app.app_state.torrents.is_empty());
        assert!(app.torrent_manager_command_txs.is_empty());

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn shared_follower_with_shared_config_default_queues_leader_request_without_manual_browser(
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
            "client_port = 0\nalways_show_add_location_prompt = true\n",
        )
        .expect("write host config");

        let magnet_path = shared_root.path().join("manual-input.magnet");
        let magnet_link = "magnet:?xt=urn:btih:5555555555555555555555555555555555555555";
        std::fs::write(&magnet_path, magnet_link).expect("write magnet file");
        let settings = crate::config::load_settings().expect("load shared settings");
        assert_eq!(
            settings.default_download_folder.as_deref(),
            Some(shared_root.path())
        );
        assert!(settings.always_show_add_location_prompt);
        let app = App::new(settings, AppRuntimeMode::SharedFollower)
            .await
            .expect("build app");

        let action = app.resolve_add_ingress_action(IngestSource::MagnetFile, &magnet_path);

        match action {
            super::AddIngressAction::QueueControlRequest(ControlRequest::AddMagnet {
                magnet_link: queued_link,
                download_path,
                container_name,
                ..
            }) => {
                assert_eq!(queued_link, magnet_link);
                assert_eq!(download_path.as_deref(), Some(shared_root.path()));
                assert!(container_name.is_none());
            }
            other => panic!("unexpected follower add action: {:?}", other),
        }
        assert!(!matches!(app.app_state.mode, AppMode::FileBrowser));
        assert!(app.app_state.torrents.is_empty());
        assert!(app.torrent_manager_command_txs.is_empty());

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
    async fn hydrated_pending_magnet_confirm_queues_selected_location_container_and_priorities() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let selected_download_path = temp_dir.path().join("chosen-downloads");
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(temp_dir.path().join("downloads")),
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}

        let magnet_link = "magnet:?xt=urn:btih:5555555555555555555555555555555555555555";
        let info_hash = vec![0x55; 20];
        app.app_state.pending_torrent_link = magnet_link.to_string();
        app.app_state.pending_magnet_preview_info_hash = Some(info_hash.clone());
        app.app_state.torrents.insert(
            info_hash.clone(),
            TorrentDisplayState {
                latest_state: TorrentMetrics {
                    info_hash: info_hash.clone(),
                    torrent_or_magnet: magnet_link.to_string(),
                    torrent_name: "sample-preview".to_string(),
                    torrent_control_state: TorrentControlState::Running,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        app.app_state.torrent_list_order.push(info_hash.clone());
        let (manager_tx, mut manager_rx) = mpsc::channel(1);
        app.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_tx);
        app.app_state.ui.file_browser.state.current_path = selected_download_path.clone();
        let preview_tree = build_torrent_preview_tree(
            vec![(vec!["folder".to_string(), "file.bin".to_string()], 42)],
            &HashMap::from([(0, FilePriority::Skip)]),
        );
        app.app_state.ui.file_browser.browser_mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::PendingAdd,
            torrent_files: vec![],
            container_name: "Hydrated Magnet".to_string(),
            use_container: true,
            is_editing_name: false,
            preview_tree,
            preview_state: Default::default(),
            focused_pane: BrowserPane::TorrentPreview,
            cursor_pos: 0,
            original_name_backup: "Hydrated Magnet".to_string(),
        };

        let payload = build_download_confirm_payload(
            &app.app_state.ui.file_browser.state,
            &app.app_state.ui.file_browser.browser_mode,
        )
        .expect("confirm payload");
        let transition = execute_confirm_decision(
            &mut app,
            crate::tui::screens::browser::ConfirmDecision::Download(payload),
        )
        .await;

        assert!(matches!(
            transition,
            Some(crate::tui::screens::browser::BrowserTransition::ToNormal)
        ));
        let command = time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(command) = app.app_command_rx.recv().await {
                    if matches!(command, AppCommand::SubmitManualAddRequest { .. }) {
                        break command;
                    }
                }
            }
        })
        .await
        .expect("queued manual add request");

        let AppCommand::SubmitManualAddRequest {
            request:
                ControlRequest::AddMagnet {
                    magnet_link: queued_link,
                    download_path,
                    container_name,
                    file_priorities,
                    ..
                },
            ..
        } = &command
        else {
            panic!("expected add magnet control request");
        };
        assert_eq!(queued_link.as_str(), magnet_link);
        assert_eq!(download_path.as_ref(), Some(&selected_download_path));
        assert_eq!(container_name.as_deref(), Some("Hydrated Magnet"));
        assert_eq!(file_priorities.len(), 1);
        assert_eq!(file_priorities[0].file_index, 0);
        assert_eq!(file_priorities[0].priority, FilePriority::Skip);
        assert!(app.app_state.pending_torrent_link.is_empty());
        assert_eq!(
            app.app_state.pending_magnet_preview_info_hash,
            Some(info_hash.clone())
        );

        let mut pending_settings = app.client_configs.clone();
        let pending_payload =
            build_persist_payload(&mut pending_settings, &mut app.app_state, &VecDeque::new());
        assert!(pending_payload.settings.torrents.is_empty());

        app.handle_app_command(command).await;

        let manager_command = manager_rx
            .try_recv()
            .expect("selected magnet config should be sent to preview runtime");
        match manager_command {
            ManagerCommand::SetUserTorrentConfig {
                torrent_data_path,
                file_priorities,
                container_name,
            } => {
                assert_eq!(torrent_data_path, selected_download_path);
                assert_eq!(container_name.as_deref(), Some("Hydrated Magnet"));
                assert_eq!(file_priorities, HashMap::from([(0, FilePriority::Skip)]));
            }
            other => panic!("unexpected manager command: {:?}", other),
        }
        assert!(app.app_state.pending_magnet_preview_info_hash.is_none());

        let display = app
            .app_state
            .torrents
            .get(&info_hash)
            .expect("configured magnet should remain in app state");
        assert_eq!(
            display.latest_state.download_path.as_ref(),
            Some(&selected_download_path)
        );
        assert_eq!(
            display.latest_state.container_name.as_deref(),
            Some("Hydrated Magnet")
        );
        assert_eq!(
            display.latest_state.file_priorities,
            HashMap::from([(0, FilePriority::Skip)])
        );

        let mut applied_settings = app.client_configs.clone();
        let applied_payload =
            build_persist_payload(&mut applied_settings, &mut app.app_state, &VecDeque::new());
        let persisted = applied_payload
            .settings
            .torrents
            .iter()
            .find(|torrent| torrent.torrent_or_magnet == magnet_link)
            .expect("configured magnet should be persisted after apply");
        assert_eq!(
            persisted.download_path.as_ref(),
            Some(&selected_download_path)
        );
        assert_eq!(persisted.container_name.as_deref(), Some("Hydrated Magnet"));
        assert_eq!(
            persisted.file_priorities,
            HashMap::from([(0, FilePriority::Skip)])
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn unrelated_submit_control_request_does_not_archive_pending_manual_ingest() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let source_path = temp_dir.path().join("manual-input.magnet");
        std::fs::write(
            &source_path,
            "magnet:?xt=urn:btih:5555555555555555555555555555555555555555",
        )
        .expect("write manual magnet");
        let archived_path = source_path.with_extension("magnet.added");
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}
        app.app_state.pending_manual_ingest = Some(PendingManualIngest {
            source: IngestSource::MagnetFile,
            path: source_path.clone(),
        });

        app.handle_app_command(AppCommand::SubmitControlRequest(ControlRequest::StatusNow))
            .await;

        let pending = app
            .app_state
            .pending_manual_ingest
            .as_ref()
            .expect("unrelated request should not consume pending manual ingest");
        assert_eq!(pending.path, source_path);
        assert!(source_path.exists());
        assert!(!archived_path.exists());

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn non_shared_manual_prompt_replacement_clears_deferred_manual_ingest() {
        let _guard = lock_shared_env();
        let _temp_paths = configure_temp_app_paths_for_test();
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let download_folder = temp_dir.path().join("downloads");
        std::fs::create_dir_all(&download_folder).expect("create download folder");
        let stale_source_path = temp_dir.path().join("stale-shared.magnet");
        let later_source_path = temp_dir.path().join("later-local.magnet");
        let later_magnet = "magnet:?xt=urn:btih:6666666666666666666666666666666666666666";
        std::fs::write(
            &stale_source_path,
            "magnet:?xt=urn:btih:5555555555555555555555555555555555555555",
        )
        .expect("write stale manual source");
        std::fs::write(&later_source_path, later_magnet).expect("write later manual source");
        let stale_archived_path = stale_source_path.with_extension("magnet.added");
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(download_folder.clone()),
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}
        app.app_state.pending_manual_ingest = Some(PendingManualIngest {
            source: IngestSource::MagnetFile,
            path: stale_source_path.clone(),
        });

        app.execute_add_ingress_action(
            IngestSource::MagnetFile,
            later_source_path.clone(),
            super::AddIngressAction::OpenManualBrowser {
                payload: ResolvedAddPayload::MagnetLink {
                    magnet_link: later_magnet.to_string(),
                },
            },
        )
        .await;

        assert!(app.app_state.pending_manual_ingest.is_none());
        app.app_state.ui.file_browser.state.current_path = download_folder.clone();
        let payload = build_download_confirm_payload(
            &app.app_state.ui.file_browser.state,
            &app.app_state.ui.file_browser.browser_mode,
        )
        .expect("confirm payload");
        let transition = execute_confirm_decision(
            &mut app,
            crate::tui::screens::browser::ConfirmDecision::Download(payload),
        )
        .await;
        assert!(matches!(
            transition,
            Some(crate::tui::screens::browser::BrowserTransition::ToNormal)
        ));

        let command = time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(command) = app.app_command_rx.recv().await {
                    if matches!(command, AppCommand::SubmitManualAddRequest { .. }) {
                        break command;
                    }
                }
            }
        })
        .await
        .expect("queued manual add request");

        let AppCommand::SubmitManualAddRequest { pending_ingest, .. } = command else {
            panic!("expected manual add request");
        };
        assert!(pending_ingest.is_none());
        assert!(stale_source_path.exists());
        assert!(!stale_archived_path.exists());

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn failed_torrent_request_prepare_keeps_deferred_manual_ingest() {
        let _guard = lock_shared_env();
        let shared_root = tempfile::tempdir().expect("create shared root");
        let original_shared_dir = env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
        let original_host_id = env::var_os("SUPERSEEDR_SHARED_HOST_ID");
        env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", shared_root.path());
        env::set_var("SUPERSEEDR_SHARED_HOST_ID", "node-a");
        clear_shared_config_state_for_tests();

        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let download_folder = temp_dir.path().join("downloads");
        std::fs::create_dir_all(&download_folder).expect("create download folder");
        let missing_torrent_path = temp_dir.path().join("missing.torrent");
        let inbox_path = temp_dir.path().join("manual-input.path");
        std::fs::write(
            &inbox_path,
            missing_torrent_path.to_string_lossy().as_bytes(),
        )
        .expect("write path input");

        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(download_folder.clone()),
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::SharedFollower)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}
        app.app_state.pending_torrent_path = Some(missing_torrent_path.clone());
        app.app_state.pending_manual_ingest = Some(PendingManualIngest {
            source: IngestSource::TorrentPathFile,
            path: inbox_path.clone(),
        });
        app.app_state.ui.file_browser.state.current_path = download_folder;
        app.app_state.ui.file_browser.browser_mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::PendingAdd,
            torrent_files: vec![],
            container_name: "New Torrent".to_string(),
            use_container: false,
            is_editing_name: false,
            preview_tree: Vec::new(),
            preview_state: TreeViewState::default(),
            focused_pane: BrowserPane::FileSystem,
            cursor_pos: 0,
            original_name_backup: "New Torrent".to_string(),
        };

        let payload = build_download_confirm_payload(
            &app.app_state.ui.file_browser.state,
            &app.app_state.ui.file_browser.browser_mode,
        )
        .expect("confirm payload");
        let transition = execute_confirm_decision(
            &mut app,
            crate::tui::screens::browser::ConfirmDecision::Download(payload),
        )
        .await;

        assert!(transition.is_none());
        assert!(app.app_state.system_error.is_some());
        assert_eq!(
            app.app_state.pending_torrent_path.as_ref(),
            Some(&missing_torrent_path)
        );
        let pending_manual = app
            .app_state
            .pending_manual_ingest
            .as_ref()
            .expect("failed preparation should keep deferred ingest");
        assert_eq!(pending_manual.path, inbox_path);
        assert_eq!(pending_manual.source, IngestSource::TorrentPathFile);

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
    async fn failed_manual_add_request_does_not_archive_stale_ingest_on_later_success() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let download_folder = temp_dir.path().join("downloads");
        std::fs::create_dir_all(&download_folder).expect("create download folder");
        let stale_source_path = temp_dir.path().join("stale-manual.magnet");
        let later_source_path = temp_dir.path().join("later-manual.magnet");
        std::fs::write(
            &stale_source_path,
            "magnet:?xt=urn:btih:5555555555555555555555555555555555555555",
        )
        .expect("write stale manual source");
        std::fs::write(
            &later_source_path,
            "magnet:?xt=urn:btih:6666666666666666666666666666666666666666",
        )
        .expect("write later manual source");
        let stale_archived_path = stale_source_path.with_extension("magnet.added");
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(download_folder.clone()),
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}

        app.handle_app_command(AppCommand::SubmitManualAddRequest {
            request: ControlRequest::AddTorrentFile {
                source_path: temp_dir.path().join("missing.torrent"),
                download_path: Some(download_folder.clone()),
                container_name: None,
                validation_status: false,
                file_priorities: Vec::new(),
            },
            pending_ingest: Some(PendingManualIngest {
                source: IngestSource::MagnetFile,
                path: stale_source_path.clone(),
            }),
        })
        .await;

        assert!(app.app_state.system_error.is_some());
        assert!(app.app_state.pending_manual_ingest.is_none());
        assert!(stale_source_path.exists());
        assert!(!stale_archived_path.exists());

        app.handle_app_command(AppCommand::SubmitManualAddRequest {
            request: ControlRequest::AddMagnet {
                magnet_link: "magnet:?xt=urn:btih:6666666666666666666666666666666666666666"
                    .to_string(),
                download_path: Some(download_folder),
                container_name: None,
                validation_status: false,
                file_priorities: Vec::new(),
            },
            pending_ingest: Some(PendingManualIngest {
                source: IngestSource::MagnetFile,
                path: later_source_path.clone(),
            }),
        })
        .await;

        assert!(stale_source_path.exists());
        assert!(!stale_archived_path.exists());
        assert!(!later_source_path.exists());

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn deferred_manual_add_records_ingest_result_and_retires_pending_path() {
        let _guard = lock_shared_env();
        let _temp_paths = configure_temp_app_paths_for_test();
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let download_folder = temp_dir.path().join("downloads");
        std::fs::create_dir_all(&download_folder).expect("create download folder");
        let source_path = temp_dir.path().join("manual-input.magnet");
        let magnet_link = "magnet:?xt=urn:btih:7777777777777777777777777777777777777777";
        std::fs::write(&source_path, magnet_link).expect("write manual source");
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(download_folder.clone()),
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}
        let initial_entry_count = app.app_state.event_journal_state.entries.len();
        let source_watch_folder = Some(temp_dir.path().to_path_buf());
        assert!(app.record_ingest_queued(
            source_path.clone(),
            IngestOrigin::WatchFolder,
            IngestKind::MagnetFile,
            source_watch_folder.clone(),
        ));

        app.handle_app_command(AppCommand::SubmitManualAddRequest {
            request: ControlRequest::AddMagnet {
                magnet_link: magnet_link.to_string(),
                download_path: Some(download_folder.clone()),
                container_name: None,
                validation_status: false,
                file_priorities: Vec::new(),
            },
            pending_ingest: Some(PendingManualIngest {
                source: IngestSource::MagnetFile,
                path: source_path.clone(),
            }),
        })
        .await;

        assert!(!app
            .app_state
            .pending_ingest_by_path
            .contains_key(&source_path));
        let entries = &app.app_state.event_journal_state.entries[initial_entry_count..];
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].event_type, EventType::IngestQueued);
        assert_eq!(entries[1].event_type, EventType::IngestAdded);
        assert_eq!(entries[0].correlation_id, entries[1].correlation_id);
        let archived_path = entries[1]
            .source_path
            .clone()
            .expect("terminal ingest should record archived source");
        assert_ne!(archived_path, source_path);
        assert!(!source_path.exists());
        assert!(archived_path.exists());
        assert_eq!(entries[0].source_path.as_ref(), Some(&archived_path));
        assert_eq!(entries[1].source_path.as_ref(), Some(&archived_path));
        let EventDetails::Ingest {
            origin,
            ingest_kind,
            download_path,
            container_name,
            ..
        } = &entries[1].details
        else {
            panic!("expected ingest event details");
        };
        assert_eq!(*origin, IngestOrigin::WatchFolder);
        assert_eq!(*ingest_kind, IngestKind::MagnetFile);
        assert_eq!(download_path.as_ref(), Some(&download_folder));
        assert!(container_name.is_none());

        std::fs::write(
            &source_path,
            "magnet:?xt=urn:btih:8888888888888888888888888888888888888888",
        )
        .expect("write replacement source");
        assert!(app.record_ingest_queued(
            source_path.clone(),
            IngestOrigin::WatchFolder,
            IngestKind::MagnetFile,
            source_watch_folder,
        ));
        assert!(app
            .app_state
            .pending_ingest_by_path
            .contains_key(&source_path));

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn existing_torrent_browser_preserves_confirmed_pending_magnet_preview_marker() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(temp_dir.path().join("downloads")),
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}

        let pending_info_hash = vec![0x55; 20];
        let existing_info_hash = vec![0x66; 20];
        app.app_state.pending_magnet_preview_info_hash = Some(pending_info_hash.clone());
        app.app_state.torrents.insert(
            existing_info_hash.clone(),
            TorrentDisplayState {
                latest_state: TorrentMetrics {
                    info_hash: existing_info_hash.clone(),
                    torrent_name: "existing-sample".to_string(),
                    ..Default::default()
                },
                file_preview_tree: vec![RawNode {
                    name: "existing.bin".to_string(),
                    full_path: PathBuf::from("existing.bin"),
                    children: vec![],
                    payload: TorrentPreviewPayload {
                        size: 10,
                        priority: FilePriority::Normal,
                        file_index: Some(0),
                    },
                    is_dir: false,
                }],
                ..Default::default()
            },
        );
        app.app_state
            .torrent_list_order
            .push(existing_info_hash.clone());

        app.open_existing_torrent_file_browser(existing_info_hash);
        assert_eq!(
            app.app_state.pending_magnet_preview_info_hash,
            Some(pending_info_hash.clone())
        );

        execute_browser_dialog_effects(
            &mut app,
            vec![BrowserDialogEffect::ToNormalAndClearPending],
        )
        .await;
        assert_eq!(
            app.app_state.pending_magnet_preview_info_hash,
            Some(pending_info_hash)
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn direct_magnet_apply_clears_pending_preview_before_persistence() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let download_folder = temp_dir.path().join("downloads");
        std::fs::create_dir_all(&download_folder).expect("create downloads");
        let ingest_path = temp_dir.path().join("same-hash.magnet");
        let magnet_link = "magnet:?xt=urn:btih:5555555555555555555555555555555555555555";
        std::fs::write(&ingest_path, magnet_link).expect("write magnet");
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(download_folder.clone()),
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}

        let info_hash = vec![0x55; 20];
        app.app_state.pending_torrent_link = magnet_link.to_string();
        app.app_state.pending_magnet_preview_info_hash = Some(info_hash.clone());
        app.app_state.torrents.insert(
            info_hash.clone(),
            TorrentDisplayState {
                latest_state: TorrentMetrics {
                    info_hash: info_hash.clone(),
                    torrent_or_magnet: magnet_link.to_string(),
                    torrent_name: "sample-preview".to_string(),
                    torrent_control_state: TorrentControlState::Running,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        app.app_state.torrent_list_order.push(info_hash.clone());
        let (manager_tx, mut manager_rx) = mpsc::channel(1);
        app.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_tx);

        app.execute_add_ingress_action(
            IngestSource::MagnetFile,
            ingest_path,
            super::AddIngressAction::ApplyDirectly {
                payload: ResolvedAddPayload::MagnetLink {
                    magnet_link: magnet_link.to_string(),
                },
                download_path: download_folder.clone(),
            },
        )
        .await;

        let manager_command = manager_rx
            .try_recv()
            .expect("direct add should apply config to the preview runtime");
        match manager_command {
            ManagerCommand::SetUserTorrentConfig {
                torrent_data_path,
                file_priorities,
                container_name,
            } => {
                assert_eq!(torrent_data_path, download_folder);
                assert!(file_priorities.is_empty());
                assert!(container_name.is_none());
            }
            other => panic!("unexpected manager command: {:?}", other),
        }
        assert!(app.app_state.pending_magnet_preview_info_hash.is_none());

        let mut applied_settings = app.client_configs.clone();
        let applied_payload =
            build_persist_payload(&mut applied_settings, &mut app.app_state, &VecDeque::new());
        let persisted = applied_payload
            .settings
            .torrents
            .iter()
            .find(|torrent| torrent.torrent_or_magnet == magnet_link)
            .expect("directly applied magnet should persist after marker clears");
        assert_eq!(persisted.download_path.as_ref(), Some(&download_folder));

        app.cleanup_pending_magnet_preview_runtime();
        assert!(app.app_state.torrents.contains_key(&info_hash));
        assert!(app.torrent_manager_command_txs.contains_key(&info_hash));

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn direct_torrent_file_apply_clears_pending_preview_before_persistence() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let watch_folder = temp_dir.path().join("watch");
        let download_folder = temp_dir.path().join("downloads");
        std::fs::create_dir_all(&watch_folder).expect("create watch folder");
        std::fs::create_dir_all(&download_folder).expect("create downloads");

        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("integration_tests")
            .join("torrents")
            .join("v1")
            .join("single_4k.bin.torrent");
        let ingest_path = watch_folder.join("same-hash.torrent");
        std::fs::copy(&fixture, &ingest_path).expect("copy fixture");
        let torrent_bytes = std::fs::read(&ingest_path).expect("read torrent");
        let info_hash = info_hash_from_torrent_bytes(&torrent_bytes).expect("torrent info hash");
        let magnet_link = format!("magnet:?xt=urn:btih:{}", hex::encode(&info_hash));

        let settings = crate::config::Settings {
            client_port: 0,
            watch_folder: Some(watch_folder),
            default_download_folder: Some(download_folder.clone()),
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}

        app.app_state.pending_torrent_link = magnet_link.clone();
        app.app_state.pending_magnet_preview_info_hash = Some(info_hash.clone());
        app.app_state.torrents.insert(
            info_hash.clone(),
            TorrentDisplayState {
                latest_state: TorrentMetrics {
                    info_hash: info_hash.clone(),
                    torrent_or_magnet: magnet_link.clone(),
                    torrent_name: "sample-preview".to_string(),
                    torrent_control_state: TorrentControlState::Running,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        app.app_state.torrent_list_order.push(info_hash.clone());
        let (manager_tx, mut manager_rx) = mpsc::channel(1);
        app.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_tx);

        app.execute_add_ingress_action(
            IngestSource::TorrentFile,
            ingest_path.clone(),
            super::AddIngressAction::ApplyDirectly {
                payload: ResolvedAddPayload::TorrentFile {
                    source_path: ingest_path.clone(),
                },
                download_path: download_folder.clone(),
            },
        )
        .await;

        let manager_command = manager_rx
            .try_recv()
            .expect("direct add should apply config to the preview runtime");
        match manager_command {
            ManagerCommand::SetUserTorrentConfig {
                torrent_data_path,
                file_priorities,
                container_name,
            } => {
                assert_eq!(torrent_data_path, download_folder);
                assert!(file_priorities.is_empty());
                assert!(container_name.is_none());
            }
            other => panic!("unexpected manager command: {:?}", other),
        }
        let display = app
            .app_state
            .torrents
            .get(&info_hash)
            .expect("preview runtime should remain");
        assert_eq!(
            display.latest_state.download_path.as_ref(),
            Some(&download_folder)
        );
        assert!(app.app_state.pending_magnet_preview_info_hash.is_none());

        let mut applied_settings = app.client_configs.clone();
        let applied_payload =
            build_persist_payload(&mut applied_settings, &mut app.app_state, &VecDeque::new());
        let persisted = applied_payload
            .settings
            .torrents
            .iter()
            .find(|torrent| torrent.torrent_or_magnet == magnet_link)
            .expect("directly applied torrent file should persist after marker clears");
        assert_eq!(persisted.download_path.as_ref(), Some(&download_folder));

        app.cleanup_pending_magnet_preview_runtime();
        assert!(app.app_state.torrents.contains_key(&info_hash));
        assert!(app.torrent_manager_command_txs.contains_key(&info_hash));
        assert!(!ingest_path.exists());

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn torrent_file_control_add_clears_pending_preview_before_persistence() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let download_folder = temp_dir.path().join("downloads");
        std::fs::create_dir_all(&download_folder).expect("create downloads");

        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("integration_tests")
            .join("torrents")
            .join("v1")
            .join("single_4k.bin.torrent");
        let source_path = temp_dir.path().join("same-hash.torrent");
        std::fs::copy(&fixture, &source_path).expect("copy fixture");
        let torrent_bytes = std::fs::read(&source_path).expect("read torrent");
        let info_hash = info_hash_from_torrent_bytes(&torrent_bytes).expect("torrent info hash");
        let magnet_link = format!("magnet:?xt=urn:btih:{}", hex::encode(&info_hash));

        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(download_folder.clone()),
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}

        app.app_state.pending_torrent_link = magnet_link.clone();
        app.app_state.pending_magnet_preview_info_hash = Some(info_hash.clone());
        app.app_state.torrents.insert(
            info_hash.clone(),
            TorrentDisplayState {
                latest_state: TorrentMetrics {
                    info_hash: info_hash.clone(),
                    torrent_or_magnet: magnet_link.clone(),
                    torrent_name: "sample-preview".to_string(),
                    torrent_control_state: TorrentControlState::Running,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        app.app_state.torrent_list_order.push(info_hash.clone());
        let (manager_tx, mut manager_rx) = mpsc::channel(1);
        app.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_tx);

        app.apply_control_request(&ControlRequest::AddTorrentFile {
            source_path: source_path.clone(),
            download_path: Some(download_folder.clone()),
            container_name: None,
            validation_status: false,
            file_priorities: Vec::new(),
        })
        .await
        .expect("control torrent add");

        let manager_command = manager_rx
            .try_recv()
            .expect("control add should apply config to the preview runtime");
        match manager_command {
            ManagerCommand::SetUserTorrentConfig {
                torrent_data_path,
                file_priorities,
                container_name,
            } => {
                assert_eq!(torrent_data_path, download_folder);
                assert!(file_priorities.is_empty());
                assert!(container_name.is_none());
            }
            other => panic!("unexpected manager command: {:?}", other),
        }
        assert!(app.app_state.pending_magnet_preview_info_hash.is_none());

        let mut applied_settings = app.client_configs.clone();
        let applied_payload =
            build_persist_payload(&mut applied_settings, &mut app.app_state, &VecDeque::new());
        let persisted = applied_payload
            .settings
            .torrents
            .iter()
            .find(|torrent| torrent.torrent_or_magnet == magnet_link)
            .expect("control-applied torrent file should persist after marker clears");
        assert_eq!(persisted.download_path.as_ref(), Some(&download_folder));

        app.cleanup_pending_magnet_preview_runtime();
        assert!(app.app_state.torrents.contains_key(&info_hash));
        assert!(app.torrent_manager_command_txs.contains_key(&info_hash));

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn duplicate_torrent_file_ingest_keeps_existing_config_without_pending_preview() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let initial_download_path = temp_dir.path().join("initial-downloads");
        let duplicate_download_path = temp_dir.path().join("duplicate-downloads");
        std::fs::create_dir_all(&initial_download_path).expect("create initial downloads");
        std::fs::create_dir_all(&duplicate_download_path).expect("create duplicate downloads");

        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("integration_tests")
            .join("torrents")
            .join("v1")
            .join("single_4k.bin.torrent");
        let source_path = temp_dir.path().join("duplicate.torrent");
        std::fs::copy(&fixture, &source_path).expect("copy fixture");
        let torrent_bytes = std::fs::read(&source_path).expect("read torrent");
        let info_hash = info_hash_from_torrent_bytes(&torrent_bytes).expect("torrent info hash");
        let original_priorities = HashMap::from([(0, FilePriority::Skip)]);

        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}
        app.app_state.torrents.insert(
            info_hash.clone(),
            TorrentDisplayState {
                latest_state: TorrentMetrics {
                    info_hash: info_hash.clone(),
                    torrent_name: "existing-sample".to_string(),
                    torrent_control_state: TorrentControlState::Running,
                    download_path: Some(initial_download_path.clone()),
                    container_name: Some("Existing Sample".to_string()),
                    file_priorities: original_priorities.clone(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        app.app_state.torrent_list_order.push(info_hash.clone());
        let (manager_tx, mut manager_rx) = mpsc::channel(1);
        app.torrent_manager_command_txs
            .insert(info_hash.clone(), manager_tx);

        let result = app
            .add_torrent_from_file(
                source_path,
                Some(duplicate_download_path),
                false,
                TorrentControlState::Running,
                HashMap::new(),
                None,
            )
            .await;

        assert!(matches!(result, CommandIngestResult::Duplicate { .. }));
        assert!(manager_rx.try_recv().is_err());
        let display = app
            .app_state
            .torrents
            .get(&info_hash)
            .expect("existing runtime should remain");
        assert_eq!(
            display.latest_state.download_path.as_ref(),
            Some(&initial_download_path)
        );
        assert_eq!(
            display.latest_state.container_name.as_deref(),
            Some("Existing Sample")
        );
        assert_eq!(display.latest_state.file_priorities, original_priorities);

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn duplicate_magnet_config_update_persists_file_priorities_in_app_state() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let initial_download_path = temp_dir.path().join("initial-downloads");
        let selected_download_path = temp_dir.path().join("chosen-downloads");
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let magnet_link = "magnet:?xt=urn:btih:5555555555555555555555555555555555555555";

        let first = app
            .add_magnet_torrent(
                "Fetching name...".to_string(),
                magnet_link.to_string(),
                Some(initial_download_path),
                false,
                TorrentControlState::Running,
                HashMap::new(),
                None,
            )
            .await;
        assert!(matches!(first, CommandIngestResult::Added { .. }));

        let selected_priorities = HashMap::from([(0, FilePriority::Skip), (2, FilePriority::High)]);
        let second = app
            .add_magnet_torrent(
                "Hydrated Magnet".to_string(),
                magnet_link.to_string(),
                Some(selected_download_path.clone()),
                false,
                TorrentControlState::Running,
                selected_priorities.clone(),
                Some("Hydrated Magnet".to_string()),
            )
            .await;

        let info_hash = vec![0x55; 20];
        assert!(matches!(second, CommandIngestResult::Duplicate { .. }));
        let display = app
            .app_state
            .torrents
            .get(&info_hash)
            .expect("existing preview torrent should remain in app state");
        assert_eq!(
            display.latest_state.download_path,
            Some(selected_download_path)
        );
        assert_eq!(
            display.latest_state.container_name,
            Some("Hydrated Magnet".to_string())
        );
        assert_eq!(display.latest_state.file_priorities, selected_priorities);

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn missing_default_download_folder_routes_magnet_to_manual_browser() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let magnet_path = temp_dir.path().join("manual-input.magnet");
        std::fs::write(
            &magnet_path,
            "magnet:?xt=urn:btih:5555555555555555555555555555555555555555",
        )
        .expect("write magnet");
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: None,
            always_show_add_location_prompt: false,
            ..Default::default()
        };
        let app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");

        let action = app.resolve_add_ingress_action(IngestSource::MagnetFile, &magnet_path);

        assert!(matches!(
            action,
            super::AddIngressAction::OpenManualBrowser { .. }
        ));
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn shared_leader_always_show_prompt_overrides_shared_inbox_magnet_fast_path() {
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
            "client_port = 0\ndefault_download_folder = '/tmp/superseedr-test-downloads'\nalways_show_add_location_prompt = true\n",
        )
        .expect("write host config");
        let inbox = effective_root.join("inbox");
        std::fs::create_dir_all(&inbox).expect("create shared inbox");
        let magnet_path = inbox.join("manual-input.magnet");
        std::fs::write(
            &magnet_path,
            "magnet:?xt=urn:btih:5555555555555555555555555555555555555555",
        )
        .expect("write magnet");

        let mut app = App::new(
            crate::config::load_settings().expect("load shared settings"),
            AppRuntimeMode::SharedLeader,
        )
        .await
        .expect("build shared app");
        while app.app_command_rx.try_recv().is_ok() {}

        let action = app.resolve_add_ingress_action(IngestSource::MagnetFile, &magnet_path);

        assert!(matches!(
            action,
            super::AddIngressAction::OpenManualBrowser { .. }
        ));
        app.execute_add_ingress_action(IngestSource::MagnetFile, magnet_path.clone(), action)
            .await;
        let processed_path = effective_root.join("processed").join("manual-input.magnet");
        assert!(magnet_path.exists());
        assert!(!processed_path.exists());
        let pending_manual = app
            .app_state
            .pending_manual_ingest
            .as_ref()
            .expect("manual ingest should wait for confirmation");
        assert_eq!(pending_manual.path, magnet_path);
        assert_eq!(pending_manual.source, IngestSource::MagnetFile);

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
    async fn shared_leader_always_show_prompt_defers_shared_inbox_torrent_archive() {
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
            "client_port = 0\ndefault_download_folder = '/tmp/superseedr-test-downloads'\nalways_show_add_location_prompt = true\n",
        )
        .expect("write host config");
        let inbox = effective_root.join("inbox");
        std::fs::create_dir_all(&inbox).expect("create shared inbox");
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("integration_tests")
            .join("torrents")
            .join("v1")
            .join("single_4k.bin.torrent");
        let torrent_path = inbox.join("manual-input.torrent");
        std::fs::copy(&fixture, &torrent_path).expect("copy fixture");

        let mut app = App::new(
            crate::config::load_settings().expect("load shared settings"),
            AppRuntimeMode::SharedLeader,
        )
        .await
        .expect("build shared app");
        while app.app_command_rx.try_recv().is_ok() {}
        assert!(app.record_ingest_queued(
            torrent_path.clone(),
            IngestOrigin::WatchFolder,
            IngestKind::TorrentFile,
            crate::config::shared_inbox_path(),
        ));

        let action = app.resolve_add_ingress_action(IngestSource::TorrentFile, &torrent_path);

        assert!(matches!(
            action,
            super::AddIngressAction::OpenManualBrowser { .. }
        ));
        app.execute_add_ingress_action(IngestSource::TorrentFile, torrent_path.clone(), action)
            .await;
        let processed_path = effective_root
            .join("processed")
            .join("manual-input.torrent");
        assert!(torrent_path.exists());
        assert!(!processed_path.exists());
        assert_eq!(
            app.app_state.pending_torrent_path.as_ref(),
            Some(&torrent_path)
        );
        let pending_manual = app
            .app_state
            .pending_manual_ingest
            .as_ref()
            .expect("manual ingest should wait for confirmation");
        assert_eq!(pending_manual.path, torrent_path);
        assert_eq!(pending_manual.source, IngestSource::TorrentFile);

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

    #[test]
    fn torrent_preview_tree_marks_only_ancestor_folders_mixed() {
        let priorities = HashMap::from([(0, FilePriority::Skip)]);
        let tree = build_torrent_preview_tree(
            vec![
                (vec!["changed".to_string(), "one.bin".to_string()], 10),
                (vec!["changed".to_string(), "two.bin".to_string()], 20),
                (vec!["unchanged".to_string(), "three.bin".to_string()], 30),
            ],
            &priorities,
        );

        let changed = tree
            .iter()
            .find(|node| node.name == "changed")
            .expect("changed folder");
        let unchanged = tree
            .iter()
            .find(|node| node.name == "unchanged")
            .expect("unchanged folder");

        assert_eq!(changed.payload.priority, FilePriority::Mixed);
        assert_eq!(unchanged.payload.priority, FilePriority::Normal);
    }

    #[test]
    fn torrent_preview_tree_marks_uniform_priority_folder_as_that_priority() {
        let priorities = HashMap::from([(0, FilePriority::Skip), (1, FilePriority::Skip)]);
        let tree = build_torrent_preview_tree(
            vec![
                (vec!["season".to_string(), "one.bin".to_string()], 10),
                (vec!["season".to_string(), "two.bin".to_string()], 20),
            ],
            &priorities,
        );

        assert_eq!(tree[0].payload.priority, FilePriority::Skip);
    }

    #[tokio::test]
    async fn open_existing_torrent_file_browser_starts_on_priority_preview() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(temp_dir.path().to_path_buf()),
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}

        let info_hash = vec![9; 20];
        let file_priorities = HashMap::from([(0, FilePriority::High)]);
        let preview_tree = build_torrent_preview_tree(
            vec![(vec!["sample".to_string(), "segment.bin".to_string()], 42)],
            &file_priorities,
        );
        app.app_state.torrents.insert(
            info_hash.clone(),
            TorrentDisplayState {
                latest_state: TorrentMetrics {
                    info_hash: info_hash.clone(),
                    torrent_name: "Sample Selector".to_string(),
                    download_path: Some(temp_dir.path().to_path_buf()),
                    is_multi_file: true,
                    file_priorities,
                    ..Default::default()
                },
                file_preview_tree: preview_tree,
                ..Default::default()
            },
        );

        app.open_existing_torrent_file_browser(info_hash.clone());

        assert!(app.app_command_rx.try_recv().is_err());
        assert!(matches!(app.app_state.mode, AppMode::FileBrowser));
        assert!(app.app_state.ui.file_browser.data.is_empty());
        match &app.app_state.ui.file_browser.browser_mode {
            FileBrowserMode::DownloadLocSelection {
                target,
                focused_pane,
                preview_tree,
                use_container,
                container_name,
                ..
            } => {
                assert_eq!(
                    target,
                    &DownloadSelectionTarget::ExistingTorrent { info_hash }
                );
                assert_eq!(*focused_pane, BrowserPane::TorrentPreview);
                assert!(!preview_tree.is_empty());
                assert!(!*use_container);
                assert!(container_name.is_empty());
            }
            _ => panic!("expected priority-only existing torrent browser"),
        }

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn metadata_loaded_preserves_existing_torrent_priority_overrides_in_empty_preview() {
        fn record_leaf_priorities(
            node: &RawNode<TorrentPreviewPayload>,
            priorities_by_index: &mut HashMap<usize, FilePriority>,
        ) {
            if let Some(file_index) = node.payload.file_index {
                priorities_by_index.insert(file_index, node.payload.priority);
            }
            for child in &node.children {
                record_leaf_priorities(child, priorities_by_index);
            }
        }

        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let info_hash = vec![3; 20];
        let file_priorities = HashMap::from([(0, FilePriority::Skip), (2, FilePriority::High)]);
        app.app_state.torrents.insert(
            info_hash.clone(),
            TorrentDisplayState {
                latest_state: TorrentMetrics {
                    info_hash: info_hash.clone(),
                    torrent_name: "Priority Hydration".to_string(),
                    file_priorities,
                    ..Default::default()
                },
                file_preview_tree: Vec::new(),
                ..Default::default()
            },
        );
        app.app_state.ui.file_browser.browser_mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::ExistingTorrent {
                info_hash: info_hash.clone(),
            },
            torrent_files: Vec::new(),
            container_name: String::new(),
            use_container: false,
            is_editing_name: false,
            preview_tree: Vec::new(),
            preview_state: Default::default(),
            focused_pane: BrowserPane::TorrentPreview,
            cursor_pos: 0,
            original_name_backup: String::new(),
        };

        let torrent = crate::torrent_file::Torrent {
            info: crate::torrent_file::Info {
                name: "Priority Hydration".to_string(),
                files: vec![
                    crate::torrent_file::InfoFile {
                        length: 10,
                        path: vec!["group".to_string(), "skip.bin".to_string()],
                        md5sum: None,
                        attr: None,
                    },
                    crate::torrent_file::InfoFile {
                        length: 20,
                        path: vec!["group".to_string(), "normal.bin".to_string()],
                        md5sum: None,
                        attr: None,
                    },
                    crate::torrent_file::InfoFile {
                        length: 30,
                        path: vec!["group".to_string(), "high.bin".to_string()],
                        md5sum: None,
                        attr: None,
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        };

        app.handle_manager_event(ManagerEvent::MetadataLoaded {
            info_hash: info_hash.clone(),
            torrent: Box::new(torrent),
        });

        let FileBrowserMode::DownloadLocSelection { preview_tree, .. } =
            &app.app_state.ui.file_browser.browser_mode
        else {
            panic!("expected download selection browser");
        };
        let mut priorities_by_index = HashMap::new();
        for node in preview_tree {
            record_leaf_priorities(node, &mut priorities_by_index);
        }

        assert_eq!(priorities_by_index.get(&0), Some(&FilePriority::Skip));
        assert_eq!(priorities_by_index.get(&1), Some(&FilePriority::Normal));
        assert_eq!(priorities_by_index.get(&2), Some(&FilePriority::High));

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn stale_file_browser_fetch_is_ignored() {
        let stale_dir = tempfile::tempdir().expect("create stale dir");
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        app.app_state.mode = AppMode::Normal;
        app.app_state.ui.file_browser.browser_generation = 2;
        let initial_path = app.app_state.ui.file_browser.state.current_path.clone();
        let initial_request_id = app.app_state.ui.file_browser.fetch_request_id;

        app.handle_app_command(AppCommand::FetchFileTree {
            browser_generation: 1,
            path: stale_dir.path().to_path_buf(),
            browser_mode: FileBrowserMode::Directory,
            preserve_browser_mode: false,
            highlight_path: None,
        })
        .await;

        assert!(matches!(app.app_state.mode, AppMode::Normal));
        assert_eq!(
            app.app_state.ui.file_browser.state.current_path,
            initial_path
        );
        assert_eq!(
            app.app_state.ui.file_browser.fetch_request_id,
            initial_request_id
        );
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn file_browser_fetch_preserves_hydrated_pending_magnet_preview() {
        let current_dir = tempfile::tempdir().expect("create current dir");
        let next_dir = tempfile::tempdir().expect("create next dir");
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        app.app_state.mode = AppMode::FileBrowser;
        app.app_state.ui.file_browser.browser_generation = 1;
        app.app_state.ui.file_browser.state.current_path = current_dir.path().to_path_buf();
        app.app_state.ui.file_browser.search_state = BrowserSearchState::Applied;
        app.app_state.ui.file_browser.search_query = "hydrated".to_string();
        app.app_state.ui.file_browser.browser_mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::PendingAdd,
            torrent_files: vec![],
            container_name: "Hydrated Magnet [abcd]".to_string(),
            use_container: true,
            is_editing_name: false,
            preview_tree: vec![RawNode {
                name: "hydrated.bin".to_string(),
                full_path: PathBuf::from("hydrated.bin"),
                children: vec![],
                payload: TorrentPreviewPayload {
                    size: 10,
                    priority: FilePriority::Normal,
                    file_index: Some(0),
                },
                is_dir: false,
            }],
            preview_state: TreeViewState::default(),
            focused_pane: BrowserPane::TorrentPreview,
            cursor_pos: 0,
            original_name_backup: "Hydrated Magnet [abcd]".to_string(),
        };

        app.handle_app_command(AppCommand::FetchFileTree {
            browser_generation: 1,
            path: next_dir.path().to_path_buf(),
            browser_mode: FileBrowserMode::DownloadLocSelection {
                target: DownloadSelectionTarget::PendingAdd,
                torrent_files: vec![],
                container_name: AWAITING_MAGNET_METADATA_LABEL.to_string(),
                use_container: true,
                is_editing_name: false,
                preview_tree: Vec::new(),
                preview_state: TreeViewState::default(),
                focused_pane: BrowserPane::FileSystem,
                cursor_pos: 0,
                original_name_backup: AWAITING_MAGNET_METADATA_LABEL.to_string(),
            },
            preserve_browser_mode: true,
            highlight_path: None,
        })
        .await;

        assert_eq!(
            app.app_state.ui.file_browser.state.current_path,
            next_dir.path()
        );
        let FileBrowserMode::DownloadLocSelection {
            container_name,
            original_name_backup,
            preview_tree,
            focused_pane,
            ..
        } = &app.app_state.ui.file_browser.browser_mode
        else {
            panic!("expected download selection browser");
        };
        assert_eq!(container_name, "Hydrated Magnet [abcd]");
        assert_eq!(original_name_backup, "Hydrated Magnet [abcd]");
        assert_eq!(*focused_pane, BrowserPane::TorrentPreview);
        assert_eq!(preview_tree.len(), 1);
        assert_eq!(preview_tree[0].name, "hydrated.bin");
        assert_eq!(
            app.app_state.ui.file_browser.search_state,
            BrowserSearchState::Applied
        );
        assert_eq!(app.app_state.ui.file_browser.search_query, "hydrated");

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn file_browser_fetch_replaces_pending_add_preview_for_new_open() {
        let current_dir = tempfile::tempdir().expect("create current dir");
        let next_dir = tempfile::tempdir().expect("create next dir");
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        app.app_state.mode = AppMode::FileBrowser;
        app.app_state.ui.file_browser.browser_generation = 1;
        app.app_state.ui.file_browser.state.current_path = current_dir.path().to_path_buf();
        app.app_state.ui.file_browser.search_state = BrowserSearchState::Applied;
        app.app_state.ui.file_browser.search_query = "old pending".to_string();
        app.app_state.ui.file_browser.fetch_error = Some("old browser failure".to_string());
        app.app_state.system_error = Some("unrelated application failure".to_string());
        app.app_state.ui.file_browser.browser_mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::PendingAdd,
            torrent_files: vec![],
            container_name: "Old Pending [aaaa]".to_string(),
            use_container: true,
            is_editing_name: false,
            preview_tree: vec![RawNode {
                name: "old.bin".to_string(),
                full_path: PathBuf::from("old.bin"),
                children: vec![],
                payload: TorrentPreviewPayload {
                    size: 10,
                    priority: FilePriority::Normal,
                    file_index: Some(0),
                },
                is_dir: false,
            }],
            preview_state: TreeViewState::default(),
            focused_pane: BrowserPane::TorrentPreview,
            cursor_pos: 0,
            original_name_backup: "Old Pending [aaaa]".to_string(),
        };

        app.handle_app_command(AppCommand::FetchFileTree {
            browser_generation: 1,
            path: next_dir.path().to_path_buf(),
            browser_mode: FileBrowserMode::DownloadLocSelection {
                target: DownloadSelectionTarget::PendingAdd,
                torrent_files: vec![],
                container_name: "New Pending [bbbb]".to_string(),
                use_container: false,
                is_editing_name: false,
                preview_tree: vec![RawNode {
                    name: "new.bin".to_string(),
                    full_path: PathBuf::from("new.bin"),
                    children: vec![],
                    payload: TorrentPreviewPayload {
                        size: 20,
                        priority: FilePriority::Normal,
                        file_index: Some(0),
                    },
                    is_dir: false,
                }],
                preview_state: TreeViewState::default(),
                focused_pane: BrowserPane::FileSystem,
                cursor_pos: 0,
                original_name_backup: "New Pending [bbbb]".to_string(),
            },
            preserve_browser_mode: false,
            highlight_path: None,
        })
        .await;

        let FileBrowserMode::DownloadLocSelection {
            container_name,
            original_name_backup,
            preview_tree,
            focused_pane,
            use_container,
            ..
        } = &app.app_state.ui.file_browser.browser_mode
        else {
            panic!("expected download selection browser");
        };
        assert_eq!(container_name, "New Pending [bbbb]");
        assert_eq!(original_name_backup, "New Pending [bbbb]");
        assert_eq!(*focused_pane, BrowserPane::FileSystem);
        assert!(!use_container);
        assert_eq!(preview_tree.len(), 1);
        assert_eq!(preview_tree[0].name, "new.bin");
        assert_eq!(
            app.app_state.ui.file_browser.search_state,
            BrowserSearchState::Closed
        );
        assert!(app.app_state.ui.file_browser.search_query.is_empty());
        assert!(app.app_state.ui.file_browser.fetch_error.is_none());
        assert_eq!(
            app.app_state.system_error.as_deref(),
            Some("unrelated application failure")
        );

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn stale_file_browser_update_is_ignored() {
        let current_dir = tempfile::tempdir().expect("create current dir");
        let stale_dir = tempfile::tempdir().expect("create stale dir");
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        app.app_state.mode = AppMode::FileBrowser;
        app.app_state.ui.file_browser.fetch_request_id = 2;
        app.app_state.ui.file_browser.state.current_path = current_dir.path().to_path_buf();

        app.handle_app_command(AppCommand::UpdateFileBrowserData {
            request_id: 1,
            path: stale_dir.path().to_path_buf(),
            data: vec![RawNode {
                name: "stale.bin".to_string(),
                full_path: stale_dir.path().join("stale.bin"),
                children: vec![],
                payload: FileMetadata {
                    size: 1,
                    modified: std::time::UNIX_EPOCH,
                },
                is_dir: false,
            }],
            highlight_path: None,
        })
        .await;

        assert!(app.app_state.ui.file_browser.data.is_empty());

        app.handle_app_command(AppCommand::UpdateFileBrowserData {
            request_id: 2,
            path: current_dir.path().to_path_buf(),
            data: vec![RawNode {
                name: "current.bin".to_string(),
                full_path: current_dir.path().join("current.bin"),
                children: vec![],
                payload: FileMetadata {
                    size: 1,
                    modified: std::time::UNIX_EPOCH,
                },
                is_dir: false,
            }],
            highlight_path: None,
        })
        .await;

        assert_eq!(app.app_state.ui.file_browser.data.len(), 1);
        assert_eq!(app.app_state.ui.file_browser.data[0].name, "current.bin");
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn invalidating_browser_generation_rejects_late_fetch_success_and_failure() {
        let current_dir = tempfile::tempdir().expect("create current dir");
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let retained_path = current_dir.path().join("retained.bin");
        app.app_state.mode = AppMode::FileBrowser;
        app.app_state.ui.file_browser.fetch_request_id = 7;
        app.app_state.ui.file_browser.fetch_error = Some("retained crawl failure".to_string());
        app.app_state.ui.file_browser.state.current_path = current_dir.path().to_path_buf();
        app.app_state.ui.file_browser.data = vec![RawNode {
            name: "retained.bin".to_string(),
            full_path: retained_path.clone(),
            children: vec![],
            payload: FileMetadata {
                size: 1,
                modified: std::time::UNIX_EPOCH,
            },
            is_dir: false,
        }];

        app.app_state
            .ui
            .file_browser
            .invalidate_browser_generation();
        assert_eq!(app.app_state.ui.file_browser.fetch_request_id, 8);

        app.handle_app_command(AppCommand::UpdateFileBrowserData {
            request_id: 7,
            path: current_dir.path().to_path_buf(),
            data: vec![RawNode {
                name: "late.bin".to_string(),
                full_path: current_dir.path().join("late.bin"),
                children: vec![],
                payload: FileMetadata {
                    size: 2,
                    modified: std::time::UNIX_EPOCH,
                },
                is_dir: false,
            }],
            highlight_path: None,
        })
        .await;
        app.handle_app_command(AppCommand::FileBrowserFetchFailed {
            request_id: 7,
            path: current_dir.path().to_path_buf(),
            message: "late crawl failure".to_string(),
        })
        .await;

        assert_eq!(app.app_state.ui.file_browser.data.len(), 1);
        assert_eq!(
            app.app_state.ui.file_browser.data[0].full_path,
            retained_path
        );
        assert!(app.app_state.ui.file_browser.fetch_error.is_none());
        assert!(app.app_state.system_error.is_none());
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn file_browser_update_selects_a_visible_directory_when_no_target_files_exist() {
        let current_dir = tempfile::tempdir().expect("create current dir");
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let hidden_path = current_dir.path().join("alpha.txt");
        let visible_path = current_dir.path().join("folder");
        app.app_state.mode = AppMode::FileBrowser;
        app.app_state.ui.file_browser.fetch_request_id = 4;
        app.app_state.ui.file_browser.state.current_path = current_dir.path().to_path_buf();
        app.app_state.ui.file_browser.browser_mode =
            FileBrowserMode::File(vec![".torrent".to_string()]);

        app.handle_app_command(AppCommand::UpdateFileBrowserData {
            request_id: 4,
            path: current_dir.path().to_path_buf(),
            data: vec![
                RawNode {
                    name: "alpha.txt".to_string(),
                    full_path: hidden_path,
                    children: vec![],
                    payload: FileMetadata {
                        size: 1,
                        modified: std::time::UNIX_EPOCH,
                    },
                    is_dir: false,
                },
                RawNode {
                    name: "folder".to_string(),
                    full_path: visible_path.clone(),
                    children: vec![],
                    payload: FileMetadata {
                        size: 0,
                        modified: std::time::UNIX_EPOCH,
                    },
                    is_dir: true,
                },
            ],
            highlight_path: None,
        })
        .await;

        assert_eq!(
            app.app_state.ui.file_browser.state.cursor_path,
            Some(visible_path)
        );
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn file_browser_update_reconciles_cursor_with_applied_search() {
        let current_dir = tempfile::tempdir().expect("create current dir");
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let matching_path = current_dir.path().join("beta-folder");
        app.app_state.mode = AppMode::FileBrowser;
        app.app_state.ui.file_browser.fetch_request_id = 5;
        app.app_state.ui.file_browser.state.current_path = current_dir.path().to_path_buf();
        app.app_state.ui.file_browser.browser_mode = FileBrowserMode::Directory;
        app.app_state.ui.file_browser.search_state = BrowserSearchState::Applied;
        app.app_state.ui.file_browser.search_query = "beta".to_string();
        app.app_state.ui.file_browser.search_mode = SearchMode::Fuzzy;

        app.handle_app_command(AppCommand::UpdateFileBrowserData {
            request_id: 5,
            path: current_dir.path().to_path_buf(),
            data: vec![
                RawNode {
                    name: "alpha-folder".to_string(),
                    full_path: current_dir.path().join("alpha-folder"),
                    children: vec![],
                    payload: FileMetadata {
                        size: 0,
                        modified: std::time::UNIX_EPOCH,
                    },
                    is_dir: true,
                },
                RawNode {
                    name: "beta-folder".to_string(),
                    full_path: matching_path.clone(),
                    children: vec![],
                    payload: FileMetadata {
                        size: 0,
                        modified: std::time::UNIX_EPOCH,
                    },
                    is_dir: true,
                },
            ],
            highlight_path: Some(current_dir.path().join("alpha-folder")),
        })
        .await;

        assert_eq!(
            app.app_state.ui.file_browser.state.cursor_path,
            Some(matching_path)
        );
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn matching_file_browser_fetch_failure_clears_stale_rows() {
        let current_dir = tempfile::tempdir().expect("create current dir");
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let stale_path = current_dir.path().join("stale.bin");
        app.app_state.mode = AppMode::FileBrowser;
        app.app_state.ui.file_browser.fetch_request_id = 7;
        app.app_state.ui.file_browser.fetch_pending = true;
        app.app_state.ui.file_browser.state.current_path = current_dir.path().to_path_buf();
        app.app_state.ui.file_browser.state.cursor_path = Some(stale_path.clone());
        app.app_state.ui.file_browser.data = vec![RawNode {
            name: "stale.bin".to_string(),
            full_path: stale_path,
            children: vec![],
            payload: FileMetadata {
                size: 1,
                modified: std::time::UNIX_EPOCH,
            },
            is_dir: false,
        }];

        app.handle_app_command(AppCommand::FileBrowserFetchFailed {
            request_id: 7,
            path: current_dir.path().to_path_buf(),
            message: "Directory could not be read".to_string(),
        })
        .await;

        assert!(!app.app_state.ui.file_browser.fetch_pending);
        assert!(app.app_state.ui.file_browser.data.is_empty());
        assert!(app.app_state.ui.file_browser.state.cursor_path.is_none());
        assert_eq!(
            app.app_state.ui.file_browser.fetch_error.as_deref(),
            Some("Directory could not be read")
        );
        assert!(app.app_state.system_error.is_none());
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn matching_file_browser_success_clears_only_browser_fetch_error() {
        let current_dir = tempfile::tempdir().expect("create current dir");
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        app.app_state.mode = AppMode::FileBrowser;
        app.app_state.ui.file_browser.fetch_request_id = 8;
        app.app_state.ui.file_browser.fetch_error = Some("old browser failure".to_string());
        app.app_state.ui.file_browser.state.current_path = current_dir.path().to_path_buf();
        app.app_state.system_error = Some("unrelated application failure".to_string());

        app.handle_app_command(AppCommand::UpdateFileBrowserData {
            request_id: 8,
            path: current_dir.path().to_path_buf(),
            data: Vec::new(),
            highlight_path: None,
        })
        .await;

        assert!(app.app_state.ui.file_browser.fetch_error.is_none());
        assert_eq!(
            app.app_state.system_error.as_deref(),
            Some("unrelated application failure")
        );
        let _ = app.shutdown_tx.send(());
    }

    #[test]
    fn torrent_file_preview_loader_builds_cached_render_data() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("integration_tests")
            .join("torrents")
            .join("v1")
            .join("single_4k.bin.torrent");

        let preview = load_torrent_file_preview(&fixture).expect("load preview");

        assert!(!preview.name.is_empty());
        assert_eq!(preview.protocol_version, "BitTorrent v1");
        assert_eq!(preview.total_size, 4096);
        assert!(!preview.tree.is_empty());
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
        let _guard = lock_shared_env();
        let _temp_paths = configure_temp_app_paths_for_test();
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
        let display_state = app
            .display_state_from_torrent_settings(&app.client_configs.torrents[0])
            .expect("display state");
        app.app_state
            .torrents
            .insert(info_hash.clone(), display_state);
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
        let display_state = app
            .display_state_from_torrent_settings(&app.client_configs.torrents[0])
            .expect("display state");
        app.app_state
            .torrents
            .insert(info_hash.clone(), display_state);
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

        let host_status_path = crate::config::shared_status_path().expect("host status path");
        let leader_status_path =
            crate::config::shared_leader_status_path().expect("leader status path");

        app.dump_status_to_file();
        time::timeout(Duration::from_secs(5), async {
            while !host_status_path.exists() || !leader_status_path.exists() {
                time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("status dumps should be written");

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
    async fn standalone_zero_status_interval_remains_disabled() {
        let settings = crate::config::Settings {
            client_port: 0,
            output_status_interval: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build standalone app");

        assert_eq!(app.effective_status_dump_interval_secs(), 0);
        app.reschedule_status_dump_deadline();
        assert!(app.next_status_dump_at.is_none());

        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn shared_nodes_default_status_follow_to_five_seconds() {
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
        for runtime_mode in [AppRuntimeMode::SharedLeader, AppRuntimeMode::SharedFollower] {
            let mut app = App::new(settings.clone(), runtime_mode)
                .await
                .expect("build shared app");

            assert_eq!(app.client_configs.output_status_interval, 0);
            assert_eq!(app.effective_status_dump_interval_secs(), 5);
            app.reschedule_status_dump_deadline();
            assert!(
                app.next_status_dump_at.is_some(),
                "{runtime_mode:?} should keep the shared status timer armed"
            );

            let _ = app.shutdown_tx.send(());
        }
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
        let (_occupied_network_handle, occupied_network_lease) = unrestricted_network_lease();
        let occupied = occupied_network_lease
            .bind_tcp_listener(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
            .await
            .expect("bind occupied IPv4 port");
        let port = occupied.local_addr().expect("occupied local addr").port();
        let ipv6_can_bind_alongside_ipv4 = match occupied_network_lease
            .bind_tcp_listener(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port))
            .await
        {
            Ok(listener) => {
                drop(listener);
                true
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::AddrInUse
                        | io::ErrorKind::AddrNotAvailable
                        | io::ErrorKind::Unsupported
                ) =>
            {
                false
            }
            Err(error) => panic!("probe IPv6 bind with occupied IPv4 port: {error}"),
        };

        let (_network_handle, network_lease) = unrestricted_network_lease();
        match ListenerSet::bind(&network_lease, port, true, false).await {
            Ok(listener_set) => {
                assert!(
                    ipv6_can_bind_alongside_ipv4,
                    "expected full bind failure when IPv4 occupancy also blocks IPv6"
                );
                assert!(listener_set.ipv6_bound);
                assert!(!listener_set.ipv4_bound);
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

        let (_network_handle, network_lease) = unrestricted_network_lease();
        match ListenerSet::bind(&network_lease, port, true, false).await {
            Ok(listener_set) => {
                assert!(listener_set.ipv4_bound);
                assert!(!listener_set.ipv6_bound);
                assert_eq!(listener_set.local_port(), Some(port));
            }
            Err(error) => {
                assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
            }
        }
    }

    #[tokio::test]
    async fn listener_set_bind_can_run_utp_without_tcp() {
        let (_network_handle, network_lease) = unrestricted_network_lease();
        let listener_set = ListenerSet::bind(&network_lease, 0, false, true)
            .await
            .expect("bind uTP-only listener");

        assert!(!listener_set.ipv4_bound);
        assert!(!listener_set.ipv6_bound);
        assert!(listener_set.utp_bound);
        assert!(listener_set.local_port().is_some());
    }

    #[tokio::test]
    async fn listener_set_stops_accepting_after_generation_invalidation() {
        let (network_handle, supervisor_task) =
            NetworkSupervisor::spawn_unrestricted().expect("start network supervisor");
        let network_lease = network_handle.try_lease().expect("network lease");
        let listener_set = ListenerSet::bind(&network_lease, 0, true, false)
            .await
            .expect("bind listener");
        let mut state_rx = network_handle.subscribe();

        network_handle
            .block("test generation invalidation")
            .await
            .expect("block network generation");
        state_rx.changed().await.expect("blocked state");

        let error = match listener_set.accept().await {
            Ok(_) => panic!("invalidated listener accepted a connection"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);

        network_handle
            .shutdown()
            .await
            .expect("shutdown supervisor");
        supervisor_task.await.expect("join network supervisor");
    }

    #[tokio::test]
    async fn generation_invalidation_cancels_backpressured_accept_delivery() {
        let (network_handle, supervisor_task) =
            NetworkSupervisor::spawn_unrestricted().expect("start network supervisor");
        let network_lease = network_handle.try_lease().expect("network lease");
        let listener_set = ListenerSet::bind(&network_lease, 0, true, false)
            .await
            .expect("bind listener");
        let port = listener_set.local_port().expect("listener port");
        let mut clients = Vec::new();
        for _ in 0..65 {
            clients.push(
                time::timeout(
                    Duration::from_secs(1),
                    TcpStream::connect((Ipv4Addr::LOCALHOST, port)),
                )
                .await
                .expect("client connect should not time out")
                .expect("connect client"),
            );
        }
        time::timeout(Duration::from_secs(1), async {
            loop {
                if listener_set.accept_rx.lock().await.len() == 64 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("accept queue should become full");

        network_handle
            .block("test backpressured listener invalidation")
            .await
            .expect("block network generation");

        time::timeout(Duration::from_millis(500), async {
            while listener_set
                .accept_task
                .as_ref()
                .is_some_and(|task| !task.is_finished())
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("backpressured accept task should stop promptly");
        drop(clients);
        drop(listener_set);
        network_handle
            .shutdown()
            .await
            .expect("shutdown supervisor");
        supervisor_task.await.expect("join network supervisor");
    }

    #[tokio::test]
    async fn accepted_peer_connection_retains_listener_generation_invalidation() {
        let (network_handle, supervisor_task) =
            NetworkSupervisor::spawn_unrestricted().expect("start network supervisor");
        let network_lease = network_handle.try_lease().expect("network lease");
        let listener_set = ListenerSet::bind(&network_lease, 0, true, false)
            .await
            .expect("bind listener");
        let port = listener_set.local_port().expect("listener port");
        let client = tokio::spawn(TcpStream::connect((Ipv4Addr::LOCALHOST, port)));
        let connection = time::timeout(Duration::from_secs(1), listener_set.accept())
            .await
            .expect("accept should complete")
            .expect("accept peer connection");
        client.await.expect("client task").expect("connect client");
        let mut invalidation_rx = connection
            .subscribe_network_invalidation()
            .expect("accepted peer must carry its generation");

        network_handle
            .block("test accepted peer cancellation")
            .await
            .expect("block generation");
        time::timeout(Duration::from_millis(500), invalidation_rx.changed())
            .await
            .expect("accepted peer should be invalidated promptly")
            .expect("invalidation channel");
        assert!(*invalidation_rx.borrow());

        network_handle
            .shutdown()
            .await
            .expect("shutdown supervisor");
        supervisor_task.await.expect("join network supervisor");
    }

    #[tokio::test]
    async fn generation_invalidation_closes_listener_without_app_polling() {
        let (network_handle, supervisor_task) =
            NetworkSupervisor::spawn_unrestricted().expect("start network supervisor");
        let network_lease = network_handle.try_lease().expect("network lease");
        let listener_set = ListenerSet::bind(&network_lease, 0, true, false)
            .await
            .expect("bind listener");
        let port = listener_set.local_port().expect("listener port");
        let initial_connect = tokio::spawn(TcpStream::connect((Ipv4Addr::LOCALHOST, port)));
        let connection = time::timeout(Duration::from_secs(1), listener_set.accept())
            .await
            .expect("initial accept should complete")
            .expect("accept initial connection");
        initial_connect
            .await
            .expect("initial client task")
            .expect("initial listener must accept IPv4");
        drop(connection);
        let mut state_rx = network_handle.subscribe();

        network_handle
            .block("test listener ownership")
            .await
            .expect("block generation");
        state_rx.changed().await.expect("blocked network state");

        let accept_result = time::timeout(Duration::from_secs(1), listener_set.accept())
            .await
            .expect("listener accept task should stop promptly");
        let accept_error = match accept_result {
            Ok(_) => panic!("invalidated listener must stop accepting"),
            Err(error) => error,
        };
        assert_eq!(accept_error.kind(), io::ErrorKind::Interrupted);

        let (replacement_handle, replacement_task) =
            NetworkSupervisor::spawn_unrestricted().expect("start replacement supervisor");
        let replacement_lease = replacement_handle.try_lease().expect("replacement lease");
        let replacement_listener = ListenerSet::bind(&replacement_lease, port, true, false)
            .await
            .expect("invalidating the old generation must release its listener port");
        assert_eq!(replacement_listener.local_port(), Some(port));
        drop(replacement_listener);
        replacement_handle
            .shutdown()
            .await
            .expect("shutdown replacement supervisor");
        replacement_task.await.expect("join replacement supervisor");

        network_handle
            .shutdown()
            .await
            .expect("shutdown supervisor");
        supervisor_task.await.expect("join network supervisor");
    }

    #[cfg(feature = "dht")]
    #[tokio::test]
    async fn utp_and_dht_rebind_same_port_across_network_generations() {
        let (network_handle, supervisor_task) =
            NetworkSupervisor::spawn_unrestricted().expect("start network supervisor");
        let old_lease = network_handle.try_lease().expect("old network lease");
        let old_generation_id = old_lease.generation_id();
        let (mut activation_publisher, network_activation) =
            crate::networking::NetworkActivationPublisher::channel();
        let old_scope = activation_publisher.prepare(old_lease).unwrap();
        let old_listener = ListenerSet::bind(old_scope.lease(), 0, false, true)
            .await
            .expect("bind old uTP listener");
        let port = old_listener.local_port().expect("old listener port");
        let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);
        let active_config = DhtServiceConfig {
            port,
            bootstrap_nodes: Vec::new(),
            preferred_backend: DhtBackendKind::InternalPrototype,
            force_internal_failure: false,
        };
        activation_publisher
            .activate_prepared(old_scope, port)
            .unwrap();
        let dht_service = DhtService::new(
            network_activation,
            active_config.clone(),
            shutdown_tx.subscribe(),
        )
        .await
        .expect("start DHT on old shared UDP port");

        network_handle
            .rebuild_unrestricted()
            .await
            .expect("rebuild network generation");
        let mut state_rx = network_handle.subscribe();
        time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    &*state_rx.borrow(),
                    NetworkState::Ready(generation) if generation.id() > old_generation_id
                ) {
                    break;
                }
                state_rx.changed().await.expect("network state channel");
            }
        })
        .await
        .expect("replacement network generation");

        let mut disabled_config = active_config.clone();
        disabled_config.preferred_backend = DhtBackendKind::Disabled;
        dht_service
            .reconfigure_and_wait(disabled_config)
            .await
            .expect("stop old DHT generation");
        drop(old_listener);

        let new_lease = network_handle.try_lease().expect("new network lease");
        let new_scope = activation_publisher.prepare(new_lease).unwrap();
        let new_listener = ListenerSet::bind(new_scope.lease(), port, false, true)
            .await
            .expect("rebind uTP listener on same port");
        activation_publisher
            .activate_prepared(new_scope, port)
            .unwrap();
        dht_service
            .reconfigure_and_wait(active_config)
            .await
            .expect("start DHT on new shared UDP port");

        assert_eq!(new_listener.local_port(), Some(port));
        assert!(dht_service.current_status().health.enabled);

        let _ = shutdown_tx.send(());
        network_handle
            .shutdown()
            .await
            .expect("shutdown supervisor");
        supervisor_task.await.expect("join network supervisor");
    }

    #[tokio::test]
    async fn app_recovers_listener_after_binding_configuration_is_restored() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let initial_generation_id = app
            .active_network_generation_id()
            .expect("initial network generation");
        let dht_recorder = TestDhtRecorder::default();
        app.dht_service = DhtService::from_test_recorder(dht_recorder.clone());
        app.dht_status_rx = app.dht_service.subscribe_status();
        app.app_state.externally_accessable_port_v4 = true;
        app.app_state.externally_accessable_port_v6 = true;
        app.app_state.inbound_peer_transports.tcp_ipv4_seen = true;
        app.app_state.inbound_peer_transports.utp_ipv6_seen = true;
        app.app_state.externally_accessable_port_v4_highlight_until = Some(Instant::now());
        app.app_state.externally_accessable_port_v6_highlight_until = Some(Instant::now());

        let mut blocked_settings = app.client_configs.clone();
        blocked_settings.network_binding = crate::networking::NetworkBindingConfig {
            mode: crate::networking::runtime::NetworkBindingMode::Interface,
            interface: Some("missing-interface-test".to_string()),
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: None,
            ipv6_address: None,
            dns_policy: crate::networking::DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        app.apply_settings_update(blocked_settings, false).await;
        wait_for_app_network_state(&mut app, |state| {
            matches!(state, NetworkState::Blocked(reason) if missing_interface_reason(&reason.to_string()))
        })
        .await;
        app.handle_network_state_changed().await;

        assert!(app.listener.is_none());
        assert!(app.active_network_generation_id().is_none());
        assert!(!app.app_state.externally_accessable_port_v4);
        assert!(!app.app_state.externally_accessable_port_v6);
        assert_eq!(
            app.app_state.inbound_peer_transports,
            InboundPeerTransportStatus::default()
        );
        assert!(app
            .app_state
            .externally_accessable_port_v4_highlight_until
            .is_none());
        assert!(app
            .app_state
            .externally_accessable_port_v6_highlight_until
            .is_none());
        assert!(app
            .network_warning
            .as_deref()
            .is_some_and(missing_interface_reason));
        let blocked_status = app
            .generate_output_state()
            .network
            .expect("live network status");
        assert_eq!(
            blocked_status.phase,
            crate::networking::runtime::NetworkRuntimePhase::Blocked
        );
        assert_eq!(
            blocked_status.interface.as_deref(),
            Some("missing-interface-test")
        );
        assert!(blocked_status
            .blocked_reason
            .as_deref()
            .is_some_and(missing_interface_reason));
        let blocked_reconfigures = wait_for_dht_reconfigures(&dht_recorder, 1).await;
        assert_eq!(
            blocked_reconfigures
                .last()
                .map(|config| config.preferred_backend),
            Some(crate::dht_service::DhtBackendKind::Disabled)
        );

        let mut restored_settings = app.client_configs.clone();
        restored_settings.network_binding = crate::networking::NetworkBindingConfig::default();
        app.apply_settings_update(restored_settings, false).await;
        wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Ready(_))).await;
        app.handle_network_state_changed().await;

        assert!(app.listener.is_some());
        assert!(app
            .active_network_generation_id()
            .is_some_and(|generation_id| generation_id > initial_generation_id));
        assert!(app.network_warning.is_none());
        let ready_status = app
            .generate_output_state()
            .network
            .expect("live network status");
        assert_eq!(
            ready_status.phase,
            crate::networking::runtime::NetworkRuntimePhase::Ready
        );
        assert!(ready_status
            .generation_id
            .is_some_and(|generation_id| generation_id > initial_generation_id));
        let recovered_reconfigures = wait_for_dht_reconfigures(&dht_recorder, 2).await;
        assert_eq!(
            recovered_reconfigures
                .last()
                .map(|config| config.preferred_backend),
            Some(build_app_dht_service_config(&app.client_configs).preferred_backend)
        );

        app.network_handle
            .shutdown()
            .await
            .expect("shutdown network supervisor");
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn app_starts_blocked_and_recovers_when_strict_interface_becomes_available() {
        let settings = crate::config::Settings {
            client_port: 0,
            network_binding: crate::networking::NetworkBindingConfig {
                mode: crate::networking::runtime::NetworkBindingMode::Interface,
                interface: Some("missing-interface-test".to_string()),
                enable_ipv4: true,
                enable_ipv6: false,
                ipv4_address: None,
                ipv6_address: None,
                dns_policy: crate::networking::DnsPolicy::System,
                dns_servers: Vec::new(),
            },
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("start app in fail-closed state");

        assert!(app.listener.is_none());
        assert!(app.active_network_generation_id().is_none());
        assert!(app
            .app_state
            .system_warning
            .as_deref()
            .is_some_and(missing_interface_reason));

        let mut restored_settings = app.client_configs.clone();
        restored_settings.network_binding = crate::networking::NetworkBindingConfig::default();
        app.apply_settings_update(restored_settings, false).await;
        wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Ready(_))).await;
        app.handle_network_state_changed().await;

        assert!(app.listener.is_some());
        assert!(app.active_network_generation_id().is_some());
        assert!(app.network_warning.is_none());

        app.network_handle
            .shutdown()
            .await
            .expect("shutdown network supervisor");
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn initial_listener_failure_keeps_app_alive_in_blocked_state() {
        let occupied_tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("reserve occupied TCP port");
        let occupied_port = occupied_tcp
            .local_addr()
            .expect("occupied TCP address")
            .port();
        let occupied_udp = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, occupied_port))
            .await
            .expect("reserve occupied UDP port");
        let settings = crate::config::Settings {
            client_port: occupied_port,
            network_binding: crate::networking::NetworkBindingConfig {
                mode: crate::networking::runtime::NetworkBindingMode::LocalAddress,
                interface: None,
                enable_ipv4: true,
                enable_ipv6: false,
                ipv4_address: Some(Ipv4Addr::LOCALHOST),
                ipv6_address: None,
                dns_policy: crate::networking::DnsPolicy::System,
                dns_servers: Vec::new(),
            },
            ..Default::default()
        };

        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("listener failure must not abort application startup");

        assert!(app.listener.is_none());
        assert!(app.active_network_generation_id().is_none());
        assert_eq!(
            app.app_state
                .network_runtime_status
                .as_ref()
                .map(|status| status.phase),
            Some(crate::networking::runtime::NetworkRuntimePhase::Blocked)
        );
        assert!(app
            .app_state
            .system_warning
            .as_deref()
            .is_some_and(|warning| warning.contains("initial listener preflight failed")));

        drop(occupied_udp);
        drop(occupied_tcp);
        wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Ready(_))).await;
        app.handle_network_state_changed().await;

        assert!(app.listener.is_some());
        assert!(app.active_network_generation_id().is_some());

        app.network_handle
            .shutdown()
            .await
            .expect("shutdown network supervisor");
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn blocked_startup_preserves_random_port_semantics_during_recovery() {
        let occupied_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("reserve retained numeric port");
        let occupied_port = occupied_listener
            .local_addr()
            .expect("reserved address")
            .port();
        let settings = crate::config::Settings {
            client_port: occupied_port,
            randomize_client_port: true,
            network_binding: crate::networking::NetworkBindingConfig {
                mode: crate::networking::runtime::NetworkBindingMode::Interface,
                interface: Some("missing-interface-random-port-test".to_string()),
                enable_ipv4: true,
                enable_ipv6: false,
                ipv4_address: None,
                ipv6_address: None,
                dns_policy: crate::networking::DnsPolicy::System,
                dns_servers: Vec::new(),
            },
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("start blocked random-port app");

        assert!(app.listener.is_none());
        assert_eq!(app.client_configs.client_port, 0);
        assert!(app.client_configs.randomize_client_port);

        let mut restored_settings = app.client_configs.clone();
        restored_settings.network_binding = crate::networking::NetworkBindingConfig {
            mode: crate::networking::runtime::NetworkBindingMode::LocalAddress,
            interface: None,
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: Some(Ipv4Addr::LOCALHOST),
            ipv6_address: None,
            dns_policy: crate::networking::DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        app.apply_settings_update(restored_settings, false).await;
        wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Ready(_))).await;
        app.handle_network_state_changed().await;

        let bound_port = app
            .listener
            .as_ref()
            .and_then(ListenerSet::local_port)
            .expect("recovered random listener port");
        assert_ne!(bound_port, occupied_port);
        assert_eq!(app.client_configs.client_port, bound_port);
        assert!(app.client_configs.randomize_client_port);

        app.network_handle
            .shutdown()
            .await
            .expect("shutdown network supervisor");
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn random_port_change_is_persisted_while_networking_is_blocked() {
        let _guard = lock_shared_env();
        let _temp_paths = configure_temp_app_paths_for_test();
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let previous_port = app
            .listener
            .as_ref()
            .and_then(ListenerSet::local_port)
            .expect("initial listener port");

        let mut blocked_settings = app.client_configs.clone();
        blocked_settings.network_binding = crate::networking::NetworkBindingConfig {
            mode: crate::networking::runtime::NetworkBindingMode::Interface,
            interface: Some("missing-random-change-interface-test".to_string()),
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: None,
            ipv6_address: None,
            dns_policy: crate::networking::DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        app.apply_settings_update(blocked_settings, false).await;
        wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Blocked(_)))
            .await;
        app.handle_network_state_changed().await;

        let mut random_settings = app.client_configs.clone();
        random_settings.randomize_client_port = true;
        app.apply_settings_update(random_settings, true).await;
        app.flush_persistence_writer().await;

        assert!(app.listener.is_none());
        assert!(app.client_configs.randomize_client_port);
        let persisted = crate::config::load_settings().expect("reload persisted settings");
        assert!(persisted.randomize_client_port);

        let occupied_previous_port = TcpListener::bind((Ipv4Addr::LOCALHOST, previous_port))
            .await
            .expect("reserve previous fixed port during recovery");
        let mut restored_settings = app.client_configs.clone();
        restored_settings.network_binding = crate::networking::NetworkBindingConfig::default();
        app.apply_settings_update(restored_settings, false).await;
        wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Ready(_))).await;
        app.handle_network_state_changed().await;

        let recovered_port = app
            .listener
            .as_ref()
            .and_then(ListenerSet::local_port)
            .expect("recovered random listener port");
        assert_ne!(recovered_port, previous_port);
        assert!(app.client_configs.randomize_client_port);

        drop(occupied_previous_port);
        app.network_handle.shutdown().await.unwrap();
        let _ = app.shutdown_tx.send(());
        set_app_paths_override_for_tests(None);
    }

    #[tokio::test]
    async fn blocked_listener_retries_after_random_port_change() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let previous_port = app
            .listener
            .as_ref()
            .and_then(ListenerSet::local_port)
            .expect("initial listener port");

        app.network_handle
            .block("replacement listener preflight failed")
            .await
            .unwrap();
        wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Blocked(_)))
            .await;
        app.handle_network_state_changed().await;
        let occupied_previous_port = TcpListener::bind((Ipv4Addr::LOCALHOST, previous_port))
            .await
            .expect("reserve previous fixed port");

        let mut random_settings = app.client_configs.clone();
        random_settings.randomize_client_port = true;
        app.apply_settings_update(random_settings, false).await;
        wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Ready(_))).await;
        app.handle_network_state_changed().await;

        let recovered_port = app
            .listener
            .as_ref()
            .and_then(ListenerSet::local_port)
            .expect("listener should recover on a random port");
        assert_ne!(recovered_port, previous_port);
        assert!(app.client_configs.randomize_client_port);

        drop(occupied_previous_port);
        app.network_handle.shutdown().await.unwrap();
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn network_binding_changes_invalidate_before_backpressured_settings_sync() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let source = "magnet:?xt=urn:btih:4444444444444444444444444444444444444444";
        let info_hash = info_hash_from_torrent_source(source).expect("info hash");
        app.client_configs.torrents.push(TorrentSettings {
            torrent_or_magnet: source.to_string(),
            name: "Sample Network Ordering".to_string(),
            ..Default::default()
        });
        let (manager_tx, mut manager_rx) = mpsc::channel(1);
        manager_tx.send(ManagerCommand::Pause).await.unwrap();
        app.torrent_manager_command_txs
            .insert(info_hash, manager_tx);
        let old_lease = app.network_handle.try_lease().expect("old network lease");

        let mut updated_settings = app.client_configs.clone();
        updated_settings.torrents.clear();
        updated_settings.network_binding = crate::networking::NetworkBindingConfig {
            mode: crate::networking::runtime::NetworkBindingMode::LocalAddress,
            interface: None,
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: Some(Ipv4Addr::LOCALHOST),
            ipv6_address: None,
            dns_policy: crate::networking::DnsPolicy::System,
            dns_servers: Vec::new(),
        };

        {
            let update = app.apply_settings_update(updated_settings, false);
            tokio::pin!(update);
            let invalidated = tokio::select! {
                result = old_lease.cancel_on_invalidation(std::future::pending::<()>()) => result,
                _ = &mut update => panic!("settings update bypassed the full manager queue"),
                _ = time::sleep(Duration::from_secs(1)) => {
                    panic!("old network generation remained active during settings sync")
                }
            };
            assert!(invalidated.is_err());

            assert!(matches!(
                manager_rx.recv().await,
                Some(ManagerCommand::Pause)
            ));
            time::timeout(Duration::from_secs(1), &mut update)
                .await
                .expect("settings update should finish after manager queue drains");
        }
        assert!(matches!(
            manager_rx.recv().await,
            Some(ManagerCommand::Shutdown)
        ));

        app.network_handle.shutdown().await.unwrap();
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn version_check_waits_for_blocked_network_recovery() {
        let blocked_config = crate::networking::NetworkBindingConfig {
            mode: crate::networking::runtime::NetworkBindingMode::Interface,
            interface: Some("missing-version-check-interface-test".to_string()),
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: None,
            ipv6_address: None,
            dns_policy: crate::networking::DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        let (network_handle, supervisor_task) =
            NetworkSupervisor::spawn_with_config(&blocked_config);
        let (mut activation_publisher, network_activation) =
            crate::networking::NetworkActivationPublisher::channel();
        activation_publisher.block("network unavailable");
        let mut network_state_rx = network_activation.subscribe();
        let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);
        let mut shutdown_rx = shutdown_tx.subscribe();
        let attempts = Arc::new(AtomicUsize::new(0));
        let check = tokio::spawn({
            let network_activation = network_activation.clone();
            let attempts = Arc::clone(&attempts);
            async move {
                App::fetch_latest_version_when_network_ready(
                    &network_activation,
                    &mut network_state_rx,
                    &mut shutdown_rx,
                    move |_lease| {
                        attempts.fetch_add(1, Ordering::SeqCst);
                        async { Ok("2.0.0".to_string()) }
                    },
                )
                .await
            }
        });

        time::sleep(Duration::from_millis(25)).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
        network_handle
            .reconfigure(crate::networking::NetworkBindingConfig::default())
            .await
            .unwrap();
        activation_publisher
            .activate(network_handle.try_lease().unwrap(), 0)
            .unwrap();

        assert_eq!(
            time::timeout(Duration::from_secs(1), check)
                .await
                .expect("version check should resume after recovery")
                .expect("version check task")
                .expect("version checker should remain active"),
            Some("2.0.0".to_string())
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1);

        network_handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
    }

    #[tokio::test]
    async fn version_check_retries_after_in_flight_generation_invalidation() {
        let (network_handle, supervisor_task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let (mut activation_publisher, network_activation) =
            crate::networking::NetworkActivationPublisher::channel();
        activation_publisher
            .activate(network_handle.try_lease().unwrap(), 0)
            .unwrap();
        let mut network_state_rx = network_activation.subscribe();
        let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);
        let mut shutdown_rx = shutdown_tx.subscribe();
        let attempts = Arc::new(AtomicUsize::new(0));
        let first_attempt_started = Arc::new(Notify::new());
        let check = tokio::spawn({
            let network_activation = network_activation.clone();
            let attempts = Arc::clone(&attempts);
            let first_attempt_started = Arc::clone(&first_attempt_started);
            async move {
                App::fetch_latest_version_when_network_ready(
                    &network_activation,
                    &mut network_state_rx,
                    &mut shutdown_rx,
                    move |network_scope| {
                        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                        let first_attempt_started = Arc::clone(&first_attempt_started);
                        async move {
                            if attempt == 0 {
                                first_attempt_started.notify_one();
                                let error = network_scope
                                    .run(std::future::pending::<()>())
                                    .await
                                    .expect_err("first version request should be invalidated");
                                return Err(Box::new(error) as super::VersionCheckError);
                            }
                            Ok("2.0.1".to_string())
                        }
                    },
                )
                .await
            }
        });

        first_attempt_started.notified().await;
        network_handle
            .reconfigure(crate::networking::NetworkBindingConfig::default())
            .await
            .unwrap();
        activation_publisher
            .activate(network_handle.try_lease().unwrap(), 0)
            .unwrap();

        assert_eq!(
            time::timeout(Duration::from_secs(1), check)
                .await
                .expect("version check should retry on the replacement generation")
                .expect("version check task")
                .expect("version checker should remain active"),
            Some("2.0.1".to_string())
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 2);

        network_handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
    }

    #[tokio::test]
    async fn generation_recovery_propagates_a_simultaneously_changed_fixed_port() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let initial_generation_id = app
            .active_network_generation_id()
            .expect("initial network generation");
        let reserved_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("reserve replacement port");
        let replacement_port = reserved_listener
            .local_addr()
            .expect("replacement address")
            .port();
        drop(reserved_listener);
        let mut activation_rx = app.network_activation.subscribe();

        let mut updated_settings = app.client_configs.clone();
        updated_settings.client_port = replacement_port;
        updated_settings.randomize_client_port = false;
        updated_settings.network_binding = crate::networking::NetworkBindingConfig {
            mode: crate::networking::runtime::NetworkBindingMode::LocalAddress,
            interface: None,
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: Some(Ipv4Addr::LOCALHOST),
            ipv6_address: None,
            dns_policy: crate::networking::DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        app.apply_settings_update(updated_settings, false).await;
        wait_for_app_network_state(&mut app, |state| {
            matches!(state, NetworkState::Ready(generation) if generation.id() > initial_generation_id)
        })
        .await;
        app.handle_network_state_changed().await;
        activation_rx
            .changed()
            .await
            .expect("replacement activation should be published");

        assert_eq!(
            app.listener.as_ref().and_then(ListenerSet::local_port),
            Some(replacement_port)
        );
        assert!(matches!(
            &*activation_rx.borrow(),
            crate::networking::NetworkActivationState::Active(active)
                if active.scope().id().generation_id() > initial_generation_id
                    && active.listen_port() == replacement_port
        ));

        app.network_handle
            .shutdown()
            .await
            .expect("shutdown network supervisor");
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn generation_recovery_honors_a_simultaneously_enabled_random_port() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("create app");
        let initial_generation_id = app
            .active_network_generation_id()
            .expect("initial network generation");
        let previous_port = app
            .listener
            .as_ref()
            .and_then(ListenerSet::local_port)
            .expect("initial listener port");

        let mut updated_settings = app.client_configs.clone();
        updated_settings.randomize_client_port = true;
        updated_settings.network_binding = crate::networking::NetworkBindingConfig {
            mode: crate::networking::runtime::NetworkBindingMode::LocalAddress,
            interface: None,
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: Some(Ipv4Addr::LOCALHOST),
            ipv6_address: None,
            dns_policy: crate::networking::DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        app.apply_settings_update(updated_settings, false).await;
        wait_for_app_network_state(&mut app, |state| {
            matches!(state, NetworkState::Ready(generation) if generation.id() > initial_generation_id)
        })
        .await;

        app.listener = None;
        let occupied_previous_port = TcpListener::bind((Ipv4Addr::LOCALHOST, previous_port))
            .await
            .expect("reserve previous fixed port during generation recovery");
        app.handle_network_state_changed().await;

        let random_port = app
            .listener
            .as_ref()
            .and_then(ListenerSet::local_port)
            .expect("replacement random listener port");
        assert_ne!(random_port, previous_port);
        assert_eq!(app.client_configs.client_port, random_port);
        assert!(app.client_configs.randomize_client_port);

        drop(occupied_previous_port);
        app.network_handle
            .shutdown()
            .await
            .expect("shutdown network supervisor");
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn generation_recovery_ignores_a_full_manager_command_queue() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal).await.unwrap();
        let initial_generation_id = app.active_network_generation_id().unwrap();
        let (manager_tx, mut manager_rx) = mpsc::channel(1);
        manager_tx.send(ManagerCommand::Shutdown).await.unwrap();
        app.torrent_manager_command_txs
            .insert(b"full-generation-queue-test".to_vec(), manager_tx);
        let mut activation_rx = app.network_activation.subscribe();

        app.network_handle.rebuild_unrestricted().await.unwrap();
        wait_for_app_network_state(&mut app, |state| {
            matches!(state, NetworkState::Ready(generation) if generation.id() > initial_generation_id)
        })
        .await;
        app.handle_network_state_changed().await;

        activation_rx.changed().await.unwrap();
        assert!(matches!(
            &*activation_rx.borrow(),
            crate::networking::NetworkActivationState::Active(active)
                if active.scope().id().generation_id() > initial_generation_id
        ));
        assert!(matches!(
            manager_rx.recv().await,
            Some(ManagerCommand::Shutdown)
        ));
        app.network_handle.shutdown().await.unwrap();
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn generation_recovery_notifies_managers_before_waiting_for_dht() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal).await.unwrap();

        let mut blocked_settings = app.client_configs.clone();
        blocked_settings.network_binding = crate::networking::NetworkBindingConfig {
            mode: crate::networking::runtime::NetworkBindingMode::Interface,
            interface: Some("missing-dht-order-interface-test".to_string()),
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: None,
            ipv6_address: None,
            dns_policy: crate::networking::DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        app.apply_settings_update(blocked_settings, false).await;
        wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Blocked(_)))
            .await;
        app.handle_network_state_changed().await;

        let dht_recorder = TestDhtRecorder::with_blocked_reconfigure();
        app.dht_service = DhtService::from_test_recorder(dht_recorder.clone());
        app.dht_status_rx = app.dht_service.subscribe_status();
        let mut activation_rx = app.network_activation.subscribe();

        let mut restored_settings = app.client_configs.clone();
        restored_settings.network_binding = crate::networking::NetworkBindingConfig::default();
        app.apply_settings_update(restored_settings, false).await;
        wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Ready(_))).await;
        let expected_generation_id = match &*app.network_state_rx.borrow() {
            NetworkState::Ready(generation) => generation.id(),
            NetworkState::Blocked(reason) => panic!("network remained blocked: {reason}"),
        };

        {
            let recovery = app.handle_network_state_changed();
            tokio::pin!(recovery);
            tokio::select! {
                active = activation_rx.wait_for(|state| matches!(
                    state,
                    crate::networking::NetworkActivationState::Active(active)
                        if active.scope().id().generation_id() == expected_generation_id
                )) => {
                    active.expect("activation channel should remain open");
                }
                _ = &mut recovery => panic!("network recovery completed while DHT was blocked"),
                _ = tokio::time::sleep(Duration::from_millis(250)) => {
                    panic!("network activation waited for DHT")
                }
            };
            assert!(matches!(
                &*activation_rx.borrow(),
                crate::networking::NetworkActivationState::Active(active)
                    if active.scope().id().generation_id() == expected_generation_id
            ));

            dht_recorder.release_reconfigure();
            tokio::time::timeout(Duration::from_secs(2), &mut recovery)
                .await
                .expect("network recovery should complete after DHT is released");
        }

        app.network_handle.shutdown().await.unwrap();
        let _ = app.shutdown_tx.send(());
    }

    async fn wait_for_app_network_state(app: &mut App, predicate: impl Fn(&NetworkState) -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if predicate(&app.network_state_rx.borrow()) {
                    return;
                }
                app.network_state_rx
                    .changed()
                    .await
                    .expect("network state channel open");
            }
        })
        .await
        .expect("network state transition");
    }

    fn missing_interface_reason(reason: &str) -> bool {
        reason.contains("was not found") || reason.contains("not supported")
    }

    async fn wait_for_dht_reconfigures(
        recorder: &TestDhtRecorder,
        expected: usize,
    ) -> Vec<crate::dht_service::DhtServiceConfig> {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let reconfigures = recorder.recorded_reconfigures();
                if reconfigures.len() >= expected {
                    return reconfigures;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("DHT reconfigure request")
    }
}
