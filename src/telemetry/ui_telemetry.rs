// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::torrent_manager_protocol::{DiskIoOperation, FileActivityDirection, ManagerEvent};
use crate::app::{AppMode, AppState, PeerInfo, TorrentDisplayState, TorrentMetrics};
use crate::config::{PeerSortColumn, SortDirection};
use std::collections::VecDeque;
use std::time::Duration;
use tracing::{event as tracing_event, Level};
use web_time::{Instant, SystemTime, UNIX_EPOCH};

pub const SECONDS_HISTORY_MAX: usize = 3600; // 1 hour of per-second data
pub const MINUTES_HISTORY_MAX: usize = 48 * 60; // 48 hours of per-minute data
const RECENT_FILE_ACTIVITY_RETENTION: Duration = Duration::from_secs(120);
const RECEIVE_TO_WRITE_LATENCY_SAMPLES_MAX: usize = 1024;
const PENDING_WRITE_START_TIMES_MAX: usize = 100_000;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SystemTelemetrySnapshot {
    pub(crate) cpu_usage: f32,
    pub(crate) ram_usage_percent: f32,
    pub(crate) app_ram_usage: u64,
    pub(crate) run_time: u64,
}

/// Apply exactly the native ten-sample EMA recurrence. Repeated equal observations can stop
/// once floating-point convergence is reached, keeping large batches inexpensive.
fn record_latency_samples(ema: &mut f64, average: &mut Duration, duration: Duration, count: usize) {
    let current_micros = duration.as_micros() as f64;
    const ALPHA: f64 = 2.0 / 11.0;
    for _ in 0..count {
        let previous = *ema;
        *ema = if previous == 0.0 {
            current_micros
        } else {
            (current_micros * ALPHA) + (previous * (1.0 - ALPHA))
        };
        if *ema == previous {
            break;
        }
    }
    if count > 0 {
        *average = Duration::from_micros(*ema as u64);
    }
}

pub struct UiTelemetry;

impl UiTelemetry {
    /// Aggregates measured peer/block activity independently of event transport.
    pub(crate) fn record_peer_activity(
        state: &mut AppState,
        info_hash: &[u8],
        discovered: u64,
        connected: u64,
        disconnected: u64,
        received: u64,
        sent: u64,
    ) {
        if let Some(torrent) = state.torrents.get_mut(info_hash) {
            torrent.peers_discovered_this_tick += discovered;
            torrent.peers_connected_this_tick += connected;
            torrent.peers_disconnected_this_tick += disconnected;
            torrent.latest_state.blocks_in_this_tick += received;
            torrent.latest_state.blocks_out_this_tick += sent;
        }
    }

    /// Byte totals describe all operations; the log may contain only sampled operations.
    pub(crate) fn record_disk_reads(
        state: &mut AppState,
        info_hash: &[u8],
        bytes: u64,
        samples: &[DiskIoOperation],
    ) {
        for op in samples {
            state.global_disk_read_history_log.push_front(*op);
        }
        state.global_disk_read_history_log.truncate(100);
        if let Some(torrent) = state.torrents.get_mut(info_hash) {
            torrent.bytes_read_this_tick += bytes;
            for op in samples {
                torrent.disk_read_history_log.push_front(*op);
            }
            torrent.disk_read_history_log.truncate(50);
        }
    }

    pub(crate) fn record_disk_writes(
        state: &mut AppState,
        info_hash: &[u8],
        bytes: u64,
        samples: &[DiskIoOperation],
    ) {
        for op in samples {
            state.global_disk_write_history_log.push_front(*op);
        }
        state.global_disk_write_history_log.truncate(100);
        if let Some(torrent) = state.torrents.get_mut(info_hash) {
            torrent.bytes_written_this_tick += bytes;
            for op in samples {
                torrent.disk_write_history_log.push_front(*op);
            }
            torrent.disk_write_history_log.truncate(50);
        }
    }

    pub(crate) fn record_reads_finished(
        state: &mut AppState,
        count: usize,
        latency: Option<Duration>,
    ) {
        if let Some(latency) = latency {
            record_latency_samples(
                &mut state.read_latency_ema,
                &mut state.avg_disk_read_latency,
                latency,
                count,
            );
        }
        state.reads_completed_this_tick += count as u32;
    }

    pub(crate) fn record_writes_finished(
        state: &mut AppState,
        count: usize,
        latency: Option<Duration>,
    ) {
        if let Some(latency) = latency {
            record_latency_samples(
                &mut state.write_latency_ema,
                &mut state.avg_disk_write_latency,
                latency,
                count,
            );
        }
        state.writes_completed_this_tick += count as u32;
    }

    /// Completion latency is measured from write start, not block receipt.
    /// A batch's representative latency stands for each of its completed operations.
    pub(crate) fn record_writes_completed(
        state: &mut AppState,
        bytes: u64,
        latency: Option<Duration>,
        count: usize,
    ) {
        state.bytes_written_completed_this_tick = state
            .bytes_written_completed_this_tick
            .saturating_add(bytes);
        if let Some(latency) = latency {
            for _ in 0..count.min(RECEIVE_TO_WRITE_LATENCY_SAMPLES_MAX) {
                state.recv_to_write_latency_samples.push_back(latency);
            }
            while state.recv_to_write_latency_samples.len() > RECEIVE_TO_WRITE_LATENCY_SAMPLES_MAX {
                state.recv_to_write_latency_samples.pop_front();
            }
        }
    }

    pub(crate) fn record_disk_backoff(state: &mut AppState, duration: Duration) {
        state.max_disk_backoff_this_tick_ms = state
            .max_disk_backoff_this_tick_ms
            .max(duration.as_millis() as u64);
        if state.system_warning.is_none() {
            state.system_warning = Some("System Warning: Potential FD limit hit (detected via Disk I/O backoff). Increase 'ulimit -n' if issues persist.".to_string());
        }
    }

    pub fn on_manager_event_metrics(app_state: &mut AppState, event: &ManagerEvent) -> bool {
        match event {
            ManagerEvent::DiskReadStarted { info_hash, op } => {
                app_state.read_op_start_times.push_front(Instant::now());
                Self::record_disk_reads(
                    app_state,
                    info_hash,
                    op.length as u64,
                    std::slice::from_ref(op),
                );
            }
            ManagerEvent::DiskReadFinished => {
                let latency = app_state
                    .read_op_start_times
                    .pop_front()
                    .map(|start| start.elapsed());
                Self::record_reads_finished(app_state, 1, latency);
            }
            ManagerEvent::DiskWriteStarted { info_hash, op } => {
                app_state.write_op_start_times.push_front(Instant::now());
                if app_state.pending_piece_write_start_times.len() > PENDING_WRITE_START_TIMES_MAX {
                    app_state.pending_piece_write_start_times.clear();
                }
                app_state
                    .pending_piece_write_start_times
                    .insert((info_hash.clone(), op.piece_index), Instant::now());
                Self::record_disk_writes(
                    app_state,
                    info_hash,
                    op.length as u64,
                    std::slice::from_ref(op),
                );
            }
            ManagerEvent::DiskWriteCompleted { info_hash, op } => {
                let latency = app_state
                    .pending_piece_write_start_times
                    .remove(&(info_hash.clone(), op.piece_index))
                    .map(|start| start.elapsed());
                Self::record_writes_completed(app_state, op.length as u64, latency, 1);
            }
            ManagerEvent::DiskWriteFinished {
                info_hash,
                piece_index,
            } => {
                app_state
                    .pending_piece_write_start_times
                    .remove(&(info_hash.clone(), *piece_index));
                let latency = app_state
                    .write_op_start_times
                    .pop_front()
                    .map(|start| start.elapsed());
                Self::record_writes_finished(app_state, 1, latency);
            }
            ManagerEvent::DiskIoBackoff { duration } => {
                Self::record_disk_backoff(app_state, *duration)
            }
            ManagerEvent::PeerDiscovered { info_hash } => {
                Self::record_peer_activity(app_state, info_hash, 1, 0, 0, 0, 0)
            }
            ManagerEvent::PeerConnected { info_hash } => {
                Self::record_peer_activity(app_state, info_hash, 0, 1, 0, 0, 0)
            }
            ManagerEvent::PeerDisconnected { info_hash } => {
                Self::record_peer_activity(app_state, info_hash, 0, 0, 1, 0, 0)
            }
            ManagerEvent::BlockReceived { info_hash } => {
                Self::record_peer_activity(app_state, info_hash, 0, 0, 0, 1, 0)
            }
            ManagerEvent::BlockSent { info_hash } => {
                Self::record_peer_activity(app_state, info_hash, 0, 0, 0, 0, 1)
            }
            _ => return false,
        }
        true
    }

