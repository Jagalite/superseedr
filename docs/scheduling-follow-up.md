# Scheduling follow-up after the full-pipeline guard

The native payload admission errors do not explain a fixed low download-speed
ceiling. A longer replay with the existing guard received 170 and 160 Mbps in
its first two post-completion minutes despite many admission errors. Its final
two minutes fell to 44 and 38 Mbps with zero failed write attempts.

These are observations from a development build at revision
`f51cd98357517e0c21ed1efe22275b2d21860826` plus the candidate guard and isolated
instrumentation. The main checkout now includes the candidate guard, per-sort
rarity-key caching, and indexed rarity counting. The live experiments used
ignored source copies with reversible switches and instrumentation that are
absent from production.

## Five-minute replay with the existing guard

The same three input hashes were submitted in the same order and offsets, with
fresh payloads on the same HDD volume and the same settings as the prior replay.
Both smaller torrents completed at elapsed 95.58 seconds. Per-session counters
flush about every five seconds, so minute boundaries have some error; counters
not flushed before peer teardown are missing. Received bytes are distinct from
successful writes and can include data that must later be downloaded again.

| Minute after the smaller torrents complete | Received | Successful backend writes | Failed write attempts |
| --- | ---: | ---: | ---: |
| 1 | 170.50 Mbps | 24.47 Mbps | 1,930 |
| 2 | 159.72 Mbps | 28.52 Mbps | 1,756 |
| 3 | 93.57 Mbps | 45.02 Mbps | 416 |
| 4 | 43.50 Mbps | 33.69 Mbps | 0 |
| 5 | 37.69 Mbps | 30.76 Mbps | 0 |

In a three-second process sample starting around elapsed 270 seconds, the busy
manager worker had 237 sampled stacks: 139 included rarity rebuilding, 69
included candidate sorting, and another 23 included other state-update work.
Those are sampled stack residence counts, not exclusive CPU accounting. Sorting
is a subset of state updates; it is not counted twice in the figures above.

Across the five post-completion minutes, flushed target-manager measurements
recorded 27.54 seconds scanning candidates and 67.85 seconds sorting them. The
outer state/effect application recorded 107.34 seconds. These elapsed timers
include descheduling and probe overhead, and exclude unfinished/unflushed work.
The once-per-second rarity rebuild runs outside that outer state-action timer.

The guard still works: zero-capacity calls use empty candidate pools. Work
remains when capacity exists, and every completed piece triggers assignment for
all peers. Separately, `BlockManager::update_rarity` does a hash-map update for
every advertised piece of every peer. `PieceManager::update_rarity` then copies
and filters the resulting map. Both run synchronously on the manager task.

The manager's biased select checks its rarity timer before its peer-command
receiver. Long synchronous rebuilds therefore directly reduce time available to
process incoming blocks. This is application work, not evidence of a Tokio
semaphore leak or of remote peers withholding data. In the preceding replay's
last low-speed interval, already-parsed blocks waited about 2.3 seconds for
session handling while no new write failures occurred.

Evidence: `tmp/window-recovery/instrumented/deep-guard/`. The stop command was
queued at elapsed 395.70 seconds but was not handled until about 424.69 seconds.
The runner's 30-second fallback then closed the PTY; wait status was 1. This was
not a clean graceful shutdown. Performance analysis excludes records at or after
the stop request. The final sample also shows sustained UI bitfield scanning;
UI changes remain deferred.

## Controlled changes within one live client

The next experiment retains the same scheduler decisions and toggles two ways
of computing them without restarting, pausing, or reconnecting peers:

- `cached`: compute each rarity key once per candidate sort, using
  `sort_by_cached_key`, in normal mode without explicit priorities. This caches
  keys only for that sort; it does not retain an ordered piece list across actions.
- `dense`: accumulate rarity counts in a temporary indexed vector, then populate
  the existing hash map once. Counts, absent zero-count entries, uneven bitfield
  lengths, and stale-entry removal are preserved.
- `both`: enable both changes. `original`: use the current guard with neither
  additional optimization.

Modes are scheduled relative to the smaller downloads completing: original for
60 seconds, cached for 60, dense for 60, both for 90, original for 60, then both
for 90. The first ten seconds after each observed switch are excluded from phase
summaries. Peers and their request windows can still evolve during the run, so
the reversal strengthens causal evidence without creating identical workloads.
The growth rule, UI, resource limits, payload backend, and HDD volume are retained.
Three-second process samples run at 10 ms intervals roughly once a minute.

Before the live comparison, the experimental source passed 148 state tests and
19 block-manager tests with both optimizations enabled. The latter include a
new count-equivalence check using 33,080 pieces and changing peer populations,
including short/empty bitfields and complete peer removal.

Evidence: `tmp/window-recovery/instrumented/deep-crossover/`; source preparation
and analysis scripts are in its parent directory. Live phase results are recorded
in `crossover-results.json`.

## Live comparison results

Both smaller torrents completed at elapsed 188.65 seconds. Rates below exclude
the first ten seconds after completion or each observed mode switch. Sorting
timers are included only when their complete flush interval falls within the
phase; `sorting_covered_s` records their actual coverage. All rates are Mbps.

