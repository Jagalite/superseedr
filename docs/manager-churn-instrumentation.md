# Manager and UI instrumentation under peer churn

The instrumented run identifies **full peer-state telemetry processing** as the
clearest bottleneck in this workload. The largest measured UI cost is availability
aggregation across peer bitfields, followed by snapshot copying. Manager snapshot
publication also grows substantially. Disconnect/history cleanup did not become
expensive enough to explain the frame degradation, and the manager queues did not
show sustained saturation in the sampled data.

The follow-up [optimization and fidelity report](telemetry-optimization.md)
records the implementation, accuracy tests and before/after production measurements.

## Run and outcome

The normal production `App::run` rendered in a real 129-by-35 macOS Terminal
window on 2026-09-05 America/New_York. It used eight generated v1 torrents,
16,384 pieces per torrent and sixteen stable seeders supplying 1 MB/s total.
Incoming synthetic peers completed valid handshakes and sent empty bitfields.
Connection attempts were capped at 200/s, with phase targets of 500 and 3,000
additional peers. Peers and tracker ran in a separate Python process. The app
used a fresh isolated profile with DHT disabled and TCP transport enabled.

The production release optimization settings were retained, with
`--features synthetic-load` and `RUSTFLAGS='--cfg tokio_unstable'`. The build
finished before measurement. Compiler checks at phase starts and every second
throughout the run all found no `rustc` or `clippy-driver` processes. System-wide
swap-ins still occurred, so this was not a machine isolated from all background
activity; no swap-outs were recorded during these phases.

| Workload | App-reported connected peers | FPS | Availability ms/frame* | UI copy ms/frame* | Manager publish ms/tick** | Worst queue probe ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Background only | 16–16 | 60.0 | 0.58 | 0.04 | 0.034 | 0.08 |
| 500 stable extra peers | 516–516 | 60.0 | 4.12 | 0.52 | 0.313 | 0.59 |
| 500 extra peers, 5-second lifetime | 468–516 | 60.0 | 4.24 | 0.63 | 0.398 | 0.95 |
| 3,000 target, admission pressure | 1424–1792 | 46.9 | 14.42 | 2.43 | 1.352 | 4.18 |
| 3,000 target, 20-second lifetime | 1229–1775 | 51.2 | 13.16 | 2.31 | 1.263 | 5.77 |
| Recovery | 16–16 | 60.0 | 0.40 | 0.50 | 0.407 | 0.90 |

\* Per-frame figures divide stage totals by the number of UI metrics-drain
calls in the same selected aggregate windows. They are approximate per-frame
costs, because nested spans may straddle aggregate-window boundaries.

\** `manager.send_metrics_ns` is inclusive: snapshot construction, departed
records, comparison/retention copies, watch-channel publication and destruction
of replaced values. It is per manager invocation, not the sum of all eight
managers. Timings are elapsed wall time, not CPU samples.

Frame summaries exclude each phase's first five seconds. Stage summaries use
only aggregate windows wholly contained in that remaining interval, which
slightly narrows their coverage. Phase durations were 12, 12, 18, 26, 42 and
10 seconds. The high-count target was not reached; actual admitted counts are
shown above. In the high-count phases download rates remained approximately
0.98 and 0.97 MB/s.

The high-count phase averaged 46.9 FPS, with a 29.4 ms p99 frame gap and a
71.4 ms maximum. High-count churn averaged 51.2 FPS, with a 23.4 ms p99 gap
and a 33.3 ms maximum. Churn had a lower actual connected count, so these phases
are not a matched-count test of churn alone. At about 500 peers, stable and
churning phases both held approximately 60 FPS in this run.

The earlier 26 FPS result and four-second terminal stall were **not reproduced**.
They remain observations from the earlier experiment, not the reproducible
severity demonstrated by this more closely monitored run.

## What the instrumentation isolates

At high-count admission pressure, the selected profile windows showed:

- UI availability aggregation: approximately **14.42 ms per frame**.
- Cloning torrent snapshots into the UI: approximately **2.43 ms per frame**.
- Entire UI metrics drain: **17.23 ms per call**, including the above stages.
- Widget rendering: **0.44 ms per frame** on average.
- Remaining terminal draw work: **0.16 ms per frame** on average. This includes
  Ratatui diffing/backend work and terminal writes, not just terminal I/O.

Availability plus UI copying alone exceeded the 16.67 ms budget for 60 FPS.
Availability accounted for about 84% of measured metrics-drain time. Overall
preparation in the frame records averaged 20.69 ms in that phase; the profile
also leaves other preparation work outside the metrics drain unpartitioned.

