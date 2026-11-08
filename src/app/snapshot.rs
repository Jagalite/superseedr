// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Serializable engine read model for attached TUI clients.
//!
//! The daemon captures an [`EngineSnapshot`] from its `AppState`; an attached
//! client applies it to its own local `AppState` and renders with the normal
//! TUI. View state (`ui`, `mode`, sorts, theme, ...) is never part of a
//! snapshot, so each client keeps its own. See `docs/headless-attach.md`.

// Only the Unix headless/attach path uses this; other targets would warn.
#![cfg_attr(not(unix), allow(dead_code))]

use super::*;
use crate::peer_manager::PeerManagerTrackedPeer;

/// Bump whenever the serialized shape of [`EngineSnapshot`] changes.
pub const ATTACH_PROTOCOL_VERSION: u32 = 4;

#[derive(Serialize, Deserialize, Clone)]
pub struct EngineSnapshot {
    pub settings: Settings,
    /// `Settings` skips this when serialized for persistence.
    pub randomize_client_port: bool,
    pub dht_status: DhtStatus,
    pub dht_wave: DhtWaveTelemetry,
    pub status: StatusSection,
    pub histories: HistorySection,
    pub torrents: TorrentSection,
}

/// Small scalar and status values.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct StatusSection {
    /// Daemon wall-clock second the frame was captured. Clients timestamp the
    /// activity samples they build with it, not with when the frame arrived.
    pub captured_unix: u64,
    pub update_available: Option<String>,
    pub shutdown_progress: f64,
    pub system_warning: Option<String>,
    pub system_error: Option<String>,
    pub network_runtime_status: Option<crate::networking::NetworkRuntimeStatus>,
    pub network_activation_status: Option<crate::networking::NetworkActivationStatus>,
    pub externally_accessable_port_v4: bool,
    pub externally_accessable_port_v6: bool,
    pub inbound_peer_transports: InboundPeerTransportStatus,
    /// Ingest, control, and watch queue lengths; the queues stay on the daemon.
    pub pending_queue_counts: [usize; 3],
    pub peer_policy: PeerPolicy,
    /// Every tracked peer, past ones included. Large, so only full frames
    /// carry it; `None` keeps the client's copy.
    pub peer_manager_view: Option<Arc<PeerManagerView>>,
    /// The connected peers, on every frame. The client merges them into its
    /// copy of the full view.
    pub active_peers: ActivePeers,
    pub auto_graph_activity: crate::telemetry::auto_graph::AutoGraphActivity,
    pub cluster_role_label: Option<String>,
    pub cluster_runtime_label: Option<String>,
    pub runtime_paths: RuntimePathView,
    pub data_rate: DataRate,
    pub limits: CalculatedLimits,
    pub last_tuning_limits: CalculatedLimits,
    pub last_tuning_score: u64,
    pub current_tuning_score: u64,
    pub tuning_countdown: u64,
    pub effective_download_limit_bps: u64,
    pub active_peer_limit: Option<usize>,
    pub adaptive_max_scpb: f64,
    pub cpu_usage: f32,
    pub ram_usage_percent: f32,
    pub app_ram_usage: u64,
    pub run_time: u64,
    pub session_total_downloaded: u64,
    pub session_total_uploaded: u64,
    pub lifetime_downloaded_from_config: u64,
    pub lifetime_uploaded_from_config: u64,
    /// `(loaded, total)`; the pacer deadline is process-local.
    pub startup_load_progress: Option<(usize, usize)>,
    pub avg_disk_read_bps: u64,
    pub avg_disk_write_bps: u64,
    pub avg_disk_write_completed_bps: u64,
    pub avg_disk_read_latency: Duration,
    pub avg_disk_write_latency: Duration,
    pub recv_to_write_p95: Duration,
    pub read_iops: u32,
    pub write_iops: u32,
    pub global_disk_read_thrash_score: u64,
    pub global_disk_write_thrash_score: u64,
    pub global_disk_thrash_score: f64,
    pub disk_health_ema: f64,
    pub disk_health_phase: f64,
    pub disk_health_peak_hold: f64,
    pub disk_health_state_level: u8,
    pub rss_runtime: RssRuntimeState,
    pub rss_derived: RssDerivedState,
}

/// Rate and activity histories.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct HistorySection {
    pub avg_download_history: Vec<u64>,
    pub avg_upload_history: Vec<u64>,
    pub minute_avg_dl_history: Vec<u64>,
    pub minute_avg_ul_history: Vec<u64>,
    pub network_history_state: NetworkHistoryPersistedState,
    // No activity history: on a long-running daemon it was most of every frame
    // (every series at every tier). Each client builds its own from the
    // frames it receives; see [`EngineSnapshot::apply`].
    /// Capped at `EVENT_JOURNAL_CAP` entries.
    pub event_journal_state: EventJournalState,
}

/// Torrent catalog. Candidate for changed-entries-only updates later.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct TorrentSection {
    // No list order: the displayed order is per-client (its own sort and search).
    /// A list rather than a map: info-hash keys are not valid JSON object keys.
    pub torrents: Vec<TorrentEntry>,
    pub startup_expected_seeds: Vec<Vec<u8>>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct TorrentEntry {
    pub info_hash: Vec<u8>,
    pub display: TorrentDisplayState,
    pub metrics_extras: MetricsExtras,
}