    pub fn on_metrics(app_state: &mut AppState, message: TorrentMetrics) {
        let info_hash = message.info_hash.clone();
        let display_state = app_state
            .torrents
            .entry(message.info_hash)
            .or_insert_with(|| TorrentDisplayState {
                added_at_unix_secs: Some(current_unix_secs()),
                ..Default::default()
            });
        let now = Instant::now();
        prune_stale_recent_file_activity(display_state, now);
        for activity_update in &message.file_activity_updates {
            for relative_path in &activity_update.touched_relative_paths {
                let activity = display_state
                    .recent_file_activity
                    .entry(relative_path.clone())
                    .or_default();
                match activity_update.direction {
                    FileActivityDirection::Download => activity.download_at = Some(now),
                    FileActivityDirection::Upload => activity.upload_at = Some(now),
                }
            }
        }
        let downloaded_delta = message
            .session_total_downloaded
            .saturating_sub(display_state.last_seen_session_total_downloaded);
        let uploaded_delta = message
            .session_total_uploaded
            .saturating_sub(display_state.last_seen_session_total_uploaded);
        app_state.session_total_downloaded += downloaded_delta;
        app_state.session_total_uploaded += uploaded_delta;
        display_state.last_seen_session_total_downloaded = message.session_total_downloaded;
        display_state.last_seen_session_total_uploaded = message.session_total_uploaded;

        display_state.latest_state.info_hash = info_hash;
        display_state
            .latest_state
            .number_of_successfully_connected_peers =
            message.number_of_successfully_connected_peers;
        display_state.latest_state.tcp_peer_count = message.tcp_peer_count;
        display_state.latest_state.utp_peer_count = message.utp_peer_count;
        display_state.latest_state.beneficial_tcp_peer_count = message.beneficial_tcp_peer_count;
        display_state.latest_state.beneficial_utp_peer_count = message.beneficial_utp_peer_count;
        display_state.latest_state.number_of_pieces_total = message.number_of_pieces_total;
        display_state.latest_state.number_of_pieces_completed = message.number_of_pieces_completed;
        display_state.latest_state.download_speed_bps = message.download_speed_bps;
        display_state.latest_state.upload_speed_bps = message.upload_speed_bps;
        display_state.latest_state.bytes_downloaded_this_tick = message.bytes_downloaded_this_tick;
        display_state.latest_state.bytes_uploaded_this_tick = message.bytes_uploaded_this_tick;
        display_state.latest_state.session_total_downloaded = message.session_total_downloaded;
        display_state.latest_state.session_total_uploaded = message.session_total_uploaded;
        display_state.latest_state.eta = message.eta;
        display_state.latest_state.next_announce_in = message.next_announce_in;

        if let Some(path) = message.download_path {
            display_state.latest_state.download_path = Some(path);
        }
        if !message.torrent_name.is_empty() {
            display_state.latest_state.torrent_name = message.torrent_name;
        }
        display_state.latest_state.container_name = message.container_name;
        display_state.latest_state.file_count = message.file_count;
        display_state.latest_state.file_priorities = message.file_priorities;
        display_state.latest_state.file_verified_bytes = message.file_verified_bytes;
        display_state.latest_state.data_available = message.data_available;
        display_state.latest_state.is_complete = message.is_complete;
        display_state.latest_state.total_size = message.total_size;
        display_state.latest_state.bytes_written = message.bytes_written;

        display_state
            .download_history
            .push(display_state.latest_state.download_speed_bps);
        display_state
            .upload_history
            .push(display_state.latest_state.upload_speed_bps);

        if display_state.download_history.len() > 200 {
            display_state.download_history.remove(0);
            display_state.upload_history.remove(0);
        }

        if app_state.total_download_history.len() > 200 {
            app_state.total_download_history.remove(0);
            app_state.total_upload_history.remove(0);
        }

        display_state.smoothed_download_speed_bps = display_state.latest_state.download_speed_bps;
        display_state.smoothed_upload_speed_bps = display_state.latest_state.upload_speed_bps;
        display_state.latest_state.peers = message.peers;

        display_state.latest_state.activity_message = message.activity_message;

        let current_swarm_availability = aggregate_peers_to_availability(
            &display_state.latest_state.peers,
            display_state.latest_state.number_of_pieces_total as usize,
        );
        if !display_state.latest_state.peers.is_empty() && !current_swarm_availability.is_empty() {
            display_state
                .swarm_availability_history
                .push(current_swarm_availability);
        }
        if display_state.swarm_availability_history.len() > 200 {
            display_state.swarm_availability_history.remove(0);
        }
    }

    pub(crate) fn second_tick_requires_system_snapshot(app_state: &AppState) -> bool {
        !matches!(app_state.mode, AppMode::PowerSaving) || app_state.run_time.is_multiple_of(5)
    }

    /// Runs production telemetry aggregation with platform-provided system metrics.
    pub(crate) fn on_second_tick_with_system_snapshot(
        app_state: &mut AppState,
        snapshot: Option<SystemTelemetrySnapshot>,
    ) {
        if Self::second_tick_requires_system_snapshot(app_state) {
            if let Some(snapshot) = snapshot {
                app_state.cpu_usage = snapshot.cpu_usage;
                app_state.ram_usage_percent = snapshot.ram_usage_percent;
                app_state.app_ram_usage = snapshot.app_ram_usage;
                app_state.run_time = snapshot.run_time;
            } else {
                app_state.run_time += 1;
            }
        } else {
            app_state.run_time += 1;
        }
        // Counters and histories describe one second even when system sampling is throttled.
        Self::aggregate_second_tick(app_state);
    }

