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

/// Bump whenever the serialized shape of [`EngineSnapshot`] changes.
pub const ATTACH_PROTOCOL_VERSION: u32 = 1;

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
    /// Only populated while a client is attached (the daemon activates the view).
    pub peer_manager_view: PeerManagerView,
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

/// Rate and activity histories. Candidates for append-only updates later.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct HistorySection {
    pub avg_download_history: Vec<u64>,
    pub avg_upload_history: Vec<u64>,
    pub minute_avg_dl_history: Vec<u64>,
    pub minute_avg_ul_history: Vec<u64>,
    pub network_history_state: NetworkHistoryPersistedState,
    pub activity_history_state: ActivityHistoryPersistedState,
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
    pub peers: Vec<PeerInfo>,
    pub availability_revision: Option<u64>,
    pub departed_peers: Vec<PeerInfo>,
    pub peer_reconnect_counts: Vec<(IpAddr, u64)>,
    pub blocks_in_history: Vec<u64>,
    pub blocks_out_history: Vec<u64>,
}

impl MetricsExtras {
    fn capture(metrics: &TorrentMetrics) -> Self {
        Self {
            file_verified_bytes: metrics.file_verified_bytes.clone(),
            peers: metrics.peers.clone(),
            availability_revision: metrics.availability_revision,
            departed_peers: metrics.departed_peers.clone(),
            peer_reconnect_counts: metrics
                .peer_reconnect_counts
                .iter()
                .map(|(ip, count)| (*ip, *count))
                .collect(),
            blocks_in_history: metrics.blocks_in_history.clone(),
            blocks_out_history: metrics.blocks_out_history.clone(),
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

impl EngineSnapshot {
    pub fn capture(
        state: &AppState,
        settings: &Settings,
        dht_status: &DhtStatus,
        dht_wave: &DhtWaveTelemetry,
    ) -> Self {
        Self {
            settings: settings.clone(),
            randomize_client_port: settings.randomize_client_port,
            dht_status: dht_status.clone(),
            dht_wave: dht_wave.clone(),
            status: StatusSection {
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
                peer_manager_view: (*state.peer_manager_view).clone(),
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
                activity_history_state: state.activity_history_state.clone(),
                event_journal_state: state.event_journal_state.clone(),
            },
            torrents: TorrentSection {
                torrents: state
                    .torrents
                    .iter()
                    .map(|(info_hash, torrent)| TorrentEntry {
                        info_hash: info_hash.clone(),
                        display: torrent.clone(),
                        metrics_extras: MetricsExtras::capture(&torrent.latest_state),
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
        state.peer_manager_view = Arc::new(status.peer_manager_view);
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
        state.activity_history_state = histories.activity_history_state;
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
        crate::telemetry::network_history_telemetry::update_auto_graph_window(
            state,
            now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
        );
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
}