/// `TorrentMetrics` fields that its own serialization skips (it is also the
/// status-file format), but the TUI renders.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct MetricsExtras {
    pub file_verified_bytes: Vec<Option<u64>>,
    /// Packed while serializing, which runs off the daemon's event loop.
    #[serde(with = "packed_peers")]
    pub peers: Vec<PeerInfo>,
    pub availability_revision: Option<u64>,
    #[serde(with = "packed_peers")]
    pub departed_peers: Vec<PeerInfo>,
    pub peer_reconnect_counts: Vec<(IpAddr, u64)>,
    pub blocks_in_history: Vec<u64>,
    pub blocks_out_history: Vec<u64>,
}

/// A peer with its piece bitfield packed into hex. As JSON `true`/`false`
/// arrays, bitfields were ~80% of a large snapshot; packed they are ~20x smaller.
#[derive(Serialize, Deserialize, Clone)]
pub struct WirePeer {
    /// The peer with an empty `bitfield`.
    pub peer: PeerInfo,
    pub bitfield_len: usize,
    /// Bits packed most-significant first, hex encoded.
    pub bitfield_hex: String,
}

impl WirePeer {
    fn pack(peer: &PeerInfo) -> Self {
        let bitfield = &peer.bitfield;
        let mut bytes = vec![0u8; bitfield.len().div_ceil(8)];
        for (index, _) in bitfield.iter().enumerate().filter(|(_, has)| **has) {
            bytes[index / 8] |= 0x80 >> (index % 8);
        }
        Self {
            peer: peer.clone_without_bitfield(),
            bitfield_len: bitfield.len(),
            bitfield_hex: hex::encode(bytes),
        }
    }

    fn unpack(self) -> PeerInfo {
        let bytes = hex::decode(&self.bitfield_hex).unwrap_or_default();
        let bitfield = (0..self.bitfield_len)
            .map(|index| {
                bytes
                    .get(index / 8)
                    .is_some_and(|byte| byte & (0x80 >> (index % 8)) != 0)
            })
            .collect();
        PeerInfo {
            bitfield,
            ..self.peer
        }
    }
}

impl PeerInfo {
    fn clone_without_bitfield(&self) -> Self {
        Self {
            address: self.address.clone(),
            peer_id: self.peer_id.clone(),
            am_choking: self.am_choking,
            peer_choking: self.peer_choking,
            am_interested: self.am_interested,
            peer_interested: self.peer_interested,
            bitfield: Vec::new(),
            download_speed_bps: self.download_speed_bps,
            upload_speed_bps: self.upload_speed_bps,
            total_downloaded: self.total_downloaded,
            total_uploaded: self.total_uploaded,
            connection_count: self.connection_count,
            disconnect_count: self.disconnect_count,
            last_action: self.last_action.clone(),
        }
    }
}

mod packed_peers {
    use super::{PeerInfo, WirePeer};
    use serde::{ser::SerializeSeq, Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        peers: &[PeerInfo],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(peers.len()))?;
        for peer in peers {
            seq.serialize_element(&WirePeer::pack(peer))?;
        }
        seq.end()
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<PeerInfo>, D::Error> {
        Ok(Vec::<WirePeer>::deserialize(deserializer)?
            .into_iter()
            .map(WirePeer::unpack)
            .collect())
    }
}

impl MetricsExtras {
    /// Moves the fields out of an already-cloned `TorrentMetrics`, so the
    /// event loop clones each peer list once.
    fn take(metrics: &mut TorrentMetrics) -> Self {
        Self {
            file_verified_bytes: std::mem::take(&mut metrics.file_verified_bytes),
            peers: std::mem::take(&mut metrics.peers),
            availability_revision: metrics.availability_revision,
            departed_peers: std::mem::take(&mut metrics.departed_peers),
            peer_reconnect_counts: std::mem::take(&mut metrics.peer_reconnect_counts)
                .into_iter()
                .collect(),
            blocks_in_history: std::mem::take(&mut metrics.blocks_in_history),
            blocks_out_history: std::mem::take(&mut metrics.blocks_out_history),
        }
    }

    fn apply(self, metrics: &mut TorrentMetrics) {
        metrics.file_verified_bytes = self.file_verified_bytes;
        metrics.peers = self.peers;
        metrics.availability_revision = self.availability_revision;
        metrics.departed_peers = self.departed_peers;
        metrics.peer_reconnect_counts = self.peer_reconnect_counts.into_iter().collect();
        metrics.blocks_in_history = self.blocks_in_history;
        metrics.blocks_out_history = self.blocks_out_history;
    }
}

impl AppState {
    /// Ingest, control, and watch queue lengths. Attached clients have no
    /// queues of their own, so they show the daemon's counts.
    pub(crate) fn pending_queue_counts(&self) -> [usize; 3] {
        self.attached_queue_counts.unwrap_or([
            self.pending_ingest_by_path.len(),
            self.pending_control_by_path.len(),
            self.pending_watch_commands.len(),
        ])
    }
}

/// The active rows of a [`PeerManagerView`]: a few hundred at most, while the
/// full view keeps every past peer too (tens of thousands on a long run).
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct ActivePeers {
    pub registered_torrents: usize,
    pub metrics_updates: u64,
    pub tracked_peers: Vec<PeerManagerTrackedPeer>,
}

impl ActivePeers {
    fn capture(view: &PeerManagerView) -> Self {
        Self {
            registered_torrents: view.registered_torrents,
            metrics_updates: view.metrics_updates,
            tracked_peers: view
                .tracked_peers
                .iter()
                .filter(|peer| peer.is_active)
                .cloned()
                .collect(),
        }
    }

