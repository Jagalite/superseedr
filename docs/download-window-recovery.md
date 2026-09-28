# Download window recovery characterization

Latest: [the deeper scheduling follow-up](scheduling-follow-up.md) reproduces
low throughput without write failures and demonstrates recovery when rarity
counting and candidate-key computation are made cheaper, without changing the
growth rule or UI.

Follow-up: [the sorting reproduction](sorting-reproduction.md) now isolates
unnecessary new-piece selection after pending-piece refill consumes all request
slots. The one/two/three-torrent synthetic comparison retains the original growth
rule. The user clarified that the healthy smaller torrents were different from
the stuck large batch, so the live observation does not establish a hard
three-torrent threshold.

Latest follow-up: the candidate guard is now applied locally with 150 passing
state tests. Replaying the same three live torrents improved the large target's
three-minute post-completion received average from 11.0 to 40.9 Mbps and successful
backend writes from 8.3 to 18.3 Mbps, but encountered payload admission failures
and remained variable. It is not a complete resolution. See the sorting report
above for the comparison and limitations. Growth and UI changes remain deferred.

Earlier live finding: the instrumented original controller reproduces the low
post-contention rate and exposes substantial local processing delays. Global
permit waits are short; stack samples identify piece-candidate sorting and UI
bitfield scanning as hot paths. The synthetic window-controller improvement
below is not yet a demonstrated fix for the live slowdown. See the final section.

This local debug-build experiment uses three native torrent managers, real TCP
sessions, shared connection/disk resources and an unlimited shared download token
bucket. It exercises piece scheduling, hash verification, and temporary payload
storage. It does not run the application UI, global adaptive disk/peer tuning,
discovery, or a live swarm.

The generated fictional payloads are 8 MiB, 8 MiB, and 64 MiB, with 16 KiB
pieces. Each torrent has one synthetic seeder. Every response is independently
delayed by 250 ms so outstanding requests can overlap. The peer services at
most one block every 4 ms (a nominal ceiling of 3.90625 MiB/s, with real timer
and socket overhead). Both smaller torrents must finish before the recovery
observation starts; their payload bytes are independently read back and checked.
The observation continues for 35 seconds after their completion.

Two cases use the same fixture:

- Equal capacity: all three peers have the same service policy throughout.
- Initial contention: the third peer serves at most one block every 250 ms
  until both smaller torrents finish, then switches to the same 4 ms service
  policy. This deliberately imposes the initial unequal allocation. It tests
  recovery from contention, not the cause of initial starvation in the report.

Each second records wire payload bytes, cumulative requests, requests waiting at
the seeder, connected peers, and manager-reported verified pieces. Seeder queue
counts are observations, not direct reads of the session's configured window.
The managers validate received piece hashes; the two completed smaller payloads
also receive full readback checks. Partial large-torrent payloads are not claimed
as completed downloads.

## Reproduction

Run each revision separately, with the same test fixture and build profile:

```sh
cargo test --lib synthetic_window_recovery -- --ignored --nocapture --test-threads=1
cargo test --lib networking::session::tests
```

The real-time scenarios are ignored by default. They characterize throughput and
assert healthy completion/progress, without imposing a host-dependent speed
threshold. Separate ordinary session tests assert that a steady, saturated peer
dispatches another queued request, recovery continues through gains below 10%,
and idle/minimum/maximum bounds remain intact.

## Mechanism

Previously, the request window could grow by one only if the current second's
block count exceeded the previous count by more than 10%. A full small window
can itself prevent that gain. Even under an ideal window-limited model, the
percentage benefit of adding one request falls below that threshold as the
window grows. The focused pre-fix tests reproduced both a queued request left
waiting at steady speed and growth stopping at 11 outstanding requests.

The change permits one additional request per adjustment tick when the window
is saturated and blocks are arriving without a greater-than-10% slowdown. The
existing minimum, 512-request ceiling, slowdown backoff, and stall timeout remain.

## Evidence

