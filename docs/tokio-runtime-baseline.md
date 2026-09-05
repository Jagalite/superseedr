# Tokio runtime baseline

The first active local upload run sustained **474.72 MiB/s (3.982 Gbit/s)** with
payload in every measured sample. Eight Tokio workers reported **98.17% average
busy wall time**; sampler wakeup lateness stayed at or below **1.808 ms**. This
establishes a working instrumented baseline, not the CPU cost of Tokio's
scheduler in isolation.

Run: `run_20260904_220719`, started 2026-09-05 02:07:18 UTC
(2026-09-04 22:07:18 America/New_York). Platform: macOS 26.5.2, arm64,
8 logical CPUs. Branch: `perf/tokio-benchmark-metrics`, based on
`eab382ba8d4b46ac7c09661b3c950ef2d5e6f59b`, with the uncommitted instrumentation
and synthetic incoming-scope fix in this worktree. The repository's release
profile and default worker configuration were used, with
`RUSTFLAGS='--cfg tokio_unstable'` and the `synthetic-load` feature. No compiler
or test processes from this task ran concurrently with the benchmark.

## Workload

Eight v1 torrents, 64 local TCP peers, 8 MiB per torrent (64 MiB logical fixture
set), 256 KiB pieces, request pipeline 32, and 32 disk-read permits. Upload
leechers repeatedly request blocks, so this is a sustained workload over a small
file set that can remain cached. Throughput is received synthetic payload, not
a physical-disk bandwidth measurement. Both production torrent managers and
synthetic peers execute in the same process/runtime. App/TUI and DHT are absent.

The run used a 3-second warmup and 20-second measurement window, sampled every
250 ms. All 80 post-warmup intervals contained upload traffic. An external
90-second timeout supervised the process; it exited normally in 23.383 seconds.

```sh
RUSTFLAGS='--cfg tokio_unstable' cargo build --release --locked --features synthetic-load
./target/release/superseedr --json synthetic-load \
  --mode upload --transport tcp --torrents 8 --peers 64 \
  --size-per-torrent 8MiB --piece-size 256KiB \
  --warmup-secs 3 --duration-secs 20 --metrics-interval-ms 250 \
  --leecher-pipeline 32 --disk-read-permits 32 \
  --out tmp/tokio-baseline/active
```

## Observations

Tokio counters below cover 20.000037 observed seconds after the warmup boundary.
Queue/task/thread peaks are sampled peaks, not continuously observed maxima.

| Measurement | Observed value |
| --- | ---: |
| Received upload payload | 474.72 MiB/s; 3.982 Gbit/s |
| Tokio worker count | 8 |
| Average worker busy wall time | 98.17% |
| Busiest worker's aggregate busy wall time | 98.23% |
| Worker polls | 3,649,859; 182,493/s |
| Spawned async tasks | 607,646; 30,382/s |
| Forced cooperative-budget yields | 10,551; 528/s |
| Tasks stolen between workers | 851,479 |
| Steal operations | 229,439 |
| Remote scheduling events | 1,546,496 |
| I/O readiness events | 1,702,063 |
| Sampled peak live tasks | 789 |
| Sampled peak global runnable queue | 29 |
| Sampled peak summed local runnable queues | 104 |
| Local queue overflow counter | 0 |
| Sampled peak blocking-pool threads | 261 |
| Sampled peak active blocking-pool threads | 130 |
| Sampled peak blocking queue | 31 |
| Sampler lateness p50 / p95 / p99 / max | 0.741 / 1.462 / 1.808 / 1.808 ms |
| Reported protocol errors | 0 |

The entire process, including setup, warmup, measurement, and cleanup, used
34.566 user CPU seconds and 122.509 system CPU seconds over 23.383 wall seconds
(about 6.72 CPU cores on average). Peak resident memory was 55.94 MiB. These
process measurements include synthetic peers and blocking threads, and cover a
different interval from the Tokio summary.

Busy duration measures wall time during worker processing, including task
bodies and time the OS may deschedule a worker. It is not scheduler-only CPU
time. Tokio publishes counters in batches; short interval fractions can exceed
100%, and publication can cross the warmup boundary. Reported task poll timing
in raw samples is Tokio's EWMA, not a per-task latency percentile. The sampler
lateness quantiles use nearest rank over only 80 observations; p99 therefore
equals the observed maximum and says nothing about rarer stalls or UI latency.

The most useful follow-up is to attribute task creation and blocking work to
specific application operations. High task churn, many blocking threads, and
substantial system CPU time are leads for profiling; this run cannot establish
that a custom executor would improve them. A larger library, cold storage,
download hashing, uTP, and a separate peer-generator process need their own
measurements before drawing broader conclusions.

## Harness correction and validation

The initial upload attempt accepted and then lost all 64 peers without payload.
Its incoming hub omitted the active network scope, and the production manager
correctly rejected those connections. The harness now passes the activation's
scoped lease into its hubs and attaches it before routing each TCP/uTP/mixed
incoming connection. The production admission check is unchanged. That initial
attempt is excluded from the load baseline.

The timer-lateness calculation now uses each ticker's scheduled deadline.
Previously skipped ticks accumulated as permanent lag. A paused-time regression
checks recovery after skipped deadlines.

Validation completed:

- 29 benchmark-module tests passed with standard Tokio metrics and again with
  `--cfg tokio_unstable`. This includes a real post-warmup upload assertion and
  active-scope routing through TCP, uTP, and mixed hubs.
- Strict all-target/all-feature Clippy passed in both instrumentation modes;
  strict no-default-feature Clippy also passed.
- Release build, formatting, and diff whitespace checks passed.
- Analysis verified that all selected JSONL durations, worker busy counters,
  and detailed runtime/worker counters sum to the recorded summary values.
- The active run exited successfully, transferred payload in all measured
  samples, and reported no protocol errors.

## Local evidence

Raw artifacts remain in ignored `tmp/` paths in this worktree:

- [Active summary](../tmp/tokio-baseline/active/run_20260904_220719/summary.json)
- [Active samples](../tmp/tokio-baseline/active/run_20260904_220719/samples.jsonl)
- [Derived statistics and accounting checks](../tmp/tokio-baseline/active/analysis.json)
- [Command, platform, binary hash, process usage, and exit status](../tmp/tokio-baseline/active/environment.json)
- [Initial invalid idle attempt](../tmp/tokio-baseline/run_20260904_215332/summary.json)
- [External-timeout runner](../tmp/tokio-baseline/run_baseline.py)
- [Analysis script](../tmp/tokio-baseline/analyze.py)

The raw files are local evidence and will not accompany a Git checkout unless
copied separately. Usage and metric definitions are documented in
[synthetic-benchmark.md](synthetic-benchmark.md#measuring-the-tokio-runtime).
