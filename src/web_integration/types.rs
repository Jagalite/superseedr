// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser-facing command, update, and diagnostic data.

use std::path::PathBuf;
use std::time::Duration;

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
