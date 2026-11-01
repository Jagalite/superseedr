// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Pacing for bringing previously complete torrents online at startup.
//!
//! Every torrent that comes online dials its whole peer list, so loading seeds back to back
//! piles up connections and memory. Seeds wait longer while downloads are active and back
//! off while memory or connection counts are climbing or swinging; downloads are not paced
//! here.

use std::collections::VecDeque;
use std::time::Duration;

pub(crate) const ROLLOUT_BASE_INTERVAL: Duration = Duration::from_secs(1);
pub(crate) const ROLLOUT_DOWNLOADING_SEED_INTERVAL: Duration = Duration::from_secs(5);
pub(crate) const ROLLOUT_MAX_SEED_INTERVAL: Duration = Duration::from_secs(30);
const ROLLOUT_WINDOW: Duration = Duration::from_secs(10);
const ROLLOUT_CALM_SECS_BEFORE_RECOVERY: u32 = 5;
/// Growth over one window that counts as a surge.
const ROLLOUT_MEMORY_SURGE_BYTES: f64 = 24.0 * 1024.0 * 1024.0;
const ROLLOUT_PEER_SURGE: f64 = 100.0;
/// Per-second memory swings this large (one standard deviation) count as unstable.
const ROLLOUT_MEMORY_SWING_BYTES: f64 = 4.0 * 1024.0 * 1024.0;

#[derive(Debug, Clone, Copy)]
pub(crate) struct RolloutSample<T> {
    pub(crate) at: T,
    pub(crate) memory_bytes: u64,
    pub(crate) connected_peers: usize,
    pub(crate) downloading: bool,
}

/// Generic over the clock so tests can drive it without real time.
#[derive(Debug, Clone)]
pub(crate) struct StartupRolloutPacer<T> {
    window: VecDeque<RolloutSample<T>>,
    seed_interval: Duration,
    calm_secs: u32,
    downloading: bool,
    volatile: bool,
    seed_deferred_at: Option<T>,
}

impl<T> Default for StartupRolloutPacer<T> {
    fn default() -> Self {
        Self {
            window: VecDeque::new(),
            seed_interval: ROLLOUT_BASE_INTERVAL,
            calm_secs: 0,
            downloading: false,
            volatile: false,
            seed_deferred_at: None,
        }
    }
}

pub(crate) trait RolloutClock: Copy {
    fn since(self, earlier: Self) -> Duration;
}

impl RolloutClock for crate::execution::time::Instant {
    fn since(self, earlier: Self) -> Duration {
        self.saturating_duration_since(earlier)
    }
}

impl<T: RolloutClock> StartupRolloutPacer<T> {
    /// Feed the latest state; call about once per second.
    pub(crate) fn observe(&mut self, sample: RolloutSample<T>) {
        // Repeated observations inside one second would shrink the window and fake calm.
        if let Some(last) = self.window.back() {
            if sample.at.since(last.at) < Duration::from_millis(900) {
                self.downloading = sample.downloading;
                return;
            }
        }
        self.downloading = sample.downloading;
        self.window.push_back(sample);
        while self
            .window
            .front()
            .is_some_and(|oldest| sample.at.since(oldest.at) > ROLLOUT_WINDOW)
        {
            self.window.pop_front();
        }

        self.volatile = self.window_is_volatile();
        if self.volatile {
            self.calm_secs = 0;
            self.seed_interval = (self.seed_interval * 2).min(ROLLOUT_MAX_SEED_INTERVAL);
        } else {
            self.calm_secs = self.calm_secs.saturating_add(1);
            if self.calm_secs >= ROLLOUT_CALM_SECS_BEFORE_RECOVERY {
                self.calm_secs = 0;
                self.seed_interval = (self.seed_interval / 2).max(ROLLOUT_BASE_INTERVAL);
            }
        }
    }

    fn window_is_volatile(&self) -> bool {
        let (Some(first), Some(last)) = (self.window.front(), self.window.back()) else {
            return false;
        };
        let memory_growth = last.memory_bytes as f64 - first.memory_bytes as f64;
        let peer_growth = last.connected_peers as f64 - first.connected_peers as f64;
        if memory_growth > ROLLOUT_MEMORY_SURGE_BYTES || peer_growth > ROLLOUT_PEER_SURGE {
            return true;
        }

        let deltas: Vec<f64> = self
            .window
            .iter()
            .zip(self.window.iter().skip(1))
            .map(|(earlier, later)| later.memory_bytes as f64 - earlier.memory_bytes as f64)
            .collect();
        if deltas.len() < 3 {
            return false;
        }
        let mean = deltas.iter().sum::<f64>() / deltas.len() as f64;
        let variance = deltas.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / deltas.len() as f64;
        variance.sqrt() > ROLLOUT_MEMORY_SWING_BYTES
    }

    /// Delay before the next startup load; downloads always use the base interval.
    pub(crate) fn delay_before(&self, next_is_seed: bool) -> Duration {
        if !next_is_seed {
            return ROLLOUT_BASE_INTERVAL;
        }
        let floor = if self.downloading {
            ROLLOUT_DOWNLOADING_SEED_INTERVAL
        } else {
            ROLLOUT_BASE_INTERVAL
        };
        self.seed_interval.max(floor)
    }

