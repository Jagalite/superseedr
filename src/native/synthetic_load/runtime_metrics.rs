// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Observational Tokio counters; sampling does not change runtime configuration.
//! Counters are published in batches and snapshots are not globally atomic. Busy
//! time measures wall time processing work, not process CPU time, and an interval
//! busy fraction can exceed one when an earlier batch is published late. Queue
//! depths are sampled gauges, so their observed peaks can miss intervening peaks.

use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Instant;
use tokio::runtime::{Handle, RuntimeMetrics};

#[derive(Clone, Debug, Serialize)]
pub(super) struct RuntimeSample {
    pub elapsed_seconds: f64,
    pub num_workers: usize,
    pub alive_tasks: usize,
    pub global_queue_depth: usize,
    pub workers: Vec<WorkerSample>,
    pub details: Option<RuntimeDetails>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct WorkerSample {
    pub worker: usize,
    pub busy_ns: Option<u64>,
    pub park_count: Option<u64>,
    pub busy_fraction: Option<f64>,
    pub details: Option<WorkerDetails>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct RuntimeDetails {
    pub counters_delta: RuntimeCounters,
    pub blocking_threads: usize,
    pub idle_blocking_threads: usize,
    pub blocking_queue_depth: usize,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct WorkerDetails {
    pub counters_delta: WorkerCounters,
    pub local_queue_depth: usize,
    /// Tokio's moving average, not this sample's interval mean; unavailable on
    /// the current-thread runtime even when detailed metrics are compiled in.
    pub mean_poll_time_ewma_ns: Option<u64>,
}

macro_rules! counters {
    ($name:ident { $($field:ident),+ $(,)? }) => {
        #[derive(Clone, Debug, Default, Serialize)]
        pub(super) struct $name { $(pub $field: Option<u64>),+ }

        impl $name {
            fn subtract(&mut self, previous: &Self) {
                $(self.$field = delta(self.$field, previous.$field);)+
            }

            fn add(&mut self, observed: &Self) {
                $(if let Some(value) = observed.$field {
                    self.$field = Some(self.$field.unwrap_or(0).saturating_add(value));
                })+
            }
        }
    };
}

counters!(RuntimeCounters {
    spawned_tasks,
    remote_schedules,
    forced_yields,
    io_ready_events,
});
counters!(WorkerCounters {
    polls,
    steals,
    steal_operations,
    local_schedules,
    overflows,
});

pub(super) struct RuntimeSampler {
    metrics: RuntimeMetrics,
    previous: RuntimeSample,
    previous_at: Instant,
    multi_thread: bool,
}

impl RuntimeSampler {
    pub(super) fn new() -> Self {
        let handle = Handle::current();
        let metrics = handle.metrics();
        let multi_thread = matches!(
            handle.runtime_flavor(),
            tokio::runtime::RuntimeFlavor::MultiThread
        );
        let previous_at = Instant::now();
        let previous = read_cumulative(&metrics, multi_thread);
        Self {
            metrics,
            previous,
            previous_at,
            multi_thread,
        }
    }

    pub(super) fn sample(&mut self) -> RuntimeSample {
        let now = Instant::now();
        let current = read_cumulative(&self.metrics, self.multi_thread);
        let elapsed = now.duration_since(self.previous_at).as_secs_f64();
        let sample = interval(current.clone(), &self.previous, elapsed);
        self.previous = current;
        self.previous_at = now;
        sample
    }
}

// Private snapshots reuse the output shape with cumulative counter values;
// `interval` converts every counter into a delta before exposing the sample.
fn read_cumulative(metrics: &RuntimeMetrics, multi_thread: bool) -> RuntimeSample {
    let workers = (0..metrics.num_workers())
        .map(|worker| {
            #[cfg(target_has_atomic = "64")]
            let (busy_ns, park_count) = (
                Some(metrics.worker_total_busy_duration(worker).as_nanos() as u64),
                Some(metrics.worker_park_count(worker)),
            );
            #[cfg(not(target_has_atomic = "64"))]
            let (busy_ns, park_count) = (None, None);
            WorkerSample {
                worker,
                busy_ns,
                park_count,
                busy_fraction: None,
                details: worker_details(metrics, worker, multi_thread),
            }
        })
        .collect();
    RuntimeSample {
        elapsed_seconds: 0.0,
        num_workers: metrics.num_workers(),
        alive_tasks: metrics.num_alive_tasks(),
        global_queue_depth: metrics.global_queue_depth(),
        workers,
        details: runtime_details(metrics),
    }
}

fn runtime_details(metrics: &RuntimeMetrics) -> Option<RuntimeDetails> {
    #[cfg(tokio_unstable)]
    {
        #[cfg(target_has_atomic = "64")]
        let counters_delta = RuntimeCounters {
            spawned_tasks: Some(metrics.spawned_tasks_count()),
            remote_schedules: Some(metrics.remote_schedule_count()),
            // Counts cooperative-budget exhaustion, not voluntary yield_now().
            forced_yields: Some(metrics.budget_forced_yield_count()),
            io_ready_events: Some(metrics.io_driver_ready_count()),
        };
        #[cfg(not(target_has_atomic = "64"))]
        let counters_delta = RuntimeCounters::default();
        Some(RuntimeDetails {
            counters_delta,
            blocking_threads: metrics.num_blocking_threads(),
            idle_blocking_threads: metrics.num_idle_blocking_threads(),
            blocking_queue_depth: metrics.blocking_queue_depth(),
        })
    }
    #[cfg(not(tokio_unstable))]
    {
        let _ = metrics;
        None
    }
}

fn worker_details(
    metrics: &RuntimeMetrics,
    worker: usize,
    multi_thread: bool,
) -> Option<WorkerDetails> {
    #[cfg(tokio_unstable)]
    {
        #[cfg(target_has_atomic = "64")]
        let (counters_delta, mean_poll_time_ewma_ns) = (
            WorkerCounters {
                polls: Some(metrics.worker_poll_count(worker)),
                steals: Some(metrics.worker_steal_count(worker)),
                steal_operations: Some(metrics.worker_steal_operations(worker)),
                local_schedules: Some(metrics.worker_local_schedule_count(worker)),
                overflows: Some(metrics.worker_overflow_count(worker)),
            },
            multi_thread.then(|| metrics.worker_mean_poll_time(worker).as_nanos() as u64),
        );
        #[cfg(not(target_has_atomic = "64"))]
        let (counters_delta, mean_poll_time_ewma_ns) = {
            let _ = multi_thread;
            (WorkerCounters::default(), None)
        };
        Some(WorkerDetails {
            counters_delta,
            mean_poll_time_ewma_ns,
            local_queue_depth: metrics.worker_local_queue_depth(worker),
        })
    }
    #[cfg(not(tokio_unstable))]
    {
        let _ = (metrics, worker, multi_thread);
        None
    }
}

fn delta(current: Option<u64>, previous: Option<u64>) -> Option<u64> {
    // A reset/wrap cannot establish a valid interval count; report unavailable.
    current?.checked_sub(previous?)
}

fn fraction(busy_ns: u64, elapsed_seconds: f64) -> Option<f64> {
    (elapsed_seconds > 0.0 && elapsed_seconds.is_finite())
        .then(|| busy_ns as f64 / 1_000_000_000.0 / elapsed_seconds)
}

fn interval(mut current: RuntimeSample, previous: &RuntimeSample, elapsed: f64) -> RuntimeSample {
    current.elapsed_seconds = elapsed;
    for worker in &mut current.workers {
        let prior = previous
            .workers
            .get(worker.worker)
            .filter(|prior| prior.worker == worker.worker);
        worker.busy_ns = delta(worker.busy_ns, prior.and_then(|prior| prior.busy_ns));
        worker.park_count = delta(worker.park_count, prior.and_then(|prior| prior.park_count));
        worker.busy_fraction = worker.busy_ns.and_then(|busy| fraction(busy, elapsed));
        if let Some(details) = worker.details.as_mut() {
            let counters = prior
                .and_then(|prior| prior.details.as_ref())
                .map(|details| details.counters_delta.clone())
                .unwrap_or_default();
            details.counters_delta.subtract(&counters);
        }
    }
    if let Some(details) = current.details.as_mut() {
        let counters = previous
            .details
            .as_ref()
            .map(|details| details.counters_delta.clone())
            .unwrap_or_default();
        details.counters_delta.subtract(&counters);
    }
    current
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct RuntimeSummary {
    pub samples: u64,
    pub observed_seconds: f64,
    pub num_workers: usize,
    pub peak_alive_tasks: usize,
    pub peak_global_queue_depth: usize,
    pub total_busy_ns: Option<u64>,
    pub total_park_count: Option<u64>,
    pub mean_worker_busy_fraction: Option<f64>,
    /// Maximum of each worker's aggregate observed fraction, not a peak sample.
    pub max_worker_busy_fraction: Option<f64>,
    pub details: Option<RuntimeDetailsSummary>,
    #[serde(skip)]
    worker_totals: BTreeMap<usize, (u64, f64)>,
}

/// Totals include only available counters in observed intervals; unavailable or
/// reset counters do not contribute invented zero measurements. Gauge peaks are
/// sampled peaks, not high-water marks observed continuously by the runtime.
#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct RuntimeDetailsSummary {
    pub runtime_counters: RuntimeCounters,
    pub worker_counters: WorkerCounters,
    pub peak_blocking_threads: usize,
    pub peak_active_blocking_threads: usize,
    pub peak_blocking_queue_depth: usize,
    pub peak_total_local_queue_depth: usize,
}

impl RuntimeSummary {
    pub(super) fn observe(&mut self, sample: &RuntimeSample) {
        self.samples += 1;
        self.observed_seconds += sample.elapsed_seconds;
        self.num_workers = sample.num_workers;
        self.peak_alive_tasks = self.peak_alive_tasks.max(sample.alive_tasks);
        self.peak_global_queue_depth = self.peak_global_queue_depth.max(sample.global_queue_depth);
        if let Some(observed) = &sample.details {
            let details = self
                .details
                .get_or_insert_with(RuntimeDetailsSummary::default);
            details.runtime_counters.add(&observed.counters_delta);
            details.peak_blocking_threads =
                details.peak_blocking_threads.max(observed.blocking_threads);
            details.peak_active_blocking_threads = details.peak_active_blocking_threads.max(
                observed
                    .blocking_threads
                    .saturating_sub(observed.idle_blocking_threads),
            );
            details.peak_blocking_queue_depth = details
                .peak_blocking_queue_depth
                .max(observed.blocking_queue_depth);
            let mut local_depth = 0_usize;
            for worker in sample
                .workers
                .iter()
                .filter_map(|worker| worker.details.as_ref())
            {
                details.worker_counters.add(&worker.counters_delta);
                local_depth = local_depth.saturating_add(worker.local_queue_depth);
            }
            details.peak_total_local_queue_depth =
                details.peak_total_local_queue_depth.max(local_depth);
        }
        for worker in &sample.workers {
            if let Some(busy) = worker.busy_ns {
                self.total_busy_ns = Some(self.total_busy_ns.unwrap_or(0).saturating_add(busy));
                let total = self.worker_totals.entry(worker.worker).or_default();
                total.0 = total.0.saturating_add(busy);
                total.1 += sample.elapsed_seconds;
            }
            if let Some(parks) = worker.park_count {
                self.total_park_count =
                    Some(self.total_park_count.unwrap_or(0).saturating_add(parks));
            }
        }
        // Weight by observed worker-time; missing/reset counters do not invent
        // idle capacity. This also handles intervals of different lengths.
        let (busy, seconds) = self
            .worker_totals
            .values()
            .fold((0_u64, 0.0), |sum, total| {
                (sum.0.saturating_add(total.0), sum.1 + total.1)
            });
        self.mean_worker_busy_fraction = fraction(busy, seconds);
        self.max_worker_busy_fraction = self
            .worker_totals
            .values()
            .filter_map(|&(busy, seconds)| fraction(busy, seconds))
            .reduce(f64::max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(busy: &[u64], parks: u64) -> RuntimeSample {
        RuntimeSample {
            elapsed_seconds: 0.0,
            num_workers: busy.len(),
            alive_tasks: 3,
            global_queue_depth: 2,
            workers: busy
                .iter()
                .enumerate()
                .map(|(worker, &busy)| WorkerSample {
                    worker,
                    busy_ns: Some(busy),
                    park_count: Some(parks),
                    busy_fraction: None,
                    details: None,
                })
                .collect(),
            details: None,
        }
    }

    #[test]
    fn interval_uses_deltas_and_retains_current_gauges() {
        let before = snapshot(&[100, 200], 2);
        let mut after = snapshot(&[600, 800], 5);
        after.alive_tasks = 9;
        let sample = interval(after, &before, 0.000_001);
        assert_eq!(sample.workers[0].busy_ns, Some(500));
        assert_eq!(sample.workers[0].park_count, Some(3));
        assert_eq!(sample.workers[0].busy_fraction, Some(0.5));
        assert_eq!(sample.alive_tasks, 9);
        // Batched counters may report more busy time than this wall interval.
        assert_eq!(fraction(2_000_000_000, 1.0), Some(2.0));
    }

    #[test]
    fn reset_missing_and_zero_duration_are_not_fabricated() {
        let sample = interval(snapshot(&[2], 1), &snapshot(&[8], 4), 0.0);
        assert_eq!(sample.workers[0].busy_ns, None);
        assert_eq!(sample.workers[0].park_count, None);
        assert_eq!(sample.workers[0].busy_fraction, None);
        assert_eq!(delta(Some(4), None), None);
        assert_eq!(delta(None, Some(4)), None);
        assert_eq!(fraction(5, 0.0), None);
        let mut summary = RuntimeSummary::default();
        summary.observe(&sample);
        assert_eq!(summary.total_busy_ns, None);
        assert_eq!(summary.mean_worker_busy_fraction, None);
    }

    #[test]
    fn summary_weights_time_and_tracks_each_worker() {
        let mut first = snapshot(&[1_000_000_000, 0], 1);
        first.elapsed_seconds = 1.0;
        let mut second = snapshot(&[0, 1_000_000_000], 2);
        second.elapsed_seconds = 3.0;
        second.alive_tasks = 7;
        let mut summary = RuntimeSummary::default();
        summary.observe(&first);
        summary.observe(&second);
        assert_eq!(summary.samples, 2);
        assert_eq!(summary.observed_seconds, 4.0);
        assert_eq!(summary.peak_alive_tasks, 7);
        assert_eq!(summary.total_busy_ns, Some(2_000_000_000));
        assert_eq!(summary.total_park_count, Some(6));
        assert_eq!(summary.mean_worker_busy_fraction, Some(0.25));
        assert_eq!(summary.max_worker_busy_fraction, Some(0.25));
    }

    #[test]
    fn detailed_counter_resets_are_unknown() {
        let mut counters = RuntimeCounters {
            spawned_tasks: Some(12),
            forced_yields: Some(4),
            ..Default::default()
        };
        counters.subtract(&RuntimeCounters {
            spawned_tasks: Some(8),
            forced_yields: Some(7),
            ..Default::default()
        });
        assert_eq!(counters.spawned_tasks, Some(4));
        assert_eq!(counters.forced_yields, None);
        assert_eq!(counters.io_ready_events, None);
    }

    #[test]
    fn detailed_intervals_and_summary_preserve_counter_and_gauge_semantics() {
        let with_details = |mut sample: RuntimeSample, count: u64, gauge: usize| {
            sample.details = Some(RuntimeDetails {
                counters_delta: RuntimeCounters {
                    spawned_tasks: Some(count),
                    remote_schedules: Some(count),
                    forced_yields: Some(count),
                    io_ready_events: None,
                },
                blocking_threads: gauge + 1,
                idle_blocking_threads: 1,
                blocking_queue_depth: gauge,
            });
            for worker in &mut sample.workers {
                worker.details = Some(WorkerDetails {
                    counters_delta: WorkerCounters {
                        polls: Some(count),
                        steals: Some(count),
                        steal_operations: Some(count),
                        local_schedules: Some(count),
                        overflows: None,
                    },
                    local_queue_depth: gauge,
                    mean_poll_time_ewma_ns: Some(count),
                });
            }
            sample
        };
        let baseline = with_details(snapshot(&[0, 0], 0), 10, 1);
        let first = with_details(snapshot(&[1_000_000_000, 0], 1), 13, 4);
        let second = with_details(snapshot(&[1_000_000_000, 1_000_000_000], 3), 18, 2);
        let first_interval = interval(first.clone(), &baseline, 1.0);
        let second_interval = interval(second, &first, 3.0);
        let worker = first_interval.workers[0].details.as_ref().unwrap();
        assert_eq!(worker.counters_delta.polls, Some(3));
        assert_eq!(worker.local_queue_depth, 4);
        assert_eq!(worker.mean_poll_time_ewma_ns, Some(13));
        let mut summary = RuntimeSummary::default();
        summary.observe(&first_interval);
        summary.observe(&second_interval);
        assert_eq!(summary.num_workers, 2);
        assert_eq!(summary.mean_worker_busy_fraction, Some(0.25));
        let details = summary.details.unwrap();
        assert_eq!(details.runtime_counters.spawned_tasks, Some(8));
        assert_eq!(details.runtime_counters.remote_schedules, Some(8));
        assert_eq!(details.runtime_counters.forced_yields, Some(8));
        assert_eq!(details.runtime_counters.io_ready_events, None);
        assert_eq!(details.worker_counters.polls, Some(16));
        assert_eq!(details.worker_counters.steals, Some(16));
        assert_eq!(details.worker_counters.steal_operations, Some(16));
        assert_eq!(details.worker_counters.local_schedules, Some(16));
        assert_eq!(details.worker_counters.overflows, None);
        assert_eq!(details.peak_blocking_threads, 5);
        assert_eq!(details.peak_active_blocking_threads, 4);
        assert_eq!(details.peak_blocking_queue_depth, 4);
        assert_eq!(details.peak_total_local_queue_depth, 8);
    }

    #[tokio::test]
    async fn live_sampler_observes_runtime_without_network() {
        let mut sampler = RuntimeSampler::new();
        let task = tokio::spawn(std::future::pending::<()>());
        let sample = sampler.sample();
        assert_eq!(sample.num_workers, 1);
        assert!(sample.alive_tasks >= 1);
        assert_eq!(sample.workers.len(), 1);
        assert!(serde_json::to_string(&sample).is_ok());
        let mut summary = RuntimeSummary::default();
        summary.observe(&sample);
        assert_eq!(summary.num_workers, 1);
        assert!(serde_json::to_string(&summary).is_ok());
        #[cfg(not(tokio_unstable))]
        {
            assert!(sample.details.is_none());
            assert!(summary.details.is_none());
        }
        #[cfg(tokio_unstable)]
        {
            assert!(summary.details.is_some());
            assert!(sample.workers[0]
                .details
                .as_ref()
                .unwrap()
                .mean_poll_time_ewma_ns
                .is_none());
        }
        assert_eq!(
            sample.workers[0].busy_ns.is_some(),
            cfg!(target_has_atomic = "64")
        );
        task.abort();
        let _ = task.await;
    }
}
