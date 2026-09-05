# Production TUI frame test

A short controlled run **did not reproduce sustained, download-related FPS
loss**. The production Normal screen stayed near 60 FPS at 1 MB/s, 25 MB/s,
and approximately 144 MB/s. There were brief frame-spacing spikes, including
28.61 ms at 25 MB/s. The highest-rate phase had a maximum gap of 18.46 ms.

This is a limited negative result: the high phase briefly reached about
250 MB/s for one second, then averaged about 144 MB/s after settling. It does
not establish behavior during a sustained 200+ MB/s download.

A subsequent [peer-churn test](production-tui-peer-churn.md) reproduced
sustained FPS loss at much lower download rates when many peers were admitted.

## Setup

- Run on 2026-09-04, approximately 23:06–23:07 America/New_York.
- macOS arm64; the normal production `App::run` loop in an actual Terminal
  application window, including real terminal output, normal application
  handlers, system telemetry, and adaptive tuning.
- Release build using the repository's LTO and single codegen unit, with the
  `synthetic-load` feature and `--cfg tokio_unstable` for tooling. No synthetic
  subcommand was used for the application.
- Two generated v1 torrents, each 4 GiB logical size, 256 KiB pieces, zero-filled
  payloads, and 16 local TCP peer listeners. Incoming blocks were written and
  completed pieces verified through the production manager/storage path.
- A separate Python process supplied the local tracker and synthetic peers.
  It shared the machine, but not the application's Tokio runtime.
- An isolated standalone profile was verified with `show-configs --all` before
  launch. Settings, locks, watch folder, logs, metadata, and persistence were
  under the test directory. User settings and saved torrents were not loaded.
- Target 60 FPS, default theme, auto layout. Terminal reported 129 columns and
  35 rows after the run; its requested 140×45 size was constrained by the desktop.
- DHT disabled and only the generated local tracker advertised peers. This does
  not cover public-swarm discovery or uTP behavior.
- No Rust compilers were present at any of the four phase-start checks. The
  release build and Clippy finished before the application launched.

The phases were low traffic (8 seconds), 200 Mbit/s (10 seconds), high traffic
(12 seconds), and recovery (10 seconds). Statistics below exclude the first
second after each rate change. All phase intervals had 16 connected peers and
ongoing payload delivery.

## Results

MB/s below means decimal megabytes per second. 25 MB/s is 200 Mbit/s.

| Phase | Actual received MB/s | Average FPS | Frame interval p99 | Maximum frame interval |
| --- | ---: | ---: | ---: | ---: |
| Low | 1.00 | 59.99 | 19.15 ms | 22.27 ms |
| 200 Mbit/s | 25.03 | 60.00 | 17.71 ms | 28.61 ms |
| High | 143.87 | 60.00 | 18.09 ms | 18.46 ms |
| Recovery | 1.00 | 60.00 | 18.35 ms | 25.81 ms |

During the high phase, frame preparation p99 was 0.97 ms, drawing p99 was
0.77 ms, and frame-start lateness p99 was 1.92 ms. The longest measured manager
telemetry handler was 0.206 ms. These observations do not point to a Tokio
scheduler bottleneck for this particular workload.

All rendered frames finished within one frame period of their scheduled start
(`over_budget = false`). This does **not** mean perfectly uniform presentation:
a late frame can be followed by a shorter interval and still preserve average
FPS. The 28.61 ms gap is real application-side jitter. The terminal emulator's
later display/compositing latency was not measured.

The reason throughput settled below its 250 MB/s target was not attributed.
Application backpressure, storage, and generator capacity need to be separated
before treating the attempted rate as an achieved sustained workload.

## Shutdown and excluded startup

All four measurement phases completed. Ctrl+C then triggered the application's
20-second shutdown timeout: two managers did not acknowledge shutdown, and the
application forced exit. Its process returned zero. The controller had waited
only 10 seconds for shutdown and therefore reported a missing exit file before
the application finished. The eventual exit file and application log confirm
the timeout. This is a separate shutdown finding, not evidence of a frame stall
during the measured phases; clean manager shutdown was not validated.

An earlier startup used private-marked fixtures, which the normal build
correctly rejected. No download occurred in that attempt. The generated fixtures
were corrected for the normal build, and that startup is excluded from the table.

The test application and peer process were stopped, and the dedicated Terminal
window was closed. Generated data and evidence remain in ignored local paths.

## Evidence and validation

- [Frame records](../tmp/production-tui-test/frames.jsonl)
- [Per-phase analysis](../tmp/production-tui-test/analysis.json)
- [Traffic phases](../tmp/production-tui-test/phases.json)
- [Independent peer counters](../tmp/production-tui-test/peer-samples.jsonl)
- [Build hash and environment](../tmp/production-tui-test/environment.json)
- [Verified application paths](../tmp/production-tui-test/config-snapshot.json)
- [Application exit](../tmp/production-tui-test/app-exit.json)
- [Local tracker/peer generator](../tmp/production-tui-test/peers.py)
- [Production application launcher](../tmp/production-tui-test/launch_app.py)
- [Phase controller](../tmp/production-tui-test/measure.py)
- [Analysis script](../tmp/production-tui-test/analyze.py)

The release build, strict all-target/all-feature Clippy, formatting, and diff
whitespace checks passed. The generator's handshake, info hash, and block data
were checked independently before the application run. Evidence paths are local
ignored artifacts and do not automatically accompany a Git checkout.

A closer reproduction should match the user's terminal emulator, window size,
visualizations, torrent/peer counts, and a sustained achieved download rate.
