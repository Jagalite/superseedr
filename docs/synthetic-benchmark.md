# Synthetic Benchmark And Load Testing

## Overview

Superseedr has a feature-gated synthetic load harness for local performance
testing without external trackers, public peers, or real content fixtures.

The harness is intended for engineering validation:

- find local CPU, disk, scheduler, and connection bottlenecks
- exercise many torrents and many synthetic peers on one machine
- compare download-only, upload-only, and mixed swarm behavior
- collect JSON summaries and per-sample metrics for later analysis

It is not part of the default production build. Build with
`--features synthetic-load` to expose these commands.

Use `--transport tcp`, `--transport utp`, or `--transport all` to force the
synthetic peer transport mode. The default is `all`, matching the normal runtime
behavior where TCP and uTP are both enabled.

Use `--torrent-format v1`, `--torrent-format v2`, or
`--torrent-format hybrid` to choose the generated metainfo format. The default
is `v1` for compatibility with older benchmark runs. The v2 and hybrid formats
generate synthetic file-tree metadata and piece layers so the normal v2
verification and write paths are exercised without external torrent fixtures.

## Benchmark Mode

`benchmark` is the high-level adaptive wrapper around the lower-level synthetic
load harness. It runs all three scenarios by default:

- `download`: Superseedr managers download from local synthetic seeders
- `upload`: Superseedr managers seed to local synthetic leechers
- `swarm`: both download and upload sides run together

Each profile starts at the requested torrent and peer counts, scales upward, and
stops when it reaches the configured limits or sees the first issue.

A full benchmark run has been observed to take about 33 minutes on an M1
MacBook. Runtime varies with hardware, OS connection limits, disk speed, and
the selected benchmark limits.

Benchmark output is scenario-oriented. For each scenario, text mode prints:

- the planned step count
- the final torrent and peer target for that scenario
- estimated disk for the current step and final planned step
- per-step throughput, bytes, pieces, add progress, tick lag, protocol errors,
  outbound failures, and disk read/write counters
- ETA after every step for both the current scenario and the full benchmark
- scenario aggregate metrics after all scenarios finish
- a final capacity report with runtime, clean torrent and peer estimates,
  configured resource limits, disk payload rates, and likely bottleneck signals

Example:

```bash
cargo run --release --features synthetic-load -- benchmark \
  --start-torrents 10 \
  --start-peers 100 \
  --max-torrents 1000 \
  --max-peers 100000 \
  --max-steps 12 \
  --torrent-format hybrid \
  --duration-secs 30 \
  --disk-budget 8GiB \
  --size-per-torrent 8MiB \
  --piece-size 256KiB
```

JSON output:

```bash
cargo run --release --features synthetic-load -- --json benchmark \
  --start-torrents 10 \
  --start-peers 100 \
  --max-torrents 1000 \
  --max-peers 100000
```

## Rendering the TUI during a run

Add `--tui` to either `synthetic-load` or `benchmark` to render the production
Normal screen at 60 FPS while the synthetic transfers run. Without this flag,
the harness remains headless. The renderer uses live manager snapshots, the
shared application reducers, UI telemetry aggregation, and animation code.
Manager telemetry events and frame deadlines share the harness's main-thread
loop; rendering is not spawned as a separate worker task.

```sh
mkdir -p tmp
cargo run --release --locked --features synthetic-load -- --json synthetic-load \
  --tui --mode download --transport tcp --torrents 2 --peers 16 \
  --size-per-torrent 256MiB --piece-size 256KiB --target-gbps 0.2 \
  --warmup-secs 2 --duration-secs 10 --metrics-interval-ms 250 \
  --out tmp/synthetic-tui > tmp/synthetic-tui-result.json
```

Run this in a terminal. The display uses stderr's alternate screen, leaving
stdout available for the final JSON result. Per-sample text is suppressed while
the display is active. Ctrl+C stops the run and restores the terminal; this is
a passive display, so normal application keyboard commands are not enabled.
No user settings or saved torrents are loaded. Default theme and layout are used,
and terminal resizing takes effect on subsequent frames. Compare runs using the
same terminal emulator, dimensions, and workload.

