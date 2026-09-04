// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use rand::seq::SliceRandom;
use rand::{Rng, RngExt};

use crate::app::CalculatedLimits;
use crate::resource::ResourceType;

pub(crate) const MIN_STEP_RATE: f64 = 0.01;
pub(crate) const MAX_STEP_RATE: f64 = 0.10;
pub(crate) const BASELINE_ALPHA: f64 = 0.1;
pub(crate) const REALITY_CHECK_FACTOR: f64 = 2.0;
#[cfg(test)]
pub(crate) const DEFAULT_TUNING_CADENCE_SECS: u64 = 90;
#[cfg(test)]
pub(crate) const DEFAULT_TUNING_LOOKBACK_SECS: usize = 60;
pub(crate) const MIN_TUNING_CADENCE_SECS: u64 = 15;
pub(crate) const MAX_TUNING_CADENCE_SECS: u64 = 180;
pub(crate) const FAST_START_CADENCE_SECS: u64 = 20;
pub(crate) const FAST_START_CYCLES: u8 = 3;
pub(crate) const MIN_TUNING_LOOKBACK_SECS: usize = 15;
pub(crate) const MAX_TUNING_LOOKBACK_SECS: usize = 60;
const LOOKBACK_RATIO: f64 = 0.7;
const IMPROVEMENT_SPEEDUP_FACTOR: f64 = 0.85;
const STAGNATION_BACKOFF_FACTOR: f64 = 1.6;
const REGRESSION_SPEEDUP_FACTOR: f64 = 0.5;
const STAGNATION_BACKOFF_START_CYCLES: u32 = 2;
const RAPID_REGRESSION_RATIO: f64 = 0.90;
const SEVERE_REGRESSION_RATIO: f64 = 0.75;
const PENALTY_SPIKE_DELTA: f64 = 0.25;
const CADENCE_CHANGE_PRESSURE_TRIGGER: u8 = 4;
const CADENCE_CHANGE_PRESSURE_DECAY: u8 = 1;
const HIGH_NOISE_REL_STDDEV: f64 = 0.25;
const REGRESSION_SPEEDUP_BUDGET_CYCLES: u8 = 3;
const MIN_CADENCE_NO_IMPROVEMENT_BACKOFF_CYCLES: u32 = 3;
const STALE_BEST_DECAY_START_CYCLES: u32 = 6;
const STALE_BEST_DECAY_FACTOR: f64 = 0.97;

pub(crate) const MIN_PEERS: usize = 20;
pub(crate) const MIN_DISK: usize = 2;
pub(crate) const MIN_RESERVE: usize = 0;

pub(crate) const MAX_TRADE_ATTEMPTS: usize = 5;

#[derive(Debug, Clone)]
pub(crate) struct TuningState {
    pub(crate) last_tuning_score: u64,
    pub(crate) current_tuning_score: u64,
    pub(crate) last_tuning_limits: CalculatedLimits,
    pub(crate) baseline_speed_ema: f64,
}