Raw logs, source/fixture SHA-256 manifests, and the analysis script are retained
locally under `tmp/window-recovery/`. The baseline uses production revision
`f51cd983` with only test additions. The first harness run incorrectly rejected
normal seed disconnections after completion; its failed output is retained as
`baseline-harness-initial.log` and excluded from the comparison. The final
baseline and patched runs use the same corrected fixture.

One baseline and one patched run of each case produced the following results.
Rates are the third torrent's mean wire payload rate during seconds 21 through
30 after both smaller torrents completed; none of these rate windows includes
the third torrent's completion. These are bounded local observations, not a
general throughput benchmark.

| Case | Baseline rate | Patched rate | Baseline verified at end | Patched verified at end |
| --- | ---: | ---: | ---: | ---: |
| Initial contention | 0.741 MiB/s | 2.113 MiB/s | 1,698 / 4,096 pieces | 4,096 / 4,096 pieces |
| Equal capacity | 0.741 MiB/s | 2.072 MiB/s | 2,186 / 4,096 pieces | 4,096 / 4,096 pieces |

The baseline seeder queue remained at 11–12 requests during that measurement
window in both cases. With the patch, it grew through 36–45 requests in the
contention case and 32–42 in the control. The patched third torrent completed
35 seconds after the smaller torrents in the contention case and 34 seconds
after them in the control. Both baseline third torrents remained incomplete.
All four scenarios passed their manager/progress and smaller-payload readback
checks. All three focused session regression tests passed with the patch; two
failed against the original controller. The broader peer-session suite passed
all 41 tests, and formatting and diff-whitespace checks passed.

The equal-capacity baseline did not reproduce initial starvation of one torrent:
all three initially progressed at similar rates. It did reproduce the same
persistent request-window plateau. Thus the recovery defect is established,
while the reported initial unequal allocation remains unisolated.

These controlled cases can establish a request-window recovery defect. They do
not establish why a particular live torrent initially gets little bandwidth,
or predict the speed its actual peers can sustain.

## Confirmed live run: 2026-09-27, 18:09–18:15 Eastern

The user confirmed this is the reported run. An anonymized extraction, source
paths and SHA-256 hashes, and its reproduction script are under
`tmp/window-recovery/incident/`. No media titles are included in that extraction.
The run managed 119 torrents, so the three-manager synthetic is not a complete
model of its background activity. Three other downloading torrents reached
completion at 18:11:15, 18:11:44, and 18:11:58. The remaining torrent's roughly
15-second counter deltas show 8–14 Mbps across much of the next two minutes,
then lower rates near shutdown. These are transfer-accounting deltas, not wire
captures or an assumption that each diagnostic interval is exactly 15 seconds.

| Suspect | Evidence | Qualification |
| --- | --- | --- |
| Global connection permits | Wake-lag protection reduced the limit from 1,368 to 342 at 18:13:54, with 315 connected peers; it restored 1,368 by 18:14:04. An earlier reduction cleared by 18:09:20. | No evidence of a persistently stuck connection limit. These permits admit connections, not blocks on established connections. |
| Storage admission / disk permits | 23 app-log write attempts reported `payload admission full`, ending at 18:10:19. No `Permit Starvation` warning appeared. | Payload admission is a separate per-payload operation/byte budget. Its rejection is not proof of leaked global disk permits. Disk-permit queue residence/utilization was not recorded. |
| Runtime scheduling | The connection throttle observed a roughly 1.2-second UI wake delay at 18:13:54, followed by recovery. | UI wake delay cannot distinguish Tokio scheduling from synchronous application work or OS/storage pressure. No manager-turn or task-poll timings were recorded. |
| Per-peer request windows | The remaining torrent logged sampled local request-permit waits of 1.266–14.558 seconds, across TCP and uTP. | These are the session's request permits, not global connection permits. Waiting alone does not distinguish a small window from slow remote responses, transport pressure, or a global read throttle delaying permit returns. Actual window sizes were not logged. |
| Remote availability/choking | At 18:14:58, 98 of 125 connected peers were choking the client. The last saved status lists 37 TCP and 82 uTP peers; only 12 TCP and 15 uTP peers had ever moved payload in either direction. | Connections, currently unchoked peers, and peers actually supplying useful data are different counts. The synthetic only covered TCP. |
| Global adaptive download cap | The saved configured download/upload limits are unlimited, but `App::update_disk_backpressure_download_throttle` can still apply a finite shared byte cap. | The effective cap, score, and disk latency input were not recorded, so this alternative cannot be excluded. |

