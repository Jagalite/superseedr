# Memory lifecycle experiments

> Archived experiment report. The prototypes and raw artifacts live in the separate
> `superseedr-memory-lifecycle` worktree. See [selected implementation](memory-selected-implementation.md)
> for the narrower changes adopted on develop; measurements below are historical
> and do not describe a build containing only those changes.

Follow-up: [independent idle-memory candidates and post-traffic recovery](idle-memory-experiments.md).

Base: `7758300039285a74eb6ee1da7fb73be1b090fd1f`, isolated worktree
`superseedr-memory-lifecycle`. The user's catalog and payloads are not inputs to
these experiments. All generated names and content are synthetic.

## Measurement contract

`memory-profile` is an opt-in feature, excluded from default builds. It wraps the
system Rust allocator with atomic counters for requested live bytes, lifetime
peak bytes, cumulative allocation/growth bytes, and allocation calls. Successful
reallocations account for positive size growth, not their entire new size.
Counters exclude allocator overhead, native library allocations, stacks, mapped
files, page cache and kernel socket buffers. Simultaneous samples of the separate
atomic counters are approximate. Instrumentation adds CPU overhead.

Synthetic samples retain process RSS and CPU measurements and additionally expose
`heap`. `memory-lifecycle.jsonl` records setup, sampling, shutdown with harness
handles still retained, and (for `synthetic-load`) return from the harness after a
100 ms settling interval. These labels deliberately distinguish manager shutdown
from dropping the harness. Lifetime peaks include setup; sampled RSS peaks can
miss short transients. Benchmark subruns share process-lifetime heap peaks.

Run comparisons in fresh processes, with matching features and compiler options.
The runner records SHA-256 identities, full commands, return codes, summaries and
raw samples. It alternates variant order between repetitions. Synthetic remote
peers and managers share a process, so total memory includes both endpoints.

## Candidates

1. **Compact block completion flags.** Rust `Vec<bool>` stores one byte per flag.
   `BlockBitfield` uses one bit per flag, retaining the existing block geometry,
   indexing bounds, duplicate handling, completion and invalidation behavior.
   This targets metadata arrival, downloads, validation and long-lived seeding.
2. **Rarity and completion cleanup.** Do not scan peer bitfields or allocate
   rarity maps when all pieces are complete. Release rarity-map and empty need
   queue capacity on the final completion. Rebuild on invalidation. For active
   downloads, collect filtered rarity entries from references instead of cloning
   the entire intermediate map first.
3. **Lazy writer buffer.** Do not reserve 17 KiB per writer before it has traffic.
   Small control messages use small allocations; payload writers grow and retain
   their buffer under the existing batching policy. This does not lower the batch
   limit or add idle-time shrinking.

4. **Bounded outgoing read-ahead.** Reduce the session writer queue from 1,000 to
   64 messages. At 16 KiB per piece message, its payload capacity falls from
   15.625 MiB to 1 MiB per peer. This is a message-count bound, not a global byte
   limit: other queues, upload tasks, writer batches and non-piece messages have
   separate costs. Choke/interest, metadata replies/updates, and v2 hash commands
   use the existing single-message backpressure slot instead of losing messages
   or failing immediately when the queue fills. Existing best-effort keepalives,
   PEX and wire cancels remain best-effort. No unbounded control queue is added.

5. **Retain upload admission through the session mailbox.** A native heap
   inspection found 7,931 allocations of 16 KiB (129,941,504 bytes) in a throttled
   upload run. Reducing only the writer queue did not reduce total heap because
   completed disk reads could accumulate in the upstream session mailbox (up to
   10,000 commands for incoming peers). `UploadPayload` retains the existing
   per-peer read permit with its bytes until the session consumes the command.
   Command clones share the bytes and permit. The final consumer moves the bytes
   without copying; cancelled/closed queues release the permit by dropping the
   payload. Coupled with the bounded writer queue, this prevents queue-to-queue
   displacement. Waiting upload tasks still have overhead; this is not a global
   application memory limit.

## Reproduction

All Rust compilation goes through Kyoto. The standard LTO baseline was stopped
by Kyoto for sustained memory/swap pressure, so experimental release builds use
these matching per-job options (no shared scheduler-policy changes):