Each run directory gains `tui-frames.jsonl`, with one record per rendered frame:

- `elapsed_us` and `phase`: frame-start time and warmup/measurement selection.
- `lateness_us`: time from the scheduled frame start to entering frame work.
- `preparation_us`: state/metrics updates, periodic telemetry, and animation work.
- `draw_us`: production rendering, buffer diffing, and terminal output.
- `frame_interval_us`: time between successive frame starts; null for the first.
- `over_budget`: frame work finished more than 16.67 ms after its scheduled start.
- `manager_events_since_frame` and `manager_event_work_us`: telemetry events and
  reducer wall time since the preceding frame (the first record starts at run setup).
- `terminal_width` and `terminal_height`: the rendered size in character cells.

`summary.json` gains `tui` (null when disabled), including measured frame count,
average FPS, over-budget frame count, and total/maximum timing values. Frame
summary selection uses the requested warmup boundary and excludes warmup frames;
its time window is separate from the transfer sampler's interval boundaries.
Raw frame records preserve spikes for offline percentile analysis. JSONL writing
is buffered and excluded from `draw_us`; that instrumentation can still delay a
subsequent frame. These are wall times, not scheduler-only CPU measurements.
The terminal emulator's later presentation/compositing is not measured.

This mode exercises the real rendering path, but **does not run the full
interactive `App::run` loop**: application commands, adaptive tuning, persistence,
DHT service work, and system CPU/memory sampling are absent. Non-telemetry
application effects are not executed. Consequently it can compare rendering
cost and frame contention under load, but cannot rule out stalls in those other
application handlers. The benchmark sweep still judges capacity by its existing
transfer/protocol criteria; frame overruns are reported, not a new pass/fail gate.

Downloads finish after receiving their finite payload. Size the data set so it
lasts through warmup and measurement, and confirm that measured samples actually
contain download traffic before interpreting FPS. `--target-gbps 0.2` means
200 Mbit/s (about 25 MB/s); 200 MB/s requires approximately 1.6 Gbit/s.

## Profiling the production application loop

Tooling builds (`--features synthetic-load`) also support opt-in environment
variables for tests that run the normal application, without a synthetic-load
subcommand:

- `SUPERSEEDR_BENCHMARK_APP_ROOT`: an absolute directory containing isolated
  `config/` and `data/` directories. Settings, locks, watch-folder defaults, logs,
  and persistence resolve under that root. This override is absent in normal
  builds; unset any shared-config environment override when using it.
- `SUPERSEEDR_TUI_PROFILE`: path to buffered JSONL frame measurements from the
  real `App::run` draw branch. Records include frame interval, deadline lateness,
  preparation and draw time, received/written byte counters, connected peers,
  and aggregate manager-event, stats, command, and tuning handler timings since
  the preceding rendered frame. Frames carry Unix timestamps for matching to
  external traffic-generator phases.

- `SUPERSEEDR_PERF_PROFILE_DIR`: absolute output directory for per-thread JSONL
  aggregates of manager actions, cleanup/history maintenance, snapshot building,
  telemetry comparison/copying, UI metric reduction and rendering.
  Each sample carries `count`, `total` and `max`; names ending in `_ns` are
  wall-clock durations, and other names are queue depths or counts. Windows
  normally flush once per second per active thread, with a final flush on
  thread exit. Nested spans are inclusive and must not be added together.
  Manager ticks sample command/incoming queue depths and insert at most one
  FIFO command probe per manager per second. Probe waiting time includes queue
  service delay and intervening scheduling; it does not measure every command's
  age or distinguish Tokio scheduling from manager work. Frame records also
  include the base and active peer limits.

This path preserves normal application scheduling and adaptive tuning. Generate
local torrent fixtures and supply peers from another process to avoid including
the simulated peers in the application's Tokio runtime. Run the application in
a real terminal emulator when investigating terminal output cost. The profiling
writer adds overhead after drawing, and frame timings still do not measure the
emulator's later display/compositing latency. Only rendered frames are recorded;
intentional idle redraw suppression must be distinguished from missed frames.

