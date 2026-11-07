# Shared-catalog connection lifecycle audit

Measured on 2026-11-07 UTC (2026-11-06 local). Current main `f11d6ddaa0913d95f1e0225edc89eeb79af19d79` plus local lifecycle instrumentation; release executable SHA-256 `96a4886b67e2ebd678ebc01fc67941c55c95d574b0edb86b7e472f65b0a6a008`. Kyoto job `26728b34594a44a682c2817b8bec005d` successfully executed `cargo build --release --locked` with default features.

This is an instrumented startup investigation, not a benchmark, allocation audit, or long-running leak qualification. The real shared catalog contains 129 entries; the final five-minute status sample contained 120 entries during adaptive startup. The process subsequently exited normally after a graceful stop. No claim is made about settled performance with all 129 entries loaded.

## Confirmed overlapping outbound work

The lifecycle trace recorded:

- 118,794 outbound task spans; 114,498 reached the dial stage.
- 54,877 starts overlapped an earlier pending attempt for the same torrent and endpoint, across 20,971 such keys. A pending attempt ends at registration or task completion, so established session lifetime is excluded from this overlap count.
- 54,857 consecutive repeats for a torrent/endpoint occurred less than one second apart.
- 30,824 transports connected; 21,325 registered and 9,499 were rejected at registration. Rejection includes current admission decisions, so not every rejection is attributed to duplication.
- 83,674 dial failures. The 4,296 outbound tasks that did not reach dialing may have failed admission, timed out waiting for a permit, or been canceled; this trace does not distinguish those outcomes.
- Up to 3,134 live outbound task spans. This includes admitted sessions as well as pending work; it is not a pending-dial gauge.

`TorrentManager::connect_to_peer_with_key` checks registered peers and recorded backoff, then creates a generated bitfield and spawns a permit/dial task. It does not reserve a pending endpoint. Multiple discoveries can pass those checks before registration or failure accounting. The trace confirms that this path creates overlapping work on the real catalog. TCP/uTP racing occurs inside one logical attempt and does not account for this overlap count. Different torrents using the same endpoint are counted separately.

Recommended first change: reserve a normalized `(torrent, network generation, endpoint)` before generating a bitfield or spawning work; release it on every completion, rejection, cancellation, and permit failure. Re-check admission/backoff after waiting for a permit. Verify duplicate discoveries, canceled generations, queued permit timeouts, and successful redial after release. Repeat the real-catalog run to measure effect rather than treating all overlap as quantified RAM savings.

## Cleanup and retention evidence

All 118,794 outbound, 21,501 peer-session, and 403 WebRTC spans ended before process exit. All 403 WebRTC objects recorded successful close completion; none recorded close error. Maximum concurrent WebRTC spans was 69. This demonstrates eventual instrumented lifecycle completion in this bounded run, including shutdown; it does not prove prompt cleanup during steady operation or complete destruction of library-internal allocations.

61 socket snapshots found one CLOSE_WAIT socket in one sample, absent in later samples. There is no evidence here of accumulating CLOSE_WAIT sockets. Socket snapshots cannot observe every short-lived descriptor or the individual uTP sessions multiplexed on UDP sockets.

21,322 manager peer-removal events were observed. Key-based matching correlated 21,322 of 21,501 ended session spans with removals. Some removals occurred before session end; unmatched sessions can include sessions that never registered. Matching uses torrent and peer key rather than a shared registration incarnation, so the observed approximately 32.9-second maximum positive matching delay remains a lead, not proof of a stuck disconnect. Extend instrumentation with shared registration/session IDs and explicit enqueue/consume timestamps before diagnosing it.

## Resource observations

Sampled peak RSS was 270.8 MiB, peak CPU 184% (multi-core process CPU), peak network descriptor count 3,114, and peak SYN_SENT count 2,180. Final sampled RSS was 270.5 MiB. These include startup, tracing, terminal rendering, and normal networking; they do not isolate avoidable memory.

A post-sampling vmmap snapshot reported physical footprint 156.6M, peak physical footprint 183.8M, and malloc allocated bytes 105.2M. These are different metrics sampled at different times; do not add them or equate RSS with live heap. No allocation stacks were collected, and the RAM cost of duplicate task captures/bitfields is not yet quantified.

WebRTC uses per-negotiation peer connections and ICE setup. Although all instrumented closes completed, speculative negotiation and per-interface UDP allocation remain performance candidates. The earlier passive run's ICE warning counts are not reused as results from this executable.

## Address-family configuration finding

The saved host config says `mode = "any"`, `enable_ipv6 = false`, while this run's effective network status says IPv6 enabled. `ResolvedNetworkBinding::resolve` maps Any to `unrestricted()`, which enables both families without consulting those flags. This explains the discrepancy without assuming the saved file changed. It also permits WebRTC under the current transport eligibility check. Decide whether Any should honor the family flags or explicitly reject/document them; the current effective behavior is easy to misread.