    /// Updates the client's copy of the full view. A peer that disconnected
    /// since is kept as a past peer; the next full view brings its final
    /// counters. Rows stay sorted by torrent, then IP, as the daemon sorts them.
    fn merge_into(self, view: &mut PeerManagerView) {
        view.registered_torrents = self.registered_torrents;
        view.metrics_updates = self.metrics_updates;
        for peer in view.tracked_peers.iter_mut().filter(|peer| peer.is_active) {
            peer.is_active = false;
            peer.endpoints.clear();
        }
        for peer in self.tracked_peers {
            let key = (peer.torrent_info_hash.as_slice(), peer.ip);
            match view
                .tracked_peers
                .binary_search_by(|row| (row.torrent_info_hash.as_slice(), row.ip).cmp(&key))
            {
                Ok(index) => view.tracked_peers[index] = peer,
                Err(index) => view.tracked_peers.insert(index, peer),
            }
        }
    }
}

impl EngineSnapshot {
    /// A snapshot with every section.
    #[cfg(test)]
    pub fn capture(
        state: &AppState,
        settings: &Settings,
        dht_status: &DhtStatus,
        dht_wave: &DhtWaveTelemetry,
    ) -> Self {
        Self::capture_with(state, settings, dht_status, dht_wave, true)
    }

    /// True when the frame carries every section, so a new client can start
    /// from it.
    pub fn is_full(&self) -> bool {
        self.status.peer_manager_view.is_some()
    }

    /// For a client that replaces an unapplied snapshot with this newer one:
    /// keeps the older one's full peer view if this one has none, so the
    /// client's view is still reconciled (this frame's active peers merge on
    /// top of it).
    pub fn inherit_full_view(&mut self, older: EngineSnapshot) {
        if self.status.peer_manager_view.is_none() {
            self.status.peer_manager_view = older.status.peer_manager_view;
        }
    }

    /// `full` adds the full peer view (past peers included); other frames
    /// carry only the active peers.
    pub fn capture_with(
        state: &AppState,
        settings: &Settings,
        dht_status: &DhtStatus,
        dht_wave: &DhtWaveTelemetry,
        full: bool,
    ) -> Self {
        Self {
            settings: settings.clone(),
            randomize_client_port: settings.randomize_client_port,
            dht_status: dht_status.clone(),
            dht_wave: dht_wave.clone(),
            status: StatusSection {
                captured_unix: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                update_available: state.update_available.clone(),
                shutdown_progress: state.shutdown_progress,
                system_warning: state.system_warning.clone(),
                system_error: state.system_error.clone(),
                network_runtime_status: state.network_runtime_status.clone(),
                network_activation_status: state.network_activation_status.clone(),
                externally_accessable_port_v4: state.externally_accessable_port_v4,
                externally_accessable_port_v6: state.externally_accessable_port_v6,
                inbound_peer_transports: state.inbound_peer_transports,
                pending_queue_counts: state.pending_queue_counts(),
                peer_policy: (*state.peer_policy).clone(),
                peer_manager_view: full.then(|| Arc::clone(&state.peer_manager_view)),
                active_peers: ActivePeers::capture(&state.peer_manager_view),
                auto_graph_activity: state.auto_graph_activity,
                cluster_role_label: state.cluster_role_label.clone(),
                cluster_runtime_label: state.cluster_runtime_label.clone(),
                runtime_paths: state.runtime_paths.clone(),
                data_rate: state.data_rate,
                limits: state.limits.clone(),
                last_tuning_limits: state.last_tuning_limits.clone(),
                last_tuning_score: state.last_tuning_score,
                current_tuning_score: state.current_tuning_score,
                tuning_countdown: state.tuning_countdown,
                effective_download_limit_bps: state.effective_download_limit_bps,
                active_peer_limit: state.active_peer_limit,
                adaptive_max_scpb: state.adaptive_max_scpb,
                cpu_usage: state.cpu_usage,
                ram_usage_percent: state.ram_usage_percent,
                app_ram_usage: state.app_ram_usage,
                run_time: state.run_time,
                session_total_downloaded: state.session_total_downloaded,
                session_total_uploaded: state.session_total_uploaded,
                lifetime_downloaded_from_config: state.lifetime_downloaded_from_config,
                lifetime_uploaded_from_config: state.lifetime_uploaded_from_config,
                startup_load_progress: state
                    .startup_load_progress
                    .map(|progress| (progress.loaded, progress.total)),
                avg_disk_read_bps: state.avg_disk_read_bps,
                avg_disk_write_bps: state.avg_disk_write_bps,
                avg_disk_write_completed_bps: state.avg_disk_write_completed_bps,
                avg_disk_read_latency: state.avg_disk_read_latency,
                avg_disk_write_latency: state.avg_disk_write_latency,
                recv_to_write_p95: state.recv_to_write_p95,
                read_iops: state.read_iops,
                write_iops: state.write_iops,
                global_disk_read_thrash_score: state.global_disk_read_thrash_score,
                global_disk_write_thrash_score: state.global_disk_write_thrash_score,
                global_disk_thrash_score: state.global_disk_thrash_score,
                disk_health_ema: state.disk_health_ema,
                disk_health_phase: state.disk_health_phase,
                disk_health_peak_hold: state.disk_health_peak_hold,
                disk_health_state_level: state.disk_health_state_level,
                rss_runtime: state.rss_runtime.clone(),
                rss_derived: state.rss_derived.clone(),
            },
            histories: HistorySection {
                avg_download_history: state.avg_download_history.clone(),
                avg_upload_history: state.avg_upload_history.clone(),
                minute_avg_dl_history: state.minute_avg_dl_history.clone(),
                minute_avg_ul_history: state.minute_avg_ul_history.clone(),
                network_history_state: state.network_history_state.clone(),
                event_journal_state: state.event_journal_state.clone(),
            },
            torrents: TorrentSection {
                torrents: state
                    .torrents
                    .iter()
                    .map(|(info_hash, torrent)| {
                        let mut display = torrent.clone();
                        let metrics_extras = MetricsExtras::take(&mut display.latest_state);
                        TorrentEntry {
                            info_hash: info_hash.clone(),
                            display,
                            metrics_extras,
                        }
                    })
                    .collect(),
                startup_expected_seeds: state.startup_expected_seeds.iter().cloned().collect(),
            },
        }
    }