```sh
CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 CARGO_BUILD_JOBS=2 \
  python3 /Volumes/seed1/Projects/kyoto/kyoto.py submit -- \
  cargo build --release --features memory-profile
```

Save each executable before building the next variant. The baseline contains
instrumentation only; the structural candidate includes the structural changes above. A separate candidate also reduces the writer queue.

```sh
python3 scripts/measure-memory-lifecycle.py \
  --binary baseline=/absolute/baseline --binary optimized=/absolute/optimized \
  --out /absolute/new-results-directory --repeats 3
```

The runner creates private XDG directories and an explicit migration-complete
marker, removes inherited application environment overrides, and runs from the
private case directory. It does not change `HOME`, read the real catalog, or
clean up existing files. Generated payloads remain under the specified output.

The ignored `torrent_manager::memory_experiments::memory_lifecycle_characterization`
test compares independent block geometry and rarity cases, then verifies the
completion/invalidation lifecycle. Compile through Kyoto with `cargo test --lib
--features memory-profile`; if its policy selects compile-only, execute the
resulting test binary directly with the exact filter, `--ignored --nocapture
--test-threads=1`. Do not treat compilation as test execution.

## Isolated measurements

Executed against the production core through an isolated test process (no payload
files). These are requested heap bytes, not RSS claims:

| Experiment | Before | After | Meaning |
| --- | ---: | ---: | --- |
| Block flags for 1 TiB / 16 KiB blocks | 64 MiB | 8 MiB | 87.5% less block-flag storage |
| Completed torrent, 100,000 pieces: retained rarity | 2,228,232 B | 0 B | No rarity allocations or peer scan for a seed |
| Completed torrent, 100,000 pieces: allocation per rebuild | 7,484,740 B | 0 B | Avoid dense scratch counters and transient maps |
| Download, 100,000 pieces: allocation per rebuild | 11,941,248 B | 9,713,016 B | Remove the intermediate map clone; retained rarity stays 4,456,464 B |
| Finish 4,096-piece download | — | 294,928 B released | Empty need queue and rarity capacities released; invalidation/requeue verified |

The structural candidate's three-repetition TCP comparison used four torrents,
32 peers, 10-second sampling windows, two seconds warmup, and a 0.1 Gbit/s target.
Idle-download end-of-run heap fell from 10.43 to 9.89 MiB; idle-upload from 10.99
to 10.46 MiB. Each is about 0.53 MiB across 32 peers, consistent with removing a
17 KiB initial writer reservation. Active upload heap stayed about 148 MiB;
whole-process download/completion peaks varied. The structural changes are not
claimed to solve payload-buffering peaks.

Reducing disk read/write concurrency from 256 to four permits was also tested
with the baseline (three runs per direction). Median RSS was 103.88 MiB for
download and 155.95 MiB for upload; both sustained about 11.9 MiB/s. This was an
exploratory separate series, not a rigorously interleaved concurrency comparison.
It does not support changing the default disk concurrency.

The queue-only pilot likewise did not reduce memory: structural versus 64-message
writer had 147.96 versus 148.48 MiB peak requested heap, at 11.91 versus 11.92
MiB/s upload. It is a single exploratory pair. The native heap inspection then
found dominant 16 KiB allocations; source review showed how upload payloads could
accumulate upstream. This led to retaining the upload permit across the mailbox
rather than just shrinking the downstream queue.

## Combined release comparison

Three repetitions per variant, four torrents, 32 peers, 10-second measurement
windows after two seconds warmup, TCP, 256 KiB pieces, and 0.1 Gbit/s per active
direction. Values below are medians of per-run measurements; peak heap includes
fixture setup, while peak RSS is sampled during the workload.

| Case | Variant | Runs (failed) | Peak RSS MiB | Peak heap MiB | End heap MiB | Download MiB/s | Upload MiB/s |
|---|---|---:|---:|---:|---:|---:|---:|
| download | baseline | 3 (0) | 112.19 | 94.72 | 91.05 | 11.92 | 0.00 |
| download | optimized | 3 (0) | 107.88 | 88.59 | 87.33 | 11.92 | 0.00 |
| upload | baseline | 3 (0) | 171.14 | 148.41 | 147.78 | 0.00 | 11.90 |
| upload | optimized | 3 (0) | 66.19 | 50.20 | 49.57 | 0.00 | 11.92 |
| swarm | baseline | 3 (0) | 233.44 | 202.57 | 199.15 | 11.90 | 11.88 |
| swarm | optimized | 3 (0) | 126.47 | 103.03 | 99.54 | 11.90 | 11.90 |