    fn aggregate_second_tick(app_state: &mut AppState) {
        app_state.global_disk_read_thrash_score =
            calculate_thrash_score(&app_state.global_disk_read_history_log);
        app_state.global_disk_write_thrash_score =
            calculate_thrash_score(&app_state.global_disk_write_history_log);

        let global_read_thrash_f64 =
            calculate_thrash_score_seek_cost_f64(&app_state.global_disk_read_history_log);
        let global_write_thrash_f64 =
            calculate_thrash_score_seek_cost_f64(&app_state.global_disk_write_history_log);
        app_state.global_disk_thrash_score = global_read_thrash_f64 + global_write_thrash_f64;

        if app_state.global_disk_thrash_score > 0.01 {
            app_state
                .global_seek_cost_per_byte_history
                .push(app_state.global_disk_thrash_score);
        }
        if app_state.global_seek_cost_per_byte_history.len() > 1000 {
            app_state.global_seek_cost_per_byte_history.remove(0);
        }
        const MIN_SAMPLES_TO_LEARN: usize = 50;
        if app_state.global_seek_cost_per_byte_history.len() > MIN_SAMPLES_TO_LEARN {
            let mut sorted_history = app_state.global_seek_cost_per_byte_history.clone();
            sorted_history.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let percentile_index = percentile_index_nearest_rank(sorted_history.len(), 95)
                .expect("non-empty seek cost history");
            let new_scpb_max = sorted_history[percentile_index];
            app_state.adaptive_max_scpb = new_scpb_max.max(1.0);
        }

        let mut global_disk_read_bps = 0;
        let mut global_disk_write_bps = 0;

        for torrent in app_state.torrents.values_mut() {
            torrent.disk_read_speed_bps = torrent.bytes_read_this_tick * 8;
            torrent.disk_write_speed_bps = torrent.bytes_written_this_tick * 8;

            global_disk_read_bps += torrent.disk_read_speed_bps;
            global_disk_write_bps += torrent.disk_write_speed_bps;

            torrent.bytes_read_this_tick = 0;
            torrent.bytes_written_this_tick = 0;

            torrent.disk_read_thrash_score = calculate_thrash_score(&torrent.disk_read_history_log);
            torrent.disk_write_thrash_score =
                calculate_thrash_score(&torrent.disk_write_history_log);

            torrent
                .peer_discovery_history
                .push(torrent.peers_discovered_this_tick);
            torrent
                .peer_connection_history
                .push(torrent.peers_connected_this_tick);
            torrent
                .peer_disconnect_history
                .push(torrent.peers_disconnected_this_tick);
            torrent.peers_discovered_this_tick = 0;
            torrent.peers_connected_this_tick = 0;
            torrent.peers_disconnected_this_tick = 0;
            if torrent.peer_discovery_history.len() > 200 {
                torrent.peer_discovery_history.remove(0);
                torrent.peer_connection_history.remove(0);
                torrent.peer_disconnect_history.remove(0);
            }

            torrent
                .latest_state
                .blocks_in_history
                .push(torrent.latest_state.blocks_in_this_tick);
            torrent
                .latest_state
                .blocks_out_history
                .push(torrent.latest_state.blocks_out_this_tick);
            torrent.latest_state.blocks_in_this_tick = 0;
            torrent.latest_state.blocks_out_this_tick = 0;
            if torrent.latest_state.blocks_in_history.len() > 200 {
                torrent.latest_state.blocks_in_history.remove(0);
                torrent.latest_state.blocks_out_history.remove(0);
            }
        }

        app_state.disk_read_history.push(global_disk_read_bps);
        app_state.disk_write_history.push(global_disk_write_bps);
        if app_state.disk_read_history.len() > 60 {
            app_state.disk_read_history.remove(0);
            app_state.disk_write_history.remove(0);
        }

        app_state.avg_disk_read_bps = global_disk_read_bps;
        app_state.avg_disk_write_bps = global_disk_write_bps;
        app_state.avg_disk_write_completed_bps = app_state
            .bytes_written_completed_this_tick
            .saturating_mul(8);
        app_state.bytes_written_completed_this_tick = 0;
        app_state.recv_to_write_p95 =
            calculate_duration_p95(&app_state.recv_to_write_latency_samples);

        let mut total_dl = 0;
        let mut total_ul = 0;
        for torrent in app_state.torrents.values() {
            total_dl += torrent.smoothed_download_speed_bps;
            total_ul += torrent.smoothed_upload_speed_bps;
        }

        app_state.total_download_history.push(total_dl);
        app_state.total_upload_history.push(total_ul);
        app_state.avg_download_history.push(total_dl);
        app_state.avg_upload_history.push(total_ul);

        app_state.read_iops = app_state.reads_completed_this_tick;
        app_state.write_iops = app_state.writes_completed_this_tick;
        app_state.reads_completed_this_tick = 0;
        app_state.writes_completed_this_tick = 0;

        app_state
            .disk_backoff_history_ms
            .push_back(app_state.max_disk_backoff_this_tick_ms);
        if app_state.disk_backoff_history_ms.len() > SECONDS_HISTORY_MAX {
            app_state.disk_backoff_history_ms.pop_front();
        }

        let run_time = app_state.run_time;
        if run_time > 0 && run_time.is_multiple_of(60) {
            let history_len = app_state.disk_backoff_history_ms.len();
            let start_index = history_len.saturating_sub(60);

            let backoff_slice_ms =
                &app_state.disk_backoff_history_ms.make_contiguous()[start_index..];
            let max_backoff_in_minute_ms = backoff_slice_ms.iter().max().copied().unwrap_or(0);
            app_state
                .minute_disk_backoff_history_ms
                .push_back(max_backoff_in_minute_ms);
            if app_state.minute_disk_backoff_history_ms.len() > MINUTES_HISTORY_MAX {
                app_state.minute_disk_backoff_history_ms.pop_front();
            }

            let seconds_dl = &app_state.avg_download_history;
            let minute_slice_dl = &seconds_dl[seconds_dl.len().saturating_sub(60)..];
            if !minute_slice_dl.is_empty() {
                let minute_avg_dl =
                    minute_slice_dl.iter().sum::<u64>() / minute_slice_dl.len() as u64;
                app_state.minute_avg_dl_history.push(minute_avg_dl);
            }

            let seconds_ul = &app_state.avg_upload_history;
            let minute_slice_ul = &seconds_ul[seconds_ul.len().saturating_sub(60)..];
            if !minute_slice_ul.is_empty() {
                let minute_avg_ul =
                    minute_slice_ul.iter().sum::<u64>() / minute_slice_ul.len() as u64;
                app_state.minute_avg_ul_history.push(minute_avg_ul);
            }
        }
        update_disk_health_state(app_state);
        app_state.max_disk_backoff_this_tick_ms = 0;

        if app_state.avg_download_history.len() > SECONDS_HISTORY_MAX {
            app_state.avg_download_history.remove(0);
            app_state.avg_upload_history.remove(0);
        }
        if app_state.minute_avg_dl_history.len() > MINUTES_HISTORY_MAX {
            app_state.minute_avg_dl_history.remove(0);
            app_state.minute_avg_ul_history.remove(0);
        }

        let is_leeching = app_state.torrents.values().any(|t| {
            t.latest_state.number_of_pieces_completed < t.latest_state.number_of_pieces_total
        });
        let is_seeding = !is_leeching;

        if is_seeding != app_state.is_seeding {
            tracing_event!(
                Level::DEBUG,
                "Self-Tune: Objective changed to {}.",
                if is_seeding { "Seeding" } else { "Leeching" }
            );

            if is_seeding {
                if !app_state.peer_sort_pinned {
                    app_state.peer_sort = (PeerSortColumn::UL, SortDirection::Descending);
                }
            } else {
                if !app_state.peer_sort_pinned {
                    app_state.peer_sort = (PeerSortColumn::DL, SortDirection::Descending);
                }
            }
        }
        app_state.is_seeding = is_seeding;
    }
}

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn prune_stale_recent_file_activity(
    display_state: &mut crate::app::TorrentDisplayState,
    now: Instant,
) {
    display_state.recent_file_activity.retain(|_, activity| {
        if activity.download_at.is_some_and(|seen_at| {
            now.saturating_duration_since(seen_at) > RECENT_FILE_ACTIVITY_RETENTION
        }) {
            activity.download_at = None;
        }
        if activity.upload_at.is_some_and(|seen_at| {
            now.saturating_duration_since(seen_at) > RECENT_FILE_ACTIVITY_RETENTION
        }) {
            activity.upload_at = None;
        }

        activity.download_at.is_some() || activity.upload_at.is_some()
    });
}

fn compute_disk_health_raw(app_state: &AppState) -> f64 {
    if !has_current_disk_health_signal(app_state) {
        return 0.0;
    }

    let net_total_bps = app_state.avg_download_history.last().copied().unwrap_or(0)
        + app_state.avg_upload_history.last().copied().unwrap_or(0);
    let disk_total_bps = app_state.avg_disk_read_bps + app_state.avg_disk_write_bps;
    let throughput_gap = if net_total_bps == 0 {
        0.0
    } else {
        ((net_total_bps.saturating_sub(disk_total_bps)) as f64 / net_total_bps as f64)
            .clamp(0.0, 1.0)
    };

    let thrash_ratio = app_state.global_disk_thrash_score / app_state.adaptive_max_scpb.max(1.0);
    let thrash_norm = (thrash_ratio.min(2.0) / 2.0).clamp(0.0, 1.0);

    let latency_ms = app_state
        .avg_disk_read_latency
        .max(app_state.avg_disk_write_latency)
        .as_millis() as f64;
    let latency_norm = ((latency_ms - 2.0) / (25.0 - 2.0)).clamp(0.0, 1.0);

    let backoff_norm = (app_state.max_disk_backoff_this_tick_ms as f64 / 200.0).clamp(0.0, 1.0);

    (0.45 * throughput_gap + 0.25 * thrash_norm + 0.20 * latency_norm + 0.10 * backoff_norm)
        .clamp(0.0, 1.0)
}