| Mode, in order | Measured seconds | Received | Successful backend writes | Failed write attempts |
| --- | ---: | ---: | ---: | ---: |
| Existing guard only | 50.51 | 60.98 | 20.59 | 292 |
| Cached sort keys only | 49.85 | 159.93 | 51.66 | 1,133 |
| Indexed rarity counting only | 49.83 | 85.85 | 76.09 | 0 |
| Both optimizations | 80.18 | 116.91 | 113.61 | 0 |
| Existing guard only, restored | 49.72 | 38.61 | 33.40 | 0 |
| Both optimizations, restored | 80.60 | 109.43 | 105.64 | 0 |

The last three phases demonstrate the reversal without write failures: useful
storage throughput falls when the extra CPU work is restored, then recovers
when it is removed. Peers were not deliberately paused or reconnected. Natural
peer churn continued, and the median active request window was 11, 11, and 12
across those three phases; the growth controller was unchanged.

Rarity rebuilds averaged 624.56 ms, with a 1,407.08 ms maximum, in the restored
guard-only phase. They averaged 65.59 ms with a 137.72 ms maximum after both
optimizations were restored. Parsed-block-to-session delay fell from 829.26 ms
to 279.97 ms on average across that same reversal. This remains nonzero local
delay; the result is not a guarantee of 200 Mbps or of all bottlenecks disappearing.

The source explains the measured delay: rarity rebuilding repeatedly hashes
each peer-piece pair, and candidate sorting repeatedly hashes each comparison's
rarity key. Both prevent the manager from servicing queued work while executing.
The changes reduce those hash operations while preserving counts, normal-mode
sort order, priority behavior, endgame behavior, and the one-second rarity cadence.
Indexed counting needs temporary storage proportional to the longest bitfield
(about 258 KiB for 33,080 `usize` counts on this platform).

The run requested stop at elapsed 609.35 seconds and exited with status zero at
629.67 seconds, but all three managers failed to reply within the 20-second
shutdown timeout. That shutdown limitation is recorded, not treated as a clean
manager shutdown. Performance summaries exclude shutdown. UI scanning and the
growth rule remain deferred.

## Checkout validation

The production checkout passed 150 state tests, 19 block-manager tests, and 22
piece-manager tests after applying the changes (191 targeted tests total).
Assignment coverage now also asserts that normal candidate selection reads each
candidate's rarity at most once, using a test-only counter. The earlier full
pending-refill regression and exact request-set checks remain in place. The
rarity count-equivalence test is included in the normal block-manager suite.
The normal development binary builds successfully; formatting and diff checks
also pass. No experiment clients remain running.
Logs are under `tmp/window-recovery/fix-validation/follow-up-*.log`.

### Focused property-test follow-up

Three proptests now exercise these optimizations through the normal test suite:

- `prop_full_refill_preserves_work_without_scanning` varies block geometry,
  existing active requests, endgame/normal mode, priorities, and piece count. It
  checks the exact remaining request set and bookkeeping, unchanged queues, and
  zero candidate visits/rarity lookups once pending refill fills the window.
- `prop_assignment_matches_original_stable_sort` varies candidate order, tied or
  missing rarity values, availability, verifying/writing exclusions, priorities,
  and occupied window slots. Both normal and priority selection are checked
  against the original stable-sort semantics, including exact request order and
  resulting request/queue bookkeeping. Normal selection also checks the rarity
  lookup bound. Large generated cases use 33,080 candidates.
- `prop_rarity_rebuild_matches_original_across_snapshots` compares complete maps
  against the original counting loop across generated peer snapshots, uneven and
  empty bitfields, reversed peer order, and removal of stale counts.

Validation used `PROPTEST_CASES=1000 PROPTEST_RNG_SEED=20260927` for the state and
block-manager suites: all 152 state tests and 20 block-manager tests passed. All
22 piece-manager tests also passed (194 tests total; 3,000 cases across the three
new properties). The seed controls generated inputs; endgame request ordering
still uses engine randomness, so that property compares sets rather than order.
These are focused behavior/work-count checks, not whole-engine differential
replay against another revision.

Commands:

```sh
PROPTEST_CASES=1000 PROPTEST_RNG_SEED=20260927 cargo test --locked --lib torrent_manager::state::
PROPTEST_CASES=1000 PROPTEST_RNG_SEED=20260927 cargo test --locked --lib torrent_manager::block_manager::
cargo test --locked --lib torrent_manager::piece_manager::
```

Logs: `tmp/window-recovery/fix-validation/assignment-proptest-{state,block,piece}.log`.

### Deferred review finding

Dense rarity counting allocates from the longest peer bitfield. Before magnet
metadata arrives, peer bitfields are not clamped to a known piece count, while
the rarity timer still runs. A local allocation check using the original and
changed counting functions showed that a 1 MiB all-zero wire bitfield causes
64 MiB of additional peak counter allocation with the dense implementation,
versus none with the original sparse counting loop. Bounding or deferring this
work before metadata is intentionally deferred; this change retains that risk.
