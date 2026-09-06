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

## Connectivity and session workloads

Both commands accept the same independent session controls. Existing defaults
remain payload traffic over TCP/uTP; `--transport all` retains that meaning.
`--transport webrtc` uses only WebRTC peers, and `--transport mixed` distributes
peer slots across TCP, uTP, and WebRTC. These two choices require the `webtorrent`
Cargo feature (included in normal native defaults) and v1 or hybrid metadata.

WebRTC runs use a localhost WebSocket tracker and the production manager's
WebTorrent announce, negotiation, admission, peer session, and transport driver.
The remote synthetic peers use native DataChannels. No public tracker or STUN
service is needed. This measures native WebRTC integration; it does not exercise
the browser Window/worker bridge, OPFS, NAT traversal, or TURN relays.

| Control | Behavior |
| --- | --- |
| `--activity payload` | Existing piece requests and transfers |
| `--activity idle` | Complete peer-wire handshakes, keep sessions connected, read control messages, and send keepalives; no piece requests or payload |
| `--activity mixed --idle-percent 50` | Deterministic selection of idle and payload peers; percentages distribute over 100 indices; socket seeders use accept order |
| `--session-lifetime-ms 0` | Stable sessions (default) |
| `--session-lifetime-ms 2000 --reconnect-delay-ms 250` | Close established sessions after two seconds, then reconnect; applies with idle or payload activity |
| `--failure-percent 25 --failure-case reject-handshake` | A deterministic share of synthetic peers closes instead of completing the BitTorrent handshake |
| `--failure-case stall-handshake --handshake-timeout-ms 5000` | Selected peers withhold their handshake until the configured deadline, then close |
| `--rtc-setup-timeout-ms 30000` | Separate deadline for synthetic RTC signaling and DataChannel establishment |
| `--keepalive-ms 60000` | Synthetic idle-peer keepalive interval; production session timers stay unchanged |
| `--rtc-offer-side manager`, `peer`, or `mixed` | Select which side originates RTC offers (default: mixed) |
| `--tracker-interval-secs 5` | Local tracker announce interval; scheduling still belongs to the production manager |
| `benchmark --scenarios download,upload` | Select benchmark roles independently; default remains download, upload, swarm |

Idle synthetic leechers express interest but never request blocks. This keeps
seeding managers connected beyond the normal uninterested-peer grace period;
engine admission and removal policy remains unchanged.

A stable idle case requires the requested healthy peer count to be connected at
the final sample, not merely submitted to the connector. Idle peers reject any
received piece payload, and an all-idle run checks both payload byte counters and
manager block counters. Intentional handshake failures have their own counter;
transport negotiation failures remain failures. The lower-level command writes
its summary before returning an error for a failed session contract. Adaptive
benchmark mode includes these errors in its existing step/retry decisions.

For example, measure ongoing session overhead with no payload transfer:

```sh
cargo run --release --features synthetic-load -- benchmark \
  --scenarios upload --transport webrtc --activity idle \
  --start-torrents 1 --max-torrents 1 --start-peers 4 --max-peers 32 \
  --max-steps 4 --duration-secs 120 --warmup-secs 10 \
  --size-per-torrent 64KiB --piece-size 16KiB --disk-budget 16MiB
```

To exercise connection churn alongside ongoing payload and idle sessions:

```sh
cargo run --release --features synthetic-load -- synthetic-load \
  --mode swarm --transport mixed --activity mixed --idle-percent 50 \
  --torrents 2 --peers 24 --duration-secs 120 --warmup-secs 10 \
  --session-lifetime-ms 5000 --reconnect-delay-ms 500 \
  --size-per-torrent 8MiB --piece-size 64KiB
```

Idle workloads still prepare torrent metadata and, for the upload role, seed
files before measurement. Use small fixtures to isolate session maintenance;
use a larger piece count deliberately when investigating tracker counter scans.
Do not pause the torrent or set its bandwidth to zero to simulate idle sessions:
those exercise different production state and backpressure behavior.

### Connectivity measurements

`samples.jsonl`, per-run `summary.json`, and adaptive benchmark steps include a
`sessions` object with:

- Handshake attempts, established/active/peak sessions, ended sessions, planned
  disconnects, expected failures, and unexpected failures.
- Sent/received idle keepalives and rejected idle payload bytes.
- Peer-wire handshake mean/max latency. WebRTC additionally records negotiation
  attempts, successful DataChannels, failures, and mean/max setup latency,
  including waiting for a manager-originated offer when that direction is used.
- Local signaling announce/offer/answer counts (both manager and synthetic side).
- TM command-probe counts and mean/max latency from enqueue to command handling.
  Probes use the normal manager command queue once per metrics sample. This is
  responsiveness under load, not a direct measurement of state-update CPU time.
  Compare sent/completed counts to detect incomplete observations.

Session counters and latency aggregates cover the whole run, including warmup.
Process CPU percentage and resident-memory bytes accompany samples; peak values
appear in summaries and also include warmup. Use samples tagged `measure` to
compare steady-state process usage. CPU uses the OS process convention where one busy core is
100%. The process includes Superseedr, the local tracker, and remote synthetic
peers: these measurements must not be described as TM-only CPU or per-peer RTC
library allocations. Measurements also include the sampling/probe overhead.
`sessions_after_shutdown` records the final harness session state; active
sessions must return to zero. This is not a count of library-internal tasks or
sockets.

Use matching release builds, topology, resource limits, fixture sizes, warmup,
and durations when comparing transports or enabled/disabled features. Run one
measurement at a time on an otherwise quiet machine. These local tests do not
model WAN latency or prove Internet connectivity; the existing chaos options
still affect only uTP.

### Repeatable connectivity acceptance

```sh
cargo build --release --features synthetic-load
python3 scripts/test-synthetic-connectivity.py --out /tmp/superseedr-connectivity-results
# A single bounded case, or an already-built development executable:
python3 scripts/test-synthetic-connectivity.py --case webrtc-churn \
  --binary target/debug/superseedr
```

The matrix covers stable idle TCP/uTP/WebRTC/mixed sessions, incoming and outgoing transport
churn, verified WebRTC downloading, combined idle/payload traffic, and rejected/stalled handshakes. It asserts
payload absence, connection establishment, deliberate reconnection, and terminal
session cleanup. The `webrtc-long-idle` case runs beyond the engine's 30-second
uninterested-peer grace period. These runs qualify harness behavior; they are not
throughput benchmarks. Generated files are temporary; `--out` retains summaries.

### Opt-in WebRTC diagnostics

With `synthetic-load` enabled on native builds, set `SUPERSEEDR_RTC_TRACE` to a
new file path whose parent directory exists. It writes ordered JSONL events for
tracker slot occupancy, accepted/dropped offers, offer expiry, answer matching,
DataChannel setup, TM admission, and session termination. Peer and offer IDs
allow events to be correlated across these stages. SDP and ICE credentials are
not recorded. The destination must not already exist; setup/write errors are
reported on stderr.

```sh
SUPERSEEDR_RTC_TRACE=/tmp/rtc-investigation.jsonl \
  target/debug/superseedr --json synthetic-load \
  --transport webrtc --activity idle --mode swarm --torrents 2 --peers 24 \
  --duration-secs 65 --warmup-secs 0 --rtc-setup-timeout-ms 60000 \
  --size-per-torrent 64KiB --piece-size 16KiB
```

Events include a process-relative timestamp and a sequence number. File writes
are synchronous to retain failure evidence, so use this mode for diagnosis, not
CPU or latency comparisons. Ordinary builds compile these events out; benchmark
builds leave the trace disabled unless the environment variable is set.