fn compute_disk_state_score(app_state: &AppState) -> f64 {
    if !has_current_disk_health_signal(app_state) {
        return 0.0;
    }

    let net_total_bps = app_state.avg_download_history.last().copied().unwrap_or(0)
        + app_state.avg_upload_history.last().copied().unwrap_or(0);
    let disk_total_bps = app_state.avg_disk_read_bps + app_state.avg_disk_write_bps;
    let throughput_gap = if net_total_bps == 0 {
        0.0
    } else {
        ((net_total_bps.saturating_sub(disk_total_bps)) as f64 / net_total_bps as f64)
            .clamp(0.0, 1.0)
    };
    let thrash_norm = ((app_state.global_disk_thrash_score / app_state.adaptive_max_scpb.max(1.0))
        .min(2.0)
        / 2.0)
        .clamp(0.0, 1.0);
    let latency_ms = app_state
        .avg_disk_read_latency
        .max(app_state.avg_disk_write_latency)
        .as_millis() as f64;
    let latency_norm = ((latency_ms - 2.0) / (25.0 - 2.0)).clamp(0.0, 1.0);
    let backoff_norm = (app_state.max_disk_backoff_this_tick_ms as f64 / 200.0).clamp(0.0, 1.0);

    let mut score =
        (0.40 * throughput_gap + 0.25 * thrash_norm + 0.20 * latency_norm + 0.15 * backoff_norm)
            .clamp(0.0, 1.0);

    if backoff_norm > 0.8 {
        score = score.max(0.70);
    }
    if thrash_norm > 0.9 && throughput_gap > 0.5 {
        score = score.max(0.80);
    }
    score
}

fn has_current_disk_health_signal(app_state: &AppState) -> bool {
    app_state.avg_disk_read_bps > 0
        || app_state.avg_disk_write_bps > 0
        || app_state.read_iops > 0
        || app_state.write_iops > 0
        || app_state.max_disk_backoff_this_tick_ms > 0
}

fn update_disk_health_state_level(app_state: &mut AppState) {
    let score = compute_disk_state_score(app_state);
    let mut level = app_state.disk_health_state_level.min(3);
    const ENTER: [f64; 3] = [0.20, 0.60, 0.80];
    const HYSTERESIS: f64 = 0.06;

    while level < 3 && score >= ENTER[level as usize] + HYSTERESIS {
        level += 1;
    }
    while level > 0 && score < ENTER[(level - 1) as usize] - HYSTERESIS {
        level -= 1;
    }
    app_state.disk_health_state_level = level;
}

fn update_disk_health_state(app_state: &mut AppState) {
    let raw = compute_disk_health_raw(app_state);
    let prev_ema = app_state.disk_health_ema;
    app_state.disk_health_ema = (0.25 * raw + 0.75 * prev_ema).clamp(0.0, 1.0);

    const PEAK_DECAY_PER_SEC: f64 = 0.04;
    app_state.disk_health_peak_hold = if app_state.disk_health_ema > app_state.disk_health_peak_hold
    {
        app_state.disk_health_ema
    } else {
        (app_state.disk_health_peak_hold - PEAK_DECAY_PER_SEC)
            .max(app_state.disk_health_ema)
            .max(0.0)
    };
    update_disk_health_state_level(app_state);
}

fn calculate_thrash_score(history_log: &VecDeque<DiskIoOperation>) -> u64 {
    if history_log.len() < 2 {
        return 0;
    }

    let mut total_seek_distance = 0;
    let mut last_offset_end: Option<u64> = None;

    for op in history_log.iter().rev() {
        if let Some(prev_offset_end) = last_offset_end {
            total_seek_distance += op.offset.abs_diff(prev_offset_end);
        }
        last_offset_end = Some(op.offset + op.length as u64);
    }

    let seek_count = history_log.len() - 1;
    total_seek_distance / seek_count as u64
}

fn calculate_duration_p95(samples: &VecDeque<Duration>) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }

    let mut sorted: Vec<Duration> = samples.iter().copied().collect();
    sorted.sort();
    let percentile_index =
        percentile_index_nearest_rank(sorted.len(), 95).expect("non-empty samples");
    sorted[percentile_index]
}

fn percentile_index_nearest_rank(len: usize, percentile: usize) -> Option<usize> {
    if len == 0 || percentile == 0 {
        return None;
    }

    let rank = len.saturating_mul(percentile).saturating_add(99) / 100;
    Some(rank.saturating_sub(1).min(len - 1))
}

fn calculate_thrash_score_seek_cost_f64(history_log: &VecDeque<DiskIoOperation>) -> f64 {
    if history_log.len() < 2 {
        return 0.0;
    }

    let mut total_seek_distance = 0;
    let mut total_bytes_transferred = 0;
    let mut last_offset_end: Option<u64> = None;

    for op in history_log.iter().rev() {
        if let Some(prev_offset_end) = last_offset_end {
            total_seek_distance += op.offset.abs_diff(prev_offset_end);
        }
        last_offset_end = Some(op.offset + op.length as u64);
        total_bytes_transferred += op.length as u64;
    }

    if total_bytes_transferred == 0 {
        return 0.0;
    }

    total_seek_distance as f64 / total_bytes_transferred as f64
}

fn aggregate_peers_to_availability(peers: &[PeerInfo], total_pieces: usize) -> Vec<u32> {
    if total_pieces == 0 {
        return Vec::new();
    }
    let mut availability: Vec<u32> = vec![0; total_pieces];
    for peer in peers {
        for (i, has_piece) in peer.bitfield.iter().enumerate().take(total_pieces) {
            if *has_piece {
                availability[i] += 1;
            }
        }
    }
    availability
}

#[cfg(test)]
mod tests {
    use super::{
        compute_disk_health_raw, update_disk_health_state, update_disk_health_state_level,
        SystemTelemetrySnapshot, UiTelemetry, RECENT_FILE_ACTIVITY_RETENTION,
    };
    use crate::app::torrent_manager_protocol::{
        DiskIoOperation, FileActivityDirection, FileActivityUpdate, ManagerEvent,
    };
    use crate::app::{
        AppMode, AppState, PeerInfo, RecentFileActivity, TorrentDisplayState, TorrentMetrics,
    };
    use crate::config::{PeerSortColumn, SortDirection, TorrentSortColumn};
    use crate::telemetry::manager_telemetry::ManagerTelemetry;
    use std::collections::{HashMap, VecDeque};
    use std::time::{Duration, Instant};

    fn run_second_tick(app_state: &mut AppState) {
        let snapshot = SystemTelemetrySnapshot {
            cpu_usage: app_state.cpu_usage,
            ram_usage_percent: app_state.ram_usage_percent,
            app_ram_usage: app_state.app_ram_usage,
            run_time: app_state.run_time,
        };
        UiTelemetry::on_second_tick_with_system_snapshot(app_state, Some(snapshot));
    }

    #[test]
    fn second_tick_applies_platform_system_snapshot() {
        let mut app_state = AppState::default();

        UiTelemetry::on_second_tick_with_system_snapshot(
            &mut app_state,
            Some(SystemTelemetrySnapshot {
                cpu_usage: 12.5,
                ram_usage_percent: 34.5,
                app_ram_usage: 56_789,
                run_time: 123,
            }),
        );

        assert_eq!(app_state.cpu_usage, 12.5);
        assert_eq!(app_state.ram_usage_percent, 34.5);
        assert_eq!(app_state.app_ram_usage, 56_789);
        assert_eq!(app_state.run_time, 123);
    }

    #[test]
    fn power_saving_tick_defers_platform_system_snapshot() {
        let mut app_state = AppState {
            mode: AppMode::PowerSaving,
            run_time: 1,
            ..Default::default()
        };

        UiTelemetry::on_second_tick_with_system_snapshot(
            &mut app_state,
            Some(SystemTelemetrySnapshot {
                cpu_usage: 12.5,
                ram_usage_percent: 34.5,
                app_ram_usage: 56_789,
                run_time: 123,
            }),
        );

        assert_eq!(app_state.cpu_usage, 0.0);
        assert_eq!(app_state.ram_usage_percent, 0.0);
        assert_eq!(app_state.app_ram_usage, 0);
        assert_eq!(app_state.run_time, 2);
    }

