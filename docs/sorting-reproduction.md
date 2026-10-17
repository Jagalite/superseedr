# Candidate-selection slowdown reproduction

Follow-up: [deeper scheduling measurements](scheduling-follow-up.md) identify
rarity rebuilding and remaining candidate sorts as CPU bottlenecks. An on/off/on
comparison within one live client restored over 100 Mbps of successful writes
with two further computation optimizations, now applied locally. HDD admission
errors alone did not explain the slowdown. The guard-only results below remain
historical evidence of that narrower change.

The synthetic reproduction confirms that unnecessary candidate selection can
severely limit download throughput. `AssignWork` first fills requests from pieces
already in progress. It then collects and sorts new-piece candidates even when
that refill has consumed every available request slot. None of those sorted
candidates can be used. `PieceWrittenToDisk` also calls `AssignWork` for every
peer, multiplying this work as pieces finish.

The fix changes only the new-piece candidate iterator when
`available_slots == 0`. It returns an empty iterator in that case, preserving the
request batch already assembled from pending pieces. This guard is now applied
to the local production scheduler. The earlier growth-rule experiment is shelved;
the session source matches the original revision, and UI behavior is unchanged.

## Regression coverage

`src/torrent_manager/assignment_tests.rs` exercises the real state action with
33,080 fictional 1 MiB pieces. A full pending refill must emit exactly the same
512 block requests and perform zero new-candidate visits, across normal/endgame
mode and priorities enabled/disabled. A partial refill must still select new
pieces by rarity and priority and preserve request bookkeeping.

The corrected regression failed before the guard: **33,072 candidate visits**
instead of zero. The partial-refill check already passed. After the guard,
`cargo test --locked --lib torrent_manager::state:: -- --nocapture` passed all
**150 tests**, including existing property tests. The visit counter exists only
under `cfg(test)`; it adds no production instrumentation. These checks assert
work avoided and requests preserved, rather than machine-dependent timing.

## Live replay with the guard

On September 27, 2026, at approximately 22:41–22:46 Eastern, a fresh isolated
client replayed the same three user-selected torrents in the original submission
order and offsets. Both smaller torrents completed naturally. The run retained
the development profile, original growth rule, UI, unlimited configured rates,
and soft file-descriptor limit of 2,560 used by the earlier instrumented baseline.
The watcher compresses the actual manager-creation spacing in both runs. The
original background torrent population was not restored. No compilation or
other experiment client ran concurrently with the measurement.

Both builds contain the same probes; the only runtime source difference is the
candidate guard. The earlier baseline and this run used fresh separate payloads.
They are sequential live-swarm observations, not controlled identical peer sets.

| First three minutes after both competitors complete | Original | Guard |
| --- | ---: | ---: |
| Target session-received rate, minute 1 | 11.94 Mbps | 57.09 Mbps |
| Target session-received rate, minute 2 | 10.83 Mbps | 38.58 Mbps |
| Target session-received rate, minute 3 | 10.25 Mbps | 26.98 Mbps |
| Target session-received average | 11.01 Mbps | 40.88 Mbps |
| Successful backend-write average | 8.30 Mbps | 18.27 Mbps |
| Failed backend-write attempts | 0 | 533 |

Session counters flush approximately every five seconds, so these minute rates
have boundary error and exclude counters not flushed before peer teardown.
Received bytes are not committed progress, and write totals are successful
backend completions. Failed writes can cause retries and additional traffic.
Independent probes were used because baseline status publication had a 139.73 s
gap; the changed run's largest status gap was 17.73 s.

Between completion + 10 s and the stop request, parsed-block-to-session delay
fell from **2,256.85 ms to 288.31 ms** on average, and session-to-manager wait
fell from **206.39 ms to 34.14 ms**. These are elapsed completed-operation timings,
including scheduling delays; pending operations are not represented. Global
download-token waits remained negligible. Active peer windows remained small
(changed run median 12, range 8–22), with median available capacity zero.

The scheduler change improves this replay but **does not fully resolve the live
slowdown**. The changed run repeatedly reports `payload admission full` from the
native payload backend, whose operation and byte budgets use nonblocking
semaphore acquisition. This is distinct from the global resource permits.
Successful backend writes also have a long latency tail (post-completion median
239 ms, p95 4,394 ms). A displayed burst reached 111 Mbps, but the final displayed
sample was back near 10 Mbps. The storage admission failures require separate
investigation; this experiment does not prove their cause or identify which
payload budget was exhausted. Growth and UI changes remain deferred.

The client received stop at elapsed 287.44 s and exited with status zero at
294.92 s; no manager shutdown timeout was logged. Evidence is preserved under
`tmp/window-recovery/instrumented/sorting-fixed/`, including raw logs, probes,
status samples, manifest/source hashes, and `live-comparison.json`. The shared
analysis is `tmp/window-recovery/instrumented/compare_sorting.py`. Regression
before/after logs and the shelved growth patch are under
`tmp/window-recovery/fix-validation/`.

## Why the earlier synthetic missed this

The previous window-recovery fixture had one complete seeder per torrent,
512/512/4,096 pieces, and 16 KiB pieces containing one block each. All pieces had
equal rarity. It exercised request-window recovery but did not represent the
large live torrent's piece count, multi-block pieces, varied rarity, or peer
fanout. Attributing the incident to the window controller from that fixture was
premature.

The new isolated cost test runs the real `TorrentState::update(AssignWork)` with
64 deterministic peer bitfields. Each of 16 calls starts from the same cloned
state; cloning is outside the action timers. It verifies that all 512 requested
blocks are emitted. Mean candidate-sort times in the development/test build:

