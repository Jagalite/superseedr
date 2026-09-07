// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared application throttle definitions and transitions.

use super::*;

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct WakeLagPeerThrottle {
    pub(super) effective_peer_limit: Option<usize>,
    pub(super) good_ticks: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WakeLagPeerThrottleChange {
    pub(super) previous_peer_limit: usize,
    pub(super) current_peer_limit: usize,
    pub(super) action: &'static str,
}

impl WakeLagPeerThrottle {
    pub(super) fn additive_step(base_peer_limit: usize) -> usize {
        base_peer_limit
            .saturating_mul(WAKE_LAG_PEER_THROTTLE_ADDITIVE_STEP_PERCENT)
            .saturating_div(100)
            .clamp(1, WAKE_LAG_PEER_THROTTLE_ADDITIVE_STEP_PEERS)
    }

    pub(super) fn effective_peer_limit(
        self,
        base_peer_limit: usize,
        floor_peer_limit: usize,
    ) -> usize {
        if base_peer_limit == 0 {
            return 0;
        }

        self.effective_peer_limit
            .unwrap_or(base_peer_limit)
            .clamp(floor_peer_limit.min(base_peer_limit), base_peer_limit)
    }

    pub(super) fn update(
        &mut self,
        wake_lag_frame_ratio: Option<f64>,
        wake_lag_secs: Option<f64>,
        base_peer_limit: usize,
        floor_peer_limit: usize,
        connected_peers: usize,
    ) -> Option<WakeLagPeerThrottleChange> {
        if base_peer_limit == 0 {
            self.effective_peer_limit = None;
            self.good_ticks = 0;
            return None;
        }

        let floor_peer_limit = floor_peer_limit.min(base_peer_limit);
        let previous_peer_limit = self.effective_peer_limit(base_peer_limit, floor_peer_limit);
        self.effective_peer_limit =
            (previous_peer_limit < base_peer_limit).then_some(previous_peer_limit);

        let wake_lag_ratio = wake_lag_frame_ratio.filter(|ratio| ratio.is_finite());
        let wake_lag_secs = wake_lag_secs.filter(|secs| secs.is_finite());
        wake_lag_ratio?;

        let mut current_peer_limit = previous_peer_limit;
        let mut action = None;

        let wake_lag_bad = wake_lag_ratio.is_some_and(|ratio| {
            ratio >= WAKE_LAG_PEER_THROTTLE_BAD_RATIO
                && wake_lag_secs
                    .is_some_and(|secs| secs >= WAKE_LAG_PEER_THROTTLE_BAD_MIN_DELAY.as_secs_f64())
        });
        let wake_lag_good = wake_lag_ratio.is_none_or(|ratio| {
            ratio < WAKE_LAG_PEER_THROTTLE_GOOD_RATIO
                || wake_lag_secs
                    .is_some_and(|secs| secs < WAKE_LAG_PEER_THROTTLE_BAD_MIN_DELAY.as_secs_f64())
        });

        if wake_lag_bad {
            self.good_ticks = 0;
            let pressure_peer_limit = if connected_peers == 0 {
                current_peer_limit
            } else {
                current_peer_limit.min(connected_peers)
            };
            current_peer_limit = pressure_peer_limit.saturating_div(2).max(floor_peer_limit);
            if current_peer_limit < previous_peer_limit {
                action = Some("halve_wake_lag");
            }
        } else if wake_lag_good {
            self.good_ticks = self.good_ticks.saturating_add(1);
            if self.good_ticks >= WAKE_LAG_PEER_THROTTLE_GOOD_TICKS
                && current_peer_limit < base_peer_limit
            {
                current_peer_limit = current_peer_limit
                    .saturating_add(Self::additive_step(base_peer_limit))
                    .min(base_peer_limit);
                if current_peer_limit
                    >= connected_peers
                        .saturating_add(WAKE_LAG_PEER_THROTTLE_RECOVERY_HEADROOM_PEERS)
                {
                    current_peer_limit = base_peer_limit;
                    action = Some("clear");
                } else {
                    action = Some("increase");
                }
            }
        } else {
            self.good_ticks = 0;
        }

        self.effective_peer_limit =
            (current_peer_limit < base_peer_limit).then_some(current_peer_limit);

        if current_peer_limit != previous_peer_limit {
            Some(WakeLagPeerThrottleChange {
                previous_peer_limit,
                current_peer_limit,
                action: action.unwrap_or("adjust"),
            })
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct DiskBackpressureDownloadThrottle {
    pub(super) active: bool,
    pub(super) rate_bytes_per_sec: f64,
    pub(super) accepted_rate_bytes_per_sec: f64,
    pub(super) last_score: Option<f64>,
    pub(super) window_score_total: f64,
    pub(super) window_ticks: u8,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct DiskBackpressureSample {
    pub(super) is_leeching: bool,
    pub(super) configured_download_limit_bps: u64,
    pub(super) download_bps: u64,
    pub(super) disk_write_completed_bps: u64,
    pub(super) recv_to_write_p95: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum DiskBackpressureDecision {
    Disabled,
    Limited {
        rate_bytes_per_sec: f64,
        capacity_bytes: f64,
    },
}

impl DiskBackpressureDownloadThrottle {
    pub(super) fn new(configured_download_limit_bps: u64) -> Self {
        let initial_rate = initial_disk_throttle_rate(configured_download_limit_bps);
        Self {
            active: false,
            rate_bytes_per_sec: initial_rate,
            accepted_rate_bytes_per_sec: initial_rate,
            last_score: None,
            window_score_total: 0.0,
            window_ticks: 0,
        }
    }

    pub(super) fn reset(&mut self, configured_download_limit_bps: u64) {
        let initial_rate = initial_disk_throttle_rate(configured_download_limit_bps);
        self.active = false;
        self.rate_bytes_per_sec = initial_rate;
        self.accepted_rate_bytes_per_sec = initial_rate;
        self.last_score = None;
        self.window_score_total = 0.0;
        self.window_ticks = 0;
    }

    pub(super) fn update(&mut self, sample: DiskBackpressureSample) -> DiskBackpressureDecision {
        self.update_with_step_factor(sample, random_disk_throttle_step_factor())
    }

    pub(super) fn update_with_step_factor(
        &mut self,
        sample: DiskBackpressureSample,
        step_factor: f64,
    ) -> DiskBackpressureDecision {
        if !sample.is_leeching || sample.download_bps == 0 {
            self.reset(sample.configured_download_limit_bps);
            return DiskBackpressureDecision::Disabled;
        }

        let ceiling =
            configured_download_ceiling_bytes_per_sec(sample.configured_download_limit_bps);
        self.rate_bytes_per_sec = clamp_disk_throttle_rate(self.rate_bytes_per_sec, ceiling);
        self.accepted_rate_bytes_per_sec =
            clamp_disk_throttle_rate(self.accepted_rate_bytes_per_sec, ceiling);

        if !disk_backpressure_has_signal(sample) {
            self.reset(sample.configured_download_limit_bps);
            return DiskBackpressureDecision::Disabled;
        }

        if !self.active {
            self.active = true;
        }

        self.window_score_total += disk_backpressure_score(sample);
        self.window_ticks = self.window_ticks.saturating_add(1);
        if self.window_ticks >= DISK_WRITE_THROTTLE_WINDOW_TICKS {
            let score = self.window_score_total / f64::from(self.window_ticks);
            self.finish_score_window(score, step_factor, ceiling);
        }

        DiskBackpressureDecision::Limited {
            rate_bytes_per_sec: self.rate_bytes_per_sec,
            capacity_bytes: disk_throttle_capacity_for_rate(self.rate_bytes_per_sec),
        }
    }

    pub(super) fn finish_score_window(&mut self, score: f64, step_factor: f64, ceiling: f64) {
        match self.last_score {
            Some(last_score) if score < last_score => {
                self.rate_bytes_per_sec = self.accepted_rate_bytes_per_sec;
            }
            _ => {
                self.accepted_rate_bytes_per_sec = self.rate_bytes_per_sec;
                self.last_score = Some(score);
            }
        }

        let next_rate =
            self.accepted_rate_bytes_per_sec * normalize_disk_throttle_step(step_factor);
        self.rate_bytes_per_sec = clamp_disk_throttle_rate(next_rate, ceiling);
        self.window_score_total = 0.0;
        self.window_ticks = 0;
    }
}

pub(super) fn initial_disk_throttle_rate(configured_download_limit_bps: u64) -> f64 {
    let ceiling = configured_download_ceiling_bytes_per_sec(configured_download_limit_bps);
    clamp_disk_throttle_rate(DISK_WRITE_THROTTLE_START_BYTES_PER_SEC, ceiling)
}

pub(super) fn configured_download_ceiling_bytes_per_sec(configured_download_limit_bps: u64) -> f64 {
    if crate::config::is_unlimited_rate_limit_bps(configured_download_limit_bps) {
        f64::INFINITY
    } else {
        configured_download_limit_bps as f64 / 8.0
    }
}

pub(super) fn configured_download_bucket_rate(configured_download_limit_bps: u64) -> f64 {
    rate_limit_bps_to_bucket_bytes_per_sec(configured_download_limit_bps)
}

pub(super) fn configured_upload_bucket_rate(configured_upload_limit_bps: u64) -> f64 {
    rate_limit_bps_to_bucket_bytes_per_sec(configured_upload_limit_bps)
}

pub(super) fn random_disk_throttle_step_factor() -> f64 {
    rand::rng().random_range(DISK_WRITE_THROTTLE_STEP_MIN..=DISK_WRITE_THROTTLE_STEP_MAX)
}

pub(super) fn normalize_disk_throttle_step(step_factor: f64) -> f64 {
    if step_factor.is_finite() && step_factor > 0.0 {
        step_factor.clamp(DISK_WRITE_THROTTLE_STEP_MIN, DISK_WRITE_THROTTLE_STEP_MAX)
    } else {
        1.0
    }
}

pub(super) fn disk_backpressure_score(sample: DiskBackpressureSample) -> f64 {
    let recv_to_write_seconds = sample.recv_to_write_p95.as_secs_f64();
    sample.disk_write_completed_bps as f64 * DISK_WRITE_THROTTLE_TARGET_LATENCY_SECS
        / recv_to_write_seconds.max(DISK_WRITE_THROTTLE_TARGET_LATENCY_SECS)
}

pub(super) fn disk_backpressure_has_signal(sample: DiskBackpressureSample) -> bool {
    sample.disk_write_completed_bps > 0 && sample.recv_to_write_p95 > Duration::ZERO
}

pub(super) fn effective_download_limit_bps(
    configured_download_limit_bps: u64,
    adaptive_bps: Option<u64>,
) -> u64 {
    match adaptive_bps.filter(|bps| *bps > 0) {
        Some(adaptive_bps)
            if !crate::config::is_unlimited_rate_limit_bps(configured_download_limit_bps) =>
        {
            configured_download_limit_bps.min(adaptive_bps)
        }
        Some(adaptive_bps) => adaptive_bps,
        None => configured_download_limit_bps,
    }
}

pub(super) fn bytes_per_sec_to_bps(bytes_per_sec: f64) -> u64 {
    if !bytes_per_sec.is_finite() || bytes_per_sec <= 0.0 {
        return 0;
    }

    (bytes_per_sec * 8.0).round().min(u64::MAX as f64) as u64
}

pub(super) fn clamp_disk_throttle_rate(rate_bytes_per_sec: f64, ceiling_bytes_per_sec: f64) -> f64 {
    let minimum = if ceiling_bytes_per_sec.is_finite() {
        DISK_WRITE_THROTTLE_MIN_BYTES_PER_SEC.min(ceiling_bytes_per_sec)
    } else {
        DISK_WRITE_THROTTLE_MIN_BYTES_PER_SEC
    };
    let clamped = rate_bytes_per_sec.max(minimum);
    if ceiling_bytes_per_sec.is_finite() {
        clamped.min(ceiling_bytes_per_sec)
    } else {
        clamped
    }
}

pub(super) fn disk_throttle_capacity_for_rate(rate_bytes_per_sec: f64) -> f64 {
    if rate_bytes_per_sec > 0.0 && rate_bytes_per_sec.is_finite() {
        (rate_bytes_per_sec * DISK_WRITE_THROTTLE_BURST_SECS).max(1.0)
    } else {
        rate_bytes_per_sec
    }
}