    #[test]
    fn power_saving_aggregates_each_second_and_does_not_spike_on_resume() {
        let mut app_state = AppState {
            mode: AppMode::PowerSaving,
            run_time: 1,
            ..Default::default()
        };
        let hash = vec![7; 20];
        app_state.torrents.insert(hash.clone(), Default::default());

        for second in 1..=7 {
            if second == 7 {
                app_state.mode = AppMode::Normal;
            }
            let torrent = app_state.torrents.get_mut(&hash).unwrap();
            torrent.bytes_read_this_tick += 128;
            torrent.bytes_written_this_tick += 256;
            app_state.reads_completed_this_tick += 2;
            app_state.writes_completed_this_tick += 3;
            // No snapshot also covers native ticks that skip system sampling.
            UiTelemetry::on_second_tick_with_system_snapshot(&mut app_state, None);
            assert_eq!(app_state.avg_disk_read_bps, 128 * 8);
            assert_eq!(app_state.avg_disk_write_bps, 256 * 8);
            assert_eq!(app_state.read_iops, 2);
            assert_eq!(app_state.write_iops, 3);
            assert_eq!(app_state.disk_read_history.len(), second);
        }
    }

    #[test]
    fn on_metrics_updates_totals_and_histories() {
        let mut app_state = AppState::default();

        let mut message = TorrentMetrics {
            info_hash: vec![7; 20],
            torrent_name: "test".to_string(),
            file_count: Some(3),
            file_verified_bytes: vec![Some(4), Some(1), None],
            number_of_pieces_total: 10,
            number_of_pieces_completed: 3,
            download_speed_bps: 512,
            upload_speed_bps: 128,
            number_of_successfully_connected_peers: 5,
            tcp_peer_count: 3,
            utp_peer_count: 2,
            beneficial_tcp_peer_count: 2,
            beneficial_utp_peer_count: 1,
            session_total_downloaded: 64,
            session_total_uploaded: 16,
            activity_message: "Downloading".to_string(),
            ..Default::default()
        };
        message.peers = vec![PeerInfo {
            bitfield: vec![true, false, true],
            ..Default::default()
        }];

        UiTelemetry::on_metrics(&mut app_state, message);

        assert_eq!(app_state.session_total_downloaded, 64);
        assert_eq!(app_state.session_total_uploaded, 16);

        let state = app_state.torrents.get(&vec![7; 20]).unwrap();
        assert_eq!(state.latest_state.info_hash, vec![7; 20]);
        assert_eq!(state.latest_state.file_count, Some(3));
        assert_eq!(
            state.latest_state.file_verified_bytes,
            vec![Some(4), Some(1), None]
        );
        assert_eq!(state.latest_state.download_speed_bps, 512);
        assert_eq!(state.latest_state.upload_speed_bps, 128);
        assert_eq!(state.latest_state.number_of_successfully_connected_peers, 5);
        assert_eq!(state.latest_state.tcp_peer_count, 3);
        assert_eq!(state.latest_state.utp_peer_count, 2);
        assert_eq!(state.latest_state.beneficial_tcp_peer_count, 2);
        assert_eq!(state.latest_state.beneficial_utp_peer_count, 1);
        assert_eq!(state.download_history.len(), 1);
        assert_eq!(state.upload_history.len(), 1);
        assert_eq!(state.swarm_availability_history.len(), 1);
    }

    #[test]
    fn on_metrics_keeps_departed_peer_evidence_out_of_ui_state() {
        let mut app_state = AppState::default();
        let active = PeerInfo {
            address: "192.0.2.10:6881".to_string(),
            ..Default::default()
        };
        let departed = PeerInfo {
            address: "192.0.2.11:6881".to_string(),
            last_action: "Disconnected".to_string(),
            ..Default::default()
        };
        let message = TorrentMetrics {
            info_hash: vec![0x37; 20],
            peers: vec![active.clone()],
            departed_peers: vec![departed],
            ..Default::default()
        };

        UiTelemetry::on_metrics(&mut app_state, message);

        assert_eq!(
            app_state.torrents[&vec![0x37; 20]].latest_state.peers,
            vec![active]
        );
    }

    #[test]
    fn on_metrics_applies_recent_file_activity_updates() {
        let mut app_state = AppState::default();

        let message = TorrentMetrics {
            info_hash: vec![8; 20],
            torrent_name: "test".to_string(),
            file_activity_updates: vec![
                FileActivityUpdate {
                    touched_relative_paths: vec!["alpha.bin".to_string()],
                    direction: FileActivityDirection::Download,
                },
                FileActivityUpdate {
                    touched_relative_paths: vec!["beta.bin".to_string()],
                    direction: FileActivityDirection::Upload,
                },
            ],
            ..Default::default()
        };

        UiTelemetry::on_metrics(&mut app_state, message);

        let state = app_state.torrents.get(&vec![8; 20]).unwrap();
        assert!(state
            .recent_file_activity
            .get("alpha.bin")
            .and_then(|activity| activity.download_at)
            .is_some());
        assert!(state
            .recent_file_activity
            .get("beta.bin")
            .and_then(|activity| activity.upload_at)
            .is_some());
    }

    #[test]
    fn on_metrics_prunes_stale_recent_file_activity_entries() {
        let mut app_state = AppState::default();
        let now = Instant::now();
        app_state.torrents.insert(
            vec![8; 20],
            TorrentDisplayState {
                recent_file_activity: HashMap::from([
                    (
                        "stale.bin".to_string(),
                        RecentFileActivity {
                            download_at: Some(
                                now - RECENT_FILE_ACTIVITY_RETENTION - Duration::from_secs(1),
                            ),
                            upload_at: None,
                        },
                    ),
                    (
                        "fresh.bin".to_string(),
                        RecentFileActivity {
                            download_at: None,
                            upload_at: Some(now - Duration::from_secs(1)),
                        },
                    ),
                ]),
                ..TorrentDisplayState::default()
            },
        );

        let message = TorrentMetrics {
            info_hash: vec![8; 20],
            torrent_name: "test".to_string(),
            file_activity_updates: vec![FileActivityUpdate {
                touched_relative_paths: vec!["new.bin".to_string()],
                direction: FileActivityDirection::Download,
            }],
            ..Default::default()
        };

        UiTelemetry::on_metrics(&mut app_state, message);

        let state = app_state.torrents.get(&vec![8; 20]).unwrap();
        assert!(!state.recent_file_activity.contains_key("stale.bin"));
        assert!(state.recent_file_activity.contains_key("fresh.bin"));
        assert!(state
            .recent_file_activity
            .get("new.bin")
            .and_then(|activity| activity.download_at)
            .is_some());
    }

    #[test]
    fn on_manager_event_metrics_counts_peer_and_blocks() {
        use crate::app::torrent_manager_protocol::ManagerEvent;

        let info_hash = vec![1; 20];
        let mut app_state = AppState {
            torrents: HashMap::from([(info_hash.clone(), TorrentDisplayState::default())]),
            ..Default::default()
        };

        assert!(UiTelemetry::on_manager_event_metrics(
            &mut app_state,
            &ManagerEvent::PeerDiscovered {
                info_hash: info_hash.clone()
            }
        ));
        assert!(UiTelemetry::on_manager_event_metrics(
            &mut app_state,
            &ManagerEvent::BlockReceived {
                info_hash: info_hash.clone(),
            }
        ));
        assert!(UiTelemetry::on_manager_event_metrics(
            &mut app_state,
            &ManagerEvent::BlockSent {
                info_hash: info_hash.clone()
            }
        ));

        let state = app_state.torrents.get(&info_hash).unwrap();
        assert_eq!(state.peers_discovered_this_tick, 1);
        assert_eq!(state.latest_state.blocks_in_this_tick, 1);
        assert_eq!(state.latest_state.blocks_out_this_tick, 1);
    }

