# Production TUI under peer churn

The production TUI **did reproduce sustained FPS loss with many peers**, even
with a download target of only 1 MB/s. The eight-torrent run averaged 51.8 FPS
under high-count admission pressure, and 26.3 FPS with connection turnover.
Returning to the sixteen background seeders restored approximately 60 FPS.
This supports investigating peer-dependent UI work before replacing Tokio.

A subsequent [manager and UI instrumentation run](manager-churn-instrumentation.md)
provides stronger attribution: full peer-state telemetry dominated, manager queues
did not show sustained saturation, and the earlier multi-second stall did not recur.

## Measured result

Actual production `App::run`, real macOS Terminal output at 129 columns by
35 rows, the existing optimized instrumented binary, and a fresh isolated
profile. Measurements ran on 2026-09-04 America/New_York. There were no
`rustc` or `clippy-driver` processes at any phase start.

Eight generated torrents each contained 4 GiB logical zero-filled payloads
with 256 KiB pieces (16,384 pieces per torrent). Sixteen stable seeders
provided approximately 1 MB/s total. Additional incoming TCP clients used
valid handshakes, unique peer IDs and empty bitfields. They did not carry
payload traffic. A separate Python process provided tracker, seeders and
connection turnover; it shared the machine but not the app runtime.

| Phase | App-reported connected peers | Average FPS | Frame gap p99 | Largest gap | Mean preparation |
| --- | ---: | ---: | ---: | ---: | ---: |
| Background only | 16 | 60.0 | 18.4 ms | 20.2 ms | 0.9 ms |
| 500 stable extra peers | 516 | 60.0 | 17.9 ms | 18.1 ms | 6.1 ms |
| 500 extra peers, 5-second lifetimes | 464–516 | 59.6 | 19.3 ms | 80.2 ms | 6.5 ms |
| High-count admission pressure | 1,400–1,536 | 51.8 | 25.9 ms | 37.2 ms | 18.7 ms |
| High-count pressure plus 20-second lifetimes | 447–1,484 | 26.3 | 268.5 ms | 4,174.8 ms | 22.1 ms |
| Recovery | 16 | 59.6 | 19.1 ms | 58.7 ms | 2.2 ms |

Each summary excludes the first five seconds of its phase. Phase durations
were 8, 12, 18, 26, 42 and 10 seconds. FPS uses completed-frame timestamps;
frame gaps use monotonic frame-start intervals. These are application frame
measurements, not terminal compositor presentation measurements.

The high-count target was **3,000 additional connections**, but the app did
not admit that many. The generator continued bounded attempts (at most
200/s) when connections were rejected. Therefore the high-count phase is
not a pure stable-peer control: it includes admission pressure. It averaged
196 attempts/s and 6.5 successful handshakes/s in the reported window.
The following churn phase averaged 162 attempts/s, 35 successful
handshakes/s and 40 closures/s. At the 500-peer churn level, approximately
103 handshakes/s succeeded with no generator-recorded connection errors.
Connection errors combine closure, refusal and timeout; they are not a
precise breakdown of the application's rejection reasons.

Received download rates were approximately 1 MB/s in the earlier phases,
0.70 MB/s during high-count churn, and 0.99 MB/s after recovery. The generator
rate is a target, not a guarantee under stalls. These were not high-bandwidth
runs; peer count and turnover were sufficient to reproduce degradation.

## Interpretation and remaining uncertainty

The 60 FPS budget is 16.67 ms. During high-count admission pressure,
preparation alone averaged 18.67 ms, while drawing averaged 0.62 ms.
Preparation covers the work between selecting the frame branch and calling
`terminal.draw`, including draining and reducing torrent metrics.

A concrete code suspect is `UiTelemetry::on_metrics` in
`src/telemetry/ui_telemetry.rs`: every metrics update calls
`aggregate_peers_to_availability`, which scans every peer's piece bitfield.
The app also clones each changed torrent metrics snapshot in
`App::drain_latest_torrent_metrics`. With 1,500 peers and 16,384 pieces,
one full availability aggregation entails roughly 24.6 million bit
iterations. This is source-based attribution of a plausible cost, **not a
function-level profile proving that it explains all preparation time**.

