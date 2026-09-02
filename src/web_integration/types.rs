// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser-facing command, update, and diagnostic data.

use std::path::PathBuf;
use std::time::Duration;

use crate::app::{PeerInfo, TorrentControlState, TorrentMetrics};
use crate::torrent_manager::{FileActivityDirection, FileActivityUpdate};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrowserCommand {
    AddMagnet {
        magnet_link: String,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        validation_status: bool,
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
        replace_existing_config: bool,
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
    pub file_priorities: Vec<BrowserFilePriorityOverride>,
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

impl BrowserTorrentControlState {
    fn production(self) -> TorrentControlState {
        match self {
            Self::Running => TorrentControlState::Running,
            Self::Paused => TorrentControlState::Paused,
            Self::Deleting => TorrentControlState::Deleting,
        }
    }
}

impl BrowserTorrentUpdate {
    /// Converts browser fixture data at the simulated-manager boundary. The
    /// channel beyond this point carries the production `TorrentMetrics` type.
    pub fn into_torrent_metrics(self) -> TorrentMetrics {
        let tcp_peer_count = self
            .peers
            .iter()
            .filter(|peer| peer.transport == BrowserPeerTransport::Tcp)
            .count();
        let utp_peer_count = self.peers.len().saturating_sub(tcp_peer_count);
        let beneficial_tcp_peer_count = self
            .peers
            .iter()
            .filter(|peer| {
                peer.transport == BrowserPeerTransport::Tcp
                    && (peer.total_downloaded > 0 || peer.total_uploaded > 0)
            })
            .count();
        let beneficial_utp_peer_count = self
            .peers
            .iter()
            .filter(|peer| {
                peer.transport == BrowserPeerTransport::Utp
                    && (peer.total_downloaded > 0 || peer.total_uploaded > 0)
            })
            .count();
        let peers = self
            .peers
            .into_iter()
            .map(|peer| PeerInfo {
                address: peer.address,
                peer_id: peer.client.into_bytes(),
                am_choking: peer.am_choking,
                peer_choking: peer.peer_choking,
                am_interested: peer.am_interested,
                peer_interested: peer.peer_interested,
                bitfield: peer.bitfield,
                download_speed_bps: peer.download_speed_bps,
                upload_speed_bps: peer.upload_speed_bps,
                total_downloaded: peer.total_downloaded,
                total_uploaded: peer.total_uploaded,
                connection_count: peer.connection_count,
                disconnect_count: peer.disconnect_count,
                last_action: peer.last_action,
            })
            .collect::<Vec<_>>();
        let file_activity_updates = self
            .file_activity_updates
            .into_iter()
            .map(|activity| FileActivityUpdate {
                touched_relative_paths: activity.touched_relative_paths,
                direction: match activity.direction {
                    BrowserFileActivityDirection::Download => FileActivityDirection::Download,
                    BrowserFileActivityDirection::Upload => FileActivityDirection::Upload,
                },
            })
            .collect();
        let file_priorities = self
            .file_priorities
            .into_iter()
            .map(|override_value| {
                let priority = match override_value.priority {
                    BrowserFilePriority::High => crate::app::FilePriority::High,
                    BrowserFilePriority::Skip => crate::app::FilePriority::Skip,
                };
                (override_value.file_index, priority)
            })
            .collect();
        TorrentMetrics {
            torrent_control_state: self.control_state.production(),
            info_hash: self.info_hash,
            torrent_or_magnet: self.torrent_or_magnet,
            torrent_name: self.torrent_name,
            download_path: self.download_path,
            container_name: self.container_name,
            is_multi_file: self.files.len() > 1,
            file_count: Some(self.files.len()),
            file_priorities,
            data_available: self.data_available,
            is_complete: self.is_complete,
            number_of_successfully_connected_peers: peers.len(),
            tcp_peer_count,
            utp_peer_count,
            beneficial_tcp_peer_count,
            beneficial_utp_peer_count,
            number_of_pieces_total: self.pieces_total,
            number_of_pieces_completed: self.pieces_completed,
            download_speed_bps: self.download_speed_bps,
            upload_speed_bps: self.upload_speed_bps,
            bytes_downloaded_this_tick: self.bytes_downloaded_this_tick,
            bytes_uploaded_this_tick: self.bytes_uploaded_this_tick,
            session_total_downloaded: self.session_downloaded,
            session_total_uploaded: self.session_uploaded,
            eta: self.eta,
            peers,
            activity_message: self.activity_message,
            next_announce_in: self.next_announce_in,
            total_size: self.total_size,
            bytes_written: self.bytes_written,
            blocks_in_history: self.blocks_in_history,
            blocks_out_history: self.blocks_out_history,
            file_activity_updates,
            ..TorrentMetrics::default()
        }
    }
}

impl BrowserTorrentFrameUpdate {
    pub fn apply_to_torrent_metrics(self, metrics: &mut TorrentMetrics) {
        metrics.torrent_control_state = self.control_state.production();
        metrics.info_hash = self.info_hash;
        metrics.number_of_pieces_total = self.pieces_total;
        metrics.number_of_pieces_completed = self.pieces_completed;
        metrics.download_speed_bps = self.download_speed_bps;
        metrics.upload_speed_bps = self.upload_speed_bps;
        metrics.bytes_downloaded_this_tick = self.bytes_downloaded_this_tick;
        metrics.bytes_uploaded_this_tick = self.bytes_uploaded_this_tick;
        metrics.session_total_downloaded = self.session_downloaded;
        metrics.session_total_uploaded = self.session_uploaded;
        metrics.eta = self.eta;
        metrics.next_announce_in = self.next_announce_in;
        metrics.activity_message = self.activity_message;
        metrics.data_available = self.data_available;
        metrics.is_complete = self.is_complete;
        metrics.total_size = self.total_size;
        metrics.bytes_written = self.bytes_written;
        for peer_rate in self.peer_rates {
            if let Some(peer) = metrics
                .peers
                .iter_mut()
                .find(|peer| peer.address == peer_rate.address)
            {
                peer.download_speed_bps = peer_rate.download_speed_bps;
                peer.upload_speed_bps = peer_rate.upload_speed_bps;
            }
        }
    }
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