The concrete path is `App::drain_latest_torrent_metrics` cloning each changed
watch snapshot, then `UiTelemetry::on_metrics` calling
`aggregate_peers_to_availability`. That function iterates the piece bitfield of
every peer on every received metrics update. This establishes a measured cost
in that function, not merely a source-based suspicion.

The manager has a related telemetry cost: publication averaged 1.35 ms per tick
in the high-count phase. Snapshot construction accounted for 0.45 ms per tick;
`ManagerTelemetry::should_emit` accounted for 0.86 ms. The latter clones metrics
for retention, and on zero-transfer ticks also clones current/previous values
for normalized comparison. These copies include peer bitfields and departed
peer records. Even after recovery to sixteen active peers, publication averaged
0.41 ms rather than the baseline 0.034 ms; retained departed records and their
copying remain present during that short recovery window.

By contrast, during the high-count churn window:

- Disconnect actions averaged **0.015 ms**, with a 2.47 ms maximum.
- History maintenance averaged **0.003 ms**, with a 0.77 ms maximum.
- Queue probes averaged **1.27 ms**, with a 5.77 ms maximum.
- Tick gaps averaged **17.00 ms**, with a 23.00 ms maximum.
- Command queue depth averaged 0.02 at tick samples, with one sampled maximum
  of 102; incoming queue depth averaged 0.01, with a maximum of 25.

This is evidence against sustained manager queue collapse in this particular
run. The FIFO probe is inserted at most once per manager per second, so it does
not rule out every short burst or measure the waiting time of every command.
Periodic rarity work also stayed bounded here: 2.15 ms average and 4.11 ms
maximum in the high-count churn aggregate windows.

## Admission and scope limits

The frame profiler's `active_peer_limit` remained unset throughout every phase:
the wake-lag-specific global peer throttle was not active in this run. Base peer
limits remained above 8,500. No individual manager reached the per-torrent
400-peer threshold in the selected high-count profile windows. Therefore those
limits do not explain the observed connection plateau here.

The generator recorded connection/handshake timeouts while trying to reach
3,000 peers. This test does not separate TCP-connect delay from handshake-response
delay. The application's shared main loop performs both accepting connections
and the expensive UI preparation, making main-loop service delay a plausible
explanation; that causal attribution still needs direct accept/handshake timing.
The measurements do not establish a Tokio scheduler defect.

All synthetic clients used the same loopback IP. The manager's same-IP pending
disconnect flush policy therefore differs from a swarm with many distinct IPs.
The generated churn peers were idle after their bitfield, so the result does not
cover thousands of useful peers actively transferring blocks at high bandwidth.

The next targeted change to assess is avoiding repeated full availability scans
and reducing full snapshot cloning, with the same workload and instrumentation.
Manager lifecycle behavior and runtime scheduling should remain unchanged in
that comparison.

## Instrumentation and reproducibility

`SUPERSEEDR_PERF_PROFILE_DIR` enables per-thread JSONL aggregates with count,
total and maximum values. Duration names end in `_ns`; other values are queue
lengths or counts. Nested durations are inclusive and must not be summed as
independent CPU costs. Files flush at most once per second per active thread,
plus thread exit. Recording adds overhead; draw duration is captured before
writing its profile records. The probe itself adds at most eight commands per
second in this eight-manager test.

The clean local artifacts are in `tmp/manager-churn-profile-clean/`:

- `frames.jsonl`, `peer-samples.jsonl`, `phases.json`, `profile/*.jsonl`.
- `analysis.json`, `profile-analysis.json`, and their analysis scripts.
- Generator, app launcher, Terminal launcher and measurement controller scripts.
- `environment.json` with binary SHA-256 and base commit; terminal dimensions,
  app exit and cleanup evidence; isolated settings and generated fixtures.

The earlier attempted run in `tmp/manager-churn-profile/` was stopped when
unrelated Clippy processes appeared. `excluded.json` records why its samples
were excluded from clean conclusions. These local artifact directories are
ignored by Git and are not portable repository fixtures.

Validation: optimized instrumented build passed; strict Clippy passed with
`--all-targets --all-features -- -D warnings`; the aggregation unit test passed;
format and diff checks passed. The successful run parsed every complete frame
and aggregate record without errors. Both profiling apps exited successfully,
all local generators were verified stopped, and their dedicated Terminal windows
were closed. No user profiles or system-wide resource limits were changed.

This experiment was recorded on `perf/tokio-benchmark-metrics` in the isolated
worktree `/private/tmp/superseedr-tokio-benchmark-metrics` before PR closeout.
