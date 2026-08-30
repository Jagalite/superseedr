// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::{
    AppState, DhtStatus, DhtWaveTelemetry, DhtWaveUiState, DiskHealthVisualization,
    DISK_IDLE_WOBBLE_PHASE_SPEED, DISK_MAX_TRANSFER_PHASE_SPEED, DISK_MIN_TRANSFER_PHASE_SPEED,
    DISK_PHASE_RATE_MIDPOINT_BPS,
};

#[derive(Debug, Clone, Copy)]
pub(super) struct DhtWaveTargets {
    pub(super) amplitude: f64,
    pub(super) harmonic_amplitude: f64,
    pub(super) frequency: f64,
    pub(super) phase_speed: f64,
    pub(super) crest_bias: f64,
    pub(super) bootstrap_ratio: f64,
    pub(super) query_load: f64,
}

fn query_load_signal(telemetry: &DhtWaveTelemetry) -> f64 {
    let total_queries = (telemetry.inflight_ipv4_queries + telemetry.inflight_ipv6_queries) as f64;
    if total_queries <= 0.0 {
        0.0
    } else {
        (total_queries / (total_queries + 40.0)).clamp(0.0, 1.0)
    }
}

fn query_pressure_signal(telemetry: &DhtWaveTelemetry) -> f64 {
    let total_queries = (telemetry.inflight_ipv4_queries + telemetry.inflight_ipv6_queries) as f64;
    let unique_peers_found_last_10s = telemetry.unique_peers_found_last_10s as f64;
    if total_queries <= 0.0 {
        0.0
    } else if unique_peers_found_last_10s <= 0.0 {
        (total_queries / (total_queries + 32.0)).clamp(0.0, 1.0)
    } else {
        (total_queries / (total_queries + unique_peers_found_last_10s * 3.0)).clamp(0.0, 1.0)
    }
}

pub(super) fn dht_wave_targets(status: &DhtStatus, telemetry: &DhtWaveTelemetry) -> DhtWaveTargets {
    let health = &status.health;
    let routes = (health.cached_ipv4_routes + health.cached_ipv6_routes) as f64;
    let bootstrap_total = (health.ipv4_bootstrap_nodes + health.ipv6_bootstrap_nodes) as f64;
    let responsive_total =
        (health.responsive_ipv4_bootstrap_nodes + health.responsive_ipv6_bootstrap_nodes) as f64;
    let route_energy = (routes / 2_048.0).clamp(0.0, 1.0);
    let query_load = query_load_signal(telemetry);
    let pressure_signal = query_pressure_signal(telemetry);
    let bootstrap_ratio = if bootstrap_total > 0.0 {
        (responsive_total / bootstrap_total).clamp(0.0, 1.0)
    } else if health.enabled {
        0.0
    } else {
        1.0
    };
    let enabled_factor = if health.enabled { 1.0 } else { 0.0 };
    let firewalled_factor = match health.firewalled {
        Some(true) => 0.72,
        Some(false) => 1.0,
        None => 0.88,
    };
    let warning_boost = f64::from(status.warning.is_some() || health.recovery_pending);
    let activity_energy = query_load
        .max(pressure_signal * 0.72)
        .max((warning_boost * 0.55).clamp(0.0, 1.0));
    let amplitude = ((0.01
        + query_load * (0.08 + route_energy * 0.12)
        + pressure_signal * 0.13
        + warning_boost * 0.04)
        * firewalled_factor
        * enabled_factor)
        .clamp(0.0, 0.52);
    let harmonic_amplitude = ((0.004
        + query_load * 0.055
        + pressure_signal * 0.075
        + activity_energy * ((1.0 - bootstrap_ratio) * 0.04 + warning_boost * 0.04))
        * enabled_factor)
        .clamp(0.0, 0.20);
    let frequency = (0.08
        + query_load * 0.15
        + pressure_signal * 0.07
        + activity_energy * ((1.0 - bootstrap_ratio) * 0.04 + warning_boost * 0.03))
        .clamp(0.06, 0.38);
    let phase_speed = ((0.03
        + query_load * (0.35 + query_load * 0.85)
        + pressure_signal * 0.48
        + warning_boost * 0.35)
        * enabled_factor)
        .clamp(0.0, 2.0);
    let crest_bias = match health.firewalled {
        Some(true) => -0.10,
        Some(false) => 0.06,
        None => 0.0,
    } + ((route_energy - 0.5) * 0.08 * activity_energy)
        + ((query_load - 0.5) * 0.05 * pressure_signal);

    DhtWaveTargets {
        amplitude,
        harmonic_amplitude,
        frequency,
        phase_speed,
        crest_bias: crest_bias.clamp(-0.22, 0.22),
        bootstrap_ratio,
        query_load,
    }
}