    /// Writes engine data into a client's local state and rebuilds the client's
    /// own sorted/filtered torrent list. Other view state is untouched. Returns
    /// the values `draw` takes alongside `AppState`.
    pub fn apply(self, state: &mut AppState) -> SnapshotContext {
        let EngineSnapshot {
            mut settings,
            randomize_client_port,
            dht_status,
            dht_wave,
            status,
            histories,
            torrents,
        } = self;
        settings.randomize_client_port = randomize_client_port;
        let captured_unix = status.captured_unix;

        state.update_available = status.update_available;
        state.shutdown_progress = status.shutdown_progress;
        state.system_warning = status.system_warning;
        state.system_error = status.system_error;
        state.network_runtime_status = status.network_runtime_status;
        state.network_activation_status = status.network_activation_status;
        state.externally_accessable_port_v4 = status.externally_accessable_port_v4;
        state.externally_accessable_port_v6 = status.externally_accessable_port_v6;
        state.inbound_peer_transports = status.inbound_peer_transports;
        state.attached_queue_counts = Some(status.pending_queue_counts);
        state.peer_policy = Arc::new(status.peer_policy);
        if let Some(view) = status.peer_manager_view {
            state.peer_manager_view = view;
        }
        status
            .active_peers
            .merge_into(Arc::make_mut(&mut state.peer_manager_view));
        state.auto_graph_activity = status.auto_graph_activity;
        state.cluster_role_label = status.cluster_role_label;
        state.cluster_runtime_label = status.cluster_runtime_label;
        state.runtime_paths = status.runtime_paths;
        state.data_rate = status.data_rate;
        state.limits = status.limits;
        state.last_tuning_limits = status.last_tuning_limits;
        state.last_tuning_score = status.last_tuning_score;
        state.current_tuning_score = status.current_tuning_score;
        state.tuning_countdown = status.tuning_countdown;
        state.effective_download_limit_bps = status.effective_download_limit_bps;
        state.active_peer_limit = status.active_peer_limit;
        state.adaptive_max_scpb = status.adaptive_max_scpb;
        state.cpu_usage = status.cpu_usage;
        state.ram_usage_percent = status.ram_usage_percent;
        state.app_ram_usage = status.app_ram_usage;
        state.run_time = status.run_time;
        state.session_total_downloaded = status.session_total_downloaded;
        state.session_total_uploaded = status.session_total_uploaded;
        state.lifetime_downloaded_from_config = status.lifetime_downloaded_from_config;
        state.lifetime_uploaded_from_config = status.lifetime_uploaded_from_config;
        state.startup_load_progress =
            status
                .startup_load_progress
                .map(|(loaded, total)| StartupLoadProgress {
                    loaded,
                    total,
                    next_load_at: None,
                });
        state.avg_disk_read_bps = status.avg_disk_read_bps;
        state.avg_disk_write_bps = status.avg_disk_write_bps;
        state.avg_disk_write_completed_bps = status.avg_disk_write_completed_bps;
        state.avg_disk_read_latency = status.avg_disk_read_latency;
        state.avg_disk_write_latency = status.avg_disk_write_latency;
        state.recv_to_write_p95 = status.recv_to_write_p95;
        state.read_iops = status.read_iops;
        state.write_iops = status.write_iops;
        state.global_disk_read_thrash_score = status.global_disk_read_thrash_score;
        state.global_disk_write_thrash_score = status.global_disk_write_thrash_score;
        state.global_disk_thrash_score = status.global_disk_thrash_score;
        state.disk_health_ema = status.disk_health_ema;
        state.disk_health_phase = status.disk_health_phase;
        state.disk_health_peak_hold = status.disk_health_peak_hold;
        state.disk_health_state_level = status.disk_health_state_level;
        state.rss_runtime = status.rss_runtime;
        state.rss_derived = status.rss_derived;

        state.avg_download_history = histories.avg_download_history;
        state.avg_upload_history = histories.avg_upload_history;
        state.minute_avg_dl_history = histories.minute_avg_dl_history;
        state.minute_avg_ul_history = histories.minute_avg_ul_history;
        state.network_history_state = histories.network_history_state;
        state.event_journal_state = histories.event_journal_state;

        let selected_hash = state
            .torrent_list_order
            .get(state.ui.selected_torrent_index)
            .cloned();
        state.torrents = torrents
            .torrents
            .into_iter()
            .map(|entry| {
                let mut display = entry.display;
                entry.metrics_extras.apply(&mut display.latest_state);
                (entry.info_hash, display)
            })
            .collect();
        state.startup_expected_seeds = torrents.startup_expected_seeds.into_iter().collect();
        sort_and_filter_torrent_list_state(state);
        // Keep the cursor on the same torrent when the order shifts.
        if let Some(index) = selected_hash.and_then(|hash| {
            state
                .torrent_list_order
                .iter()
                .position(|candidate| *candidate == hash)
        }) {
            state.ui.selected_torrent_index = index;
        }
        clamp_selected_indices_in_state(state);

        let now = SystemTime::now();
        let now_unix = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        append_activity_samples(state, captured_unix);
        crate::telemetry::network_history_telemetry::update_auto_graph_window(state, now_unix);
        if matches!(state.mode, AppMode::PeerManagement) {
            crate::tui::screens::peers::recompute_peer_management_derived(state, now);
        }
        state.ui.needs_redraw = true;

        SnapshotContext {
            settings,
            dht_status,
            dht_wave,
        }
    }
}

