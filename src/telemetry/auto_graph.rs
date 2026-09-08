// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::GraphDisplayMode;

const SIGNAL_ALPHA: f64 = 0.35;
const BASELINE_ALPHA: f64 = 0.02;
const MINIMUM_RISE_BPS: f64 = 4_096.0;
const BURST_COOLDOWN_SECS: u64 = 120;

fn elapsed_alpha(alpha: f64, elapsed_secs: u64) -> f64 {
    if elapsed_secs == 1 {
        alpha
    } else {
        1.0 - (1.0 - alpha).powf(elapsed_secs as f64)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct BurstPeriod {
    started_unix: u64,
    last_active_unix: u64,
    mode: GraphDisplayMode,
}

/// Live-only burst detection and framing. Persisted history is deliberately not
/// an input: expiring a history bucket cannot start, extend, or erase a burst.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct AutoGraphActivity {
    last_sample_unix: Option<u64>,
    recent_rates: [f64; 3],
    samples_seen: u8,
    baseline_bps: f64,
    smoothed_bps: f64,
    deviation_bps: f64,
    burst: Option<BurstPeriod>,
}

impl AutoGraphActivity {
    pub(crate) fn mode(&self) -> GraphDisplayMode {
        self.burst
            .map(|burst| burst.mode)
            .unwrap_or(GraphDisplayMode::OneMinute)
    }

    pub(crate) fn observe(&mut self, now_unix: u64, rate_bps: u64) {
        let rate = rate_bps as f64;
        let mut elapsed_secs = 1;
        if let Some(previous) = self.last_sample_unix {
            if now_unix == previous {
                return;
            }
            if now_unix < previous || now_unix - previous > BURST_COOLDOWN_SECS {
                // A clock rollback or a long sampling interruption needs a new
                // baseline, rather than invented activity across missing time.
                *self = Self::default();
            } else {
                // Keep real observations when ticks are delayed. Resetting on
                // every missed second prevents a slower stream detecting rises.
                elapsed_secs = now_unix - previous;
            }
        }
        self.last_sample_unix = Some(now_unix);
        self.recent_rates.rotate_left(1);
        self.recent_rates[2] = rate;
        let warming_up = self.samples_seen < 3;
        self.samples_seen = (self.samples_seen + 1).min(3);

        if self.samples_seen < 3 {
            self.baseline_bps = if self.samples_seen == 1 {
                rate
            } else {
                (self.recent_rates[1] + rate) / 2.0
            };
            self.smoothed_bps = self.baseline_bps;
            return;
        }

        // A single zero or high sample must not redefine the signal or baseline.
        let mut sorted = self.recent_rates;
        sorted.sort_by(f64::total_cmp);
        let filtered = sorted[1];
        if warming_up {
            self.baseline_bps = filtered;
            self.smoothed_bps = filtered;
            return;
        }
        self.smoothed_bps +=
            elapsed_alpha(SIGNAL_ALPHA, elapsed_secs) * (filtered - self.smoothed_bps);
        let margin = (self.baseline_bps * 0.5)
            .max(self.deviation_bps * 4.0)
            .max(MINIMUM_RISE_BPS);
        let enter_threshold = self.baseline_bps + margin;
        let continue_threshold = self.baseline_bps + margin * 0.5;

        if self.burst.is_none() && self.smoothed_bps > enter_threshold && filtered > enter_threshold
        {
            self.burst = Some(BurstPeriod {
                started_unix: now_unix,
                last_active_unix: now_unix,
                mode: GraphDisplayMode::OneMinute,
            });
        }

        if let Some(burst) = self.burst.as_mut() {
            if filtered > continue_threshold {
                burst.last_active_unix = now_unix;
                let span = now_unix.saturating_sub(burst.started_unix);
                // Leave about 20% for context, with a rolling 10m ceiling for
                // long bursts. Quiet time alone never widens the chart.
                let target = if span < 48 {
                    GraphDisplayMode::OneMinute
                } else if span < 240 {
                    GraphDisplayMode::FiveMinutes
                } else {
                    GraphDisplayMode::TenMinutes
                };
                if target.as_seconds() > burst.mode.as_seconds() {
                    burst.mode = target;
                }
            } else if now_unix.saturating_sub(burst.last_active_unix) > BURST_COOLDOWN_SECS {
                self.burst = None;
                self.smoothed_bps = filtered;
            }
        }

        // Freeze the reference during a burst, its cooldown, and a candidate
        // rise. Otherwise learning can absorb a moderate spike before detection,
        // or let a large spike hide the smaller spikes that follow it.
        if self.burst.is_none() && filtered <= continue_threshold {
            let error = filtered - self.baseline_bps;
            let alpha = elapsed_alpha(BASELINE_ALPHA, elapsed_secs);
            self.baseline_bps += alpha * error;
            self.deviation_bps += alpha * (error.abs().min(margin) - self.deviation_bps);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const BASELINE: u64 = 5_000_000;

    fn run(end: u64, rate: impl Fn(u64) -> u64) -> Vec<GraphDisplayMode> {
        let mut activity = AutoGraphActivity::default();
        (0..=end)
            .map(|second| {
                activity.observe(second, rate(second));
                activity.mode()
            })
            .collect()
    }

    #[test]
    fn smoothing_weights_follow_elapsed_time() {
        for alpha in [SIGNAL_ALPHA, BASELINE_ALPHA] {
            let mut repeated = 0.0;
            for _ in 0..3 {
                repeated += alpha * (100.0 - repeated);
            }
            assert!((elapsed_alpha(alpha, 3) * 100.0 - repeated).abs() < 1e-12);
        }
    }

    #[test]
    fn delayed_samples_frame_grouped_spikes_and_reset_by_wall_time() {
        for cadence in [&[1][..], &[2], &[3], &[1, 3, 2, 5]] {
            let mut activity = AutoGraphActivity::default();
            let mut t = 0;
            let mut index = 0;
            while t <= 1_200 {
                let active = (600..720).contains(&t) || (780..900).contains(&t);
                activity.observe(t, if active { 100_000_000 } else { BASELINE });
                if (680..720).contains(&t) {
                    assert_eq!(
                        activity.mode(),
                        GraphDisplayMode::FiveMinutes,
                        "{cadence:?} at {t}"
                    );
                }
                if (870..1_010).contains(&t) {
                    assert_eq!(
                        activity.mode(),
                        GraphDisplayMode::TenMinutes,
                        "{cadence:?} at {t}"
                    );
                }
                if t >= 1_040 {
                    assert!(activity.burst.is_none(), "{cadence:?} at {t}");
                }
                t += cadence[index % cadence.len()];
                index += 1;
            }
        }
    }

    #[test]
    fn delayed_samples_still_reject_isolated_outliers() {
        for step in [2, 3] {
            for outlier in [0, u64::MAX] {
                let mut activity = AutoGraphActivity::default();
                for sample in 0..600 {
                    activity.observe(
                        sample * step,
                        if sample == 300 { outlier } else { BASELINE },
                    );
                    assert!(activity.burst.is_none());
                }
            }
        }
    }

    #[test]
    fn frames_a_two_minute_rise_and_stays_reset_after_it_ends() {
        let modes = run(2_000, |t| {
            if (600..720).contains(&t) {
                100_000_000
            } else {
                BASELINE
            }
        });
        assert!(modes[..600]
            .iter()
            .all(|m| *m == GraphDisplayMode::OneMinute));
        assert_eq!(modes[719], GraphDisplayMode::FiveMinutes);
        assert_eq!(modes[800], GraphDisplayMode::FiveMinutes);
        assert!(modes[850..]
            .iter()
            .all(|m| *m == GraphDisplayMode::OneMinute));
    }

    #[test]
    fn groups_different_height_spikes_across_two_minute_quiet_gaps() {
        let modes = run(1_300, |t| match t {
            600..630 => 100_000_000,
            750..780 => 20_000_000,
            900..930 => 40_000_000,
            1050..1080 => 10_000_000,
            _ => BASELINE,
        });
        assert_eq!(modes[779], GraphDisplayMode::FiveMinutes);
        assert_eq!(modes[929], GraphDisplayMode::TenMinutes);
        assert!(modes[929..1_200]
            .iter()
            .all(|m| *m == GraphDisplayMode::TenMinutes));
        assert_eq!(modes[1_210], GraphDisplayMode::OneMinute);
    }

    #[test]
    fn long_quiet_gaps_start_a_new_period() {
        let modes = run(1_400, |t| match t {
            600..900 | 1100..1120 => 100_000_000,
            _ => BASELINE,
        });
        assert_eq!(modes[899], GraphDisplayMode::TenMinutes);
        assert_eq!(modes[1_030], GraphDisplayMode::OneMinute);
        assert!(modes[1_030..]
            .iter()
            .all(|m| *m == GraphDisplayMode::OneMinute));
    }

    #[test]
    fn quiet_time_does_not_widen_a_short_burst() {
        let modes = run(900, |t| {
            if (600..620).contains(&t) {
                100_000_000
            } else {
                BASELINE
            }
        });
        assert!(modes.iter().all(|m| *m == GraphDisplayMode::OneMinute));
    }

    #[test]
    fn a_permanent_rise_holds_ten_minutes_without_relearning_the_burst() {
        let modes = run(10_000, |t| if t >= 600 { 100_000_000 } else { BASELINE });
        assert!(modes[850..]
            .iter()
            .all(|m| *m == GraphDisplayMode::TenMinutes));
    }

    #[test]
    fn ignores_isolated_zero_and_high_samples() {
        let clean = run(1_000, |t| if t >= 600 { 15_000_000 } else { BASELINE });
        for outlier in [0, u64::MAX] {
            let noisy = run(1_000, |t| match t {
                300 => outlier,
                600.. => 15_000_000,
                _ => BASELINE,
            });
            assert_eq!(clean, noisy);
        }
    }

    #[test]
    fn startup_uses_a_complete_median_before_detecting_changes() {
        for position in 0..3 {
            for outlier in [0, u64::MAX] {
                let modes = run(600, |t| if t == position { outlier } else { BASELINE });
                assert!(modes
                    .iter()
                    .all(|mode| *mode == GraphDisplayMode::OneMinute));
            }
        }
    }

    #[test]
    fn a_larger_later_peak_does_not_erase_the_period_start() {
        let mut activity = AutoGraphActivity::default();
        for t in 0..800 {
            activity.observe(t, if t >= 600 { 20_000_000 } else { BASELINE });
        }
        let started = activity.burst.unwrap().started_unix;
        for t in 800..1_000 {
            activity.observe(t, 100_000_000);
            assert_eq!(activity.burst.unwrap().started_unix, started);
        }
        assert_eq!(activity.mode(), GraphDisplayMode::TenMinutes);
    }

    #[test]
    fn clock_changes_and_sampling_gaps_do_not_reuse_stale_activity() {
        let mut activity = AutoGraphActivity::default();
        for t in 1_000..1_900 {
            activity.observe(t, if t >= 1_600 { 100_000_000 } else { BASELINE });
        }
        assert_eq!(activity.mode(), GraphDisplayMode::TenMinutes);
        let previous = activity;
        activity.observe(1_899, 0);
        assert_eq!(activity, previous);
        activity.observe(2_100, BASELINE);
        assert_eq!(activity.mode(), GraphDisplayMode::OneMinute);
        activity = previous;
        activity.observe(500, BASELINE);
        assert_eq!(activity.mode(), GraphDisplayMode::OneMinute);
    }

    #[test]
    fn a_sampling_gap_does_not_multiply_one_outlier_into_a_burst() {
        let mut activity = AutoGraphActivity::default();
        for t in 0..600 {
            activity.observe(t, BASELINE);
        }
        activity.observe(605, u64::MAX);
        for t in 606..900 {
            activity.observe(t, BASELINE);
            assert!(activity.burst.is_none());
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(96))]

        #[test]
        fn burst_grouping_is_stable_across_rates_gaps_and_small_noise(
            baseline in 100_000_u64..100_000_000,
            height in 2_u64..30,
            gap in 10_u64..121,
            shift in 0_u64..20,
            seed in any::<u32>(),
        ) {
            let mut activity = AutoGraphActivity::default();
            let start = 600 + shift;
            let end = start + 4 * (60 + gap);
            let mut random = seed;
            let mut detected = [false; 4];
            let mut first_start = None;
            let mut previous_mode = GraphDisplayMode::OneMinute;
            for t in 0..end + 800 {
                random = random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = 90 + u64::from(random % 21);
                let age = t.saturating_sub(start);
                let spike = t >= start && age / (60 + gap) < 4 && age % (60 + gap) < 60;
                let multiplier = if spike {
                    // Smaller spikes must remain visible after the first peak.
                    if age / (60 + gap) == 0 { height } else { 2 }
                } else { 1 };
                activity.observe(t, baseline * multiplier * noise / 100);
                prop_assert!(activity.mode().as_seconds() <= 600);
                if spike && age % (60 + gap) >= 15 {
                    prop_assert!(activity.burst.is_some());
                    let burst = activity.burst.unwrap();
                    prop_assert!(t.saturating_sub(burst.last_active_unix) <= 2);
                    detected[(age / (60 + gap)) as usize] = true;
                    if let Some(first) = first_start {
                        prop_assert_eq!(burst.started_unix, first);
                    } else {
                        first_start = Some(burst.started_unix);
                    }
                }
                if t >= start + 15 && t < end - gap {
                    prop_assert!(activity.mode().as_seconds() >= previous_mode.as_seconds());
                }
                if t >= end + 130 {
                    prop_assert_eq!(activity.mode(), GraphDisplayMode::OneMinute);
                }
                previous_mode = activity.mode();
            }
            prop_assert!(detected.iter().all(|d| *d));
        }
    }
}