The disk throttle compares completed-write scores to a retained best score and
does not reset merely because some downloading torrents finish. It does reset
when there is no download activity, no disk signal, or the configured limit
changes. A workload transition can therefore leave an old score/cap in place;
source inspection does not show that the cap was 8–10 Mbps in this run.

The app also emitted 5,683 warnings about unsuccessful disconnect-command
delivery. That branch handles both full and closed channels, so this count must
not be presented as 5,683 observations of a saturated queue. The remaining
torrent's two `piece_write_failed` diagnostic events occurred at 18:15:30 and
18:15:38; the latter coincided with shutdown, rather than establishing a sustained
storage failure throughout the slowdown.

Validation after this audit: 42 existing resource-manager, token-bucket,
disk-throttle, and slow-disk tests passed with:

```sh
cargo test --lib -- resource::native::tests token_bucket::tests disk_backpressure
```

This supports the covered accounting and throttle behavior, not an exclusion of
live contention. The request-window defect remains independently reproduced,
but attribution of this live incident solely to the 10% condition is unproven.
A decisive subsequent capture needs the effective global byte cap, disk permit
wait/utilization, per-peer actual window and response timing, and manager queue
or execution delay on the same timeline.

## Upstream pause/reconnect source comparison

Read-only inspection pinned qBittorrent `master` at
`d6d86ff1eca92802a1f34a7bfd20dbd02c1040ee` and libtorrent `RC_2_0` at
`2bc9c4f7dacb70e89f7ac73e9fa7fc02ed2b395a`. Downloaded source and its manifest
are under `tmp/window-recovery/upstream-source/`. This is source verification,
not an execution test against those clients or identification of the remote
clients in the live run.

- qBittorrent's `TorrentImpl::stop()` disables auto-management and calls the
  native handle's `pause()`. `start()` restores auto-management, or explicitly
  calls `resume()` in forced mode.
- libtorrent's ordinary `torrent::do_pause()` stops torrent disk activity and
  disconnects all peer connections. `do_resume()` restores discovery/announces
  and calls `do_connect_boost()`.
- A fresh libtorrent connection starts choked, not snubbed, with slow-start
  enabled. Its desired request queue initially holds four requests; during
  slow-start it grows on received blocks. Outside slow-start it is calculated
  from download rate and configured queue time, with bounds. This is different
  from Superseedr's historical requirement for a 10% rate improvement before
  adding one request per second.
- The remote libtorrent choking algorithms rank recent transfers and, for
  seed upload policies, upload performance/rotation. A reconnect does not
  necessarily erase the peer record: previous totals are restored, the banned
  flag is checked, and optimistic-unchoke selection uses the peer record's
  retained last-optimistically-unchoked timestamp.