/// Longest run of missed seconds filled in with the current frame's values.
/// Longer gaps (the client was suspended) are left as gaps.
const MAX_ACTIVITY_BACKFILL_SECS: u64 = 60;

/// The client builds its own activity history, starting when it attaches,
/// with the same per-second sampling the daemon runs, from the values the
/// frame just applied, at the second the daemon captured them. Rollups
/// (minute, 15 minutes, hour) count samples, so seconds missed between frames
/// are filled with this frame's values to keep them covering real time.
fn append_activity_samples(state: &mut AppState, captured_unix: u64) {
    let first = match latest_activity_second(state) {
        // Already sampled this second, or an older frame arrived late.
        Some(last) if captured_unix <= last => return,
        Some(last) if captured_unix - last <= MAX_ACTIVITY_BACKFILL_SECS => last + 1,
        _ => captured_unix,
    };
    for second in first..=captured_unix {
        crate::telemetry::activity_history_telemetry::ActivityHistoryTelemetry::on_second_tick_at(
            state, second,
        );
    }
}

/// Newest per-second activity sample, so two frames within one second append
/// only one.
fn latest_activity_second(state: &AppState) -> Option<u64> {
    state
        .activity_history_state
        .cpu
        .tiers
        .second_1s
        .last()
        .map(|point| point.ts_unix)
}

/// Engine values passed to `draw` separately from `AppState`.
pub struct SnapshotContext {
    pub settings: Settings,
    pub dht_status: DhtStatus,
    pub dht_wave: DhtWaveTelemetry,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state() -> AppState {
        let mut state = AppState::default();
        let info_hash = vec![0xab; 20];
        let mut torrent = TorrentDisplayState::default();
        torrent.latest_state.info_hash = info_hash.clone();
        torrent.latest_state.torrent_name = "Sample Torrent".to_string();
        torrent.latest_state.download_speed_bps = 1_234;
        torrent.download_history = vec![1, 2, 3];
        state.torrents.insert(info_hash.clone(), torrent);
        state.torrent_list_order = vec![info_hash];
        state.avg_download_history = vec![10, 20, 30];
        state.cpu_usage = 12.5;
        state.system_warning = Some("warning".to_string());
        state.startup_load_progress = Some(StartupLoadProgress {
            loaded: 3,
            total: 7,
            next_load_at: Some(Instant::now()),
        });
        state
    }

    fn capture_with(state: &AppState, full: bool) -> EngineSnapshot {
        let snapshot = EngineSnapshot::capture_with(
            state,
            &Settings::default(),
            &DhtStatus::default(),
            &DhtWaveTelemetry::default(),
            full,
        );
        let bytes = serde_json::to_vec(&snapshot).expect("serialize snapshot");
        serde_json::from_slice(&bytes).expect("decode snapshot")
    }

    const FULL: bool = true;
    const LIGHT: bool = false;

    fn tracked_peer(hash: u8, last_octet: u8, active: bool, downloaded: u64) -> PeerManagerTrackedPeer {
        PeerManagerTrackedPeer {
            torrent_info_hash: vec![hash; 20],
            torrent_name: "Sample Torrent".to_string(),
            ip: IpAddr::from([192, 0, 2, last_octet]),
            is_active: active,
            endpoints: Vec::new(),
            downloaded_evidence_bytes: 0,
            uploaded_evidence_bytes: 0,
            total_downloaded_bytes: downloaded,
            total_uploaded_bytes: 0,
            connection_count: 1,
            disconnect_count: 0,
            transfer_threshold_bytes: 0,
            reconnect_count: 0,
            reconnect_limit: 0,
            reconnect_window_secs: 0,
            last_seen: None,
            clients: Vec::new(),
        }
    }

    fn peer_view(peers: Vec<PeerManagerTrackedPeer>) -> Arc<PeerManagerView> {
        Arc::new(PeerManagerView {
            registered_torrents: 2,
            metrics_updates: 1,
            tracked_peers: peers,
        })
    }

    #[test]
    fn light_frames_carry_only_active_peers_and_the_client_merges_them() {
        let mut source = sample_state();
        source.peer_manager_view = peer_view(vec![
            tracked_peer(1, 1, false, 10),
            tracked_peer(1, 2, true, 20),
            tracked_peer(2, 1, true, 30),
        ]);
        let mut client = AppState::default();
        capture_with(&source, FULL).apply(&mut client);
        assert_eq!(client.peer_manager_view, source.peer_manager_view);

        // Since then: (1, .2) disconnected, (2, .1) moved data, (1, .3) is new.
        source.peer_manager_view = peer_view(vec![
            tracked_peer(1, 1, false, 10),
            tracked_peer(1, 2, false, 25),
            tracked_peer(1, 3, true, 5),
            tracked_peer(2, 1, true, 35),
        ]);
        let light = capture_with(&source, LIGHT);
        assert!(light.status.peer_manager_view.is_none());
        assert_eq!(light.status.active_peers.tracked_peers.len(), 2);
        light.apply(&mut client);

        let rows: Vec<_> = client
            .peer_manager_view
            .tracked_peers
            .iter()
            .map(|peer| (peer.torrent_info_hash[0], peer.ip, peer.is_active, peer.total_downloaded_bytes))
            .collect();
        let ip = |last_octet| IpAddr::from([192, 0, 2, last_octet]);
        assert_eq!(
            rows,
            vec![
                (1, ip(1), false, 10),
                // Kept as a past peer; its final counters come with the next full view.
                (1, ip(2), false, 20),
                (1, ip(3), true, 5),
                (2, ip(1), true, 35),
            ]
        );

        capture_with(&source, FULL).apply(&mut client);
        assert_eq!(client.peer_manager_view, source.peer_manager_view);
    }

