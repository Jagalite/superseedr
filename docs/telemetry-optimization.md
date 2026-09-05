# Peer telemetry optimization and fidelity checks

The optimization removes unused availability history work, shares immutable
peer bitfields across snapshots, and reuses exact availability results when
inputs have not changed. It preserves the production refresh rate, peer
admission policy, torrent-manager scheduling and complete peer data.

## Changes

- `TorrentDisplayState` now retains the capped availability sample count used by
  browser diagnostics. It no longer computes/stores 200 full availability arrays
  per torrent that the native renderer never read. The increment conditions and
  cap of 200 remain the same.
- `PeerState` and `PeerInfo` share bitfields through `Arc<Vec<bool>>`. Manager
  publication, comparison/retention and UI snapshot copies share those immutable
  values. Protocol changes use copy-on-write; old published snapshots retain
  their previous bit values. Bitfield decoding, geometry normalization and HAVE
  processing retain their original meaning. Duplicate HAVE still invokes work
  assignment, but does not needlessly detach an unchanged bitfield.
- Availability flash state checks torrent identity, piece count, peer keys and
  shared bitfield identity. It skips recomputation only when those inputs still
  match. Flash expiration continues on every animation update. Short/long
  bitfields retain the existing padding/truncation fallback.
- The heatmap reuses that availability result only after the same input check.
  Otherwise it computes availability directly as before. Its separate treatment
  of complete peers, flash colors and rendering logic remain intact.
- Peer bitfield serialization still produces/accepts the same boolean array.
  Browser peer snapshots retain their existing vector representation at the
  integration boundary.

No metrics are sampled less frequently, no peer rows or pieces are omitted,
and no synthetic-only shortcut is used for these optimizations. The changes
apply to normal production builds as well as the instrumented build.

## Accuracy validation

All 2,222 native library tests passed; one existing test was ignored. The first
attempt encountered sandbox restrictions on local networking/system network
configuration. The complete rerun with local networking available passed.

New tests verify:

- Old snapshots retain their bitfields after a real manager HAVE action; an
  unchanged duplicate HAVE preserves sharing.
- Bitfield JSON round trips retain the boolean-array representation and values.
- Cached flash/availability state matches full recomputation through 140 steps
  including churn, changing bitfields, piece counts, selection and expiration.
- Cached and recomputed heatmaps produce identical terminal cells and styles
  for empty, complete, partial and short bitfields at two terminal sizes.
- Browser diagnostic sample counting keeps its original conditions and cap.

Strict Clippy passed with all targets/features and warnings denied. Formatting
and diff checks passed. The browser/Wasm target compiled successfully; this is
build validation, not a browser runtime visual test.

The exact instrumented baseline binary is preserved locally at
`tmp/superseedr-before-telemetry-optimization`; its SHA-256 matches the earlier
clean manager-profile experiment. Both binaries use the repository's production
release optimization settings and the same opt-in profiling feature.

## Regression review

A follow-up review reproduced a cache invalidation error when duplicate peer
keys collapsed into one map entry or replaced another peer without changing the
input count. The availability sum includes every input peer, so map identity
alone was insufficient. The cache now also checks the ordered input keys;
duplicate-key snapshots and changed ordering conservatively recompute. A new
regression test covers both transitions and checks the resulting availability
against a full calculation. Normal native addresses include the transport, but
the cache no longer relies on their uniqueness for correctness.

After the guard fix, all 2,223 native library tests passed with one ignored.
The first full run failed a port hot-reload assertion and then 77 tests failed
with a poisoned shared lock; an unchanged full rerun passed. That intermittent
test failure has not been diagnosed. The targeted cache regression was observed
failing before the fix and passing afterward.

Remaining risk is concentrated in snapshot mutation and cache invalidation.
Copy-on-write isolation, churn, geometry changes, flash expiration and rendered
cells have direct tests. The terminal experiment does not establish long-running
public-swarm behavior with frequent HAVE messages, varied latency or full torrent
completion. Browser validation is compilation only. Frequent bitfield mutations
can still require full copies, so performance under that workload remains to be
measured. The SIGINT shutdown issue below also remains unresolved.

The terminal performance figures below were measured before this follow-up
cache guard; they have not been remeasured with the stricter invalidation check.

The subsequent full PR review integrated `develop` at `8c030844`, including the
current TUI redesign and Show theme. It also serialized the new upload test with
the existing shared environment lock, because the harness temporarily changes
transport and UDP chaos environment variables. This removes a test interference
path; it does not establish the cause of the earlier intermittent port failure.
Final validation against the updated base is recorded in the PR. The historical
performance measurements also predate these upstream UI changes.

