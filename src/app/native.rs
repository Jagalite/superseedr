// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native application runtime and service orchestration.

mod listeners;
mod manager_effects;
use listeners::*;
mod bootstrap;
use bootstrap::*;
mod cluster;
mod control;
mod ingest;
mod integrity;
mod network;
use network::*;
mod persistence;
use persistence::*;
mod presentation;
mod preview;
use preview::*;
mod resources;
use resources::*;
mod rss;
use rss::*;
mod runtime;
mod settings;
use settings::*;
mod status_output;
mod torrent_runtime;
mod version;
mod watch_input;
use watch_input::*;

use super::*;
use std::fs::{self, File};
use std::future::Future;
use std::io::{self, ErrorKind, Stdout};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::{
    classify_shared_mode_settings_change, get_watch_path, host_watch_paths, local_settings_path,
    refresh_shared_config_recovery_backup_now, resolve_command_watch_path, runtime_log_dir,
    runtime_watch_paths, shared_host_id, shared_inbox_path, shared_root_path, shared_settings_path,
    SettingsChangeScope,
};
use crate::dht::service::{DhtService, DhtServiceConfig};
use crate::integrations::control::service::{
    control_event_details, online_control_success_message, plan_control_request,
    ControlExecutionPlan,
};
use crate::integrations::status::AppOutputState;
use crate::integrations::watch_inbox::{archive_watch_file, relay_watch_file_to_shared_inbox};
use crate::integrations::{
    control::write_control_request, rss_ingest, rss_service, status, watcher,
};
use crate::networking::{
    available_network_interfaces, NetworkActivationHandle, NetworkActivationPublisher,
    NetworkActivationStatus, NetworkBindingConfig, NetworkHandle, NetworkLease, NetworkScope,
    NetworkState, NetworkSupervisor, PeerConnection, TcpPeerTransport, UtpListenerSet,
    UtpPeerTransport,
};
use crate::peer_manager::PeerManagerService;
use crate::persistence::{build_fs_tree, AppPersistence};
use crate::resource::{PermitGuard, ResourceManager, ResourceManagerClient, ResourceManagerError};
use crate::telemetry::activity_history_telemetry::ActivityHistoryTelemetry;
use crate::telemetry::network_history_telemetry::NetworkHistoryTelemetry;
use crate::telemetry::ui_telemetry::{SystemTelemetrySnapshot, UiTelemetry};
use crate::terminal_event::Event as CrosstermEvent;
use crate::torrent_file::parser::from_bytes;
use crate::torrent_manager::integrity_scheduler::{
    IntegrityScheduler, ProbeBatchOutcome, TorrentIntegritySnapshot,
    INTEGRITY_SCHEDULER_TICK_INTERVAL,
};
use crate::torrent_manager::{TorrentManager, TorrentParameters};
use crate::tuning::{make_random_adjustment, normalize_limits_for_mode, TuningController};
use crossterm::event;
use directories::UserDirs;
use notify::{Error as NotifyError, Event, RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    Terminal,
};
#[cfg(unix)]
use rlimit::Resource;
use sysinfo::System;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::{broadcast, mpsc, mpsc::Sender};
use tokio::time::{self, MissedTickBehavior};

const FILE_HANDLE_MINIMUM: usize = 64;
const SAFE_BUDGET_PERCENTAGE: f64 = 0.85;

struct IncomingPeerHandshake {
    connection: PeerConnection,
    buffer: Vec<u8>,
    permit: PermitGuard,
}

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub struct App {
    pub app_state: AppState,
    pub client_configs: Settings,
    app_persistence: AppPersistence,
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
    pub(super) manager_event_tx: mpsc::Sender<ManagerObservation>,
    pub(super) manager_event_rx: mpsc::Receiver<ManagerObservation>,
    manager_lifetimes: HashMap<Vec<u8>, ManagerLifetime>,
    background_tasks: tokio::task::JoinSet<()>,
    manager_tasks: tokio::task::JoinSet<()>,
    pub app_command_tx: mpsc::Sender<AppCommand>,
    pub app_command_rx: mpsc::Receiver<AppCommand>,
    tui_command_batch_task: Option<tokio::task::JoinHandle<()>>,
    pub(crate) tui_command_batch_tx: mpsc::UnboundedSender<Vec<AppCommand>>,
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

#[cfg(test)]
use crate::tui::animation::{
    DhtWaveTargets, DHT_WAVE_PHASE_WRAP_PERIOD, DISK_IDLE_WOBBLE_PHASE_SPEED,
    DISK_MAX_TRANSFER_PHASE_SPEED,
};

#[cfg(test)]
#[path = "native/app_tests.rs"]
mod tests;