impl TuningState {
    pub(crate) fn new(initial_limits: CalculatedLimits) -> Self {
        Self {
            last_tuning_score: 0,
            current_tuning_score: 0,
            last_tuning_limits: initial_limits,
            baseline_speed_ema: 0.0,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TuningController {
    cadence_secs: u64,
    lookback_secs: usize,
    countdown_secs: u64,
    adaptive_enabled: bool,
    stagnation_cycles: u32,
    no_improvement_cycles: u32,
    fast_start_cycles_remaining: u8,
    regression_speedup_budget_remaining: u8,
    last_penalty_factor: Option<f64>,
    cadence_change_pressure: u8,
    state: TuningState,
}

impl TuningController {
    #[cfg(test)]
    pub(crate) fn new_fixed(initial_limits: CalculatedLimits) -> Self {
        Self {
            cadence_secs: DEFAULT_TUNING_CADENCE_SECS,
            lookback_secs: DEFAULT_TUNING_LOOKBACK_SECS,
            countdown_secs: DEFAULT_TUNING_CADENCE_SECS,
            adaptive_enabled: false,
            stagnation_cycles: 0,
            no_improvement_cycles: 0,
            fast_start_cycles_remaining: 0,
            regression_speedup_budget_remaining: 0,
            last_penalty_factor: None,
            cadence_change_pressure: 0,
            state: TuningState::new(initial_limits),
        }
    }

    pub(crate) fn new_adaptive(initial_limits: CalculatedLimits) -> Self {
        let cadence_secs = FAST_START_CADENCE_SECS;
        Self {
            cadence_secs,
            lookback_secs: derive_lookback_secs(cadence_secs),
            countdown_secs: cadence_secs,
            adaptive_enabled: true,
            stagnation_cycles: 0,
            no_improvement_cycles: 0,
            fast_start_cycles_remaining: FAST_START_CYCLES,
            regression_speedup_budget_remaining: REGRESSION_SPEEDUP_BUDGET_CYCLES,
            last_penalty_factor: None,
            cadence_change_pressure: 0,
            state: TuningState::new(initial_limits),
        }
    }

    pub(crate) fn cadence_secs(&self) -> u64 {
        self.cadence_secs
    }

    pub(crate) fn lookback_secs(&self) -> usize {
        self.lookback_secs
    }

    pub(crate) fn countdown_secs(&self) -> u64 {
        self.countdown_secs
    }

    pub(crate) fn state(&self) -> &TuningState {
        &self.state
    }

    pub(crate) fn on_second_tick(&mut self) {
        self.countdown_secs = self.countdown_secs.saturating_sub(1);
    }

    pub(crate) fn reset_for_objective_change(&mut self, current_limits: &CalculatedLimits) {
        self.state.last_tuning_score = 0;
        self.state.current_tuning_score = 0;
        self.state.baseline_speed_ema = 0.0;
        self.state.last_tuning_limits = current_limits.clone();
        self.stagnation_cycles = 0;
        self.no_improvement_cycles = 0;
        self.last_penalty_factor = None;
        self.cadence_change_pressure = 0;
        if self.adaptive_enabled {
            self.cadence_secs = FAST_START_CADENCE_SECS;
            self.lookback_secs = derive_lookback_secs(self.cadence_secs);
            self.fast_start_cycles_remaining = FAST_START_CYCLES;
            self.regression_speedup_budget_remaining = REGRESSION_SPEEDUP_BUDGET_CYCLES;
        }
        self.countdown_secs = self.cadence_secs;
    }

    pub(crate) fn update_live_score(
        &mut self,
        relevant_history: &[u64],
        current_scpb: f64,
        scpb_max: f64,
    ) -> TuningScore {
        let live_score = compute_tuning_score(relevant_history, current_scpb, scpb_max);
        self.state.current_tuning_score = live_score.new_score;
        live_score
    }

    pub(crate) fn evaluate_cycle(
        &mut self,
        current_limits: &CalculatedLimits,
        relevant_history: &[u64],
        current_scpb: f64,
        scpb_max: f64,
    ) -> TuningEvaluation {
        let score = self.update_live_score(relevant_history, current_scpb, scpb_max);
        let evaluation = evaluate_tuning_cycle_from_score(
            current_limits,
            &self.state.last_tuning_limits,
            self.state.last_tuning_score,
            self.state.baseline_speed_ema,
            score,
        );

        self.state.baseline_speed_ema = evaluation.updated_baseline_speed_ema;
        self.state.last_tuning_score = evaluation.updated_last_tuning_score;
        self.state.last_tuning_limits = evaluation.updated_last_tuning_limits.clone();
        self.apply_cadence_policy(&evaluation, score.penalty_factor, relevant_history);
        self.countdown_secs = self.cadence_secs;
        evaluation
    }

    fn apply_cadence_policy(
        &mut self,
        evaluation: &TuningEvaluation,
        penalty_factor: f64,
        relevant_history: &[u64],
    ) {
        if !self.adaptive_enabled {
            return;
        }

        let previous_penalty = self.last_penalty_factor.unwrap_or(penalty_factor);
        let previous_cadence = self.cadence_secs;
        let rel_stddev = relative_stddev(relevant_history);
        let high_noise = rel_stddev >= HIGH_NOISE_REL_STDDEV;
        let severe_regression = evaluation.best_score_before > 0
            && (evaluation.new_score as f64)
                < ((evaluation.best_score_before as f64) * SEVERE_REGRESSION_RATIO);
        let rapid_regression = evaluation.best_score_before > 0
            && (evaluation.new_score as f64)
                < ((evaluation.best_score_before as f64) * RAPID_REGRESSION_RATIO);
        let regression_signal = severe_regression || (!high_noise && rapid_regression);
        let penalty_spike = penalty_factor > (previous_penalty + PENALTY_SPIKE_DELTA);

        if evaluation.accepted_improvement {
            self.stagnation_cycles = 0;
            self.no_improvement_cycles = 0;
            self.regression_speedup_budget_remaining = REGRESSION_SPEEDUP_BUDGET_CYCLES;
            self.cadence_secs = scaled_cadence(
                self.cadence_secs,
                IMPROVEMENT_SPEEDUP_FACTOR,
                ScaleDirection::Down,
            );
        } else {
            self.no_improvement_cycles = self.no_improvement_cycles.saturating_add(1);
            let can_speedup_regression = self.regression_speedup_budget_remaining > 0;
            if (regression_signal || penalty_spike) && can_speedup_regression {
                self.stagnation_cycles = 0;
                self.regression_speedup_budget_remaining =
                    self.regression_speedup_budget_remaining.saturating_sub(1);
                self.cadence_secs = scaled_cadence(
                    self.cadence_secs,
                    REGRESSION_SPEEDUP_FACTOR,
                    ScaleDirection::Down,
                );
            } else {
                self.stagnation_cycles = self.stagnation_cycles.saturating_add(1);
                if self.cadence_secs == MIN_TUNING_CADENCE_SECS
                    && self.no_improvement_cycles >= MIN_CADENCE_NO_IMPROVEMENT_BACKOFF_CYCLES
                {
                    self.cadence_secs = scaled_cadence(
                        self.cadence_secs,
                        STAGNATION_BACKOFF_FACTOR,
                        ScaleDirection::Up,
                    );
                    self.stagnation_cycles = 0;
                } else if self.stagnation_cycles >= STAGNATION_BACKOFF_START_CYCLES {
                    self.cadence_secs = scaled_cadence(
                        self.cadence_secs,
                        STAGNATION_BACKOFF_FACTOR,
                        ScaleDirection::Up,
                    );
                }
            }
        }

        if self.fast_start_cycles_remaining > 0 {
            self.cadence_secs = self.cadence_secs.min(FAST_START_CADENCE_SECS);
            self.fast_start_cycles_remaining = self.fast_start_cycles_remaining.saturating_sub(1);
        }

        // Failsafe: if cadence keeps changing rapidly, force a stabilizing backoff.
        if self.cadence_secs != previous_cadence {
            self.cadence_change_pressure = self.cadence_change_pressure.saturating_add(1);
        } else {
            self.cadence_change_pressure = self
                .cadence_change_pressure
                .saturating_sub(CADENCE_CHANGE_PRESSURE_DECAY);
        }
        if self.cadence_change_pressure >= CADENCE_CHANGE_PRESSURE_TRIGGER {
            self.cadence_secs = scaled_cadence(
                self.cadence_secs,
                STAGNATION_BACKOFF_FACTOR,
                ScaleDirection::Up,
            );
            self.cadence_change_pressure /= 2;
        }

        // Decay stale best score toward baseline after sustained non-improvement.
        if self.no_improvement_cycles >= STALE_BEST_DECAY_START_CYCLES {
            let baseline_floor = self.state.baseline_speed_ema as u64;
            let decayed = (self.state.last_tuning_score as f64 * STALE_BEST_DECAY_FACTOR) as u64;
            self.state.last_tuning_score = decayed.max(baseline_floor);
        }

        self.lookback_secs = derive_lookback_secs(self.cadence_secs);
        self.last_penalty_factor = Some(penalty_factor);
    }
}

#[derive(Debug, Clone, Copy)]
enum ScaleDirection {
    Up,
    Down,
}

fn scaled_cadence(cadence_secs: u64, factor: f64, direction: ScaleDirection) -> u64 {
    let scaled = match direction {
        ScaleDirection::Up => (cadence_secs as f64 * factor).ceil() as u64,
        ScaleDirection::Down => (cadence_secs as f64 * factor).floor() as u64,
    };
    scaled.clamp(MIN_TUNING_CADENCE_SECS, MAX_TUNING_CADENCE_SECS)
}

fn derive_lookback_secs(cadence_secs: u64) -> usize {
    let derived = ((cadence_secs as f64) * LOOKBACK_RATIO).round() as usize;
    let clamped = derived.clamp(MIN_TUNING_LOOKBACK_SECS, MAX_TUNING_LOOKBACK_SECS);
    clamped.min(cadence_secs as usize)
}

fn relative_stddev(values: &[u64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let mean = values.iter().copied().map(|v| v as f64).sum::<f64>() / values.len() as f64;
    if mean <= 0.0 {
        return 0.0;
    }
    let var = values
        .iter()
        .map(|v| {
            let dv = *v as f64 - mean;
            dv * dv
        })
        .sum::<f64>()
        / values.len() as f64;
    var.sqrt() / mean
}

pub(crate) fn normalize_limits_for_mode(
    limits: &CalculatedLimits,
    is_seeding: bool,
) -> CalculatedLimits {
    if is_seeding {
        let total_budget = limits
            .reserve_permits
            .saturating_add(limits.max_connected_peers)
            .saturating_add(limits.disk_read_permits)
            .saturating_add(limits.disk_write_permits);
        let peer_slots = total_budget.saturating_mul(70) / 100;
        let read_slots = total_budget.saturating_sub(peer_slots);
        return CalculatedLimits {
            reserve_permits: 0,
            max_connected_peers: peer_slots,
            disk_read_permits: read_slots,
            disk_write_permits: 0,
        };
    }

    // Downloading mode: keep total disk budget, targeting 30% read / 70% write.
    let disk_budget = limits
        .disk_read_permits
        .saturating_add(limits.disk_write_permits);
    let read_slots = disk_budget.saturating_mul(30) / 100;
    let write_slots = disk_budget.saturating_sub(read_slots);
    CalculatedLimits {
        reserve_permits: limits.reserve_permits,
        max_connected_peers: limits.max_connected_peers,
        disk_read_permits: read_slots,
        disk_write_permits: write_slots,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TuningEvaluation {
    pub(crate) new_raw_score: u64,
    pub(crate) penalty_factor: f64,
    pub(crate) new_score: u64,
    pub(crate) updated_baseline_speed_ema: f64,
    pub(crate) best_score_before: u64,
    pub(crate) baseline_u64: u64,
    pub(crate) updated_last_tuning_score: u64,
    pub(crate) updated_last_tuning_limits: CalculatedLimits,
    pub(crate) effective_limits: CalculatedLimits,
    pub(crate) accepted_improvement: bool,
    pub(crate) reality_check_applied: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TuningScore {
    pub(crate) new_raw_score: u64,
    pub(crate) penalty_factor: f64,
    pub(crate) new_score: u64,
}

pub(crate) fn compute_tuning_score(
    relevant_history: &[u64],
    current_scpb: f64,
    scpb_max: f64,
) -> TuningScore {
    let new_raw_score = if relevant_history.is_empty() {
        0
    } else {
        relevant_history.iter().sum::<u64>() / relevant_history.len() as u64
    };
    let penalty_factor = (current_scpb / scpb_max - 1.0).max(0.0);
    let new_score = (new_raw_score as f64 / (1.0 + penalty_factor)) as u64;
    TuningScore {
        new_raw_score,
        penalty_factor,
        new_score,
    }
}

#[cfg(test)]
pub(crate) fn evaluate_tuning_cycle(
    current_limits: &CalculatedLimits,
    last_tuning_limits: &CalculatedLimits,
    last_tuning_score: u64,
    baseline_speed_ema: f64,
    relevant_history: &[u64],
    current_scpb: f64,
    scpb_max: f64,
) -> TuningEvaluation {
    let score = compute_tuning_score(relevant_history, current_scpb, scpb_max);
    evaluate_tuning_cycle_from_score(
        current_limits,
        last_tuning_limits,
        last_tuning_score,
        baseline_speed_ema,
        score,
    )
}

pub(crate) fn evaluate_tuning_cycle_from_score(
    current_limits: &CalculatedLimits,
    last_tuning_limits: &CalculatedLimits,
    last_tuning_score: u64,
    baseline_speed_ema: f64,
    score: TuningScore,
) -> TuningEvaluation {
    let new_score_f64 = score.new_score as f64;
    let updated_baseline_speed_ema = if baseline_speed_ema == 0.0 {
        new_score_f64
    } else {
        (new_score_f64 * BASELINE_ALPHA) + (baseline_speed_ema * (1.0 - BASELINE_ALPHA))
    };

    let best_score_before = last_tuning_score;
    let baseline_u64 = updated_baseline_speed_ema as u64;
    let mut updated_last_tuning_score = last_tuning_score;
    let mut updated_last_tuning_limits = last_tuning_limits.clone();
    let mut effective_limits = current_limits.clone();
    let mut accepted_improvement = false;
    let mut reality_check_applied = false;

    if score.new_score > best_score_before {
        updated_last_tuning_score = score.new_score;
        updated_last_tuning_limits = current_limits.clone();
        accepted_improvement = true;
    } else {
        effective_limits = last_tuning_limits.clone();
        if best_score_before > 10_000
            && best_score_before > (updated_baseline_speed_ema * REALITY_CHECK_FACTOR) as u64
        {
            updated_last_tuning_score = baseline_u64;
            reality_check_applied = true;
        }
    }

    TuningEvaluation {
        new_raw_score: score.new_raw_score,
        penalty_factor: score.penalty_factor,
        new_score: score.new_score,
        updated_baseline_speed_ema,
        best_score_before,
        baseline_u64,
        updated_last_tuning_score,
        updated_last_tuning_limits,
        effective_limits,
        accepted_improvement,
        reality_check_applied,
    }
}

fn get_limit(limits: &CalculatedLimits, resource: ResourceType) -> usize {
    match resource {
        ResourceType::PeerConnection => limits.max_connected_peers,
        ResourceType::DiskRead => limits.disk_read_permits,
        ResourceType::DiskWrite => limits.disk_write_permits,
        ResourceType::Reserve => limits.reserve_permits,
    }
}

fn set_limit(limits: &mut CalculatedLimits, resource: ResourceType, value: usize) {
    match resource {
        ResourceType::PeerConnection => limits.max_connected_peers = value,
        ResourceType::DiskRead => limits.disk_read_permits = value,
        ResourceType::DiskWrite => limits.disk_write_permits = value,
        ResourceType::Reserve => limits.reserve_permits = value,
    }
}

pub(crate) fn make_random_adjustment(
    limits: CalculatedLimits,
    is_seeding: bool,
) -> (CalculatedLimits, String) {
    let mut rng = rand::rng();
    make_random_adjustment_with_rng(limits, is_seeding, &mut rng)
}

pub(crate) fn make_random_adjustment_with_rng<R: Rng + ?Sized>(
    limits: CalculatedLimits,
    is_seeding: bool,
    rng: &mut R,
) -> (CalculatedLimits, String) {
    let mut limits = if is_seeding {
        normalize_limits_for_mode(&limits, true)
    } else {
        limits
    };
    let mut parameters = vec![
        ResourceType::PeerConnection,
        ResourceType::DiskRead,
        ResourceType::Reserve,
    ];
    if !is_seeding {
        parameters.push(ResourceType::DiskWrite);
    }

    if parameters.len() < 2 {
        return (
            limits,
            "Skipped all trade attempts (0): insufficient adjustable resources".to_string(),
        );
    }

    for attempt in 0..MAX_TRADE_ATTEMPTS {
        parameters.shuffle(rng);
        let source_param = parameters[0];
        let dest_param = parameters[1];

        let source_val = get_limit(&limits, source_param);
        let dest_val = get_limit(&limits, dest_param);

        let source_min = match source_param {
            ResourceType::PeerConnection => MIN_PEERS,
            ResourceType::DiskRead => MIN_DISK,
            ResourceType::DiskWrite => MIN_DISK,
            ResourceType::Reserve => MIN_RESERVE,
        };

        let step_rate = rng.random_range(MIN_STEP_RATE..=MAX_STEP_RATE);
        let amount_to_trade = ((source_val as f64 * step_rate).ceil() as usize).max(1);
        let can_give = source_val >= source_min.saturating_add(amount_to_trade);

        if can_give {
            set_limit(
                &mut limits,
                source_param,
                source_val.saturating_sub(amount_to_trade),
            );
            set_limit(
                &mut limits,
                dest_param,
                dest_val.saturating_add(amount_to_trade),
            );

            let description = format!(
                "Traded {} from {:?} to {:?} (Attempt {})",
                amount_to_trade,
                source_param,
                dest_param,
                attempt + 1
            );
            return (limits, description);
        }
    }

    let description = format!(
        "Skipped all trade attempts ({}) this cycle: blocked by bounds",
        MAX_TRADE_ATTEMPTS
    );
    (limits, description)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    #[derive(Clone, Debug)]
    struct SyntheticWorkload {
        optimum: CalculatedLimits,
        peak_score: u64,
        reserve_penalty: u64,
        peer_penalty: u64,
        read_penalty: u64,
        write_penalty: u64,
        base_scpb: f64,
        scpb_slope: f64,
    }

    impl SyntheticWorkload {
        fn sample(&self, limits: &CalculatedLimits) -> (u64, f64) {
            let reserve_delta = limits
                .reserve_permits
                .abs_diff(self.optimum.reserve_permits);
            let peer_delta = limits
                .max_connected_peers
                .abs_diff(self.optimum.max_connected_peers);
            let read_delta = limits
                .disk_read_permits
                .abs_diff(self.optimum.disk_read_permits);
            let write_delta = limits
                .disk_write_permits
                .abs_diff(self.optimum.disk_write_permits);

            let raw_penalty = (peer_delta as u64)
                .saturating_mul(peer_delta as u64)
                .saturating_mul(self.peer_penalty)
                .saturating_add(
                    (reserve_delta as u64)
                        .saturating_mul(reserve_delta as u64)
                        .saturating_mul(self.reserve_penalty),
                )
                .saturating_add(
                    (read_delta as u64)
                        .saturating_mul(read_delta as u64)
                        .saturating_mul(self.read_penalty),
                )
                .saturating_add(
                    (write_delta as u64)
                        .saturating_mul(write_delta as u64)
                        .saturating_mul(self.write_penalty),
                );

            let raw_score = self.peak_score.saturating_sub(raw_penalty);

            let disk_delta = limits
                .disk_read_permits
                .saturating_add(limits.disk_write_permits)
                .abs_diff(self.optimum.disk_read_permits + self.optimum.disk_write_permits);
            let scpb = self.base_scpb + (disk_delta as f64 * self.scpb_slope);
            (raw_score, scpb)
        }
    }

    #[derive(Debug)]
    struct SimulationResult {
        best_limits: CalculatedLimits,
        best_score: u64,
        accepted_count: usize,
        reverted_count: usize,
        score_trace: Vec<u64>,
    }

    struct SimulationState {
        limits: CalculatedLimits,
        last_tuning_limits: CalculatedLimits,
        last_tuning_score: u64,
        baseline_speed_ema: f64,
        rng: StdRng,
    }

    impl SimulationState {
        fn new(initial_limits: CalculatedLimits, seed: u64) -> Self {
            Self {
                limits: initial_limits.clone(),
                last_tuning_limits: initial_limits,
                last_tuning_score: 0,
                baseline_speed_ema: 0.0,
                rng: StdRng::seed_from_u64(seed),
            }
        }

        fn run_phase(&mut self, cycles: usize, workload: &SyntheticWorkload) -> SimulationResult {
            let mut accepted_count = 0usize;
            let mut reverted_count = 0usize;
            let mut score_trace = Vec::with_capacity(cycles);
            let adaptive_max_scpb = 10.0;

            for _ in 0..cycles {
                let (raw_score, scpb) = workload.sample(&self.limits);
                let history = [raw_score; 60];
                let evaluation = evaluate_tuning_cycle(
                    &self.limits,
                    &self.last_tuning_limits,
                    self.last_tuning_score,
                    self.baseline_speed_ema,
                    &history,
                    scpb,
                    adaptive_max_scpb,
                );

                if evaluation.accepted_improvement {
                    accepted_count = accepted_count.saturating_add(1);
                } else {
                    reverted_count = reverted_count.saturating_add(1);
                }

                score_trace.push(evaluation.new_score);
                self.baseline_speed_ema = evaluation.updated_baseline_speed_ema;
                self.last_tuning_score = evaluation.updated_last_tuning_score;
                self.last_tuning_limits = evaluation.updated_last_tuning_limits;
                self.limits = evaluation.effective_limits;

                let (next_limits, _desc) =
                    make_random_adjustment_with_rng(self.limits.clone(), false, &mut self.rng);
                self.limits = next_limits;
            }

            SimulationResult {
                best_limits: self.last_tuning_limits.clone(),
                best_score: self.last_tuning_score,
                accepted_count,
                reverted_count,
                score_trace,
            }
        }
    }

    fn simulate_tuning_cycles(
        initial_limits: CalculatedLimits,
        cycles: usize,
        seed: u64,
        workload: &SyntheticWorkload,
    ) -> SimulationResult {
        let mut state = SimulationState::new(initial_limits, seed);
        state.run_phase(cycles, workload)
    }

    #[test]
    fn tuner_simulation_converges_toward_known_optimum_no_noise() {
        let initial_limits = CalculatedLimits {
            reserve_permits: 20,
            max_connected_peers: 110,
            disk_read_permits: 30,
            disk_write_permits: 20,
        };
        let workload = SyntheticWorkload {
            optimum: CalculatedLimits {
                reserve_permits: 20,
                max_connected_peers: 72,
                disk_read_permits: 14,
                disk_write_permits: 10,
            },
            peak_score: 120_000,
            reserve_penalty: 0,
            peer_penalty: 4,
            read_penalty: 70,
            write_penalty: 80,
            base_scpb: 4.0,
            scpb_slope: 0.15,
        };
        let result = simulate_tuning_cycles(initial_limits, 500, 7, &workload);

        assert!(
            result.best_score > 100_000,
            "Expected strong improvement in best score"
        );
        assert!(
            result
                .best_limits
                .max_connected_peers
                .abs_diff(workload.optimum.max_connected_peers)
                <= 12
        );
        assert!(
            result
                .best_limits
                .disk_read_permits
                .abs_diff(workload.optimum.disk_read_permits)
                <= 4
        );
        assert!(
            result
                .best_limits
                .disk_write_permits
                .abs_diff(workload.optimum.disk_write_permits)
                <= 4
        );
    }

    #[test]
    fn tuner_simulation_under_disk_pressure_converges_to_lower_disk_limits() {
        let initial_limits = CalculatedLimits {
            reserve_permits: 18,
            max_connected_peers: 72,
            disk_read_permits: 34,
            disk_write_permits: 26,
        };
        let workload = SyntheticWorkload {
            optimum: CalculatedLimits {
                reserve_permits: 18,
                max_connected_peers: 72,
                disk_read_permits: 10,
                disk_write_permits: 8,
            },
            peak_score: 100_000,
            reserve_penalty: 0,
            peer_penalty: 0,
            read_penalty: 120,
            write_penalty: 140,
            base_scpb: 7.5,
            scpb_slope: 0.18,
        };
        let initial_disk_total =
            initial_limits.disk_read_permits + initial_limits.disk_write_permits;
        let result = simulate_tuning_cycles(initial_limits, 600, 41, &workload);
        let best_disk_total =
            result.best_limits.disk_read_permits + result.best_limits.disk_write_permits;
        let optimum_disk_total =
            workload.optimum.disk_read_permits + workload.optimum.disk_write_permits;

        assert!(
            result.best_score > 80_000,
            "Expected disk-throttled workload to recover a strong score"
        );
        assert!(
            best_disk_total + 20 <= initial_disk_total,
            "Expected disk budget to throttle down materially under pressure"
        );
        assert!(
            best_disk_total.abs_diff(optimum_disk_total) <= 4,
            "Expected best disk budget to converge near the lower-pressure optimum"
        );
        assert!(
            result
                .best_limits
                .disk_read_permits
                .abs_diff(workload.optimum.disk_read_permits)
                <= 3
        );
        assert!(
            result
                .best_limits
                .disk_write_permits
                .abs_diff(workload.optimum.disk_write_permits)
                <= 3
        );
    }

    #[test]
    fn tuner_simulation_converges_toward_nonzero_reserve_optimum() {
        let initial_limits = CalculatedLimits {
            reserve_permits: 4,
            max_connected_peers: 92,
            disk_read_permits: 20,
            disk_write_permits: 14,
        };
        let workload = SyntheticWorkload {
            optimum: CalculatedLimits {
                reserve_permits: 34,
                max_connected_peers: 72,
                disk_read_permits: 14,
                disk_write_permits: 10,
            },
            peak_score: 110_000,
            reserve_penalty: 45,
            peer_penalty: 10,
            read_penalty: 55,
            write_penalty: 65,
            base_scpb: 5.0,
            scpb_slope: 0.10,
        };
        let result = simulate_tuning_cycles(initial_limits, 600, 73, &workload);

        assert!(
            result.best_score > 95_000,
            "Expected strong recovery after moving excess budget into reserve"
        );
        assert!(
            result.best_limits.reserve_permits > 20,
            "Expected tuner to increase reserve materially from the initial allocation"
        );
        assert!(
            result
                .best_limits
                .reserve_permits
                .abs_diff(workload.optimum.reserve_permits)
                <= 5,
            "Expected reserve allocation to converge near the optimum"
        );
        assert!(
            result
                .best_limits
                .max_connected_peers
                .abs_diff(workload.optimum.max_connected_peers)
                <= 8
        );
        assert!(
            result
                .best_limits
                .disk_read_permits
                .abs_diff(workload.optimum.disk_read_permits)
                <= 4
        );
        assert!(
            result
                .best_limits
                .disk_write_permits
                .abs_diff(workload.optimum.disk_write_permits)
                <= 4
        );
    }

    #[test]
    fn tuner_simulation_recovers_from_high_reserve_phase_when_demand_returns() {
        let initial_limits = CalculatedLimits {
            reserve_permits: 4,
            max_connected_peers: 92,
            disk_read_permits: 20,
            disk_write_permits: 14,
        };
        let reserve_heavy_workload = SyntheticWorkload {
            optimum: CalculatedLimits {
                reserve_permits: 34,
                max_connected_peers: 72,
                disk_read_permits: 14,
                disk_write_permits: 10,
            },
            peak_score: 110_000,
            reserve_penalty: 45,
            peer_penalty: 10,
            read_penalty: 55,
            write_penalty: 65,
            base_scpb: 5.0,
            scpb_slope: 0.10,
        };
        let active_workload = SyntheticWorkload {
            optimum: CalculatedLimits {
                reserve_permits: 6,
                max_connected_peers: 90,
                disk_read_permits: 20,
                disk_write_permits: 14,
            },
            peak_score: 140_000,
            reserve_penalty: 90,
            peer_penalty: 16,
            read_penalty: 55,
            write_penalty: 60,
            base_scpb: 5.0,
            scpb_slope: 0.08,
        };
        let mut state = SimulationState::new(initial_limits, 101);

        let reserve_phase = state.run_phase(500, &reserve_heavy_workload);
        let recovery_phase = state.run_phase(700, &active_workload);

        assert!(
            reserve_phase.best_limits.reserve_permits >= 28,
            "Expected first phase to converge into a high-reserve steady state, got {}",
            reserve_phase.best_limits.reserve_permits
        );
        assert!(
            recovery_phase.best_score > 95_000,
            "Expected strong recovery after shifting budget back out of reserve, got {}",
            recovery_phase.best_score
        );
        assert!(
            recovery_phase.best_limits.reserve_permits + 14
                <= reserve_phase.best_limits.reserve_permits,
            "Expected reserve allocation to fall materially once demand returns, reserve phase={}, recovery phase={}",
            reserve_phase.best_limits.reserve_permits,
            recovery_phase.best_limits.reserve_permits
        );
        assert!(
            recovery_phase
                .best_limits
                .reserve_permits
                .abs_diff(active_workload.optimum.reserve_permits)
                <= 5,
            "Expected reserve allocation to reconverge near the lower-demand optimum, got {}",
            recovery_phase.best_limits.reserve_permits
        );
        assert!(
            recovery_phase
                .best_limits
                .max_connected_peers
                .abs_diff(active_workload.optimum.max_connected_peers)
                <= 8
        );
        assert!(
            recovery_phase
                .best_limits
                .disk_read_permits
                .abs_diff(active_workload.optimum.disk_read_permits)
                <= 4
        );
        assert!(
            recovery_phase
                .best_limits
                .disk_write_permits
                .abs_diff(active_workload.optimum.disk_write_permits)
                <= 4
        );
    }

    #[test]
    fn tuner_evaluation_reverts_when_candidate_is_worse() {
        let current_limits = CalculatedLimits {
            reserve_permits: 30,
            max_connected_peers: 140,
            disk_read_permits: 40,
            disk_write_permits: 32,
        };
        let good_limits = CalculatedLimits {
            reserve_permits: 30,
            max_connected_peers: 80,
            disk_read_permits: 14,
            disk_write_permits: 10,
        };
        let history = [10_000u64; 60];
        let eval = evaluate_tuning_cycle(
            &current_limits,
            &good_limits,
            40_000,
            15_000.0,
            &history,
            12.0,
            10.0,
        );

        assert!(!eval.accepted_improvement);
        assert_eq!(
            eval.effective_limits.max_connected_peers,
            good_limits.max_connected_peers
        );
        assert_eq!(
            eval.effective_limits.disk_read_permits,
            good_limits.disk_read_permits
        );
        assert_eq!(
            eval.effective_limits.disk_write_permits,
            good_limits.disk_write_permits
        );
    }

    #[test]
    fn tuner_simulation_plateau_stays_stable_without_runaway() {
        let initial_limits = CalculatedLimits {
            reserve_permits: 25,
            max_connected_peers: 80,
            disk_read_permits: 12,
            disk_write_permits: 10,
        };
        let workload = SyntheticWorkload {
            optimum: CalculatedLimits {
                reserve_permits: 25,
                max_connected_peers: 80,
                disk_read_permits: 12,
                disk_write_permits: 10,
            },
            peak_score: 50_000,
            reserve_penalty: 0,
            peer_penalty: 0,
            read_penalty: 0,
            write_penalty: 0,
            base_scpb: 3.0,
            scpb_slope: 0.0,
        };
        let result = simulate_tuning_cycles(initial_limits, 120, 13, &workload);

        assert!(result.reverted_count > result.accepted_count);
        assert!(result.score_trace.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn random_adjustment_respects_min_bounds_over_many_steps() {
        let mut limits = CalculatedLimits {
            reserve_permits: 40,
            max_connected_peers: MIN_PEERS + 10,
            disk_read_permits: MIN_DISK + 5,
            disk_write_permits: MIN_DISK + 5,
        };
        let mut rng = StdRng::seed_from_u64(99);

        for _ in 0..2_000 {
            let (next, _desc) = make_random_adjustment_with_rng(limits, false, &mut rng);
            limits = next;

            assert!(limits.max_connected_peers >= MIN_PEERS);
            assert!(limits.disk_read_permits >= MIN_DISK);
            assert!(limits.disk_write_permits >= MIN_DISK);
        }
    }

    #[test]
    fn tuner_evaluation_reality_check_resets_stale_best_score() {
        let limits = CalculatedLimits {
            reserve_permits: 20,
            max_connected_peers: 90,
            disk_read_permits: 16,
            disk_write_permits: 12,
        };
        let history = [800u64; 60];
        let eval = evaluate_tuning_cycle(&limits, &limits, 60_000, 1_000.0, &history, 10.0, 10.0);

        assert!(eval.reality_check_applied);
        assert_eq!(eval.updated_last_tuning_score, eval.baseline_u64);
        assert!(!eval.accepted_improvement);
    }

    #[test]
    fn seeding_adjustment_disables_disk_write_trades_and_sets_zero_write_slots() {
        let limits = CalculatedLimits {
            reserve_permits: 20,
            max_connected_peers: 64,
            disk_read_permits: 12,
            disk_write_permits: 10,
        };
        let mut rng = StdRng::seed_from_u64(123);

        for _ in 0..200 {
            let (next, _desc) = make_random_adjustment_with_rng(limits.clone(), true, &mut rng);
            assert_eq!(next.disk_write_permits, 0);
        }
    }

    #[test]
    fn seeding_adjustment_preserves_total_disk_slots_by_moving_write_to_read() {
        let limits = CalculatedLimits {
            reserve_permits: 20,
            max_connected_peers: 64,
            disk_read_permits: 12,
            disk_write_permits: 10,
        };
        let expected_total = limits
            .reserve_permits
            .saturating_add(limits.max_connected_peers)
            .saturating_add(limits.disk_read_permits)
            .saturating_add(limits.disk_write_permits);
        let mut rng = StdRng::seed_from_u64(321);

        for _ in 0..200 {
            let (next, _desc) = make_random_adjustment_with_rng(limits.clone(), true, &mut rng);
            assert_eq!(next.disk_write_permits, 0);
            let next_total = next
                .reserve_permits
                .saturating_add(next.max_connected_peers)
                .saturating_add(next.disk_read_permits)
                .saturating_add(next.disk_write_permits);
            assert_eq!(next_total, expected_total);
        }
    }

    #[test]
    fn normalize_limits_for_mode_seeding_zeros_write_and_preserves_total() {
        let limits = CalculatedLimits {
            reserve_permits: 20,
            max_connected_peers: 64,
            disk_read_permits: 12,
            disk_write_permits: 10,
        };
        let normalized = normalize_limits_for_mode(&limits, true);
        let before_total = limits
            .reserve_permits
            .saturating_add(limits.max_connected_peers)
            .saturating_add(limits.disk_read_permits)
            .saturating_add(limits.disk_write_permits);
        let after_total = normalized
            .reserve_permits
            .saturating_add(normalized.max_connected_peers)
            .saturating_add(normalized.disk_read_permits)
            .saturating_add(normalized.disk_write_permits);
        assert_eq!(normalized.disk_write_permits, 0);
        assert_eq!(before_total, after_total);
    }

    #[test]
    fn normalize_limits_for_mode_seeding_targets_70_30_peer_read_and_zero_reserve_write() {
        let limits = CalculatedLimits {
            reserve_permits: 20,
            max_connected_peers: 64,
            disk_read_permits: 12,
            disk_write_permits: 10,
        };
        let normalized = normalize_limits_for_mode(&limits, true);
        assert_eq!(normalized.max_connected_peers, 74);
        assert_eq!(normalized.disk_read_permits, 32);
        assert_eq!(normalized.reserve_permits, 0);
        assert_eq!(normalized.disk_write_permits, 0);
    }

    #[test]
    fn normalize_limits_for_mode_downloading_targets_30_70_read_write_split() {
        let limits = CalculatedLimits {
            reserve_permits: 20,
            max_connected_peers: 64,
            disk_read_permits: 12,
            disk_write_permits: 10,
        };
        let normalized = normalize_limits_for_mode(&limits, false);
        let before_disk_total = limits.disk_read_permits + limits.disk_write_permits;
        let after_disk_total = normalized.disk_read_permits + normalized.disk_write_permits;
        assert_eq!(before_disk_total, after_disk_total);
        assert_eq!(normalized.disk_read_permits, 6);
        assert_eq!(normalized.disk_write_permits, 16);
    }

    #[test]
    fn tuning_controller_fixed_policy_uses_default_lookback_and_countdown() {
        let limits = CalculatedLimits {
            reserve_permits: 20,
            max_connected_peers: 64,
            disk_read_permits: 12,
            disk_write_permits: 10,
        };
        let mut controller = TuningController::new_fixed(limits);
        assert_eq!(controller.cadence_secs(), DEFAULT_TUNING_CADENCE_SECS);
        assert_eq!(controller.lookback_secs(), DEFAULT_TUNING_LOOKBACK_SECS);
        assert_eq!(controller.countdown_secs(), DEFAULT_TUNING_CADENCE_SECS);

        controller.on_second_tick();
        assert_eq!(
            controller.countdown_secs(),
            DEFAULT_TUNING_CADENCE_SECS.saturating_sub(1)
        );
    }

    #[test]
    fn tuning_controller_objective_reset_clears_scores_and_ema() {
        let limits = CalculatedLimits {
            reserve_permits: 20,
            max_connected_peers: 64,
            disk_read_permits: 12,
            disk_write_permits: 10,
        };
        let mut controller = TuningController::new_fixed(limits.clone());
        let history = [30_000u64; 60];
        let _ = controller.evaluate_cycle(&limits, &history, 12.0, 10.0);
        assert!(controller.state().current_tuning_score > 0);

        controller.reset_for_objective_change(&limits);
        assert_eq!(controller.state().last_tuning_score, 0);
        assert_eq!(controller.state().current_tuning_score, 0);
        assert_eq!(controller.state().baseline_speed_ema, 0.0);
    }

    #[test]
    fn tuning_controller_evaluate_cycle_resets_countdown_and_tracks_best() {
        let limits = CalculatedLimits {
            reserve_permits: 20,
            max_connected_peers: 64,
            disk_read_permits: 12,
            disk_write_permits: 10,
        };
        let mut controller = TuningController::new_fixed(limits.clone());
        controller.on_second_tick();
        controller.on_second_tick();
        assert_eq!(controller.countdown_secs(), DEFAULT_TUNING_CADENCE_SECS - 2);

        let strong_history = [40_000u64; 60];
        let eval = controller.evaluate_cycle(&limits, &strong_history, 8.0, 10.0);

        assert_eq!(controller.countdown_secs(), DEFAULT_TUNING_CADENCE_SECS);
        assert!(eval.accepted_improvement);
        assert_eq!(controller.state().last_tuning_score, eval.new_score);
        assert_eq!(
            controller.state().last_tuning_limits.max_connected_peers,
            limits.max_connected_peers
        );
    }

    #[test]
    fn adaptive_controller_starts_fast_with_linked_lookback() {
        let limits = CalculatedLimits {
            reserve_permits: 20,
            max_connected_peers: 64,
            disk_read_permits: 12,
            disk_write_permits: 10,
        };
        let controller = TuningController::new_adaptive(limits);
        assert_eq!(controller.cadence_secs(), FAST_START_CADENCE_SECS);
        assert!(controller.lookback_secs() <= controller.cadence_secs() as usize);
        assert_eq!(controller.countdown_secs(), FAST_START_CADENCE_SECS);
    }

    #[test]
    fn adaptive_controller_backs_off_after_stagnation() {
        let limits = CalculatedLimits {
            reserve_permits: 20,
            max_connected_peers: 64,
            disk_read_permits: 12,
            disk_write_permits: 10,
        };
        let mut controller = TuningController::new_adaptive(limits.clone());

        let history_good = [40_000u64; 60];
        let _ = controller.evaluate_cycle(&limits, &history_good, 8.0, 10.0);
        let cadence_after_accept = controller.cadence_secs();

        let history_same = [40_000u64; 60];
        let _ = controller.evaluate_cycle(&limits, &history_same, 8.0, 10.0);
        let cadence_after_first_stall = controller.cadence_secs();

        let _ = controller.evaluate_cycle(&limits, &history_same, 8.0, 10.0);
        let cadence_after_second_stall = controller.cadence_secs();

        assert!(cadence_after_accept <= FAST_START_CADENCE_SECS);
        assert!(cadence_after_accept >= MIN_TUNING_CADENCE_SECS);
        assert_eq!(cadence_after_first_stall, cadence_after_accept);
        assert!(cadence_after_second_stall > cadence_after_first_stall);
    }

    #[test]
    fn adaptive_controller_speeds_up_on_rapid_regression() {
        let limits = CalculatedLimits {
            reserve_permits: 20,
            max_connected_peers: 64,
            disk_read_permits: 12,
            disk_write_permits: 10,
        };
        let mut controller = TuningController::new_adaptive(limits.clone());

        let history_good = [50_000u64; 60];
        let _ = controller.evaluate_cycle(&limits, &history_good, 8.0, 10.0);

        let history_same = [50_000u64; 60];
        let _ = controller.evaluate_cycle(&limits, &history_same, 8.0, 10.0);
        let _ = controller.evaluate_cycle(&limits, &history_same, 8.0, 10.0);
        let cadence_before_drop = controller.cadence_secs();

        let history_drop = [10_000u64; 60];
        let _ = controller.evaluate_cycle(&limits, &history_drop, 8.0, 10.0);
        let cadence_after_drop = controller.cadence_secs();

        assert!(cadence_before_drop > MIN_TUNING_CADENCE_SECS);
        assert!(cadence_after_drop < cadence_before_drop);
    }

    #[test]
    fn adaptive_controller_forces_backoff_when_change_pressure_is_high() {
        let limits = CalculatedLimits {
            reserve_permits: 20,
            max_connected_peers: 64,
            disk_read_permits: 12,
            disk_write_permits: 10,
        };
        let mut controller = TuningController::new_adaptive(limits.clone());
        controller.cadence_change_pressure = CADENCE_CHANGE_PRESSURE_TRIGGER - 1;

        let history_good = [40_000u64; 60];
        let _ = controller.evaluate_cycle(&limits, &history_good, 8.0, 10.0);
        assert!(controller.cadence_secs() > FAST_START_CADENCE_SECS);
        assert!(controller.cadence_change_pressure < CADENCE_CHANGE_PRESSURE_TRIGGER);
    }

    #[test]
    fn adaptive_controller_limits_regression_speedups_then_backs_off() {
        let limits = CalculatedLimits {
            reserve_permits: 20,
            max_connected_peers: 64,
            disk_read_permits: 12,
            disk_write_permits: 10,
        };
        let mut controller = TuningController::new_adaptive(limits.clone());

        let history_good = [50_000u64; 60];
        let _ = controller.evaluate_cycle(&limits, &history_good, 8.0, 10.0);
        let baseline_cadence = controller.cadence_secs();
        let history_drop = [10_000u64; 60];
        let mut saw_backoff = false;
        let mut previous = baseline_cadence;

        for _ in 0..10 {
            let _ = controller.evaluate_cycle(&limits, &history_drop, 8.0, 10.0);
            let current = controller.cadence_secs();
            if current > previous {
                saw_backoff = true;
                break;
            }
            previous = current;
        }

        assert!(saw_backoff);
    }

    #[test]
    fn adaptive_controller_decays_stale_best_after_repeated_no_improvement() {
        let limits = CalculatedLimits {
            reserve_permits: 20,
            max_connected_peers: 64,
            disk_read_permits: 12,
            disk_write_permits: 10,
        };
        let mut controller = TuningController::new_adaptive(limits.clone());

        let history_good = [60_000u64; 60];
        let _ = controller.evaluate_cycle(&limits, &history_good, 8.0, 10.0);
        let best_before = controller.state().last_tuning_score;
        let history_worse = [35_000u64; 60];

        for _ in 0..(STALE_BEST_DECAY_START_CYCLES + 1) {
            let _ = controller.evaluate_cycle(&limits, &history_worse, 8.0, 10.0);
        }

        assert!(controller.state().last_tuning_score < best_before);
    }
}