    #[test]
    fn on_metrics_does_not_add_availability_without_peers() {
        let mut app_state = AppState::default();
        let message = TorrentMetrics {
            info_hash: vec![2; 20],
            torrent_name: "test".to_string(),
            number_of_pieces_total: 10,
            number_of_pieces_completed: 3,
            eta: Duration::from_secs(10),
            ..Default::default()
        };

        UiTelemetry::on_metrics(&mut app_state, message);

        let state = app_state.torrents.get(&vec![2; 20]).unwrap();
        assert!(state.swarm_availability_history.is_empty());
    }

    #[test]
    fn sparse_delivery_keeps_session_totals_correct_with_nonzero_ticks() {
        let mut app_state = AppState::default();
        let mut manager_telemetry = ManagerTelemetry::default();

        let base = TorrentMetrics {
            info_hash: vec![9; 20],
            torrent_name: "sparse-test".to_string(),
            number_of_pieces_total: 10,
            number_of_pieces_completed: 2,
            download_speed_bps: 1024,
            upload_speed_bps: 128,
            activity_message: "Downloading".to_string(),
            ..Default::default()
        };

        // First idle snapshot should emit once.
        assert!(manager_telemetry.should_emit(&base));
        UiTelemetry::on_metrics(&mut app_state, base.clone());
        assert!(!manager_telemetry.should_emit(&base));

        // Nonzero byte ticks must emit even if all other fields are unchanged.
        let mut tick_a = base.clone();
        tick_a.bytes_downloaded_this_tick = 64;
        tick_a.session_total_downloaded = 64;
        assert!(manager_telemetry.should_emit(&tick_a));
        UiTelemetry::on_metrics(&mut app_state, tick_a);

        let mut tick_b = base.clone();
        tick_b.bytes_downloaded_this_tick = 64;
        tick_b.session_total_downloaded = 128;
        assert!(manager_telemetry.should_emit(&tick_b));
        UiTelemetry::on_metrics(&mut app_state, tick_b);

        assert_eq!(app_state.session_total_downloaded, 128);
    }

    #[test]
    fn batched_activity_matches_native_events_through_second_tick() {
        let hash = vec![0x72; 20];
        let mut native = AppState::default();
        let mut batched = AppState::default();
        for state in [&mut native, &mut batched] {
            state
                .torrents
                .insert(hash.clone(), TorrentDisplayState::default());
        }
        let samples = (0..120)
            .map(|piece_index| DiskIoOperation {
                piece_index,
                offset: piece_index as u64 * 128,
                length: 128,
            })
            .collect::<Vec<_>>();
        for op in &samples {
            for event in [
                ManagerEvent::PeerDiscovered {
                    info_hash: hash.clone(),
                },
                ManagerEvent::PeerConnected {
                    info_hash: hash.clone(),
                },
                ManagerEvent::PeerDisconnected {
                    info_hash: hash.clone(),
                },
                ManagerEvent::BlockReceived {
                    info_hash: hash.clone(),
                },
                ManagerEvent::BlockSent {
                    info_hash: hash.clone(),
                },
                ManagerEvent::DiskReadStarted {
                    info_hash: hash.clone(),
                    op: *op,
                },
                ManagerEvent::DiskReadFinished,
                ManagerEvent::DiskWriteStarted {
                    info_hash: hash.clone(),
                    op: *op,
                },
                ManagerEvent::DiskWriteCompleted {
                    info_hash: hash.clone(),
                    op: *op,
                },
                ManagerEvent::DiskWriteFinished {
                    info_hash: hash.clone(),
                    piece_index: op.piece_index,
                },
                ManagerEvent::DiskIoBackoff {
                    duration: Duration::from_millis(20),
                },
            ] {
                assert!(UiTelemetry::on_manager_event_metrics(&mut native, &event));
            }
        }
        UiTelemetry::record_peer_activity(&mut batched, &hash, 120, 120, 120, 120, 120);
        UiTelemetry::record_disk_reads(&mut batched, &hash, 120 * 128, &samples);
        UiTelemetry::record_disk_writes(&mut batched, &hash, 120 * 128, &samples);
        UiTelemetry::record_reads_finished(&mut batched, 120, None);
        UiTelemetry::record_writes_finished(&mut batched, 120, None);
        UiTelemetry::record_writes_completed(&mut batched, 120 * 128, None, 120);
        UiTelemetry::record_disk_backoff(&mut batched, Duration::from_millis(20));
        assert_eq!(native.system_warning, batched.system_warning);
        for state in [&mut native, &mut batched] {
            assert_eq!(state.global_disk_read_history_log.len(), 100);
            assert_eq!(state.global_disk_write_history_log.len(), 100);
            assert_eq!(state.torrents[&hash].disk_read_history_log.len(), 50);
            assert_eq!(state.torrents[&hash].disk_write_history_log.len(), 50);
        }
        for _ in 0..2 {
            run_second_tick(&mut native);
            run_second_tick(&mut batched);
            let a = &native.torrents[&hash];
            let b = &batched.torrents[&hash];
            assert_eq!(a.peer_discovery_history, b.peer_discovery_history);
            assert_eq!(a.peer_connection_history, b.peer_connection_history);
            assert_eq!(a.peer_disconnect_history, b.peer_disconnect_history);
            assert_eq!(
                a.latest_state.blocks_in_history,
                b.latest_state.blocks_in_history
            );
            assert_eq!(
                a.latest_state.blocks_out_history,
                b.latest_state.blocks_out_history
            );
            assert_eq!(native.avg_disk_read_bps, batched.avg_disk_read_bps);
            assert_eq!(native.avg_disk_write_bps, batched.avg_disk_write_bps);
            assert_eq!(
                native.avg_disk_write_completed_bps,
                batched.avg_disk_write_completed_bps
            );
            assert_eq!(native.read_iops, batched.read_iops);
            assert_eq!(native.write_iops, batched.write_iops);
            assert_eq!(a.disk_read_thrash_score, b.disk_read_thrash_score);
            assert_eq!(a.disk_write_thrash_score, b.disk_write_thrash_score);
        }
    }

    #[test]
    fn latency_batches_match_individual_observations_including_window_eviction() {
        let mut individual = AppState::default();
        let mut batched = AppState::default();
        for (count, micros) in [(1, 100), (6, 500), (1, 0), (2_000, 200), (3, 800)] {
            let latency = Duration::from_micros(micros);
            for _ in 0..count {
                UiTelemetry::record_reads_finished(&mut individual, 1, Some(latency));
                UiTelemetry::record_writes_finished(&mut individual, 1, Some(latency));
                UiTelemetry::record_writes_completed(&mut individual, 128, Some(latency), 1);
            }
            UiTelemetry::record_reads_finished(&mut batched, count, Some(latency));
            UiTelemetry::record_writes_finished(&mut batched, count, Some(latency));
            UiTelemetry::record_writes_completed(
                &mut batched,
                128 * count as u64,
                Some(latency),
                count,
            );
            assert_eq!(individual.read_latency_ema, batched.read_latency_ema);
            assert_eq!(individual.write_latency_ema, batched.write_latency_ema);
            assert_eq!(
                individual.avg_disk_read_latency,
                batched.avg_disk_read_latency
            );
            assert_eq!(
                individual.avg_disk_write_latency,
                batched.avg_disk_write_latency
            );
            assert_eq!(
                individual.recv_to_write_latency_samples,
                batched.recv_to_write_latency_samples
            );
            assert_eq!(
                individual.bytes_written_completed_this_tick,
                batched.bytes_written_completed_this_tick
            );
        }
        assert_eq!(batched.recv_to_write_latency_samples.len(), 1_024);
        run_second_tick(&mut individual);
        run_second_tick(&mut batched);
        assert_eq!(individual.recv_to_write_p95, batched.recv_to_write_p95);
    }

