// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared application model definitions and transitions.

use super::*;

#[derive(Default)]
pub struct AppState {
    pub capabilities: AppCapabilities,
    pub lifecycle: AppLifecycle,
    pub cleanup_failures: HashMap<Vec<u8>, String>,
    pub settings_application: SettingsApplication,
    pub checkpoint: CheckpointState,
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
    pub auto_graph_window: AutoGraphWindowState,
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

pub(super) fn sync_peer_policy_to_app_state(
    app_state: &mut AppState,
    peer_policy_rx: &mut watch::Receiver<Arc<PeerPolicy>>,
) -> usize {
    let policy = peer_policy_rx.borrow_and_update().clone();
    let blocked_ips = policy.restrictions.len();
    app_state.peer_policy = policy;
    app_state.ui.needs_redraw = true;
    blocked_ips
}

pub(super) fn sync_peer_manager_view_to_app_state(
    app_state: &mut AppState,
    peer_manager_view_rx: &mut watch::Receiver<Arc<PeerManagerView>>,
) -> usize {
    let view = peer_manager_view_rx.borrow_and_update().clone();
    let tracked_peers = view.tracked_peers.len();
    app_state.peer_manager_view = view;
    app_state.ui.needs_redraw = true;
    tracked_peers
}

pub(super) fn should_sync_peer_manager_view(mode: &AppMode) -> bool {
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
    pub(super) fn mark_seen(&mut self, transport: PeerTransportKind, ipv4: bool) {
        match (transport, ipv4) {
            (PeerTransportKind::Tcp, true) => self.tcp_ipv4_seen = true,
            (PeerTransportKind::Tcp, false) => self.tcp_ipv6_seen = true,
            (PeerTransportKind::Utp, true) => self.utp_ipv4_seen = true,
            (PeerTransportKind::Utp, false) => self.utp_ipv6_seen = true,
            (PeerTransportKind::Quic | PeerTransportKind::WebRtc, _) => {}
        }
    }
}
