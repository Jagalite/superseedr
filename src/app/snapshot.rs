// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Serializable engine read model for attached TUI clients.
//!
//! The daemon captures an [`EngineSnapshot`] from its `AppState`; an attached
//! client applies it to its own local `AppState` and renders with the normal
//! TUI. View state (`ui`, `mode`, sorts, theme, ...) is never part of a
//! snapshot, so each client keeps its own. See `docs/headless-attach.md`.

// Wired into headless mode and `attach` in phase 2.
#![allow(dead_code)]

use super::*;

/// Bump whenever the serialized shape of [`EngineSnapshot`] changes.
pub const ATTACH_PROTOCOL_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone)]
pub struct EngineSnapshot {
    pub settings: Settings,
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
}

/// Torrent catalog. Candidate for changed-entries-only updates later.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct TorrentSection {
    pub list_order: Vec<Vec<u8>>,
    /// Map entries as pairs: info-hash keys are not valid JSON object keys.
    pub torrents: Vec<(Vec<u8>, TorrentDisplayState)>,
    pub startup_expected_seeds: Vec<Vec<u8>>,
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
            },
            torrents: TorrentSection {
                list_order: state.torrent_list_order.clone(),
                torrents: state
                    .torrents
                    .iter()
                    .map(|(info_hash, torrent)| (info_hash.clone(), torrent.clone()))
                    .collect(),
                startup_expected_seeds: state.startup_expected_seeds.iter().cloned().collect(),
            },
        }
    }

    /// Writes engine data into a client's local state. View state is untouched.
    pub fn apply(self, state: &mut AppState) {
        let EngineSnapshot {
            settings: _,
            dht_status: _,
            dht_wave: _,
            status,
            histories,
            torrents,
        } = self;

        state.update_available = status.update_available;
        state.shutdown_progress = status.shutdown_progress;
        state.system_warning = status.system_warning;
        state.system_error = status.system_error;
        state.network_runtime_status = status.network_runtime_status;
        state.network_activation_status = status.network_activation_status;
        state.externally_accessable_port_v4 = status.externally_accessable_port_v4;
        state.externally_accessable_port_v6 = status.externally_accessable_port_v6;
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

        state.torrent_list_order = torrents.list_order;
        state.torrents = torrents.torrents.into_iter().collect();
        state.startup_expected_seeds = torrents.startup_expected_seeds.into_iter().collect();
        state.ui.needs_redraw = true;
    }
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

    #[test]
    fn apply_keeps_client_view_state() {
        let snapshot = EngineSnapshot::capture(
            &sample_state(),
            &Settings::default(),
            &DhtStatus::default(),
            &DhtWaveTelemetry::default(),
        );
        let mut client = AppState {
            mode: AppMode::Help,
            ..AppState::default()
        };
        client.ui.selected_torrent_index = 4;
        client.ui.search_query = "local".to_string();

        snapshot.apply(&mut client);

        assert!(matches!(client.mode, AppMode::Help));
        assert_eq!(client.ui.selected_torrent_index, 4);
        assert_eq!(client.ui.search_query, "local");
    }
}