    #[test]
    fn native_disk_events_preserve_start_finish_and_completion_accounting() {
        let hash = vec![0x71; 20];
        let mut state = AppState::default();
        state
            .torrents
            .insert(hash.clone(), TorrentDisplayState::default());
        for piece_index in 0..105 {
            let op = DiskIoOperation {
                piece_index,
                offset: 0,
                length: 128,
            };
            for event in [
                ManagerEvent::DiskReadStarted {
                    info_hash: hash.clone(),
                    op,
                },
                ManagerEvent::DiskWriteStarted {
                    info_hash: hash.clone(),
                    op,
                },
            ] {
                UiTelemetry::on_manager_event_metrics(&mut state, &event);
            }
        }
        assert_eq!(state.torrents[&hash].bytes_read_this_tick, 105 * 128);
        assert_eq!(state.torrents[&hash].bytes_written_this_tick, 105 * 128);
        assert_eq!(state.torrents[&hash].disk_read_history_log.len(), 50);
        assert_eq!(state.global_disk_write_history_log.len(), 100);
        assert_eq!(
            state
                .global_disk_read_history_log
                .front()
                .unwrap()
                .piece_index,
            104
        );
        assert_eq!(state.bytes_written_completed_this_tick, 0);
        assert_eq!(state.writes_completed_this_tick, 0);

        UiTelemetry::on_manager_event_metrics(
            &mut state,
            &ManagerEvent::DiskWriteCompleted {
                info_hash: hash.clone(),
                op: DiskIoOperation {
                    piece_index: 104,
                    offset: 0,
                    length: 128,
                },
            },
        );
        assert_eq!(state.bytes_written_completed_this_tick, 128);
        assert_eq!(state.writes_completed_this_tick, 0);
        assert_eq!(state.recv_to_write_latency_samples.len(), 1);
        UiTelemetry::on_manager_event_metrics(
            &mut state,
            &ManagerEvent::DiskWriteFinished {
                info_hash: hash,
                piece_index: 104,
            },
        );
        assert_eq!(state.writes_completed_this_tick, 1);
        assert_eq!(state.recv_to_write_latency_samples.len(), 1);
    }

    #[test]
    fn native_latency_uses_ten_sample_ema_and_preserves_existing_warning() {
        let mut state = AppState {
            read_latency_ema: 100.0,
            ..Default::default()
        };
        let start = Instant::now() - Duration::from_millis(2);
        state.read_op_start_times.push_front(start);
        let lower = start.elapsed().as_micros() as f64;
        UiTelemetry::on_manager_event_metrics(&mut state, &ManagerEvent::DiskReadFinished);
        let upper = start.elapsed().as_micros() as f64;
        let alpha = 2.0 / 11.0;
        assert!(state.read_latency_ema >= lower * alpha + 100.0 * (1.0 - alpha));
        assert!(state.read_latency_ema <= upper * alpha + 100.0 * (1.0 - alpha));
        assert_eq!(
            state.avg_disk_read_latency,
            Duration::from_micros(state.read_latency_ema as u64)
        );
        let previous = state.read_latency_ema;
        UiTelemetry::on_manager_event_metrics(&mut state, &ManagerEvent::DiskReadFinished);
        assert_eq!(state.reads_completed_this_tick, 2);
        assert_eq!(state.read_latency_ema, previous);

        state.system_warning = Some("Existing warning".into());
        for millis in [40, 10] {
            UiTelemetry::on_manager_event_metrics(
                &mut state,
                &ManagerEvent::DiskIoBackoff {
                    duration: Duration::from_millis(millis),
                },
            );
        }
        assert_eq!(state.max_disk_backoff_this_tick_ms, 40);
        assert_eq!(state.system_warning.as_deref(), Some("Existing warning"));
    }

    #[test]
    fn disk_speed_uses_current_tick_and_returns_to_zero_when_idle() {
        let mut app_state = AppState::default();
        let torrent = TorrentDisplayState {
            bytes_read_this_tick: 1_024,
            bytes_written_this_tick: 2_048,
            ..TorrentDisplayState::default()
        };
        app_state.torrents.insert(vec![3; 20], torrent);

        run_second_tick(&mut app_state);

        assert_eq!(app_state.avg_disk_read_bps, 8_192);
        assert_eq!(app_state.avg_disk_write_bps, 16_384);

        run_second_tick(&mut app_state);

        assert_eq!(app_state.avg_disk_read_bps, 0);
        assert_eq!(app_state.avg_disk_write_bps, 0);
    }

    #[test]
    fn second_tick_commits_peer_block_and_disk_event_accumulators() {
        let info_hash = vec![0x2a; 20];
        let mut app_state = AppState {
            torrents: HashMap::from([(info_hash.clone(), TorrentDisplayState::default())]),
            ..Default::default()
        };

        for event in [
            ManagerEvent::PeerDiscovered {
                info_hash: info_hash.clone(),
            },
            ManagerEvent::PeerConnected {
                info_hash: info_hash.clone(),
            },
            ManagerEvent::PeerDisconnected {
                info_hash: info_hash.clone(),
            },
            ManagerEvent::BlockReceived {
                info_hash: info_hash.clone(),
            },
            ManagerEvent::BlockSent {
                info_hash: info_hash.clone(),
            },
            ManagerEvent::DiskWriteCompleted {
                info_hash: info_hash.clone(),
                op: DiskIoOperation {
                    piece_index: 3,
                    offset: 16_384,
                    length: 2_048,
                },
            },
        ] {
            assert!(UiTelemetry::on_manager_event_metrics(
                &mut app_state,
                &event
            ));
        }

        run_second_tick(&mut app_state);

        let torrent = &app_state.torrents[&info_hash];
        assert_eq!(torrent.peer_discovery_history, vec![1]);
        assert_eq!(torrent.peer_connection_history, vec![1]);
        assert_eq!(torrent.peer_disconnect_history, vec![1]);
        assert_eq!(torrent.latest_state.blocks_in_history, vec![1]);
        assert_eq!(torrent.latest_state.blocks_out_history, vec![1]);
        assert_eq!(torrent.peers_discovered_this_tick, 0);
        assert_eq!(torrent.peers_connected_this_tick, 0);
        assert_eq!(torrent.peers_disconnected_this_tick, 0);
        assert_eq!(torrent.latest_state.blocks_in_this_tick, 0);
        assert_eq!(torrent.latest_state.blocks_out_this_tick, 0);
        assert_eq!(app_state.avg_disk_write_completed_bps, 16_384);
    }

    #[test]
    fn disk_write_completed_speed_uses_completed_write_events() {
        let mut app_state = AppState::default();
        let info_hash = vec![4; 20];
        assert!(UiTelemetry::on_manager_event_metrics(
            &mut app_state,
            &ManagerEvent::DiskWriteCompleted {
                info_hash: info_hash.clone(),
                op: DiskIoOperation {
                    piece_index: 0,
                    offset: 0,
                    length: 2_048,
                }
            }
        ));

        run_second_tick(&mut app_state);

        assert_eq!(app_state.avg_disk_write_completed_bps, 16_384);

        run_second_tick(&mut app_state);

        assert_eq!(app_state.avg_disk_write_completed_bps, 0);
    }

    #[test]
    fn recv_to_write_p95_uses_write_start_to_completed_write_latency() {
        let mut app_state = AppState::default();
        let info_hash = vec![5; 20];

        assert!(UiTelemetry::on_manager_event_metrics(
            &mut app_state,
            &ManagerEvent::BlockReceived {
                info_hash: info_hash.clone(),
            }
        ));
        assert!(
            app_state.pending_piece_write_start_times.is_empty(),
            "receiving a block should not start disk backpressure timing"
        );

        assert!(UiTelemetry::on_manager_event_metrics(
            &mut app_state,
            &ManagerEvent::DiskWriteStarted {
                info_hash: info_hash.clone(),
                op: DiskIoOperation {
                    piece_index: 7,
                    offset: 0,
                    length: 1_024,
                },
            }
        ));
        app_state.pending_piece_write_start_times.insert(
            (info_hash.clone(), 7),
            Instant::now() - Duration::from_secs(2),
        );

        assert!(UiTelemetry::on_manager_event_metrics(
            &mut app_state,
            &ManagerEvent::DiskWriteCompleted {
                info_hash,
                op: DiskIoOperation {
                    piece_index: 7,
                    offset: 0,
                    length: 1_024,
                },
            }
        ));

        run_second_tick(&mut app_state);

        assert!(app_state.pending_piece_write_start_times.is_empty());
        assert_eq!(app_state.recv_to_write_latency_samples.len(), 1);
        assert!(app_state.recv_to_write_p95 >= Duration::from_secs(2));
    }