    #[test]
    fn the_client_builds_its_own_activity_history() {
        let mut source = sample_state();
        source.activity_history_state.cpu.tiers.second_1s.push(
            crate::persistence::activity_history::ActivityHistoryPoint {
                ts_unix: 1,
                primary: 999,
                secondary: 0,
            },
        );
        let full = capture_with(&source, FULL);
        let mut client = AppState::default();
        full.apply(&mut client);

        let cpu = &client.activity_history_state.cpu.tiers.second_1s;
        assert_eq!(cpu.len(), 1, "one local sample, none from the daemon");
        assert_eq!(cpu[0].primary, 125, "cpu 12.5% sampled from the frame");
        let torrent_key = hex::encode(&source.torrent_list_order[0]);
        assert!(client
            .activity_history_state
            .torrents
            .contains_key(&torrent_key));
    }

    #[test]
    fn frames_append_at_most_one_activity_sample_per_second() {
        let source = sample_state();
        let mut client = AppState::default();
        // A sample already exists for a later second, as after a frame
        // applied earlier within the current second.
        client.activity_history_state.cpu.tiers.second_1s.push(
            crate::persistence::activity_history::ActivityHistoryPoint {
                ts_unix: u64::MAX,
                primary: 1,
                secondary: 0,
            },
        );
        capture_with(&source, LIGHT).apply(&mut client);
        assert_eq!(client.activity_history_state.cpu.tiers.second_1s.len(), 1);
    }

    fn activity_seconds(client: &AppState) -> Vec<u64> {
        client
            .activity_history_state
            .cpu
            .tiers
            .second_1s
            .iter()
            .map(|point| point.ts_unix)
            .collect()
    }

    fn captured_at(source: &AppState, full: bool, captured_unix: u64) -> EngineSnapshot {
        let mut snapshot = capture_with(source, full);
        snapshot.status.captured_unix = captured_unix;
        snapshot
    }

    #[test]
    fn activity_samples_use_the_capture_second_and_fill_short_gaps() {
        let source = sample_state();
        let mut client = AppState::default();
        captured_at(&source, FULL, 1_000).apply(&mut client);
        // Two frames were skipped; a late frame from before is ignored.
        captured_at(&source, LIGHT, 1_003).apply(&mut client);
        captured_at(&source, LIGHT, 1_002).apply(&mut client);
        assert_eq!(activity_seconds(&client), [1_000, 1_001, 1_002, 1_003]);

        // A long gap (client suspended) is not filled.
        captured_at(&source, LIGHT, 1_003 + MAX_ACTIVITY_BACKFILL_SECS + 1).apply(&mut client);
        assert_eq!(activity_seconds(&client).len(), 5);
    }

    #[test]
    fn a_skipped_full_view_is_kept_by_the_newer_frame() {
        let mut source = sample_state();
        source.peer_manager_view = peer_view(vec![
            tracked_peer(1, 1, false, 10),
            tracked_peer(1, 2, true, 20),
        ]);
        let full = capture_with(&source, FULL);
        source.peer_manager_view = peer_view(vec![
            tracked_peer(1, 1, false, 10),
            tracked_peer(1, 2, true, 30),
        ]);
        let mut light = capture_with(&source, LIGHT);
        light.inherit_full_view(full);
        assert!(light.is_full());

        let mut client = AppState::default();
        light.apply(&mut client);
        assert_eq!(client.peer_manager_view, source.peer_manager_view);
    }

    #[test]
    fn light_frames_keep_the_full_peer_view() {
        let mut source = sample_state();
        source.peer_manager_view = Arc::new(PeerManagerView {
            registered_torrents: 3,
            ..PeerManagerView::default()
        });
        let full = capture_with(&source, FULL);
        assert!(full.is_full());
        let mut client = AppState::default();
        full.apply(&mut client);
        let light = capture_with(&source, LIGHT);
        assert!(!light.is_full());
        light.apply(&mut client);
        assert_eq!(client.peer_manager_view.registered_torrents, 3);
    }

    #[test]
    fn snapshot_round_trips_through_json_into_fresh_state() {
        let source = sample_state();
        let snapshot = EngineSnapshot::capture(
            &source,
            &Settings::default(),
            &DhtStatus::default(),
            &DhtWaveTelemetry::default(),
        );
        let bytes = serde_json::to_vec(&snapshot).expect("serialize snapshot");
        let decoded: EngineSnapshot = serde_json::from_slice(&bytes).expect("decode snapshot");

        let mut client = AppState::default();
        decoded.apply(&mut client);

        assert_eq!(client.torrent_list_order, source.torrent_list_order);
        let info_hash = &source.torrent_list_order[0];
        let torrent = client.torrents.get(info_hash).expect("torrent applied");
        assert_eq!(torrent.latest_state.torrent_name, "Sample Torrent");
        assert_eq!(torrent.latest_state.download_speed_bps, 1_234);
        assert_eq!(torrent.download_history, vec![1, 2, 3]);
        assert_eq!(client.avg_download_history, vec![10, 20, 30]);
        assert_eq!(client.cpu_usage, 12.5);
        assert_eq!(client.system_warning.as_deref(), Some("warning"));
        let progress = client.startup_load_progress.expect("progress applied");
        assert_eq!((progress.loaded, progress.total), (3, 7));
        assert!(progress.next_load_at.is_none());
    }

