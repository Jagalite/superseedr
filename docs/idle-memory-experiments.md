# Idle memory candidate experiments

> Archived experiment report. The prototypes and raw artifacts live in the separate
> `superseedr-memory-lifecycle` worktree. See [selected implementation](memory-selected-implementation.md)
> for the narrower changes adopted on develop; measurements below are historical
> and do not describe a build containing only those changes.

This follow-up compares against the previously optimized memory-lifecycle branch,
not the original application baseline. The catalog remains untouched. All payloads
and peer identities are synthetic, and all Rust compilation is submitted through
Kyoto. Saved binaries and raw results are under `tmp/memory-lifecycle/`.

## Candidates and comparison controls

- `logging`: reduce the asynchronous logging queue from 128,000 to 4,096 slots.
  Existing lossy-on-full behavior and shutdown/flush handling are retained. This
  lowers the burst capacity; it does not establish that logs can never be dropped.
- `lazy`: allocate the reader's processing buffer on demand rather than reserving
  64 KiB for each peer. The 17 KiB socket receive buffer remains allocated.
- `reclaim`: after five seconds waiting without reader/writer traffic, release
  spare processing/batching buffer capacity. Keep all bytes of incomplete frames.
  Empty buffers release all capacity; active connections retain reuse behavior.
- `combined`: all three changes. `prior` disables all three.

The `memory-profile` feature exposes `SUPERSEEDR_MEMORY_VARIANT` for controlled
experiments. The runner sets this explicitly through `--memory-variant`, records
it, and strips inherited application overrides. Ordinary builds use the candidate
behavior without reading experiment environment variables.

The same executable runs each variant. Independent idle comparisons alternate
variant order over two repetitions with one torrent and 32 peers. The recovery
workload stops requesting upload payload at connection age five seconds, drains
outstanding replies, then resumes requests at twenty seconds without disconnecting.
It measures thirty seconds and verifies both a quiet interval and resumed traffic.
The pause control is restricted to instrumented synthetic builds.

Native synthetic counterparts live in the measured process. In particular, idle
counterparts reserve 64 KiB per peer themselves; these are not production buffers.
Requested Rust heap, sampled RSS, process-lifetime peaks and phase measurements
are kept distinct. RSS may retain allocator pages after buffers have been freed.

## Correctness checks

Kyoto test job `83f1de2d4ae74f82a10014fce95421d4` compiled
`cargo test --lib --no-default-features --features memory-profile,dht,pex` under its
compile-only policy. The resulting executable was then run directly: 183
networking tests, 11 logging tests, 316 torrent-manager tests, and 25 synthetic
harness tests passed (535 total). Nine existing manager tests remained ignored.
New coverage verifies partial-frame preservation, empty capacity release, reader
resume after a fragmented frame spans the idle interval, and writer resume after
idle reclamation. Native WebRTC is excluded from this unit-test configuration.

Receipts: `tmp/memory-lifecycle/idle-candidate-tests/results.json` and adjacent
logs include the test executable identity and return codes. Compilation alone is
not counted as passing tests. The first release attempt was stopped by Kyoto for
host pressure; its queued retry was cancelled before submitting a sequential
single-worker release build.

Both full-feature release attempts (`cc93de3b24a447c1bd8e04c7f7430b2f` and
`cd9be47aa8144688aac4a3d38e0bfbac`) were stopped for host pressure and cancelled.
The comparison build was resubmitted as `--release --no-default-features
--features memory-profile,dht,pex`, with LTO disabled, 16 codegen units and one
Cargo build worker. Comparisons must be made within this new binary; absolute
values should not be treated as a matched comparison to the earlier full-feature
executable. All candidate variants share the exact same executable and build
settings. WebRTC runtime qualification is outside this follow-up.

## Reproduction

```sh
CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 CARGO_BUILD_JOBS=1 \
  python3 /Volumes/seed1/Projects/kyoto/kyoto.py submit -- \
  cargo build --release --no-default-features --features memory-profile,dht,pex

python3 scripts/measure-idle-memory-candidates.py \
  --binary /absolute/saved-instrumented-executable --out /absolute/new-results-directory
python3 scripts/summarize-idle-memory-candidates.py /absolute/new-results-directory
```

Release job: `503c8fbab85b4806823e27c055029a6c`, succeeded.
Executable SHA-256:
`cbc7ea924c320e4ae00aafa929b34c6e29ad44490a2e6fea7633573efb832cff`.
The runner refuses an existing output directory, preserves generated payloads,
and records every variant, command, result and binary hash. Existing artifacts
from the earlier experiment remain separate.

## Independent fresh-idle results

Two repetitions per variant, 32 peers, one torrent, 12-second measurement windows.
Values are medians. All ten runs passed.

| Variant | Peak RSS MiB | Live heap at end MiB |
| --- | ---: | ---: |
| prior | 24.15 | 10.23 |
| logging | 20.41 | 6.44 |
| lazy | 23.41 | 8.23 |
| reclaim | 24.16 | 8.23 |
| combined | 19.80 | 4.44 |

Logging alone releases about 3.78 MiB of live heap, consistent with reducing
128,000 32-byte channel slots to 4,096. Lazy reader processing saves about 2 MiB
across 32 peers. Reclamation also saves about 2 MiB after the idle timeout, but
its peak RSS is essentially unchanged. Lazy allocation and idle reclamation
largely overlap for fresh-idle readers; their savings should not be added twice.
Combined end heap falls about 57%, while peak RSS falls about 18% in this series.
Native counterpart buffers are included in these totals.