    #[test]
    fn disk_write_finished_clears_pending_latency_start_without_completion() {
        let mut app_state = AppState::default();
        let info_hash = vec![6; 20];

        assert!(UiTelemetry::on_manager_event_metrics(
            &mut app_state,
            &ManagerEvent::DiskWriteStarted {
                info_hash: info_hash.clone(),
                op: DiskIoOperation {
                    piece_index: 9,
                    offset: 0,
                    length: 1_024,
                },
            }
        ));
        assert!(app_state
            .pending_piece_write_start_times
            .contains_key(&(info_hash.clone(), 9)));

        assert!(UiTelemetry::on_manager_event_metrics(
            &mut app_state,
            &ManagerEvent::DiskWriteFinished {
                info_hash: info_hash.clone(),
                piece_index: 9,
            }
        ));

        assert!(!app_state
            .pending_piece_write_start_times
            .contains_key(&(info_hash, 9)));
        assert!(app_state.recv_to_write_latency_samples.is_empty());
    }

    #[test]
    fn percentile_index_nearest_rank_uses_one_based_rank() {
        assert_eq!(super::percentile_index_nearest_rank(0, 95), None);
        assert_eq!(super::percentile_index_nearest_rank(1, 95), Some(0));
        assert_eq!(super::percentile_index_nearest_rank(20, 95), Some(18));
        assert_eq!(super::percentile_index_nearest_rank(100, 95), Some(94));
    }

    #[test]
    fn recv_to_write_p95_does_not_select_max_for_twenty_samples() {
        let samples = (1..=20).map(Duration::from_millis).collect::<VecDeque<_>>();

        assert_eq!(
            super::calculate_duration_p95(&samples),
            Duration::from_millis(19)
        );
    }

    #[test]
    fn disk_health_raw_is_near_zero_when_balanced_and_calm() {
        let app_state = AppState {
            avg_download_history: vec![40_000_000],
            avg_upload_history: vec![5_000_000],
            avg_disk_read_bps: 28_000_000,
            avg_disk_write_bps: 22_000_000,
            adaptive_max_scpb: 10.0,
            ..Default::default()
        };

        let raw = compute_disk_health_raw(&app_state);
        assert!(
            raw < 0.05,
            "expected near-zero disk health pressure for calm balanced flow, got {raw}"
        );
    }

    #[test]
    fn disk_health_raw_rises_with_throughput_gap() {
        let app_state = AppState {
            avg_download_history: vec![80_000_000],
            avg_upload_history: vec![20_000_000],
            avg_disk_read_bps: 10_000_000,
            avg_disk_write_bps: 10_000_000,
            adaptive_max_scpb: 10.0,
            ..Default::default()
        };

        let raw = compute_disk_health_raw(&app_state);
        assert!(
            raw > 0.30,
            "expected high pressure from throughput gap, got {raw}"
        );
    }

    #[test]
    fn disk_health_raw_rises_with_thrash_latency_and_backoff() {
        let app_state = AppState {
            avg_download_history: vec![50_000_000],
            avg_upload_history: vec![10_000_000],
            avg_disk_read_bps: 30_000_000,
            avg_disk_write_bps: 30_000_000,
            global_disk_thrash_score: 20.0,
            adaptive_max_scpb: 10.0,
            avg_disk_read_latency: Duration::from_millis(4),
            avg_disk_write_latency: Duration::from_millis(30),
            max_disk_backoff_this_tick_ms: 220,
            ..Default::default()
        };

        let raw = compute_disk_health_raw(&app_state);
        assert!(
            raw > 0.50,
            "expected high pressure from non-throughput factors, got {raw}"
        );
    }

    #[test]
    fn disk_health_state_ema_smooths_spikes() {
        let mut app_state = AppState {
            avg_download_history: vec![100_000_000],
            avg_upload_history: vec![0],
            avg_disk_read_bps: 10_000_000,
            avg_disk_write_bps: 10_000_000,
            adaptive_max_scpb: 10.0,
            ..Default::default()
        };

        let raw = compute_disk_health_raw(&app_state);
        update_disk_health_state(&mut app_state);

        assert!(
            app_state.disk_health_ema < raw,
            "EMA should smooth first spike: raw={raw}, ema={}",
            app_state.disk_health_ema
        );
        assert!(app_state.disk_health_peak_hold >= app_state.disk_health_ema);
    }

    #[test]
    fn disk_health_state_ignores_stale_pressure_when_disk_is_idle() {
        let mut app_state = AppState {
            disk_health_state_level: 1,
            disk_health_ema: 0.55,
            disk_health_peak_hold: 0.70,
            avg_download_history: vec![100_000_000],
            avg_upload_history: vec![20_000_000],
            avg_disk_read_bps: 0,
            avg_disk_write_bps: 0,
            global_disk_thrash_score: 18.0,
            adaptive_max_scpb: 10.0,
            avg_disk_write_latency: Duration::from_millis(20),
            max_disk_backoff_this_tick_ms: 0,
            ..Default::default()
        };

        assert_eq!(compute_disk_health_raw(&app_state), 0.0);
        update_disk_health_state(&mut app_state);

        assert_eq!(app_state.disk_health_state_level, 0);
        assert!(app_state.disk_health_ema < 0.55);
    }

    #[test]
    fn disk_health_state_level_uses_hysteresis() {
        let mut app_state = AppState {
            disk_health_state_level: 0,
            avg_download_history: vec![100_000_000],
            avg_upload_history: vec![20_000_000],
            avg_disk_read_bps: 20_000_000,
            avg_disk_write_bps: 20_000_000,
            global_disk_thrash_score: 18.0,
            adaptive_max_scpb: 10.0,
            avg_disk_write_latency: Duration::from_millis(20),
            max_disk_backoff_this_tick_ms: 120,
            ..Default::default()
        };
        update_disk_health_state_level(&mut app_state);
        assert!(app_state.disk_health_state_level >= 2);

        app_state.avg_disk_read_bps = 55_000_000;
        app_state.avg_disk_write_bps = 55_000_000;
        app_state.global_disk_thrash_score = 3.0;
        app_state.avg_disk_write_latency = Duration::from_millis(7);
        app_state.max_disk_backoff_this_tick_ms = 10;
        let before = app_state.disk_health_state_level;
        update_disk_health_state_level(&mut app_state);
        assert!(app_state.disk_health_state_level <= before);
    }

    #[test]
    fn leeching_objective_switch_updates_peer_sort_without_changing_torrent_sort() {
        let mut app_state = AppState {
            is_seeding: true,
            torrent_sort: (TorrentSortColumn::Name, SortDirection::Ascending),
            ..Default::default()
        };

        let mut torrent = TorrentDisplayState::default();
        torrent.latest_state.number_of_pieces_total = 10;
        torrent.latest_state.number_of_pieces_completed = 9;
        app_state.torrents.insert(vec![1; 20], torrent);

        run_second_tick(&mut app_state);

        assert!(!app_state.is_seeding);
        assert_eq!(
            app_state.torrent_sort,
            (TorrentSortColumn::Name, SortDirection::Ascending)
        );
        assert_eq!(
            app_state.peer_sort,
            (PeerSortColumn::DL, SortDirection::Descending)
        );
    }

    #[test]
    fn objective_switch_preserves_user_pinned_sorting() {
        let mut app_state = AppState {
            is_seeding: true,
            torrent_sort: (TorrentSortColumn::Name, SortDirection::Ascending),
            torrent_sort_pinned: true,
            peer_sort: (PeerSortColumn::Address, SortDirection::Ascending),
            peer_sort_pinned: true,
            ..Default::default()
        };

        let mut torrent = TorrentDisplayState::default();
        torrent.latest_state.number_of_pieces_total = 10;
        torrent.latest_state.number_of_pieces_completed = 9;
        app_state.torrents.insert(vec![1; 20], torrent);

        run_second_tick(&mut app_state);

        assert!(!app_state.is_seeding);
        assert_eq!(
            app_state.torrent_sort,
            (TorrentSortColumn::Name, SortDirection::Ascending)
        );
        assert_eq!(
            app_state.peer_sort,
            (PeerSortColumn::Address, SortDirection::Ascending)
        );
    }
}