    fn round_trip(snapshot: &EngineSnapshot) -> EngineSnapshot {
        let bytes = serde_json::to_vec(snapshot).expect("serialize snapshot");
        serde_json::from_slice(&bytes).expect("decode snapshot")
    }

    #[test]
    fn fields_skipped_by_metrics_and_settings_serialization_survive_the_wire() {
        let mut source = sample_state();
        let info_hash = source.torrent_list_order[0].clone();
        {
            let torrent = source.torrents.get_mut(&info_hash).expect("torrent");
            torrent.latest_state.peers = vec![PeerInfo {
                address: "192.0.2.10:6881".to_string(),
                ..PeerInfo::default()
            }];
            torrent.latest_state.blocks_in_history = vec![4, 5];
            torrent.latest_state.blocks_out_history = vec![6];
            torrent
                .latest_state
                .peer_reconnect_counts
                .insert("192.0.2.10".parse().expect("ip"), 3);
            torrent.latest_file_probe_status = Some(TorrentFileProbeStatus::Files(vec![
                torrent_manager_protocol::FileProbeEntry {
                    relative_path: PathBuf::from("missing.bin"),
                    absolute_path: PathBuf::from("/data/missing.bin"),
                    error: crate::persistence::StorageError::Io {
                        kind: std::io::ErrorKind::NotFound,
                        message: "gone".to_string(),
                    },
                    expected_size: 10,
                    observed_size: None,
                },
            ]));
        }
        source.inbound_peer_transports.utp_ipv4_seen = true;
        source
            .pending_watch_commands
            .push_back(AppCommand::RssSyncNow);
        let settings = Settings {
            randomize_client_port: true,
            ..Settings::default()
        };

        let snapshot = EngineSnapshot::capture(
            &source,
            &settings,
            &DhtStatus::default(),
            &DhtWaveTelemetry::default(),
        );
        let mut client = AppState::default();
        let context = round_trip(&snapshot).apply(&mut client);

        let torrent = client.torrents.get(&info_hash).expect("torrent");
        assert_eq!(torrent.latest_state.peers.len(), 1);
        assert_eq!(torrent.latest_state.blocks_in_history, vec![4, 5]);
        assert_eq!(torrent.latest_state.blocks_out_history, vec![6]);
        assert_eq!(torrent.latest_state.peer_reconnect_counts.len(), 1);
        assert_eq!(
            torrent.latest_file_probe_status,
            source.torrents[&info_hash].latest_file_probe_status
        );
        assert!(client.inbound_peer_transports.utp_ipv4_seen);
        assert_eq!(client.pending_queue_counts(), [0, 0, 1]);
        assert!(context.settings.randomize_client_port);
    }

    #[test]
    fn packed_bitfields_round_trip_at_every_length() {
        for len in [0, 1, 7, 8, 9, 63, 64, 65, 2049] {
            let peer = PeerInfo {
                address: "192.0.2.1:6881".to_string(),
                bitfield: (0..len)
                    .map(|index| index % 3 == 0 || index == len - 1)
                    .collect(),
                total_downloaded: 42,
                ..PeerInfo::default()
            };
            let wire: WirePeer =
                serde_json::from_slice(&serde_json::to_vec(&WirePeer::pack(&peer)).expect("json"))
                    .expect("decode");
            assert_eq!(wire.unpack(), peer, "length {len}");
        }
    }

    #[test]
    fn selection_follows_the_same_torrent_when_the_order_shifts() {
        let mut source = sample_state();
        let selected = source.torrent_list_order[0].clone();
        let mut client = AppState::default();
        EngineSnapshot::capture(
            &source,
            &Settings::default(),
            &DhtStatus::default(),
            &DhtWaveTelemetry::default(),
        )
        .apply(&mut client);
        client.ui.selected_torrent_index = 0;

        // A torrent whose name sorts first appears on the daemon.
        let new_hash = vec![0x01; 20];
        let mut new_torrent = TorrentDisplayState::default();
        new_torrent.latest_state.info_hash = new_hash.clone();
        new_torrent.latest_state.torrent_name = "AAA First".to_string();
        source.torrents.insert(new_hash, new_torrent);
        EngineSnapshot::capture(
            &source,
            &Settings::default(),
            &DhtStatus::default(),
            &DhtWaveTelemetry::default(),
        )
        .apply(&mut client);

        assert_eq!(client.torrent_list_order.len(), 2);
        assert_eq!(
            client.torrent_list_order[client.ui.selected_torrent_index],
            selected
        );
    }

    #[test]
    fn apply_keeps_client_view_state_and_uses_client_search() {
        let mut source = sample_state();
        let other_hash = vec![0xcd; 20];
        let mut other = TorrentDisplayState::default();
        other.latest_state.info_hash = other_hash.clone();
        other.latest_state.torrent_name = "Another Example".to_string();
        source.torrents.insert(other_hash.clone(), other);
        source.torrent_list_order.push(other_hash.clone());

        let snapshot = EngineSnapshot::capture(
            &source,
            &Settings::default(),
            &DhtStatus::default(),
            &DhtWaveTelemetry::default(),
        );
        let mut client = AppState {
            mode: AppMode::Help,
            ..AppState::default()
        };
        client.ui.selected_torrent_index = 4;
        client.ui.search_query = "another".to_string();

        snapshot.apply(&mut client);

        assert!(matches!(client.mode, AppMode::Help));
        assert_eq!(client.ui.search_query, "another");
        assert_eq!(client.torrent_list_order, vec![other_hash]);
        assert_eq!(client.ui.selected_torrent_index, 0);
    }