| Pieces | Equal rarity | Varied rarity |
| --- | ---: | ---: |
| 1,377 | 0.83 ms | 3.96 ms |
| 4,096 | 2.07 ms | 11.58 ms |
| 33,080 | 23.53 ms | 98.47 ms |

The whole action includes additional scan/refill work. These timers measure
elapsed wall time, including descheduling, rather than exclusive CPU time.

## Native TCP reproduction

All runs use the original revision
`f51cd98357517e0c21ed1efe22275b2d21860826`, with test-only timing probes, real
TCP peer sessions, torrent managers, hash verification, and temporary native
payload storage. The session source is byte-for-byte identical to that revision:
the original growth rule is retained. No UI or discovery runs in this harness.

- The target has 33,080 pieces. Competitors have 1,377 and 1,395 pieces.
- Every piece is 1 MiB, composed of 16 KiB blocks; payloads are fictional,
  generated bytes with valid SHA-1 piece hashes.
- Each torrent has 32 peers. Four advertise all pieces; the others advertise
  reproducible subsets, producing varied rarity. All remain unchoked.
- Responses become eligible after 40 ms, with at most one block sent every
  2 ms per source. Distinct loopback ports identify peers.
- Eight Tokio worker threads, an unlimited download token bucket, and shared
  connection/disk resources are used throughout.
- Runs execute sequentially, without concurrent builds or other experiment
  clients. Temporary payloads are deleted when each test exits.

The short matrix starts one, two, or three torrents. The large target is always
present; the smaller competitors are added before it. Each run observes 45
seconds. Rates below use manager useful-byte deltas from seconds 10–45:

| Torrents started | Original candidate selection | Skip new candidates when full |
| --- | ---: | ---: |
| Large target alone | 14.23 Mbps | 548.49 Mbps |
| One smaller competitor + target | 21.79 Mbps | 89.09 Mbps |
| Two smaller competitors + target | 4.42 Mbps | 72.84 Mbps |

These are one run per cell, not medians or predicted Internet speeds. Competitors
can finish sooner after the change, so the table describes the outcome of each
started workload, not a fixed active-torrent count throughout every interval.
There is also a baseline uniform-rarity three-torrent control: the target
averaged 3.56 Mbps despite lower aggregate sort time. Reducing sort complexity
alone does not remove unnecessary selection calls or all other runtime costs.

## Recovery after competitors complete

A separate 100-second original-controller run let both smaller downloads finish
naturally, at 50 and 54 seconds. The large target then averaged **5.90 Mbps**
over seconds 60–100. During those 40 seconds, its completed candidate sorts
accounted for **26.11 seconds** of elapsed time. **605 of 634 sorts** ran with
zero request slots left.

In the changed three-torrent run, the smaller downloads completed at 30 and 31
seconds. A common short comparison approximately 6–14 seconds after both
competitors completed gives:

| Measurement | Original, elapsed 60–68 s | Changed, elapsed 37–45 s |
| --- | ---: | ---: |
| Target useful download | 5.08 Mbps | 128.49 Mbps |
| Target successful committed-byte progress | 5.24 Mbps | 102.75 Mbps |

This is an eight-second recovery comparison, not a full 40-second recovery
comparison for the changed build. Received and committed bytes need not match
within a short window because pieces finish and are written later.

The synthetic demonstrates a local scheduling bottleneck without changing the
growth rule. It does not reproduce every property of the live swarm. In
particular, all synthetic peers remain unchoked, and real discovery, transport
mix, remote behavior, and application/UI work are excluded.

## Interpreting “three or more torrents”

The user clarified that the healthy one/two-torrent observations concerned the
smaller downloads, not a confirmed healthy run of the same large batch alone.
Torrent count was therefore not isolated in the original observation. The new
synthetic is slow with the large target alone and worse with two competitors;
it does not establish a hard three-torrent cutoff. The workload's piece count,
peer availability, and repeated scheduling cost matter.

## Validation and reproduction artifacts

All eight native TCP runs passed their peer-admission, verified-progress, and
manager-shutdown checks. The cost matrix passed. The changed build also passed
14 existing assignment checks, eight endgame checks, and a new pending-refill
check that verifies the full block-request set is preserved without claiming
additional pieces. These synthetic results are characterization, not production
release qualification. The separate live replay above provides live-swarm
evidence and its remaining limitations.

Artifacts are in the ignored local directory `tmp/window-recovery/sorting/`:

- `baseline-instrumentation.patch`: test probes and the original fixture applied
  to the revision above; `skip-full-candidates.patch`: the comparison's scheduler
  change.
- `prepare.py`, `reproduction.rs`, `probe.rs`, `pending_refill_test.rs`, and
  `skip_full.py`: source and preparation helpers for the isolated copy.
- `run_matrix.py`, `summarize.py`, all per-case logs, `results.json`, and
  `post-completion-comparison.json`: execution and measured results.
- `manifest.json`, source/binary hashes, and `artifact-sha256.json`: provenance.

To rerun the existing preserved binaries without rebuilding:

```sh
python3 tmp/window-recovery/sorting/run_matrix.py tmp/window-recovery/sorting/baseline-tests baseline-repeat 1,2,3
python3 tmp/window-recovery/sorting/run_matrix.py tmp/window-recovery/sorting/skip-full-tests skip-full-repeat 1,2,3
```

To rebuild independently, extract the recorded revision into a fresh isolated
directory and apply `baseline-instrumentation.patch`. Run the ignored
`candidate_sort_cost_matrix` or `native_sorting_reproduction` library test
explicitly. The latter accepts `SORT_TORRENTS`, `SORT_PEERS`, `SORT_SECONDS`, and
the presence of `SORT_UNIFORM`. Apply `skip-full-candidates.patch` for the changed
version. Keep runs sequential and preserve the development profile for comparison.
