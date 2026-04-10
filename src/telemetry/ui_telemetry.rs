// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{AppMode, AppState, PeerInfo, TorrentMetrics};
use crate::config::{PeerSortColumn, SortDirection, TorrentSortColumn};
use crate::torrent_manager::{DiskIoOperation, ManagerEvent};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use sysinfo::System;
use tracing::{event as tracing_event, Level};

pub const SECONDS_HISTORY_MAX: usize = 3600; // 1 hour of per-second data
pub const MINUTES_HISTORY_MAX: usize = 48 * 60; // 48 hours of per-minute data

pub struct UiTelemetry;

impl UiTelemetry {
    pub fn on_manager_event_metrics(app_state: &mut AppState, event: &ManagerEvent) -> bool {
        match event {
            ManagerEvent::DiskReadStarted { info_hash, op } => {
                app_state.read_op_start_times.push_front(Instant::now());
                app_state.global_disk_read_history_log.push_front(*op);
                app_state.global_disk_read_history_log.truncate(100);
                if let Some(torrent) = app_state.torrents.get_mut(info_hash) {
                    torrent.bytes_read_this_tick += op.length as u64;
                    torrent.disk_read_history_log.push_front(*op);
                    torrent.disk_read_history_log.truncate(50);
                }
                true
            }
            ManagerEvent::DiskReadFinished => {
                if let Some(start_time) = app_state.read_op_start_times.pop_front() {
                    let duration = start_time.elapsed();
                    const LATENCY_EMA_PERIOD: f64 = 10.0;
                    let alpha = 2.0 / (LATENCY_EMA_PERIOD + 1.0);
                    let current_micros = duration.as_micros() as f64;

                    let new_ema = if app_state.read_latency_ema == 0.0 {
                        current_micros
                    } else {
                        (current_micros * alpha) + (app_state.read_latency_ema * (1.0 - alpha))
                    };

                    app_state.read_latency_ema = new_ema;
                    app_state.avg_disk_read_latency = Duration::from_micros(new_ema as u64);
                }
                app_state.reads_completed_this_tick += 1;
                true
            }
            ManagerEvent::DiskWriteStarted { info_hash, op } => {
                app_state.write_op_start_times.push_front(Instant::now());
                app_state.global_disk_write_history_log.push_front(*op);
                app_state.global_disk_write_history_log.truncate(100);
                if let Some(torrent) = app_state.torrents.get_mut(info_hash) {
                    torrent.bytes_written_this_tick += op.length as u64;
                    torrent.disk_write_history_log.push_front(*op);
                    torrent.disk_write_history_log.truncate(50);
                }
                true
            }
            ManagerEvent::DiskWriteFinished => {
                if let Some(start_time) = app_state.write_op_start_times.pop_front() {
                    let duration = start_time.elapsed();
                    const LATENCY_EMA_PERIOD: f64 = 10.0;
                    let alpha = 2.0 / (LATENCY_EMA_PERIOD + 1.0);
                    let current_micros = duration.as_micros() as f64;

                    let new_ema = if app_state.write_latency_ema == 0.0 {
                        current_micros
                    } else {
                        (current_micros * alpha) + (app_state.write_latency_ema * (1.0 - alpha))
                    };

                    app_state.write_latency_ema = new_ema;
                    app_state.avg_disk_write_latency = Duration::from_micros(new_ema as u64);
                }
                app_state.writes_completed_this_tick += 1;
                true
            }
            ManagerEvent::DiskIoBackoff { duration } => {
                let duration_ms = duration.as_millis() as u64;
                app_state.max_disk_backoff_this_tick_ms =
                    app_state.max_disk_backoff_this_tick_ms.max(duration_ms);

                if app_state.system_warning.is_none() {
                    let warning_msg = "System Warning: Potential FD limit hit (detected via Disk I/O backoff). Increase 'ulimit -n' if issues persist.".to_string();
                    app_state.system_warning = Some(warning_msg);
                }
                true
            }
            ManagerEvent::PeerDiscovered {
                info_hash,
                candidate,
            } => {
                let _ = candidate;
                if let Some(torrent) = app_state.torrents.get_mut(info_hash) {
                    torrent.peers_discovered_this_tick += 1;
                }
                true
            }
            ManagerEvent::PeerConnectionFailed { .. } => true,
            ManagerEvent::TorrentPaused { .. } | ManagerEvent::TorrentResumed { .. } => true,
            ManagerEvent::PeerConnected {
                info_hash,
                peer_addr,
            } => {
                let _ = peer_addr;
                if let Some(torrent) = app_state.torrents.get_mut(info_hash) {
                    torrent.peers_connected_this_tick += 1;
                }
                true
            }
            ManagerEvent::PeerDisconnected {
                info_hash,
                peer_addr,
            } => {
                let _ = peer_addr;
                if let Some(torrent) = app_state.torrents.get_mut(info_hash) {
                    torrent.peers_disconnected_this_tick += 1;
                }
                true
            }
            ManagerEvent::BlockReceived { info_hash } => {
                if let Some(torrent) = app_state.torrents.get_mut(info_hash) {
                    torrent.latest_state.blocks_in_this_tick += 1;
                }
                true
            }
            ManagerEvent::BlockSent { info_hash } => {
                if let Some(torrent) = app_state.torrents.get_mut(info_hash) {
                    torrent.latest_state.blocks_out_this_tick += 1;
                }
                true
            }
            _ => false,
        }
    }