## uTP Chaos Mode

Synthetic runs can inject deterministic UDP faults into the uTP path. Use this
after a clean stress baseline so capacity limits are not confused with protocol
correctness bugs.

Fault rates are expressed in packets per million. The active chaos settings are
recorded in `utp_chaos` in the JSON summary so a failing run can be repeated
with the same seed.

Example:

```bash
cargo run --release --features synthetic-load -- benchmark \
  --transport utp \
  --start-torrents 4 \
  --start-peers 64 \
  --max-torrents 4 \
  --max-peers 64 \
  --max-steps 1 \
  --duration-secs 60 \
  --warmup-secs 5 \
  --utp-chaos-seed 42 \
  --utp-chaos-loss-ppm 1000 \
  --utp-chaos-duplicate-ppm 1000 \
  --utp-chaos-reorder-ppm 5000 \
  --utp-chaos-max-delay-ms 100 \
  --out tmp/utp-chaos
```

Start with delay, duplicate, and reorder. Add loss once the baseline is stable.
Use corruption separately because malformed payloads are expected to produce
bounded protocol errors rather than clean throughput.

## Disk Budget

Benchmark mode writes generated payload data so disk paths are exercised, but it
keeps each step inside `--disk-budget`.

Sizing rules:

- `--size-per-torrent` is the preferred generated payload size
- `--piece-size` controls the synthetic piece size
- benchmark mode clamps per-torrent size downward to fit the disk budget
- clamped sizes are rounded down to whole pieces
- `swarm` needs roughly two sides of data, so it uses about twice the working
  set of `download` or `upload`
- generated `data/` directories are removed after each step unless
  `--keep-output` is set

The summary and metrics files are kept even when generated data is removed.

## Scaling Behavior

For each profile, benchmark mode:

1. starts at `--start-torrents` and `--start-peers`
2. doubles torrent count until `--max-torrents`
3. then doubles peer count until `--max-peers`
4. enforces the minimum peer topology needed for the scenario
5. records the last clean step and the first issue step

Minimum peers:

- `download` and `upload`: at least one peer per torrent
- `swarm`: at least two peers per torrent

## Issue Detection

A benchmark step is marked as having issues when the harness sees conditions
such as:

- not all torrents were added by the end of the run
- not all synthetic peers were added by the end of the run
- sample tick delay exceeds `--max-sample-delay-ms`
- protocol errors are observed
- outbound connection permit timeouts occur
- outbound connect timeouts or connection refusals occur
- synthetic leecher connection errors occur

These are harness-level signals. A reported issue means "inspect this step"; it
does not automatically prove the production engine is wrong.

## Stop And Continue Behavior

The benchmark decides whether to continue only after a step completes. A step
runs for `--duration-secs`, then the harness inspects the step summary.

Per scenario:

- clean step: record it as the latest clean step and continue to the next
  planned step
- issue step: record it as `first_issue`, stop that scenario, then continue to
  the next scenario
- scenario planning or runtime step error: record it as an issue for that
  scenario, stop that scenario, then continue to the next scenario

By default, an issue does not stop the scenario immediately. Benchmark mode
retries the same step up to `--issue-retries` additional times, waiting
`--retry-delay-ms` before each retry. If any retry is clean, the scenario
continues to the next planned step and the failed attempt is reported as a
transient issue. If all attempts fail, the final failed attempt becomes
`first_issue`.

Scenarios run in this order:

1. `download`
2. `upload`
3. `swarm`

That means a system that cannot handle the download profile still gets upload
and swarm reports when the harness can recover and continue.

## Output

Default output root:

```text
tmp/synthetic-benchmark/
```

Each benchmark creates:

```text
tmp/synthetic-benchmark/benchmark_YYYYMMDD_HHMMSS/
  benchmark_summary.json
  download/step_.../run_.../
    summary.json
    samples.jsonl
  upload/step_.../run_.../
    summary.json
    samples.jsonl
  swarm/step_.../run_.../
    summary.json
    samples.jsonl
```

Useful summary fields:

- `torrent_format`
- `utp_chaos`
- `report.runtime_secs`
- `report.steps_run`
- `report.retry_attempts`
- `report.transient_issue_attempts`
- `report.recovered_after_retry_steps`
- `report.clean_steps`
- `report.issue_steps`
- `report.peer_connection_limit_policy`
- `report.issue_retries`
- `report.retry_delay_ms`
- `report.os_limit_note`
- `report.scenarios[]`
- `report.scenarios[].verdict`
- `report.scenarios[].capacity_estimate`
- `report.scenarios[].likely_bottleneck`
- `report.scenarios[].clean_torrents`
- `report.scenarios[].clean_peers`
- `report.scenarios[].observed_disk_read_bytes_per_sec`
- `report.scenarios[].observed_disk_write_bytes_per_sec`
- `report.scenarios[].peer_connection_limit`
- `report.scenarios[].disk_read_permits`
- `report.scenarios[].disk_write_permits`
- `profiles[].last_clean`
- `profiles[].first_issue`
- `profiles[].planned_steps`
- `profiles[].final_torrents`
- `profiles[].final_peers`
- `profiles[].final_estimated_disk_bytes`
- `profiles[].metrics`
- `profiles[].steps[]`
- `profiles[].steps[].step`
- `profiles[].steps[].planned_steps`
- `profiles[].steps[].attempt`
- `profiles[].steps[].max_attempts`
- `profiles[].steps[].will_retry`
- `profiles[].steps[].retry_delay_ms`
- `profiles[].steps[].estimated_disk_bytes`
- `profiles[].steps[].estimated_final_disk_bytes`
- `profiles[].steps[].wall_secs`
- `profiles[].steps[].eta.current_scenario_remaining_steps`
- `profiles[].steps[].eta.full_benchmark_remaining_steps`
- `profiles[].steps[].eta.current_scenario_eta_secs`
- `profiles[].steps[].eta.full_benchmark_eta_secs`
- `profiles[].steps[].eta.average_step_wall_secs`
- `profiles[].steps[].eta.elapsed_wall_secs`
- `avg_download_bps` and `avg_upload_bps`
- `download_bytes` and `upload_bytes`
- `max_sample_delay_ms`
- `protocol_errors`
- `protocol_error_detail`
- `outbound_failed`
- `outbound_permit_timeout`
- `outbound_connect`
- `completed_pieces` and `total_pieces`
- `disk_read_started` and `disk_read_finished`
- `disk_write_started` and `disk_write_finished`
- `issues`

## Lower-Level Synthetic Load

`synthetic-load` is the lower-level one-scenario harness. It is hidden from the
normal CLI help because it is mainly for focused engineering runs.

Use it when you already know the exact topology to test:

```bash
cargo run --release --features synthetic-load -- synthetic-load \
  --mode swarm \
  --torrents 100 \
  --peers 2000 \
  --torrent-format v2 \
  --peer-add-mode staggered \
  --peer-add-burst-size 50 \
  --duration-secs 60 \
  --size-per-torrent 8MiB \
  --piece-size 256KiB \
  --target-gbps 10
```

Good uses for `synthetic-load`:

- rerun a single benchmark step with more duration
- isolate upload-only or download-only behavior
- test peer roll-in settings
- test disk read and write permit settings
- preserve generated data with a custom `--out` path for local inspection

## Measuring The Tokio Runtime

Each `samples.jsonl` record and the run's `summary.json` include a
`tokio_runtime` object. Standard `synthetic-load` builds collect the runtime
metrics available through Tokio's stable API. For additional scheduling and
polling metrics, build with Tokio's optional unstable instrumentation enabled:

```bash
RUSTFLAGS='--cfg tokio_unstable' cargo build --release --locked --features synthetic-load
```