## Reproduction and limits

The run used `/Volumes/seed1/superseedr-config`, host `jagas-air`, without manually changing settings or torrent state. Normal application persistence and networking remained active. The user had stopped the old process before this run; the diagnostic process was stopped gracefully and is no longer running.

Local artifacts are under `tmp/connection-audit-current/`: `run.py`, `analyze.py`, `cleanup.py`, `identity.json`, `events.jsonl`, socket samples, aggregate samples, effective network status, vmmap snapshot, shutdown receipt, and chart. Raw artifacts contain private endpoints and catalog terminal text; keep them local. The aggregate report contains no catalog titles or peer addresses.

The trace is 53,050,301 bytes, below its 64 MiB cap. Span starts and ends reconcile. Instrumentation uses synchronous, serialized JSON writes and can perturb scheduling; this run does not qualify normal-build throughput or latency. The instrumentation is experimental and uncommitted, not a production telemetry proposal. A release build and live run passed; no regression tests were executed and no behavioral fixes were made.

The initial analyzer's simple removal-delay field must not be treated as incarnation-safe evidence. `cleanup.py` narrows the match to successive session starts but still cannot prove identity across all registration races. A longer run after full startup, exact disconnect incarnation tracing, and allocation attribution remain open.


## Pending-dial fix: full-catalog verification

The minimal follow-up adds a generation/address set to `TorrentState`. The manager reserves before allocating the dial task and sends `PeerDialFinished` from a drop guard on registration, failure, or cancellation. State removes the reservation when that command is applied. A full command queue is retried asynchronously; no periodic expiry or shared mutex was added. The generation key keeps old cleanup from affecting a replacement generation. Production changes are confined to `state.rs`, `manager.rs`, and `command.rs`; the earlier session/WebRTC tracing was removed.

Two focused tests were executed successfully after Kyoto compilation: duplicate suppression and reuse after shutdown (including address normalization and generation isolation), and aborted-task completion delivery through a full command queue. Compilation job: `bcd8fca2f26d4f78b51d3347efb80c19`. These are executed test results, not merely `--no-run` success.

The final live run used the same real shared config and host, loaded all **129 entries**, and observed the full catalog from sample second 190 through 250 before graceful shutdown. Instrumented release SHA-256: `eb6c7767fc1f71f89764953d45f9ae80416a3a20b36c525330a4d266e7fe5ffa`; build job `aa2977b79c54471389ab928931c1f782`. The run retained default features and added only temporary lifecycle tracing to the tested fix.

- **0 overlapping outbound attempts** for a torrent/endpoint.
- **63,928 duplicate requests suppressed** before creating another dial task.
- **71,450 reservations created and 71,450 released** through state cleanup, with zero duplicate reservations, missing release entries, or leftovers at manager exit. No reservations required manager destruction to clear them in this final run.
- Maximum reservation lifetime **2.081 seconds**; p99 **2.015 seconds**. Peak pending reservations: 1,180 across the catalog.
- 28,297 transports connected and all 28,297 registered; no registration rejection was recorded. 43,153 dial attempts failed. This change prevents concurrent duplicates; it does not eliminate ordinary failures or sequential retries.
- All 71,450 outbound spans, 28,469 session spans, 431 WebRTC spans, and 129 manager spans ended. All 431 WebRTC close operations succeeded. Process exit code: 0.
- 51 socket snapshots caught one transient CLOSE_WAIT observation. This does not indicate accumulating unclosed sockets.

Key-based session-removal correlation matched 28,463 of 28,469 session spans, with maximum positive delay 94 ms and p99 1.2 ms. The six unmatched sessions and the lack of a shared registration-incarnation ID still limit this separate peer-disconnect metric. Pending-set cleanup, in contrast, reconciles exactly using the set's own keys and insertion/removal events.

The final run's peak sampled RSS was 288.4 MiB. A later vmmap snapshot reported physical footprint 178.1M, peak footprint 222.1M, and malloc allocated bytes 105.2M. This is not a matched RAM/CPU benchmark: startup pacing, peer availability, active transfers, catalog coverage, and instrumentation differ from the original run. No quantified memory saving is claimed.

Raw final evidence and analyzers: `tmp/connection-audit-fixed-full/`. Trace size: 85,425,098 bytes, below the temporary 256 MiB limit, with balanced span starts/ends and reservation insertions/removals. The earlier fixed-code preliminary run is separately retained under `tmp/connection-audit-fixed/`; it was stopped before its smaller trace cap and is not substituted for full-catalog qualification. Production source was restored from byte-for-byte snapshots after building the diagnostic executable. The diagnostic process is stopped. The normal release rebuild passed (Kyoto job `cc88b1f6e2bc4f7d960175ebe3012f55`); `target/release/superseedr` contains the fix without temporary tracing, SHA-256 `3893fe478244de62f6c622630548b4eb5af166aa30f7e2b82bd857db145e7aa1`.