    /// A surge may defer a due seed for at most one maximum interval.
    /// Reset after allowing it so the next queued seed gets its own budget.
    pub(crate) fn seed_deferral_delay(&mut self, now: T) -> Option<Duration> {
        if !self.is_volatile() {
            self.seed_deferred_at = None;
            return None;
        }
        let deferred_at = *self.seed_deferred_at.get_or_insert(now);
        if now.since(deferred_at) >= ROLLOUT_MAX_SEED_INTERVAL {
            self.seed_deferred_at = None;
            return None;
        }
        Some(
            self.delay_before(true)
                .min(ROLLOUT_MAX_SEED_INTERVAL - now.since(deferred_at)),
        )
    }

    pub(crate) fn is_volatile(&self) -> bool {
        self.volatile
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    struct FakeSecs(u64);

    impl RolloutClock for FakeSecs {
        fn since(self, earlier: Self) -> Duration {
            Duration::from_secs(self.0.saturating_sub(earlier.0))
        }
    }

    const MIB: u64 = 1024 * 1024;

    fn sample(
        at: u64,
        memory_mib: u64,
        peers: usize,
        downloading: bool,
    ) -> RolloutSample<FakeSecs> {
        RolloutSample {
            at: FakeSecs(at),
            memory_bytes: memory_mib * MIB,
            connected_peers: peers,
            downloading,
        }
    }

    #[test]
    fn calm_idle_startup_loads_seeds_at_base_interval() {
        let mut pacer = StartupRolloutPacer::default();
        for second in 0..20 {
            pacer.observe(sample(second, 100, 10, false));
        }
        assert_eq!(pacer.delay_before(true), ROLLOUT_BASE_INTERVAL);
        assert!(!pacer.is_volatile());
    }

    #[test]
    fn active_download_slows_seeds_but_not_downloads() {
        let mut pacer = StartupRolloutPacer::default();
        pacer.observe(sample(0, 100, 10, true));
        assert_eq!(pacer.delay_before(true), ROLLOUT_DOWNLOADING_SEED_INTERVAL);
        assert_eq!(pacer.delay_before(false), ROLLOUT_BASE_INTERVAL);
    }

    #[test]
    fn peer_surge_backs_off_up_to_the_maximum() {
        let mut pacer = StartupRolloutPacer::default();
        for second in 0..10 {
            pacer.observe(sample(second, 100, second as usize * 40, false));
        }
        assert!(pacer.is_volatile());
        assert_eq!(pacer.delay_before(true), ROLLOUT_MAX_SEED_INTERVAL);
        assert_eq!(pacer.delay_before(false), ROLLOUT_BASE_INTERVAL);
    }

    #[test]
    fn memory_surge_backs_off_and_calm_recovers() {
        let mut pacer = StartupRolloutPacer::default();
        for second in 0..6 {
            pacer.observe(sample(second, 100 + second * 10, 10, false));
        }
        assert!(pacer.is_volatile());
        let backed_off = pacer.delay_before(true);
        assert!(backed_off > ROLLOUT_BASE_INTERVAL);

        // Flat memory: the surge leaves the window, then each calm stretch halves the gap.
        for second in 6..80 {
            pacer.observe(sample(second, 150, 10, false));
        }
        assert!(!pacer.is_volatile());
        assert_eq!(pacer.delay_before(true), ROLLOUT_BASE_INTERVAL);
    }

    #[test]
    fn erratic_memory_without_net_growth_counts_as_volatile() {
        let mut pacer = StartupRolloutPacer::default();
        for second in 0..10 {
            let memory = if second % 2 == 0 { 100 } else { 120 };
            pacer.observe(sample(second, memory, 10, false));
        }
        assert!(pacer.is_volatile());
    }

    #[test]
    fn sustained_volatility_cannot_starve_due_seeds() {
        let mut pacer = StartupRolloutPacer::default();
        for second in 0..=75 {
            pacer.observe(sample(
                second,
                if second % 2 == 0 { 100 } else { 120 },
                10,
                false,
            ));
            if second == 10 || second == 39 || second == 41 || second == 70 {
                assert!(pacer.seed_deferral_delay(FakeSecs(second)).is_some());
                if second == 39 {
                    assert_eq!(
                        pacer.seed_deferral_delay(FakeSecs(second)),
                        Some(Duration::from_secs(1))
                    );
                }
            } else if second == 40 || second == 71 {
                assert!(pacer.seed_deferral_delay(FakeSecs(second)).is_none());
            }
        }
    }

    #[test]
    fn calm_allows_a_deferred_seed_immediately() {
        let mut pacer = StartupRolloutPacer::default();
        for second in 0..=10 {
            pacer.observe(sample(
                second,
                if second % 2 == 0 { 100 } else { 120 },
                10,
                false,
            ));
        }
        assert!(pacer.seed_deferral_delay(FakeSecs(10)).is_some());
        for second in 11..=22 {
            pacer.observe(sample(second, 100, 10, false));
        }
        assert!(pacer.seed_deferral_delay(FakeSecs(22)).is_none());
    }

    #[test]
    fn bursts_of_observations_within_a_second_are_ignored() {
        let mut pacer = StartupRolloutPacer::default();
        pacer.observe(sample(0, 100, 10, false));
        for _ in 0..50 {
            pacer.observe(sample(0, 100, 10, false));
        }
        assert_eq!(pacer.window.len(), 1);
    }
}