fn smoothing_factor(frame_dt: f64, rate: f64) -> f64 {
    1.0 - (-frame_dt * rate).exp()
}

fn smooth_component(current: &mut f64, target: f64, factor: f64) {
    *current += (target - *current) * factor;
}

pub(super) const DHT_WAVE_PHASE_WRAP_PERIOD: f64 = std::f64::consts::TAU * 25.0;

pub(super) fn advance_dht_wave_state(
    wave: &mut DhtWaveUiState,
    target_wave: DhtWaveTargets,
    target_discovery_boost: f64,
    frame_dt: f64,
) {
    if !wave.initialized {
        wave.amplitude = target_wave.amplitude;
        wave.harmonic_amplitude = target_wave.harmonic_amplitude;
        wave.frequency = target_wave.frequency;
        wave.phase_speed = target_wave.phase_speed;
        wave.crest_bias = target_wave.crest_bias;
        wave.bootstrap_ratio = target_wave.bootstrap_ratio;
        wave.discovery_boost = target_discovery_boost;
        wave.query_load = target_wave.query_load;
        wave.query_surge = 0.0;
        wave.initialized = true;
    } else {
        let profile_blend = smoothing_factor(frame_dt, 9.0);
        let phase_speed_blend = smoothing_factor(frame_dt, 14.0);
        let discovery_blend = smoothing_factor(frame_dt, 12.0);
        let query_blend = smoothing_factor(frame_dt, 16.0);
        let query_load_delta = (target_wave.query_load - wave.query_load).abs();
        let target_query_surge = (query_load_delta * 0.32).clamp(0.0, 0.18);
        let query_surge_blend = if target_query_surge > wave.query_surge {
            smoothing_factor(frame_dt, 22.0)
        } else {
            smoothing_factor(frame_dt, 6.0)
        };
        smooth_component(&mut wave.amplitude, target_wave.amplitude, profile_blend);
        smooth_component(
            &mut wave.harmonic_amplitude,
            target_wave.harmonic_amplitude,
            profile_blend,
        );
        smooth_component(&mut wave.frequency, target_wave.frequency, profile_blend);
        smooth_component(
            &mut wave.phase_speed,
            target_wave.phase_speed,
            phase_speed_blend,
        );
        smooth_component(&mut wave.crest_bias, target_wave.crest_bias, profile_blend);
        smooth_component(
            &mut wave.bootstrap_ratio,
            target_wave.bootstrap_ratio,
            profile_blend,
        );
        smooth_component(
            &mut wave.discovery_boost,
            target_discovery_boost,
            discovery_blend,
        );
        smooth_component(&mut wave.query_load, target_wave.query_load, query_blend);
        smooth_component(&mut wave.query_surge, target_query_surge, query_surge_blend);
    }
    wave.phase = (wave.phase + frame_dt * (wave.phase_speed + wave.query_surge * 1.3))
        .rem_euclid(DHT_WAVE_PHASE_WRAP_PERIOD);
}

pub(super) fn disk_health_phase_speed(app_state: &AppState) -> f64 {
    match app_state.ui.visualization_focus.disk_health {
        DiskHealthVisualization::Classic => {
            let download_bps = app_state.avg_download_history.last().copied().unwrap_or(0) as f64;
            let upload_bps = app_state.avg_upload_history.last().copied().unwrap_or(0) as f64;
            let total_bps = download_bps + upload_bps;
            if total_bps <= 0.0 {
                return DISK_IDLE_WOBBLE_PHASE_SPEED;
            }
            let transfer_signal = (total_bps / 50_000_000.0).clamp(0.0, 1.0).sqrt();
            let balance = ((download_bps - upload_bps) / total_bps).clamp(-1.0, 1.0);
            let direction = if balance < -0.05 { -1.0 } else { 1.0 };
            let dominance = balance.abs();
            let disk_pressure = app_state
                .disk_health_ema
                .max(app_state.disk_health_peak_hold)
                .clamp(0.0, 1.0);
            direction
                * (DISK_MIN_TRANSFER_PHASE_SPEED
                    + 1.60 * transfer_signal
                    + 1.40 * dominance
                    + 1.40 * disk_pressure)
                    .min(DISK_MAX_TRANSFER_PHASE_SPEED)
        }
        DiskHealthVisualization::SeekPendulum | DiskHealthVisualization::StorageDial => {
            let disk_bps = app_state
                .avg_disk_read_bps
                .saturating_add(app_state.avg_disk_write_bps) as f64;
            let rate_signal = disk_bps / (disk_bps + DISK_PHASE_RATE_MIDPOINT_BPS);
            rate_signal * DISK_MAX_TRANSFER_PHASE_SPEED
        }
    }
}