All 18 runs exited successfully with no recorded protocol/session errors and no
active synthetic sessions after shutdown. Upload peak RSS fell 61.3%, and mixed
swarm peak RSS fell 45.8%. Both directions sustained the configured rates. These
rate-limited runs establish memory savings at that load; they do not alone prove
unchanged maximum throughput. Pure-download changes are smaller and involve
partial-piece/scheduling variability; no broad download-buffer reduction is
claimed from the structural-only series.

## Throughput tradeoff and lifecycle/transport coverage

With the rate limit removed, three upload runs per variant produced:

| Case | Variant | Runs (failed) | Peak RSS MiB | Peak heap MiB | End heap MiB | Download MiB/s | Upload MiB/s |
|---|---|---:|---:|---:|---:|---:|---:|
| upload | baseline | 3 (0) | 67.62 | 42.96 | 37.46 | 0.00 | 350.37 |
| upload | optimized | 3 (0) | 68.08 | 42.11 | 34.50 | 0.00 | 333.95 |

Median maximum upload throughput decreased 4.7% (350.37 to 333.95 MiB/s).
Baseline range was 350.03–358.24 MiB/s; optimized range was 329.10–350.36 MiB/s.
Peak RSS was effectively unchanged. When consumers drain promptly, the baseline
already avoids the large queue buildup. These results show a memory/throughput
tradeoff, not a universally faster implementation. Matching instrumentation and
compiler settings reduce comparison bias, but this short local series does not
isolate scheduling overhead from the extra payload ownership allocation. Keep
this as an experimental branch pending broader throughput qualification.

Completion, idle seeding, reconnect churn, v2 and hybrid each ran once per variant
with 12-second measurement windows:

| Case | Variant | Runs (failed) | Peak RSS MiB | Peak heap MiB | End heap MiB | Download MiB/s | Upload MiB/s |
|---|---|---:|---:|---:|---:|---:|---:|
| complete | baseline | 1 (0) | 56.02 | 36.70 | 4.56 | 1.26 | 0.00 |
| complete | optimized | 1 (0) | 55.91 | 36.06 | 4.52 | 1.25 | 0.00 |
| idle-upload | baseline | 1 (0) | 25.86 | 11.11 | 10.98 | 0.00 | 0.00 |
| idle-upload | optimized | 1 (0) | 25.19 | 10.57 | 10.44 | 0.00 | 0.00 |
| churn | baseline | 1 (0) | 194.17 | 140.19 | 72.53 | 12.57 | 11.88 |
| churn | optimized | 1 (0) | 113.00 | 74.97 | 52.92 | 12.54 | 11.90 |
| v2 | baseline | 1 (0) | 210.34 | 176.74 | 146.15 | 9.87 | 11.90 |
| v2 | optimized | 1 (0) | 107.77 | 80.76 | 47.99 | 10.15 | 11.91 |
| hybrid | baseline | 1 (0) | 214.34 | 176.28 | 146.41 | 9.96 | 11.90 |
| hybrid | optimized | 1 (0) | 110.64 | 81.57 | 48.27 | 9.98 | 11.90 |

All ten lifecycle/format runs passed, including actual completion in the complete
case and zero active synthetic sessions at shutdown. End heap is measured before
shutdown, so it includes handles and endpoint state still owned by the harness.

Native µTP used two torrents/eight peers and 12-second measurement windows:

| Case | Variant | Runs (failed) | Peak RSS MiB | Peak heap MiB | End heap MiB | Download MiB/s | Upload MiB/s |
|---|---|---:|---:|---:|---:|---:|---:|
| download | baseline | 1 (0) | 80.75 | 55.27 | 55.25 | 11.92 | 0.00 |
| download | optimized | 1 (0) | 80.83 | 55.41 | 54.00 | 11.92 | 0.00 |
| upload | baseline | 1 (0) | 106.53 | 78.76 | 78.37 | 0.00 | 11.92 |
| upload | optimized | 1 (0) | 51.30 | 29.94 | 29.43 | 0.00 | 11.90 |
| swarm | baseline | 1 (0) | 109.34 | 81.16 | 79.76 | 11.90 | 11.91 |
| swarm | optimized | 1 (0) | 82.38 | 58.48 | 57.20 | 11.90 | 11.89 |