Sources: [qBittorrent stop/start](https://github.com/qbittorrent/qBittorrent/blob/d6d86ff1eca92802a1f34a7bfd20dbd02c1040ee/src/base/bittorrent/torrentimpl.cpp#L2014-L2065),
[libtorrent pause](https://github.com/arvidn/libtorrent/blob/2bc9c4f7dacb70e89f7ac73e9fa7fc02ed2b395a/src/torrent.cpp#L9806-L9886),
[resume](https://github.com/arvidn/libtorrent/blob/2bc9c4f7dacb70e89f7ac73e9fa7fc02ed2b395a/src/torrent.cpp#L10034-L10111),
[request sizing](https://github.com/arvidn/libtorrent/blob/2bc9c4f7dacb70e89f7ac73e9fa7fc02ed2b395a/src/peer_connection.cpp#L4779-L4821),
[choker](https://github.com/arvidn/libtorrent/blob/2bc9c4f7dacb70e89f7ac73e9fa7fc02ed2b395a/src/choker.cpp#L51-L140),
[retained peer state](https://github.com/arvidn/libtorrent/blob/2bc9c4f7dacb70e89f7ac73e9fa7fc02ed2b395a/src/peer_list.cpp#L1286-L1358),
[optimistic unchoking](https://github.com/arvidn/libtorrent/blob/2bc9c4f7dacb70e89f7ac73e9fa7fc02ed2b395a/src/session_impl.cpp#L4139-L4217).

Thus pause/resume can plausibly restore throughput by rebuilding connections
after contention has ended. Recovery would not identify which side's state was
responsible: Superseedr also discards its sessions on pause, resetting the local
request controller on subsequent connections. Restarting our client does not
restart remote peers or guarantee that they discard their records about us.

## Live replay of the original three ingests

On September 27, 2026, the original controller was rebuilt in the dev profile
from an isolated `git archive` of
`f51cd98357517e0c21ed1efe22275b2d21860826`, using `cargo build --locked`.
The binary SHA-256 is
`b7f647b3a9dc07da5a32fa7ec233a79ff4c4ef159ff24d19398b71ae0c65a93a`.
The proposed window change was not present in this binary.

The confirmed incident's first three new ingests were replayed in this order:

| Alias | Info hash prefix | Original ingest, Eastern | Payload bytes |
| --- | --- | --- | ---: |
| A | `8a47a4da` | 18:08:56.255 | 1,443,393,922 |
| B | `312b7863` | 18:08:56.509 | 1,462,491,876 |
| C, remaining batch | `94c80e71` | 18:08:57.455 | 34,686,147,579 |

The user requested three torrents. A fourth new ingest in the original log was
excluded, as were the other restored torrents. Copies of the original torrent
metadata were used with empty payload directories; existing payloads and normal
client configuration were not reused. CLI submission delays approximated the
original gaps. The watcher grouped their processing, so actual manager starts
were closer together, but the logs confirm A, then B, then C.

### Comparable resource budget

An initial replay inherited the tool shell's 1,048,575-file soft limit. This
produced a much larger peer/disk permit budget than the original incident.
It showed slow recovery but also a late status gap and manager shutdown timeout;
it is preserved separately and is not treated as a matched resource comparison.

The final repeat set only the child client's `RLIMIT_NOFILE` soft limit to
2,560. This is inferred from the original initial peer budget of 1,485 and the
source formula `floor((file_limit - 64) * 0.85 * 0.70)`, rather than an original
OS-limit measurement. Adaptive tuning remains enabled. The repeat used its own
shared root, host identity, watch folder, random listen port, and payloads on the
same external volume. Configured upload/download limits were unlimited; RSS was
disabled. TCP and uTP remained enabled. Debug diagnostics and two-second status
sampling were enabled, with TUI output captured to observe the effective rate cap.

The final repeat ran approximately 20:06:35–20:12:31 Eastern. B completed at
126.8 seconds and A at 175.1 seconds after harness start. Neither was paused.
C continued without pause or reconnection intervention for another three minutes.

| Time after A and B completed | C transfer rate, Mbps | C committed-write rate, Mbps | C connected peers |
| --- | ---: | ---: | ---: |
| First minute | 16.38 | 13.19 | 75–108 |
| Second minute | 13.96 | 11.31 | 92–104 |
| Third minute | 14.11 | 9.77 | 91–105 |

Rates use cumulative-counter deltas over the available approximately 58-second
sample spans inside each minute. Transfer accounting may include duplicate blocks;
committed writes may include data received in an earlier interval. Initial
aggregate reported download speed peaked at 522 Mbps. This reproduces poor
recovery after the two competing downloads finish, although it does not reproduce
an exact fixed 8–10 Mbps ceiling or establish the batch swarm's attainable rate.

During the post-completion phase, the captured effective global download cap was
either unlimited, 1.00 Gbps, or 1.11 Gbps. The final displayed peer budget was
111 connected peers out of 1,242 slots. The final downloading diagnostic snapshot
had 99 batch peers, of which 69 were choking us and 30 were unchoked. An unchoked
peer is not necessarily delivering useful data. These observations argue against
a low effective rate setting or the displayed peer-count ceiling; they do not
measure semaphore wait time or eliminate a permit-accounting defect.

There were 571 `payload admission full` disk-write warnings during the initial
download burst, all between approximately 11 and 23 seconds after harness start.
None occurred after A and B completed. There were no `Permit Starvation` warnings.
The final repeat shut down cleanly about one second after the stop request, with
no manager shutdown-timeout warning. The earlier oversized-budget replay's
shutdown failure must not be attributed to this repeat.

No live patched-controller comparison, per-peer wire-window/RTT series, disk
permit-wait series, or Tokio scheduling measurement was collected. The live
result therefore establishes the symptom, not which of remote choking, request
window adaptation, or local request/storage scheduling caused it.

### Local evidence and retained state

Final evidence is under `tmp/window-recovery/replay-budget2560/`: `manifest.json`,
`sources.json`, `events.jsonl`, `status.jsonl`, `report-metrics.json`, `audit.json`,
`terminal-metrics.jsonl`, preserved app/per-torrent logs, and `artifact-sha256.json`.
The source mapping and raw status/TUI output are local experiment artifacts;
media titles are not copied into test fixtures or this report.

The stopped final client initially retained its payloads and shared config at
`/Volumes/seed1/superseedr-live-replay-budget2560-20260927`.
The two smaller downloads completed; the last regular status snapshot recorded
367,001,600 committed bytes for C. The incomplete batch was not allowed to finish
its 34.7 GB download after the observation period.

The earlier original-torrent replay is preserved under
`tmp/window-recovery/replay/` and its separate external shared root. An earlier
official Linux-torrent experiment is preserved under `tmp/window-recovery/live/`;
it was stopped when the user requested the original torrents. Its competitors
were paused rather than completed, so it is not evidence for the exact natural
completion scenario. All experiment clients and phase controllers were stopped.

## Instrumented original-controller replay, September 27, 20:27–20:34 Eastern

With the user's authorization, only the `downloads/` contents of the three prior
experiment roots were cleared. Metadata, configuration, and evidence were
preserved. The user's normal downloads and configuration were not changed.
`tmp/window-recovery/instrumented/cleared-payloads.json` records the exact roots.

An isolated source copy of `f51cd98357517e0c21ed1efe22275b2d21860826`
retained the original 10% growth condition and added native diagnostic probes.
It used fresh payloads, the same three metadata files, unlimited configured
rates, the same 2560 file-limit setting as the preceding controlled replay,
and a dedicated watch directory. Manager creation order was A, B, C at
00:28:01.625, .693, and .768 UTC respectively. Submission delays followed the
original ordering, but command ingestion compressed their spacing; this is not
an exact replay of original subsecond timing or background torrent population.

Both smaller torrents completed by 170.79 seconds. The client then continued
without pause/resume for another 180 seconds. Per-session counters and successful
backend write completions gave:

| Minute after both completions | Session received, approximate Mbps | Backend write completions, Mbps |
| --- | ---: | ---: |
| 1 | 11.94 | 8.39 |
| 2 | 10.83 | 8.67 |
| 3 | 10.25 | 7.83 |

Session counters flush approximately every five seconds, so minute boundaries
are approximate and omit unflushed counters when a peer session ends. They are
not manager useful-byte accounting. Write completion rates can lag receipt.
The regular application status feed had a 139.73-second gap, from elapsed
216.90 to 356.63 seconds; the independent probes continued during that gap.
Consequently, the status feed cannot provide three reliable minute summaries.
Before the gap, C reported approximately 9 Mbps with 77–87 connected peers.

### Measured pipeline delays

The following summaries cover probe records from 180.79 seconds (ten seconds
after completion) up to the stop request at 351.20 seconds. Histograms aggregate
completed operations, not pending waits censored at shutdown.

- Parsed block to peer-session handling: mean **2,256.85 ms** across 14,591
  samples, maximum 10,660.28 ms. About 68% of samples exceeded one second.
- Session forwarding to the torrent manager: mean **206.39 ms**. This measures
  the channel-reservation path, including any manager-command servicing while
  waiting; it is not an isolated Tokio scheduler-latency measurement.
- Request transport-write completion to response parsing: mean **434.51 ms**.
  This includes remote, transport, and application read delays; it is not RTT.
- Request writer queue: mean **0.034 ms**; transport write: **0.009 ms**.
- Global download-token waits averaged less than **0.001 ms**.
- Global connection-permit acquisition: mean **0.641 ms**; disk read and write
  permits each averaged about **0.097 ms**. No resource wait queues appeared
  in the five-second snapshots. The connection pool had a median 61 in use
  against a median limit of 1,312.
- C's backend writes had a median **83.04 ms**, p95 **1,436.77 ms**, and maximum
  **3,163.18 ms**, with no recorded failed writes in this period. These include
  backend scheduling/completion delay, not just physical device service time.

Among 482 active, unchoked peer snapshots, the actual request window ranged from
8 to 21, median 13; queued requests had a median 473.5, and available window
permits had median zero. The original growth condition rejected growth on 941
ticks while receiving blocks with a saturated window. That shows constrained
windows, but local block-processing delays also prevent timely permit recycling.
It does not establish that changing growth alone fixes the live problem.

### Stack sample and implications

A five-second macOS process sample at 20:32:38 Eastern captured two distinct
CPU hot paths while C was downloading:

- The main/UI thread was in `draw_peers_table_impl` →
  `swarm_heatmap_flashing_peer_addresses` → `swarm_heatmap_flash_peer` →
  `peer_has_all_pieces` throughout its 2,801 sampled stacks. The flash selector
  rescans each peer's bitfield for each flashing piece. C has 33,080 pieces,
  making this repeated work expensive in a development build. This is direct
  evidence of a UI hot path and consistent with the observed status gap;
  its contribution to download throughput has not been isolated.
- A torrent-manager worker spent 1,097 of its 2,801 sampled stacks inside the
  candidate `sort_by_key` reached from `TorrentState::update`. `AssignWork`
  collects and sorts eligible pieces, repeatedly looking up rarity in a hash
  map. Incoming blocks can trigger `AssignWork` when the pipeline is below its
  low-water mark. The sample and forwarding delays implicate local manager
  processing; a controlled change is still needed to measure its causal impact.

The evidence argues against sustained global permit starvation as the main
explanation in this replay. It establishes local queueing and CPU hot paths,
without proving a Tokio scheduler defect or excluding remote choking. Neither
hot path has been changed in the tracked source during this instrumentation
pass, and no live patched-controller comparison was run.

### Validation, shutdown, and evidence

The diagnostic source passed 53 focused session, probe, and resource-manager
tests, formatting, and a development binary build. Instrumentation aggregates
per-session timing histograms and limits tracked request timestamps to 1,024
per session; it adds overhead, so absolute rates remain diagnostic observations.

The stop request was queued at 351.20 seconds and observed by the application
about 5.24 seconds later. Shutdown then logged that all three managers failed
to reply within its timeout, and the process exited with status zero at 376.77
seconds. This was not a clean manager shutdown. No experiment client remains
running. The new isolated root is
`/Volumes/seed1/superseedr-instrumented-baseline-20260927`.

Evidence is in `tmp/window-recovery/instrumented/baseline/`: source/binary
manifest, ingest sources, event and status series, `probe-records.jsonl`,
`probe-summary.json`, `instrumented-results.json`, `process-sample.txt`, preserved
logs, and `artifact-sha256.json`. The instrumentation source and patch, build
log, validation log, and analysis script are under the parent directory.
Raw metadata and logs stay in ignored local experiment artifacts; this report
and synthetic fixtures use aliases or fictional payloads.