    /// Prints serialized size and time per snapshot section.
    ///
    /// `cargo test --lib measure_snapshot -- --ignored --nocapture`
    ///
    /// Env: `MEASURE_TORRENTS` (default 126), `MEASURE_PIECES` per torrent
    /// (2048), `MEASURE_PEERS` per torrent (30), `MEASURE_HISTORY` points per
    /// per-torrent history (3600), and optionally `MEASURE_PERSISTENCE_DIR`, a
    /// host `persistence/` directory whose network/activity history and journal
    /// are loaded (read-only) instead of left empty.
    #[test]
    #[ignore = "measurement, run explicitly"]
    fn measure_snapshot() {
        fn env_usize(name: &str, default: usize) -> usize {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default)
        }
        let torrents = env_usize("MEASURE_TORRENTS", 126);
        let pieces = env_usize("MEASURE_PIECES", 2048);
        let peers = env_usize("MEASURE_PEERS", 30);
        let history = env_usize("MEASURE_HISTORY", 3600);

        let mut state = AppState::default();
        for index in 0..torrents {
            let info_hash = (index as u32).to_be_bytes().repeat(5);
            let mut torrent = TorrentDisplayState::default();
            let metrics = &mut torrent.latest_state;
            metrics.info_hash = info_hash.clone();
            metrics.torrent_name = format!("Example Torrent {index}");
            metrics.number_of_pieces_total = pieces as u32;
            metrics.peers = (0..peers)
                .map(|peer| PeerInfo {
                    address: format!("192.0.2.{}:6881", peer % 250),
                    peer_id: vec![7; 20],
                    bitfield: (0..pieces).map(|piece| (piece + peer) % 3 != 0).collect(),
                    last_action: "Piece".to_string(),
                    ..PeerInfo::default()
                })
                .collect();
            metrics.blocks_in_history = vec![12; history];
            metrics.blocks_out_history = vec![34; history];
            torrent.download_history = vec![123_456; history];
            torrent.upload_history = vec![65_432; history];
            torrent.peer_discovery_history = vec![1; history];
            torrent.peer_connection_history = vec![2; history];
            torrent.peer_disconnect_history = vec![3; history];
            state.torrents.insert(info_hash, torrent);
        }
        state.avg_download_history = vec![1_000_000; 3600];
        state.avg_upload_history = vec![500_000; 3600];
        state.minute_avg_dl_history = vec![1_000_000; 48 * 60];
        state.minute_avg_ul_history = vec![500_000; 48 * 60];

        if let Some(dir) = std::env::var_os("MEASURE_PERSISTENCE_DIR").map(PathBuf::from) {
            if let Ok(bytes) = std::fs::read(dir.join("network_history.bin")) {
                state.network_history_state =
                    crate::persistence::network_history::decode_network_history_state(&bytes)
                        .expect("decode network history");
            }
            if let Ok(text) = std::fs::read_to_string(dir.join("event_journal.toml")) {
                state.event_journal_state = toml::from_str(&text).expect("decode journal");
            }
        }

        let started = Instant::now();
        let snapshot = EngineSnapshot::capture(
            &state,
            &Settings::default(),
            &DhtStatus::default(),
            &DhtWaveTelemetry::default(),
        );
        let capture = started.elapsed();

        fn measure<T: Serialize>(label: &str, value: &T) -> usize {
            let started = Instant::now();
            let bytes = serde_json::to_vec(value).expect("serialize");
            eprintln!(
                "{label:<28} {:>12} bytes {:>9.1} ms",
                bytes.len(),
                started.elapsed().as_secs_f64() * 1000.0
            );
            bytes.len()
        }
        eprintln!("torrents={torrents} pieces={pieces} peers={peers} history={history}");
        eprintln!(
            "capture (clone)              {:>27.1} ms",
            capture.as_secs_f64() * 1000.0
        );
        measure("status", &snapshot.status);
        measure(
            "histories.network",
            &snapshot.histories.network_history_state,
        );
        measure("histories.journal", &snapshot.histories.event_journal_state);
        let per_torrent: Vec<_> = snapshot
            .torrents
            .torrents
            .iter()
            .map(|entry| &entry.display)
            .collect();
        measure("torrents.display", &per_torrent);
        let extras: Vec<_> = snapshot
            .torrents
            .torrents
            .iter()
            .map(|entry| &entry.metrics_extras)
            .collect();
        measure("torrents.extras (peers)", &extras);
        let total = measure("TOTAL", &snapshot);

        let bytes = serde_json::to_vec(&snapshot).expect("serialize");
        let started = Instant::now();
        let decoded: EngineSnapshot = serde_json::from_slice(&bytes).expect("decode");
        let decode = started.elapsed();
        let mut client = AppState::default();
        let started = Instant::now();
        decoded.apply(&mut client);
        eprintln!(
            "client decode {:.1} ms, apply {:.1} ms, total {:.1} MiB/s at 1 Hz",
            decode.as_secs_f64() * 1000.0,
            started.elapsed().as_secs_f64() * 1000.0,
            total as f64 / (1024.0 * 1024.0)
        );
    }
}