    pub fn on_metrics(app_state: &mut AppState, message: TorrentMetrics) {
        let display_state = app_state.torrents.entry(message.info_hash).or_default();
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

        display_state
            .latest_state
            .number_of_successfully_connected_peers =
            message.number_of_successfully_connected_peers;
        display_state.latest_state.number_of_pieces_total = message.number_of_pieces_total;
        display_state.latest_state.number_of_pieces_completed = message.number_of_pieces_completed;
        display_state.latest_state.download_speed_bps = message.download_speed_bps;
        display_state.latest_state.upload_speed_bps = message.upload_speed_bps;
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

    pub fn on_second_tick(app_state: &mut AppState, sys: &mut System) {
        if matches!(app_state.mode, AppMode::PowerSaving) && !app_state.run_time.is_multiple_of(5) {
            app_state.run_time += 1;
            return;
        }

        let pid = match sysinfo::get_current_pid() {
            Ok(pid) => pid,
            Err(e) => {
                tracing_event!(Level::ERROR, "Could not get current PID: {}", e);
                return;
            }
        };

        sys.refresh_cpu_usage();
        sys.refresh_memory();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);

        if let Some(process) = sys.process(pid) {
            app_state.cpu_usage = process.cpu_usage() / sys.cpus().len() as f32;
            app_state.app_ram_usage = process.memory();
            app_state.ram_usage_percent =
                (process.memory() as f32 / sys.total_memory() as f32) * 100.0;
            app_state.run_time = process.run_time();
        }

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
            let percentile_index = (sorted_history.len() as f64 * 0.95) as usize;
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
                app_state.torrent_sort = (TorrentSortColumn::Up, SortDirection::Descending);
                app_state.peer_sort = (PeerSortColumn::UL, SortDirection::Descending);
            } else {
                app_state.torrent_sort = (TorrentSortColumn::Down, SortDirection::Descending);
                app_state.peer_sort = (PeerSortColumn::DL, SortDirection::Descending);
            }
        }
        app_state.is_seeding = is_seeding;
    }
}

fn compute_disk_health_raw(app_state: &AppState) -> f64 {
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
        UiTelemetry,
    };
    use crate::app::{AppState, PeerInfo, TorrentDisplayState, TorrentMetrics};
    use crate::config::{PeerSortColumn, SortDirection, TorrentSortColumn};
    use crate::telemetry::manager_telemetry::ManagerTelemetry;
    use std::collections::HashMap;
    use std::time::Duration;
    use sysinfo::System;

    #[test]
    fn on_metrics_updates_totals_and_histories() {
        let mut app_state = AppState::default();

        let mut message = TorrentMetrics {
            info_hash: vec![7; 20],
            torrent_name: "test".to_string(),
            file_count: Some(3),
            number_of_pieces_total: 10,
            number_of_pieces_completed: 3,
            download_speed_bps: 512,
            upload_speed_bps: 128,
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
        assert_eq!(state.latest_state.file_count, Some(3));
        assert_eq!(state.latest_state.download_speed_bps, 512);
        assert_eq!(state.latest_state.upload_speed_bps, 128);
        assert_eq!(state.download_history.len(), 1);
        assert_eq!(state.upload_history.len(), 1);
        assert_eq!(state.swarm_availability_history.len(), 1);
    }

    #[test]
    fn on_manager_event_metrics_counts_peer_and_blocks() {
        use crate::torrent_manager::ManagerEvent;

        let info_hash = vec![1; 20];
        let mut app_state = AppState {
            torrents: HashMap::from([(info_hash.clone(), TorrentDisplayState::default())]),
            ..Default::default()
        };

        assert!(UiTelemetry::on_manager_event_metrics(
            &mut app_state,
            &ManagerEvent::PeerDiscovered {
                info_hash: info_hash.clone(),
                candidate: crate::torrent_manager::PeerCandidate::new(
                    "127.0.0.1:6881".parse().unwrap(),
                    crate::torrent_manager::PeerSource::Dht,
                ),
            }
        ));
        assert!(UiTelemetry::on_manager_event_metrics(
            &mut app_state,
            &ManagerEvent::BlockReceived {
                info_hash: info_hash.clone()
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
    fn disk_speed_uses_current_tick_and_returns_to_zero_when_idle() {
        let mut app_state = AppState::default();
        let torrent = TorrentDisplayState {
            bytes_read_this_tick: 1_024,
            bytes_written_this_tick: 2_048,
            ..TorrentDisplayState::default()
        };
        app_state.torrents.insert(vec![3; 20], torrent);

        let mut sys = System::new();
        UiTelemetry::on_second_tick(&mut app_state, &mut sys);

        assert_eq!(app_state.avg_disk_read_bps, 8_192);
        assert_eq!(app_state.avg_disk_write_bps, 16_384);

        UiTelemetry::on_second_tick(&mut app_state, &mut sys);

        assert_eq!(app_state.avg_disk_read_bps, 0);
        assert_eq!(app_state.avg_disk_write_bps, 0);
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
    fn objective_switch_updates_mode_and_sorting() {
        let mut app_state = AppState {
            is_seeding: true,
            ..Default::default()
        };

        let mut torrent = TorrentDisplayState::default();
        torrent.latest_state.number_of_pieces_total = 10;
        torrent.latest_state.number_of_pieces_completed = 9;
        app_state.torrents.insert(vec![1; 20], torrent);

        let mut sys = System::new();
        UiTelemetry::on_second_tick(&mut app_state, &mut sys);

        assert!(!app_state.is_seeding);
        assert_eq!(
            app_state.torrent_sort,
            (TorrentSortColumn::Down, SortDirection::Descending)
        );
        assert_eq!(
            app_state.peer_sort,
            (PeerSortColumn::DL, SortDirection::Descending)
        );
    }
}