## Active to idle to active recovery

Two repetitions per variant. All six runs verified zero upload progress in the
quiet interval and positive progress after resuming on the same connections.
Quiet values are medians of samples at elapsed seconds 12–15, then medians
across repetitions.

| Variant | Quiet live heap MiB | Quiet RSS MiB | Peak RSS MiB |
| --- | ---: | ---: | ---: |
| prior | 18.98 | 31.20 | 36.52 |
| reclaim | 16.51 | 36.74 | 37.43 |
| combined | 12.55 | 29.30 | 32.20 |

Reclamation alone reduced quiet live heap by about 2.47 MiB. Combined changes
reduced it by about 6.43 MiB (34%). RSS did not consistently follow reclamation:
the reclamation-only runs had higher quiet RSS than the prior runs. Allocator
retention and host paging make a requested-heap reduction different from an
immediate resident-memory reduction. No allocator purge was forced.

The synthetic upload counterparts retain a 64 KiB socket buffer and a 256 KiB
parse-buffer reservation per peer, including during the pause (10 MiB across
32 peers). That fixture cost is included in these process totals and was not
changed. Recovery averages include the intentionally idle portion; use the
separate uninterrupted upload series to assess maximum throughput.

## Churn, scaling, slow traffic and throughput

The following compare `prior` and `combined` within the new executable. All 34
synthetic runs passed, including shutdown/session checks and the explicit pause
and resume checks. Three repetitions were used for uninterrupted upload and
download; the churn, scaling and slow cases each have one run per variant.

| Case | Variant | Runs | Peak RSS MiB | End live heap MiB | Download MiB/s | Upload MiB/s |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| churn | prior | 1 | 26.14 | 10.45 | 0.00 | 0.00 |
| churn | combined | 1 | 21.38 | 4.68 | 0.00 | 0.00 |
| scale | prior | 1 | 34.38 | 28.14 | 0.00 | 0.00 |
| scale | combined | 1 | 28.25 | 16.33 | 0.00 | 0.00 |
| slow | prior | 1 | 35.16 | 25.78 | 0.00 | 0.50 |
| slow | combined | 1 | 31.08 | 20.00 | 0.00 | 0.47 |
| throughput | prior | 3 | 35.41 | 20.47 | 0.00 | 536.04 |
| throughput | combined | 3 | 31.27 | 14.39 | 0.00 | 570.48 |
| download | prior | 3 | 90.67 | 61.94 | 11.93 | 0.00 |
| download | combined | 3 | 91.41 | 62.06 | 11.93 | 0.00 |

`scale` uses 128 idle peers; other cases use 32. Churn measured 30 seconds with
two-second sessions. Both churn variants reported zero idle payload and zero
unexpected failures. The 128-peer combined candidate saved about 11.8 MiB of
requested live heap and 6.1 MiB of peak RSS.

Uninterrupted upload median was 536.04 MiB/s prior and 570.48 MiB/s combined.
Ranges were 328.70–579.06 and 545.16–590.07 MiB/s respectively. These short,
variable measurements show no consistent additional throughput penalty; they do
not establish a speedup or undo the earlier original-baseline throughput finding.

The first download pair increased peak RSS from 90.67 to 103.73 MiB, so two
additional alternating pairs were run. Final median peak RSS was 90.67 versus
91.41 MiB; ranges were 86.62–95.84 and 88.95–103.73 MiB. Median end heap was
61.94 versus 62.06 MiB. No active-download memory improvement is established;
partial-piece assembly and scheduling variation dominate these small structural
savings in this workload. Both variants sustained the configured download rate.

The slow-upload case requested 0.001 Gbit/s, but actual averages were about
0.50 MiB/s prior and 0.47 MiB/s combined, with bursty delivery. The limiter did
not establish the intended constant rate. This single pair is diagnostic evidence
of retained buffers under slow service, not precise rate-matched qualification.

## Interpretation and delivery

The strongest additional reduction is the smaller fixed logging queue. Lazy
readers avoid per-peer reservations. Reclamation demonstrably releases spare
heap capacity after inactivity and preserves protocol progress, but it does not
guarantee immediate physical-memory return. Lazy allocation and reclamation
have overlapping savings, so the independent reductions are not fully additive.

All three candidates remain in the isolated experimental worktree, with the
comparison switches retained only for instrumented native builds. A 4,096-slot
logging queue has less burst tolerance than 128,000 slots; existing lossy-on-full
behavior was tested, but high-volume production logging was not qualified.
No install, commit, merge or deployment was performed. The real catalog and
original checkout were not modified. Results are local TCP synthetic evidence,
not full application/UI or real-catalog memory measurements.

Formatting, patch whitespace checks and Python compilation passed. The earlier
report's Clippy result predates these follow-up changes; no new Clippy result or
full-feature release qualification is claimed here.

Final raw results are in `tmp/memory-lifecycle/idle-candidates/`, including
`matrix.json` and `analysis.json`. Build, binary and test identities are retained
under `tmp/memory-lifecycle/artifacts/` and `idle-candidate-tests/`. The new
cumulative source patch and archive are `artifacts/idle-candidates.patch` and
`artifacts/idle-candidates-source.tar.gz`; earlier optimized-binary artifacts
remain separate.