Stable readings include worker busy/park deltas and live task/global queue
gauges. The optional `details` object adds task creation, polls, work stealing,
cooperative budget exhaustion, I/O readiness, local queues, and blocking-pool
measurements. Unavailable metrics are `null`, including `details` in a standard
build. See the [Tokio 1.50 RuntimeMetrics API](https://docs.rs/tokio/1.50.0/tokio/runtime/struct.RuntimeMetrics.html)
for counter definitions and availability.

This build flag exposes more measurements; it does not change the runtime's
scheduling configuration or select a different executor. Keep build flags the
same when comparing runs. Tests that cover the detailed instrumentation also
need the flag, for example:

```bash
RUSTFLAGS='--cfg tokio_unstable' cargo test --locked --features synthetic-load native::synthetic_load
```

A small sustained upload run on macOS or Linux:

```bash
tokio_benchmark_out="$(mktemp -d /tmp/superseedr-tokio-benchmark.XXXXXX)"
./target/release/superseedr --json synthetic-load \
  --mode upload \
  --transport tcp \
  --torrents 8 \
  --peers 64 \
  --size-per-torrent 8MiB \
  --piece-size 256KiB \
  --warmup-secs 3 \
  --duration-secs 20 \
  --metrics-interval-ms 250 \
  --leecher-pipeline 32 \
  --disk-read-permits 32 \
  --out "$tokio_benchmark_out"
```

The upload workload repeatedly requests blocks from production torrent
managers, so its 64 MiB logical fixture set stays active through the measurement
window. Small download fixtures can finish during warmup. Omitting
`--target-gbps` leaves this focused run uncapped. The output directory retains
the generated fixtures, samples, and summary. If `CARGO_TARGET_DIR` is set, use
the executable from that target directory instead.

Interpret these measurements with their scope in mind:

- Runtime counters cover spawned work on the process's Tokio runtime, including
  synthetic peers and event collection. They do not isolate production torrent
  managers. The main `block_on` future and blocking pool are not included in
  async worker busy time. The harness disables DHT and runs the TUI only when
  `--tui` is passed.
- Worker busy time includes time executing task bodies. It is not a measure of
  CPU time spent exclusively scheduling tasks, and an idle worker can also mean
  the workload is waiting for I/O. Tokio publishes counters in batches, so an
  individual interval's raw `busy_fraction` can exceed `1.0`; it is not a
  process CPU percentage.
- Queue and task counts are sampled gauges. Their reported peaks can miss
  bursts between samples. Tokio's mean poll duration is an exponentially
  weighted moving average, not a p99 task latency measurement.
- The runtime summary selects post-warmup snapshot intervals, excluding the
  interval that straddles the warmup boundary. Samples identify these with
  `tokio_runtime_interval_measured`. Late publication can still move counters
  across this boundary. Use the summary's own `observed_seconds` for rates;
  that duration can differ from the payload summary's `measured_secs`.
- `sample_delay_ms` measures lateness against the actual scheduled ticker
  deadline. `sample_lateness_us` preserves finer precision. Skipped ticks do
  not accumulate as permanent lateness in later samples. This measures the
  sampler's wakeup, not keyboard response or individual task waiting time.

For the first result, check that upload traffic continues across the measured
samples and that connection/protocol errors remain zero. Report payload rate,
worker busy time, sampled queue/task peaks, and available polling counters
together. A single short run establishes an instrumented baseline; it cannot
prove that the scheduler causes a bottleneck or that torrents receive fair
service.

## Practical Guidance

Start small, then scale:

```bash
cargo run --release --features synthetic-load -- benchmark \
  --start-torrents 10 \
  --start-peers 100 \
  --max-torrents 1000 \
  --max-peers 100000 \
  --disk-budget 8GiB
```

For disk-focused runs, keep `--disk-budget` realistic and increase
`--duration-secs` so the sample window captures sustained behavior.

For scheduler or connection-pressure runs, lower `--size-per-torrent` and raise
`--max-peers` so the harness spends more time on orchestration and peer traffic
than payload generation.