Native WebRTC used one torrent/two peers and 15-second measurement windows:

| Case | Variant | Runs (failed) | Peak RSS MiB | Peak heap MiB | End heap MiB | Download MiB/s | Upload MiB/s |
|---|---|---:|---:|---:|---:|---:|---:|
| download | baseline | 1 (0) | 56.77 | 29.96 | 27.72 | 11.91 | 0.00 |
| download | optimized | 1 (0) | 57.02 | 30.02 | 28.79 | 11.91 | 0.00 |
| upload | baseline | 1 (0) | 52.61 | 25.90 | 25.52 | 0.00 | 11.91 |
| upload | optimized | 1 (0) | 41.03 | 14.58 | 13.86 | 0.00 | 11.89 |
| swarm | baseline | 1 (0) | 58.16 | 29.10 | 15.93 | 6.17 | 11.90 |
| swarm | optimized | 1 (0) | 52.36 | 23.85 | 10.19 | 6.65 | 12.53 |

All twelve transport runs passed with no recorded protocol/session errors and
zero active sessions at shutdown. These single-pair transport measurements are
smoke/compatibility evidence rather than statistically stable performance claims.
WebRTC mixed traffic did not saturate the configured download target in either
variant. Local native WebRTC does not qualify browser execution or NAT traversal.

## Longer upload follow-up

One 60-second measured TCP upload run per variant used the same four torrents,
32 peers and 0.1 Gbit/s target:

| Case | Variant | Runs (failed) | Peak RSS MiB | Peak heap MiB | End heap MiB | Download MiB/s | Upload MiB/s |
|---|---|---:|---:|---:|---:|---:|---:|
| upload | baseline | 1 (0) | 199.38 | 175.32 | 165.30 | 0.00 | 11.91 |
| upload | optimized | 1 (0) | 74.62 | 57.87 | 57.60 | 0.00 | 11.91 |

Both runs passed, sustained 11.91 MiB/s, recorded no protocol/session errors and
ended with zero active synthetic sessions. Peak RSS fell 62.6%. This confirms the
reduction survives a longer local run; one minute is not a long-duration leak or
steady-state proof. Across the final six comparison series, all 48 runs passed.
Earlier exploratory series are recorded separately rather than included in that
count.

## Research and remaining candidates

