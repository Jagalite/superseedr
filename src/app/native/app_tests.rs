// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

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
    advance_dht_wave_state, align_unpinned_peer_sort_with_visible_activity,
    apply_network_history_persist_result, build_app_dht_service_config, build_persist_payload,
    build_torrent_preview_tree, bytes_per_sec_to_bps, clamp_selected_indices_in_state,
    compose_system_warning, configured_download_bucket_rate,
    configured_download_ceiling_bytes_per_sec, configured_upload_bucket_rate, dht_wave_targets,
    disk_backpressure_score, effective_download_limit_bps, extract_magnet_display_name,
    flush_persistence_writer_parts, format_filesystem_path_error, initial_disk_throttle_rate,
    is_valid_incoming_bittorrent_handshake, load_torrent_file_preview,
    move_file_with_fallback_impl, network_policy_warning, parse_hybrid_hashes,
    persisted_validation_status_from_metrics, preserve_restored_added_at, prune_rss_feed_errors,
    queue_persistence_payload, refresh_autosort_after_stats, refresh_torrent_sort_after_removal,
    reset_torrent_sort_for_current_lifecycle, resolve_magnet_torrent_name, rss_settings_changed,
    runtime_torrent_settings_changed, set_test_persistence_writer_enabled,
    should_load_persisted_torrent, should_persist_network_history_on_interval,
    sort_and_filter_torrent_list_state, swarm_availability_counts, tcp_peer_listener_enabled,
    torrent_completion_percent, torrent_is_effectively_incomplete, App, AppClusterRole, AppCommand,
    AppMode, AppRuntimeMode, AppState, BrowserPane, BrowserSearchState, ColumnId,
    CommandIngestResult, ConfigNetworkInterfaceInventory, DataRate, DhtVisualization,
    DhtWaveTargets, DhtWaveUiState, DiskBackpressureDecision, DiskBackpressureDownloadThrottle,
    DiskBackpressureSample, DiskHealthVisualization, DownloadSelectionTarget, FileBrowserMode,
    FileMetadata, FilePriority, InboundPeerTransportStatus, IngestSource, ListenerSet, LogCooldown,
    PeerInfo, PeerListenerTransportMode, PeerSortColumn, PeerStreamVisualization,
    PendingManualIngest, PersistPayload, ResolvedAddPayload, SearchMode, SelectedHeader,
    SortDirection, SwarmAvailabilityFlashState, TorrentControlState, TorrentDisplayState,
    TorrentIntegritySnapshot, TorrentMetrics, TorrentPreviewPayload, TorrentSortColumn, UiState,
    VisualizationFocusPanel, VisualizationFocusState, WakeLagPeerThrottle,
    AWAITING_MAGNET_METADATA_LABEL, BITTORRENT_PROTOCOL_STR, DHT_WAVE_PHASE_WRAP_PERIOD,
    DISK_WRITE_THROTTLE_MIN_BYTES_PER_SEC, DISK_WRITE_THROTTLE_START_BYTES_PER_SEC,
    DISK_WRITE_THROTTLE_STEP_MAX, DISK_WRITE_THROTTLE_STEP_MIN,
    DISK_WRITE_THROTTLE_TARGET_LATENCY_SECS, DISK_WRITE_THROTTLE_WINDOW_TICKS,
    SWARM_AVAILABILITY_FLASH_DURATION,
};
use crate::app::torrent_manager_protocol::{
    FileProbeBatchResult, FileProbeEntry, TorrentFileProbeStatus,
};
use crate::config::{
    clear_shared_config_state_for_tests, set_app_paths_override_for_tests, Settings,
    TorrentSettings, UiLayoutMode,
};
#[cfg(feature = "dht")]
use crate::dht::service::{DhtBackendKind, DhtServiceConfig};
use crate::dht::service::{DhtService, DhtStatus, DhtWaveTelemetry, TestDhtRecorder};
use crate::integrations::control::service::control_event_details;
use crate::integrations::control::{read_control_request, ControlRequest};
use crate::integrations::status::{self, AppOutputState};
use crate::networking::PeerTransportKind;
use crate::networking::{
    NetworkHandle, NetworkLease, NetworkScopeId, NetworkState, NetworkSupervisor, UtpPeerTransport,
};
use crate::persistence::event_journal::{
    ControlOrigin, EventDetails, EventType, IngestKind, IngestOrigin,
};
use crate::persistence::event_journal::{EventCategory, EventJournalEntry};
use crate::persistence::StorageError;
use crate::telemetry::ui_telemetry::UiTelemetry;
use crate::torrent_identity::{info_hash_from_torrent_bytes, info_hash_from_torrent_source};
use crate::torrent_manager::{ManagerCommand, ManagerEvent};
use crate::tui::effects::{BrowserDialogEffect, BrowserTransition, ConfirmDecision};
use crate::tui::layout::normal::{
    calculate_layout, LayoutContext, DEFAULT_SIDEBAR_PERCENT, PEER_STREAM_MIN_HEIGHT,
    PEER_STREAM_MIN_WIDTH,
};
use crate::tui::runtime::{execute_browser_dialog_effects, execute_native_confirm_decision};
use crate::tui::screens::browser::{
    build_download_confirm_payload, reduce_browser_dialog_action, BrowserDialogAction,
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
    assert!(!inventory.finish_refresh(request_id.wrapping_sub(1), Err("stale failure".to_string())));
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
        DISK_WRITE_THROTTLE_TARGET_LATENCY_SECS * attempted_bps as f64 / disk_capacity_bps as f64
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
    assert!(configured_download_bucket_rate(crate::config::UNLIMITED_RATE_LIMIT_BPS).is_infinite());
    assert!(configured_upload_bucket_rate(crate::config::UNLIMITED_RATE_LIMIT_BPS).is_infinite());
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
        effective_download_limit_bps(crate::config::UNLIMITED_RATE_LIMIT_BPS, Some(500_000_000)),
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
    assert!(fill_rate >= DISK_WRITE_THROTTLE_START_BYTES_PER_SEC * DISK_WRITE_THROTTLE_STEP_MIN);
    assert!(fill_rate <= DISK_WRITE_THROTTLE_START_BYTES_PER_SEC * DISK_WRITE_THROTTLE_STEP_MAX);
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
    let loaded_magnet = "magnet:?xt=urn:btih:6666666666666666666666666666666666666666".to_string();
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

#[tokio::test]
async fn reloaded_visualization_selections_update_the_live_ui() {
    let mut app = App::new(Settings::default(), AppRuntimeMode::Normal)
        .await
        .expect("create app");
    let mut reloaded = app.client_configs.clone();
    reloaded.peer_stream_visualization = PeerStreamVisualization::HelixExchange;
    reloaded.disk_health_visualization = DiskHealthVisualization::StorageDial;
    reloaded.dht_visualization = DhtVisualization::PeerBloom;

    app.apply_reloaded_settings(reloaded).await;

    assert_eq!(
        app.app_state.ui.visualization_focus.peer_stream,
        PeerStreamVisualization::HelixExchange
    );
    assert_eq!(
        app.app_state.ui.visualization_focus.disk_health,
        DiskHealthVisualization::StorageDial
    );
    assert_eq!(
        app.app_state.ui.visualization_focus.dht,
        DhtVisualization::PeerBloom
    );
    let _ = app.shutdown_tx.send(());
}

#[tokio::test]
async fn visualization_persistence_keeps_the_latest_ui_selection() {
    let mut app = App::new(Settings::default(), AppRuntimeMode::Normal)
        .await
        .expect("create app");
    let (persistence_tx, persistence_rx) = watch::channel(None);
    app.persistence_tx = Some(persistence_tx);

    app.app_state.ui.visualization_focus.peer_stream = PeerStreamVisualization::HelixExchange;
    app.persist_visualization_selections();
    app.app_state.ui.visualization_focus.disk_health = DiskHealthVisualization::StorageDial;
    app.app_state.ui.visualization_focus.dht = DhtVisualization::LookupVortex;
    app.persist_visualization_selections();

    let payload = persistence_rx
        .borrow()
        .clone()
        .expect("visualization selection persistence payload");
    assert_eq!(
        payload.settings.peer_stream_visualization,
        PeerStreamVisualization::HelixExchange
    );
    assert_eq!(
        payload.settings.disk_health_visualization,
        DiskHealthVisualization::StorageDial
    );
    assert_eq!(
        payload.settings.dht_visualization,
        DhtVisualization::LookupVortex
    );
    assert_eq!(app.client_configs, payload.settings);
    let _ = app.shutdown_tx.send(());
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
    assert!((DataRate::Rate60s.frame_interval().as_secs_f64() - (1.0 / 60.0)).abs() < 0.000_001);
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

        max_amplitude_delta = max_amplitude_delta.max((wave.amplitude - previous.amplitude).abs());
        max_frequency_delta = max_frequency_delta.max((wave.frequency - previous.frequency).abs());
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
fn stats_refresh_reorders_pinned_speed_sort_when_speeds_change() {
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

    assert!(changed);
    assert_eq!(
        app_state.torrent_list_order,
        vec![new_fast_hash, old_fast_hash]
    );
}

#[test]
fn stats_refresh_preserves_finished_progress_pin_without_a_completion_transition() {
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

    assert!(!changed);
    assert!(app_state.torrent_sort_pinned);
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
fn stats_refresh_preserves_progress_pin_for_completed_probe_issue() {
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
    unavailable.peer_discovery_history = vec![1];

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
    assert!(app_state.torrent_sort_pinned);
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
fn reset_torrent_sort_uses_upload_when_every_torrent_is_complete() {
    let mut app_state = AppState {
        torrent_sort: (TorrentSortColumn::Down, SortDirection::Descending),
        ..Default::default()
    };
    let hash = b"hash_a".to_vec();
    let mut torrent = mock_display("sample-upload.iso", 0);
    torrent.latest_state.data_available = true;
    torrent.latest_state.is_complete = true;
    torrent.smoothed_upload_speed_bps = 4_096;
    app_state.torrents.insert(hash, torrent);

    reset_torrent_sort_for_current_lifecycle(&mut app_state);

    assert_eq!(
        app_state.torrent_sort,
        (TorrentSortColumn::Up, SortDirection::Descending)
    );
}

#[test]
fn reset_torrent_sort_uses_download_when_a_torrent_is_incomplete() {
    let mut app_state = AppState {
        torrent_sort: (TorrentSortColumn::Down, SortDirection::Descending),
        ..Default::default()
    };
    let hash = b"hash_a".to_vec();
    let mut torrent = mock_display("sample-incomplete.iso", 0);
    torrent.latest_state.data_available = true;
    torrent.latest_state.number_of_pieces_total = 10;
    torrent.latest_state.number_of_pieces_completed = 9;
    app_state.torrents.insert(hash, torrent);

    reset_torrent_sort_for_current_lifecycle(&mut app_state);

    assert_eq!(
        app_state.torrent_sort,
        (TorrentSortColumn::Down, SortDirection::Descending)
    );
}

#[test]
fn removing_the_last_incomplete_torrent_selects_upload_sort() {
    let incomplete_hash = b"hash_incomplete".to_vec();
    let complete_hash = b"hash_complete".to_vec();
    let mut incomplete = mock_display("sample-incomplete.iso", 0);
    incomplete.latest_state.number_of_pieces_total = 10;
    incomplete.latest_state.number_of_pieces_completed = 9;
    let mut complete = mock_display("sample-complete.iso", 0);
    complete.latest_state.number_of_pieces_total = 10;
    complete.latest_state.number_of_pieces_completed = 10;
    complete.latest_state.is_complete = true;
    let mut app_state = AppState {
        torrent_sort: (TorrentSortColumn::Down, SortDirection::Descending),
        torrent_list_order: vec![incomplete_hash.clone(), complete_hash.clone()],
        torrents: HashMap::from([
            (incomplete_hash.clone(), incomplete),
            (complete_hash.clone(), complete),
        ]),
        ..Default::default()
    };

    app_state.torrents.remove(&incomplete_hash);
    app_state
        .torrent_list_order
        .retain(|info_hash| info_hash != &incomplete_hash);
    refresh_torrent_sort_after_removal(&mut app_state);

    assert_eq!(
        app_state.torrent_sort,
        (TorrentSortColumn::Up, SortDirection::Descending)
    );
    assert_eq!(app_state.torrent_list_order, vec![complete_hash]);
}

#[test]
fn align_unpinned_peer_sort_uses_upload_when_only_upload_is_visible() {
    let mut app_state = AppState {
        peer_sort: (PeerSortColumn::DL, SortDirection::Descending),
        ..Default::default()
    };
    let hash = b"hash_a".to_vec();
    let mut torrent = mock_display("sample-peer-upload.iso", 1);
    torrent.latest_state.peers[0].upload_speed_bps = 2_048;
    app_state.torrent_list_order = vec![hash.clone()];
    app_state.torrents.insert(hash, torrent);

    align_unpinned_peer_sort_with_visible_activity(&mut app_state);

    assert_eq!(
        app_state.peer_sort,
        (PeerSortColumn::UL, SortDirection::Descending)
    );
}

#[test]
fn align_unpinned_peer_sort_keeps_speed_sort_when_peer_activity_is_idle() {
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

    align_unpinned_peer_sort_with_visible_activity(&mut app_state);

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
    assert!(network_policy_warning(&crate::networking::NetworkBindingConfig::default()).is_none());
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

    let manager_event_tx = app.register_manager_event_source(&info_hash);
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
    runtime_settings.network_binding.interface = Some("missing-startup-interface-test".to_string());
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
    explicit_settings.network_binding.interface = Some("explicit-saved-interface-test".to_string());
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
    wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Blocked(_))).await;
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
    wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Blocked(_))).await;
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
            torrent_or_magnet: format!("magnet:?xt=urn:btih:{}", hash_digit.to_string().repeat(40)),
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
            torrent_or_magnet: format!("magnet:?xt=urn:btih:{}", hash_digit.to_string().repeat(40)),
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
            torrent_or_magnet: format!("magnet:?xt=urn:btih:{}", hash_digit.to_string().repeat(40)),
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
        VecDeque::from([
            info_hash_from_torrent_source(&torrent.torrent_or_magnet).expect("derive info hash")
        ]);
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
            torrent_or_magnet: format!("magnet:?xt=urn:btih:{}", hash_digit.to_string().repeat(40)),
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
    let expected_entry_id = app.app_state.event_journal_state.next_id;

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
        .find(|entry| {
            entry.id == expected_entry_id && entry.event_type == EventType::DataUnavailable
        })
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
async fn manual_torrent_browser_moves_shared_inbox_file_to_shared_processed_and_updates_journal() {
    let _guard = lock_shared_env();
    let shared_root = tempfile::tempdir().expect("create shared root");
    let effective_root = shared_root.path().join("superseedr-config");
    let original_shared_dir = env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
    let original_host_id = env::var_os("SUPERSEEDR_SHARED_HOST_ID");

    env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", shared_root.path());
    env::set_var("SUPERSEEDR_SHARED_HOST_ID", "node-a");
    clear_shared_config_state_for_tests();

    std::fs::create_dir_all(effective_root.join("hosts").join("node-a")).expect("create hosts dir");
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

    std::fs::create_dir_all(effective_root.join("hosts").join("node-a")).expect("create hosts dir");
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

    std::fs::create_dir_all(effective_root.join("hosts").join("node-a")).expect("create hosts dir");
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
            magnet_link: "magnet:?xt=urn:btih:5555555555555555555555555555555555555555".to_string(),
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
            magnet_link: "magnet:?xt=urn:btih:5555555555555555555555555555555555555555".to_string(),
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
            magnet_link: "magnet:?xt=urn:btih:5555555555555555555555555555555555555555".to_string(),
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
        &app.app_state.ui.file_browser.data,
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
        &app.app_state.ui.file_browser.data,
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
async fn shared_follower_with_shared_config_default_queues_leader_request_without_manual_browser() {
    let _guard = lock_shared_env();
    let shared_root = tempfile::tempdir().expect("create shared root");
    let effective_root = shared_root.path().join("superseedr-config");
    let original_shared_dir = env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
    let original_host_id = env::var_os("SUPERSEEDR_SHARED_HOST_ID");

    env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", shared_root.path());
    env::set_var("SUPERSEEDR_SHARED_HOST_ID", "node-a");
    clear_shared_config_state_for_tests();

    std::fs::create_dir_all(effective_root.join("hosts").join("node-a")).expect("create hosts dir");
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
    let transition =
        execute_native_confirm_decision(&mut app, ConfirmDecision::Download(payload)).await;

    assert!(matches!(transition, Some(BrowserTransition::ToNormal)));
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
    let transition =
        execute_native_confirm_decision(&mut app, ConfirmDecision::Download(payload)).await;
    assert!(matches!(transition, Some(BrowserTransition::ToNormal)));

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
    let transition =
        execute_native_confirm_decision(&mut app, ConfirmDecision::Download(payload)).await;

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
            magnet_link: "magnet:?xt=urn:btih:6666666666666666666666666666666666666666".to_string(),
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

    execute_browser_dialog_effects(&mut app, vec![BrowserDialogEffect::ToNormalAndClearPending])
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

    std::fs::create_dir_all(effective_root.join("hosts").join("node-a")).expect("create hosts dir");
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

    std::fs::create_dir_all(effective_root.join("hosts").join("node-a")).expect("create hosts dir");
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
    // Fuzzy search includes ancestor paths; keep them deterministic to avoid accidental matches.
    let current_dir = PathBuf::from("/virtual-cursor-fixture");
    let settings = crate::config::Settings {
        client_port: 0,
        ..Default::default()
    };
    let mut app = App::new(settings, AppRuntimeMode::Normal)
        .await
        .expect("build app");
    let matching_path = current_dir.as_path().join("beta-folder");
    app.app_state.mode = AppMode::FileBrowser;
    app.app_state.ui.file_browser.fetch_request_id = 5;
    app.app_state.ui.file_browser.state.current_path = current_dir.as_path().to_path_buf();
    app.app_state.ui.file_browser.browser_mode = FileBrowserMode::Directory;
    app.app_state.ui.file_browser.search_state = BrowserSearchState::Applied;
    app.app_state.ui.file_browser.search_query = "beta".to_string();
    app.app_state.ui.file_browser.search_mode = SearchMode::Fuzzy;

    app.handle_app_command(AppCommand::UpdateFileBrowserData {
        request_id: 5,
        path: current_dir.as_path().to_path_buf(),
        data: vec![
            RawNode {
                name: "alpha-folder".to_string(),
                full_path: current_dir.as_path().join("alpha-folder"),
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
        highlight_path: Some(current_dir.as_path().join("alpha-folder")),
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
    display.latest_file_probe_status = Some(TorrentFileProbeStatus::Files(vec![FileProbeEntry {
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

    let first_command = tokio::time::timeout(std::time::Duration::from_secs(1), manager_rx.recv())
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

    let second_command = tokio::time::timeout(std::time::Duration::from_secs(1), manager_rx.recv())
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

    std::fs::create_dir_all(effective_root.join("hosts").join("node-a")).expect("create hosts dir");
    std::fs::write(
        effective_root
            .join("hosts")
            .join("node-a")
            .join("config.toml"),
        "client_port = 0\n",
    )
    .expect("write host config");

    let initial_settings = crate::config::load_settings().expect("load initial shared settings");
    let mut app = App::new(initial_settings.clone(), AppRuntimeMode::SharedFollower)
        .await
        .expect("build shared follower app");

    let revision_path =
        crate::config::shared_cluster_revision_path().expect("shared cluster revision path");

    let mut follower_reload_settings = initial_settings.clone();
    follower_reload_settings.global_download_limit_bps = 42;
    crate::config::save_settings(&follower_reload_settings).expect("save follower reload settings");

    app.handle_app_command(AppCommand::ReloadClusterState(revision_path.clone()))
        .await;
    assert_eq!(app.client_configs.global_download_limit_bps, 42);

    app.current_cluster_role = Some(AppClusterRole::Leader);
    app.runtime_mode = AppRuntimeMode::SharedLeader;
    app.sync_cluster_role_label();

    let mut leader_ignored_settings = follower_reload_settings.clone();
    leader_ignored_settings.global_download_limit_bps = 99;
    crate::config::save_settings(&leader_ignored_settings).expect("save leader ignored settings");

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

    std::fs::create_dir_all(effective_root.join("hosts").join("node-a")).expect("create hosts dir");
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
        crate::persistence::atomic::serialize_versioned_json(&snapshot)
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

    std::fs::create_dir_all(effective_root.join("hosts").join("node-a")).expect("create hosts dir");
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

    let host_snapshot: AppOutputState = crate::persistence::atomic::deserialize_versioned_json(
        &std::fs::read_to_string(&host_status_path).expect("read host status"),
    )
    .expect("parse host status");
    let leader_snapshot: AppOutputState = crate::persistence::atomic::deserialize_versioned_json(
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

    std::fs::create_dir_all(effective_root.join("hosts").join("node-a")).expect("create hosts dir");
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

    std::fs::create_dir_all(effective_root.join("hosts").join("node-a")).expect("create hosts dir");
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

    std::fs::create_dir_all(effective_root.join("hosts").join("node-a")).expect("create hosts dir");
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
            crate::persistence::network_history::NetworkHistoryRollupState::from_snapshot(&snapshot),
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
        revision: 1,
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
        Some(crate::dht::service::DhtBackendKind::Disabled)
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
    wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Blocked(_))).await;
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
    wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Blocked(_))).await;
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
    let (network_handle, supervisor_task) = NetworkSupervisor::spawn_with_config(&blocked_config);
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
    wait_for_app_network_state(&mut app, |state| matches!(state, NetworkState::Blocked(_))).await;
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
) -> Vec<crate::dht::service::DhtServiceConfig> {
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

#[tokio::test]
async fn client_download_order_updates_all_registered_catalog_managers() {
    let settings = crate::config::Settings {
        client_port: 0,
        ..Default::default()
    };
    let mut app = App::new(settings, AppRuntimeMode::Normal)
        .await
        .expect("create app");
    let mut receivers = Vec::new();
    for byte in [0x55u8, 0x66] {
        let hash = vec![byte; 20];
        app.client_configs
            .torrents
            .push(crate::config::TorrentSettings {
                torrent_or_magnet: format!("magnet:?xt=urn:btih:{}", hex::encode(&hash)),
                ..Default::default()
            });
        let (tx, rx) = tokio::sync::mpsc::channel(10);
        app.torrent_manager_command_txs.insert(hash, tx);
        receivers.push(rx);
    }
    let preview_hash = vec![0x77u8; 20];
    let (tx, rx) = tokio::sync::mpsc::channel(10);
    app.torrent_manager_command_txs
        .insert(preview_hash.clone(), tx);
    app.app_state.pending_magnet_preview_info_hash = Some(preview_hash);
    receivers.push(rx);
    for mode in [
        crate::config::DownloadMode::Sequential,
        crate::config::DownloadMode::RarestFirst,
    ] {
        let mut requested = app.client_configs.clone();
        // Exercise the host boundary without pre-normalizing the torrent entries.
        requested.download_mode = mode;
        app.apply_settings_update(requested, false).await;
        assert_eq!(app.client_configs.download_mode, mode);
        assert!(app
            .client_configs
            .torrents
            .iter()
            .all(|torrent| torrent.download_mode == mode));
        for rx in &mut receivers {
            assert_eq!(
                rx.try_recv().expect("policy command"),
                ManagerCommand::SetDownloadMode(mode)
            );
            assert!(rx.try_recv().is_err());
        }
    }
}