## Production terminal comparison

The optimized run used the same fresh-profile, eight-torrent, 16-stable-seeder
setup as the instrumented baseline: 16,384 pieces per torrent, 60 FPS target,
129-by-35 Terminal window, 200 connection attempts/s maximum, and 1 MB/s total
background traffic. A separate final stress phase targeted 25 MB/s while churn
continued. All 136 periodic compiler checks found no compiler activity.

| Phase | Baseline FPS | Optimized FPS | Optimized connected peers | Optimized preparation ms/frame | Optimized frame gap p99 ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| Background only | 60.0 | 59.9 | 16–16 | 0.09 | 18.25 |
| 500 stable extra peers | 60.0 | 60.0 | 516–516 | 0.37 | 18.12 |
| 500 extra peers churning | 60.0 | 60.0 | 471–516 | 0.81 | 18.14 |
| 3,000 offered, stable lifetime | 46.9 | 59.9 | 1438–3016 | 1.22 | 18.91 |
| 3,000 offered, churn | 51.2 | 59.7 | 1337–3016 | 1.77 | 18.45 |
| 3,000 offered, churn + 25 MB/s target | — | 60.0 | 2484–3016 | 2.98 | 18.20 |
| Recovery | 60.0 | 60.0 | 16–16 | 0.85 | 18.29 |

Summaries omit each phase's first five seconds. The offered workload and
production policies are the same in the paired phases, but admitted counts are
not held constant: the optimized app accepted all 3,000 additional peers. The
last eight seconds of the stable high-count phase remained at 3,016 connected
peers and averaged 59.7 FPS. The baseline high-count phase had only 1,424–1,792
connected peers and averaged 46.9 FPS.

At high-count admission pressure:

| Measured path | Before | After |
| --- | ---: | ---: |
| Overall frame preparation | 20.69 ms/frame | 1.22 ms/frame |
| UI metrics drain | 17.23 ms/call | 0.69 ms/call |
| UI snapshot copying | 2.43 ms/frame | 0.39 ms/frame |
| Manager metrics publication | 1.35 ms/manager tick | 0.50 ms/manager tick |

Stage figures use aggregate windows wholly contained within the steady interval;
frame preparation uses the frame records directly. Nested durations are inclusive
and should not be added together as separate CPU costs. The old unused-history
scan no longer runs; actual heatmap availability calculations remain available
and are reused only when inputs match.

The extra traffic phase delivered approximately **23.08 MB/s** (about 185 Mbit/s)
against its 25 MB/s target, with 2,484–3,016 connected peers and 60.0 FPS average.
Its p99 frame gap was 18.2 ms. This is the observed throughput, not a claim of
sustained 25 MB/s. The generator recorded 13,918 successful churn handshakes and
zero connection errors across the complete run, versus timeouts while attempting
to reach 3,000 peers in the earlier baseline.

Occasional frame-spacing spikes remain: the stable high-count phase contained
a 62.3 ms maximum gap. This is not a hard-real-time guarantee. The generators are
local synthetic peers using one loopback IP; this is real production app/TUI and
storage execution, not public-swarm interoperability or full completion of the
32 GiB logical fixture set.

## Shutdown observation and artifacts

All seven measurement phases completed. Afterward, the previously observed
SIGINT shutdown failure recurred: the app continued rendering, and the controller
sent SIGTERM after its 35-second wait. This symptom was also observed before the
optimization in `tmp/production-tui-churn/`; these changes do not modify signal
handling or shutdown policy. Shutdown remains an unresolved issue and is not
claimed fixed or validated by this performance improvement.

One incomplete trailing frame record was excluded after forced termination. The
last complete frame is more than 37 seconds after the last measurement phase,
so the measured windows are intact. All aggregate records parsed successfully.
The app and generator were verified stopped, and the dedicated Terminal window
was closed.

Local evidence is in `tmp/manager-churn-optimized/`: `frames.jsonl`,
`peer-samples.jsonl`, `profile/*.jsonl`, `phases.json`, `analysis.json`,
`profile-analysis.json`, binary hashes in `environment.json`, isolated settings
and fixtures, launch/measurement/analysis scripts, terminal dimensions, app exit
status and cleanup verification. These local artifacts are ignored by Git.

The implementation and report are maintained on `perf/tokio-benchmark-metrics`
in the isolated worktree `/private/tmp/superseedr-tokio-benchmark-metrics`.