Churn also exposed a different timing component: the longest draw call took
4.148 seconds, and draw p99 was 250 ms. That timer includes rendering,
terminal writes and any intervening OS scheduling or blocking. This test
cannot distinguish terminal backpressure, rendering cost, memory pressure
or preemption inside that call. It would be incorrect to label those
stalls as Tokio queue delays. Main-loop wake lag can also be a consequence
of the preceding frame's expensive preparation or draw.

Production peer admission remained enabled. A preliminary two-torrent run
hit the per-torrent admission threshold of 400 (800 total), as defined by
`PEER_ADMISSION_QUALITY_THRESHOLD` and the incoming-session guard. Across
eight torrents, admissions stopped near 1,536. The application contains a
wake-lag-based global peer throttle, but this profile did not record its
active limit or rejection reason, so its involvement is not confirmed.

### Manager-side follow-up

The frame measurements do not establish where overload originates. A follow-up
source trace also found work inside each torrent manager that can grow with
churn:

- `TorrentState::record_departed_peer_transfer` invokes baseline maintenance on
  each removed peer. Maintenance scans retained departed-session records; the
  bound is 4,096 records per torrent. Disconnect batches can repeat that scan
  for each member of the batch.
- `TorrentManager::send_metrics` builds the active-peer snapshot, including
  cloned bitfields, and appends retained departed-peer records. These are full
  snapshots, not just the connections changed since the previous publication.
- `ManagerTelemetry::should_emit` clones the snapshot for retention. On ticks
  without transferred bytes, it also clones the current and previous snapshots
  for normalization/comparison before deciding whether to publish.

These operations execute synchronously within the manager task. Their durations,
command queue depths and command waiting times were not measured in this run;
manager saturation is therefore an unresolved upstream explanation, not ruled
out by the expensive UI preparation timings. Profile manager lifecycle handling,
cleanup and metrics construction alongside the UI stages before attributing
these results to either subsystem or the runtime scheduler.

The local workload also has a lifecycle-specific limitation: all connections
use the same loopback IP. Registration explicitly flushes pending disconnects
when a pending session shares the registering IP. This can produce different
batching from a public swarm with many distinct IPs, and should be controlled
in a follow-up experiment.

A useful next measurement is to split frame preparation into snapshot
cloning, availability aggregation and other reducers, and split drawing
into rendering versus terminal output. Record the active admission limit
at the same time. Then compare an optimization of the demonstrated hot
path with identical peer-count and turnover conditions. This result alone
does not justify replacing the runtime.

## Reproduction artifacts and validation

The existing worktree remains `perf/tokio-benchmark-metrics` at
`/private/tmp/superseedr-tokio-benchmark-metrics`. No Rust source changes or
rebuild were needed for this follow-up; it reused the opt-in production
frame profiler from the preceding test.

Local artifacts are under `tmp/production-tui-churn-3k/`:

- `frames.jsonl`, `peer-samples.jsonl`, `phases.json` and `analysis.json`.
- `peers.py`, `launch_app.py`, `measure.py`, `analyze.py` and
  `open_terminal.applescript` record the exact workload and orchestration.
- `environment.json` records binary SHA-256, base commit and workload sizes.
- `terminal-dimensions.txt`, `app-exit.json` and `cleanup.json` record the
  terminal size, successful app exit and stopped process verification.
- `app/config/settings.toml` and `fixtures/` preserve the isolated settings
  and generated torrent inputs. The local artifact directories are ignored
  by Git and are not portable repository fixtures.

The analysis was rerun after app exit and parsed every frame line without
errors. The preliminary two-torrent evidence is separately preserved under
`tmp/production-tui-churn/`; it stayed near 60 FPS on average but had
42–44 ms frame gaps under churn/admission pressure. Its app needed SIGTERM
after the graceful shutdown wait; one partial trailing frame was excluded.
The eight-torrent run exited successfully after its generator was stopped
and SIGINT was sent. Both dedicated Terminal windows were closed and all
four test processes were verified stopped. User profiles were isolated;
no system-wide resource limits were changed.