Libtorrent recommends measuring the performance cost of reducing buffers and
peer lists. Its [tuning guide](https://www.libtorrent.org/tuning-ref.html) explains
that socket buffers live in kernel memory and that upload read-ahead trades
memory for disk-latency tolerance. It also identifies per-torrent metadata and
piece/block tracking as costs retained by paused torrents. These observations
motivate compact representation and eliminating unused allocations before
lowering throughput-related limits.

Libtorrent 2.0's [upgrade guide](https://www.libtorrent.org/upgrade_to_2.0-ref.html)
explains its move toward memory-mapped I/O and operating-system cache management.
That is not evidence that switching this application's I/O architecture would
reduce its physical-memory use; RSS, mapped pages and kernel caching require
separate accounting.

Further candidates, not implied to be implemented or qualified here:

- Pack per-peer piece bitfields as well as local block flags. Current peer fields
  also feed UI/API snapshots; preserve those public representations explicitly.
- Add an application-wide byte budget across torrents, waiting disk tasks and
  peer channels. The implemented per-peer admission and writer bounds do not
  impose a global limit or bound the number of tasks waiting for admission.
- Serialize directly into writer batches to avoid temporary encoded frames.
- Reclaim large writer capacities after prolonged idle, with hysteresis to avoid
  allocator churn on intermittent uploads.
- Audit metadata clones, telemetry snapshots, peer history retention and paused
  managers with large synthetic catalogs. Distinguish retained ownership from
  RSS that the allocator has not returned to the operating system.

## Validation and limits

The low-memory unit build used `cargo test --lib --no-default-features --features
memory-profile,dht,pex`, with `CARGO_PROFILE_DEV_DEBUG=0`,
`CARGO_PROFILE_TEST_DEBUG=0`, and `CARGO_BUILD_JOBS=1`, submitted through Kyoto.
Kyoto selected compile-only; the produced executable was then run directly:

- 316 torrent-manager tests passed (nine ignored by the broad filter).
- 180 networking tests passed.
- 25 synthetic-harness tests passed.
- The isolated memory characterization passed separately.
- The packed-bit property test passed again with 1,000 cases and seed 20261015.

The broad suite includes the new large-mailbox read-ahead regression, shared
payload/permit ownership, completed-torrent memory release and requeue, full-duplex
operation with a saturated upload writer, and retained reliable control messages.
Eight existing long-running stall/window reproduction tests remain ignored.

Full-feature unit compilation was attempted in release and unoptimized profiles;
Kyoto stopped these attempts for sustained host pressure. They are not reported
as executed tests. Runtime experiment executables include the normal default
features, including WebRTC; unit coverage above excludes WebRTC.

`cargo fmt --all -- --check` and `git diff --check` passed. Strict all-target,
all-feature Clippy found existing style warnings under Rust 1.99 plus an
instrumentation item-order warning, which was fixed. Clippy then passed with
`-D warnings` and allowances only for the pre-existing
`unneeded_wildcard_pattern`, `useless_borrows_in_formatting`,
`chunks_exact_to_as_chunks`, and `byte_char_slices` categories. No repository-wide
lint policy or unrelated style code was changed.

Results are local macOS/Apple Silicon synthetic evidence. They do not qualify a
real catalog, browser execution, WAN latency, NAT traversal, long-running swarms,
other operating systems, kernel socket-memory totals, or the normal LTO release
profile. Both comparison binaries use the same allocator instrumentation and
compiler settings. The user's catalog and existing checkout were not modified.

## Local evidence receipts

Artifacts are retained under `tmp/memory-lifecycle/` in this worktree (ignored by
Git). `artifacts/binary-identities.json` records all saved executables and hashes;
`artifacts/optimized.patch` plus `artifacts/candidate-source.tar.gz` preserve the
candidate source. Each comparison directory contains `results.json`, commands,
raw heap/RSS samples and lifecycle checkpoints. The summarizer can regenerate the
tables without running the application:

```sh
python3 scripts/summarize-memory-lifecycle.py tmp/memory-lifecycle/combined-comparison/results.json
python3 scripts/summarize-memory-lifecycle.py tmp/memory-lifecycle/unlimited-upload/results.json --json
```

| Evidence | Receipt |
| --- | --- |
| Baseline executable SHA-256 | `97d15804a9298c4242c7ec23e1c5f035ea647fe9cee90c4235b97135a46fd90e` |
| Optimized executable SHA-256 | `ece14c995422cabf4612f29c7f132cb47201f038b5c78ab43d8428947779948d` |
| Final Kyoto release build | `30ec4bb0f0144c1a94165f4b5a7ccda6`, succeeded |
| Kyoto test compilation | `1712f9fb7faa4e9da8745715fc00afc7`, succeeded; execution recorded separately |
| Kyoto Clippy | `06511e820a184196ae2f7dc2ab4495e2`, succeeded with the documented allowances |
| Executed tests | `tests/results.json` and adjacent logs |
| Independent structural runs | `structural-comparison/results.json` |
| Negative experiments | `low-disk-concurrency/results.json`, `queue-pilot/results.json` |
| Native heap inspection | `heap-inspection/heap.txt` |
| Final repeated TCP runs | `combined-comparison/results.json`, `unlimited-upload/results.json` |
| Lifecycle and formats | `lifecycle-formats/results.json` |
| Native transports | `utp/results.json`, `webrtc/results.json` |
| Longer upload follow-up | `long-upload/results.json` |

The branch remains `experiment/memory-lifecycle`; no commit, merge or deployment
was performed. The original checkout remains at the base commit with its existing
untracked diagnostics preserved.

## Idle and churn attribution follow-up

Five additional runs used the saved optimized binary, one synthetic torrent,
TCP, and isolated configuration. No Rust code changed and no rebuild was needed.
These are single observations, not repeated benchmark medians. Idle peer-count
runs measured 15 seconds; idle churn measured 60 seconds with two-second sessions
and 250 ms reconnect delays. The slow-upload run measured 20 seconds.

| Case | Peak sampled RSS MiB | Live heap before shutdown MiB | Live heap after harness return MiB |
| --- | ---: | ---: | ---: |
| 1 idle peer | 21.30 | 4.42 | 4.05 |
| 32 idle peers | 24.78 | 10.24 | 4.08 |
| 128 idle peers | 34.91 | 28.16 | 4.10 |
| 32 idle peers with churn | 27.95 | 10.44 | 4.11 |
| 32 peers, slow upload | 35.36 | 25.80 | 4.13 |

All five runs passed. Churn established and ended 864 sessions, with peak active
32, zero idle payload bytes and zero unexpected failures. Median live heap in the
first/last 50 samples was 10.35/10.53 MiB. This does not show growth proportional
to all past connections, but is not an hours-long leak test. Heap/RSS inspection
was performed during this churn run, so timing is diagnostic, not a clean
performance benchmark. Slow upload requested 0.001 Gbit/s but actually averaged
0.50 MiB/s; this run must not be described as an achieved 1 Mbit/s rate limit.

Concrete contributors identified from source and native inspection:

- `src/networking/protocol.rs:reader_task` reserves a 17 KiB socket buffer plus
  64 KiB processing buffer per connected production peer, even without payload.
  For 128 peers that is 10.125 MiB of requested capacity. Reserved heap capacity
  and resident/touched pages are different measures.
- The idle synthetic counterpart allocates its own 64 KiB receive buffer per
  peer in `src/native/synthetic_load/workload.rs:idle`. This harness-only overhead
  is 8 MiB at 128 peers and must not be attributed to the production client.
- The log writer's `DEFAULT_BUFFERED_LINES` is 128,000. Native `heap --noContent`
  found one 4,096,000-byte allocation consistent with the fixed channel slot
  storage. This attribution is source/size correlation, not an allocation-stack
  trace. About 4.04 MiB live Rust heap already exists before fixture creation.
- Readers retain processing capacity; writers clear their batching buffer while
  retaining capacity. Slow connections can also keep queued payloads live even
  though their transfer rate is low. The implemented limits bound this cost but
  do not adapt it to each peer's speed.
- `ManagerTelemetry` retains a cloned `TorrentMetrics` snapshot; idle comparison
  clones current/previous snapshots. These include peer bitfields and strings.
  Torrent metadata, piece/hash state, channels and async tasks also stay live.
  Their individual byte totals were not stack-attributed in this pass.
- Reconnect history intentionally retains inactive baselines/departed transfers
  for a ten-second window with a 4,096-entry cap. Removing entries does not
  necessarily shrink their hash-table capacity.
- The native VM snapshot showed roughly 944 KiB resident stacks, 352 KiB malloc
  metadata, and 1,072 KiB in empty malloc-small regions. Its physical-footprint
  metric was 13.9 MiB at that instant. VM-region totals include shared mappings;
  do not add them to RSS or equate them with private application heap.

After the 128-peer run, requested heap dropped from 28.16 to 4.10 MiB, but sampled
RSS was still 35.83 MiB 100 ms after return. This is evidence of released logical
allocations without an immediate matching RSS reduction; it does not identify
all resident pages or prove how long they remain. The same distinction applies
to an external monitor's memory reading, whose metric must first be identified.

The appropriate target is a small fixed application footprint plus compact live
catalog/peer state, with payload buffers released after use. Total RAM near zero
is not a sensible target for a running networked application. Next experiments
should isolate a smaller log queue, lazy/smaller reader processing buffers,
post-traffic idle buffer reclamation, and throughput-aware payload budgets.
None of those additional production changes were made in this follow-up.

Raw receipts: `tmp/memory-lifecycle/idle-peers-{1,32,128}/results.json`,
`idle-churn-32/results.json`, `slow-upload-32/results.json`, and
`idle-native-inspection/{heap,vmmap}.txt`. The runner now supports `idle-churn`.
