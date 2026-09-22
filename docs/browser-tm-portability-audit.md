# Browser TorrentManager portability audit

Audited 2026-09-05 at checkpoint `9f5a7d5b` on `web-torrent-production`.
This supplements [the production design](web-torrent-production-design.md).
The audit below records the initial gap assessment. Section 5 records the subsequent
implementation and acceptance evidence. It does not declare the browser product complete.

## 1. Conclusion and ownership

Run the existing `TorrentManager`, `TorrentState`, peer protocol/session, and injected payload backend in the browser. Retain the existing app action/effect refactor. The required work is execution and platform integration, not a second torrent engine or another full app rewrite.

- `TorrentState` remains authoritative for logical peer acceptance/removal, scheduling, and state transitions.
- TM executes effects, owns tracker services and peer-session tasks, obtains permits, and rejects stale results under existing lifecycle rules.
- WebSocket/RTC adapters own physical signaling and connection operations; a ready connection is not permission to bypass state admission.
- `PeerSession` retains handshake, metadata exchange, requests, cancels, and bidirectional peer-wire behavior.
- The injected payload capability owns physical storage. Its OPFS backend remains the single bounded handle/operation owner.
- The browser host constructs and drives these components, routes app effects/events, and supplies actual platform capabilities and lifecycle signals.

The first executable slice is this same manager transferring and seeding through browser WebRTC into the production OPFS backend. Durable app restoration, website workflows, and playback remain subsequent acceptance milestones in the production design.

## 2. Evidence and limits

### 2.1 Existing transfer checkpoint

The committed fixes cover RTC cleanup and metadata correctness, upload/HAVE backpressure, OPFS progress-count validation, and the real-image acceptance harness. See [the image acceptance report](webtorrent-image-acceptance.md).

Validation recorded for the checkpoint includes 2,242 all-feature library tests, 2,018 no-default plus WebTorrent library tests, strict Clippy for the relevant native/Wasm builds, Chromium RTC and OPFS contracts, and full 755 MiB download/reseed acceptance in separate executions.

That acceptance uses a native Superseedr TM and an independent browser WebTorrent client with our Wasm OPFS backend. It does not establish that Superseedr's TM already runs in the browser.

### 2.2 Isolated browser execution probe

An isolated Wasm crate used Tokio 1.50.0 with `io-util`, `macros`, `rt`, `sync`, and `time`, driven by a wasm-bindgen async export. It imported the actual unchanged `src/resource/native.rs` actor directly.

The probe passed in both a Chromium 151.0.7922.34 Window and dedicated worker:

- `LocalSet::run_until` driven by the browser executor, without constructing a native Tokio runtime.
- `JoinSet::spawn_local` with non-`Send` `Rc` captures and browser Promise/timer wakeups.
- Tokio task identity, join results, explicit abort, cancellation reporting, and RAII drop on abort.
- `mpsc`, `oneshot`, and `broadcast` channels.
- `tokio::io::duplex` backpressure and exact delivery after a paused reader resumes.
- The existing resource actor: a held disk-read permit blocked another acquisition; release admitted the waiter; shutdown joined the actor.

This supports retaining Tokio channels, buffered I/O, task handles, task groups, and IDs. It does not validate Tokio timers, a native blocking pool, the complete TM, or long-lived host shutdown. Browser-backed `setTimeout` supplied the probe's delays.

The same capability check found `RTCPeerConnection` in the Window and absent in the worker; `WebSocket` existed in the worker. This is evidence for this Chromium version, not a cross-browser support claim.

Local probe source is `/private/tmp/ss-browser-audit-ygn5j5lj`; build and result logs are in ignored `target/iso-acceptance/browser-runtime-probe*.log`. These are local evidence artifacts, not checked-in tests. Production integration must promote the relevant contracts into maintained tests.

## 3. Required source changes

### 3.1 Compilation boundaries and construction

| Location | Required change |
| --- | --- |
| `src/lib.rs` | Expose the portable TM and tracker vocabulary on Wasm. Fix the native-only `Settings` alias dependency where TM imports it. |
| `Cargo.toml`, new `web/client-wasm/Cargo.toml` | Select browser RTC capabilities explicitly; keep native socket/TLS/RTC dependencies target-gated. Preserve the nested crate's standalone workspace. Start the browser engine without DHT/PEX defaults. |
| `src/networking/mod.rs` | Expose protocol/session and shared activation contracts. Keep TCP/uTP, raw sockets, DNS/interface services, and native web-seed implementation gated. |
| `src/tracker/mod.rs` | Separate shared tracker events/responses/URL normalization from the native client and error types. Use the existing `url::Url` dependency for shared normalization. |
| `src/peer_manager/{mod,data,native}.rs` | Move pure `normalize_ip` and `RECONNECT_WINDOW` exports into shared code; provide the policy receiver through browser composition. Do not port native persistence/network services just to expose these helpers. |
| `src/resource/{mod,native}.rs` | Expose the already-portable resource actor/client/permit guard. The browser probe ran its production actor unchanged. |
| `src/torrent_manager/mod.rs` | Remove unconditional native DHT/incoming-socket requirements from browser construction. Keep common channels, policy, limits, and `with_payload` injection. |

`SocketAddr`, IP address types, `PathBuf`, and `Duration` used as data are not themselves reasons to rewrite domain contracts. Browser RTC peers already register without a socket endpoint.

### 3.2 Execution, time, and hashing

Add a narrow execution boundary at the existing runtime composition seam:

- Native task spawning continues through Tokio's native scheduler.
- Browser task spawning uses `spawn_local` inside a host-owned, continuously driven `LocalSet`; task groups use `JoinSet::spawn_local`.
- Retain join handles, task IDs, abort/drop cleanup, and existing bounded channels. Do not replace every task with an untracked wasm-bindgen task.
- Provide browser-backed sleep, timeout, and interval operations with explicit cancellation and missed-tick behavior. A Tokio `time` feature in Cargo does not make native timer calls work under this browser executor.
- Replace runtime `std::time::Instant`/`SystemTime` dependencies with compatible clocks, using the already-present `web-time` crate where appropriate. Preserve elapsed-time calculations and rate-limit semantics.
- Replace `spawn_blocking` execution of SHA/Merkle validation with bounded worker execution that yields between suitable units of work. Keep the hash algorithms and stale-result/permit rules. A separate hash worker pool is an optional optimization, not a prerequisite architecture.

Call sites include `torrent_manager/manager.rs`, `torrent_manager/rtc.rs`, `torrent_manager/integrity_scheduler.rs`, `networking/session.rs`, `networking/webtorrent/tracker.rs`, `networking/activation.rs`, and `token_bucket.rs`.

The Wasm payload deliberately contains `Rc` and local futures, so tasks capturing it cannot use the native `Send` spawning path. Conversely, a session fed through Tokio `DuplexStream` may retain its existing `Send` stream bound. Relax bounds only where an actual browser value requires it.

### 3.3 State freeze: a small clock exception

`state.rs` was untouched at the audited checkpoint; the user subsequently approved the two clock-import substitutions. Its production imports use `std::time::{Instant, SystemTime}`. Browser execution requires compatible clock implementations; `PeerPolicy` already uses `web_time::SystemTime`, so enabling the current state on Wasm also exposes a clock-type mismatch.

The proposed exception is replacing the two clock imports with `web_time` equivalents and aligning callers. Native aliases preserve native clock types. This is an integration correction, not a state-machine or admission-policy refactor. Review it explicitly under the existing soft-freeze rule and verify unchanged native behavior.

### 3.4 Network lifetime and TM event loop

`networking/activation.rs` mixes useful generation/invalidation semantics with native `NetworkLease` and listen-port requirements. Separate the common lifetime contract from the OS binding implementation at this existing boundary.

The browser needs real lifecycle invalidation, cancellation, and stale-result rejection. An always-active placeholder would discard protections that currently apply to negotiation, session startup, and teardown.

In `TorrentManager::run` and effect execution:

- Gate native incoming connections, TCP/uTP connects, DHT/PEX, native HTTP/UDP trackers, and native HTTP web seeds for the initial RTC-only browser build.
- Route browser-compatible WSS trackers through the existing RTC tracker integration.
- Replace process `ctrl_c` handling with the existing host command/shutdown lifecycle on the browser path.
- Adapt timer and task execution while retaining validation, metrics, uploads, pending HAVE retries, and state action/effect processing.
- Import shared address normalization from `networking::model`, rather than pulling in the native runtime for a pure helper.

In `rtc_supported`, use browser capabilities instead of requiring native NIC binding, system DNS policy, both OS IP families, and native network activation. Preserve supported torrent-format and privacy restrictions; do not claim browser enforcement of unsupported raw-IP policies.

### 3.5 WebSocket and WebRTC adapters

`networking/webtorrent/tracker.rs` currently depends on `tokio-tungstenite` and concrete native `Driver`, `IceOptions`, and `Negotiation` types. Extract the platform operations while retaining signaling validation and the existing manager-owned service/report protocol.

The browser implementation needs WebSocket send/receive/close and RTC offer/answer/ICE/DataChannel operations. Preserve announce scheduling, negotiation bounds, per-peer failure isolation, incarnation checks, and physical close completion. Apply explicit message-size/admission limits to browser callbacks as well as Rust queues.

Keep `networking/webtorrent/native.rs` as the native adapter. Feed the browser transport into the existing session byte-stream contract, including backpressure, EOF, errors, and cancellation.

The proposed topology is TM/session/resource/payload execution in the application worker, with RTC negotiation in the Window where the measured browser exposes it. Define a bounded, generation-tagged connection bridge. Qualify direct DataChannel transfer separately; do not assume it is supported. A Window-owned channel bridge must bound queued bytes in both directions and respect DataChannel buffering before acknowledging capacity.

Required failure cases include worker loss, page-side connection failure, stale callbacks, closed channels, ICE failure, full queues, and shutdown while signaling or transfer is active. The Window adapter reports failures to the owning TM; it does not independently decide logical peer removal.

### 3.6 Browser app and storage composition

The app refactor is already in place. `BrowserSession::from_settings`, environment configuration, manager endpoints, event/metric routing, and checkpoint revision APIs are the relevant integration points. `web/wasm/src/browser_demo.rs` still starts a fixture session; add real host composition using these existing contracts.

The host must create the resource actor, browser network lifetime, OPFS payload, and TM, then drive them independently of rendering or animation frames. It must route app effects into these runtimes and report completion through existing app actions.

Keep the OPFS backend's bounded cached handles and serialized physical operations. Preserve lease retention after caller cancellation, close/drain behavior, and revalidation of retained bytes. There is no need for a new storage abstraction.

Durable browser catalog/settings persistence remains incomplete: the current browser app backend is in-memory. Use the existing checkpoint prepare/complete revision boundary for asynchronous durable commits. Catalog recovery, payload reconciliation, reload/resume, and tab ownership are a separate required milestone before claiming a complete browser product.

## 4. Implementation order and acceptance gates

1. **Shared compilation and execution.** Expose portable modules, make native dependencies conditional, add local spawning/timers/clocks, and wire existing resource permits. Include the isolated clock exception in review. Gate: browser build actually includes TM/session; maintained browser runtime tests cover tasks, timing, cancellation, and resource accounting; native feature matrices remain green.
2. **Browser network/session integration.** Implement actual lifecycle scopes, WebSocket signaling, RTC transport, and the bounded Window/worker bridge. Gate: real browser Superseedr TM obtains BEP 9 metadata, accepts peers through state, downloads verified bytes into OPFS, and uploads those bytes to an independent peer. Exercise both negotiated directions, pressure, cancellation, and stale generations.
3. **Real app composition and persistence.** Start from settings/user input, route app effects, support pause/resume/remove and graceful close, then implement durable checkpoints and restoration. Gate: reload/recheck/reseed and worker/tab failure recovery through the real app; no fixture driver or browser JS torrent engine stands in for Superseedr.
4. **Website and media completion.** Finish magnet paste/parameters, torrent upload, file selection, saving, local-data seeding, verified range reads, sequential playback, and seek behavior as specified in the production design. Gate: user workflows and claimed browser support have direct acceptance evidence.

Each patch should retain the native WebRTC and storage regression checks appropriate to its affected boundary. A compile pass alone is insufficient evidence for browser timers, lifecycle, transfer, or durable storage.

## 5. Implementation status after the approved port

### 5.1 Modes and authority

The existing live demo is preserved at `web/index.html` with its original Wasm
crate and build path. The production-engine mode is separate: `web/webtorrent.html`,
`web/client-wasm`, an app worker, and an independent `client-dist` build. Neither
mode replaces the other. See [the web README](../web/README.md) for build and host commands.

The port retains state admission, state scheduling, TM tracker/service ownership,
and the shared peer-wire session. The **only** `state.rs` changes are the approved
`web_time::Instant` and `web_time::SystemTime` imports. No POC source was copied or
consulted for this implementation.

### 5.2 Implemented integration

- Shared execution adapters retain Tokio task IDs, joins, cancellation, channels,
  and buffered I/O. Browser tasks run in a continuously driven LocalSet; browser
  timers implement sleep, timeout, and interval behavior. Hash work yields before
  bounded synchronous computation on the application worker. Native execution
  still uses Tokio scheduling, timers, and the blocking pool.
- Native TCP/uTP, DHT/PEX discovery, HTTP/UDP trackers, raw socket services, and
  HTTP web seeds are gated out of the browser build. Existing WebTorrent signaling
  runs over a bounded browser WebSocket adapter, with cancellation-safe reception.
- Window-owned RTC connects to the Wasm app worker through a bounded MessagePort
  bridge, with session/activation generations, physical-close acknowledgements,
  heartbeat failure detection, and resource permits retained through cleanup.
  Loss blocks the old activation and displays reconnecting status. The worker requests
  one replacement port at a time, bounds acknowledgment waits, and rejects stale replies;
  a new activation requires a successful heartbeat. Shutdown stops recovery.
  Ready transports still pass through TM and state's existing peer admission.
- Deferred OPFS initialization lets magnets acquire their layout from verified
  metadata. The adapter serializes operations into the existing payload backend,
  bounds operation count/bytes, retains leases after caller cancellation, reserves
  terminal admission, and seals/replays close completion.
- Browser host composition creates the actual app, resource actor, activation
  lifetime, and managers. It accepts magnets/torrents and routes pause/resume/remove,
  metadata observations, metrics, checkpoints, and orderly shutdown. TM metrics
  report actual pause state, and owned terminal tasks finish deletion before final
  payload close while notification delivery remains independently backpressured; the built-page tests exposed and verified both integration fixes.
- IndexedDB commits the torrent/settings catalog and exact verified metadata.
  Web Locks enforce one catalog owner per origin. Restoration retains source
  tracker URLs, saved priorities/configuration and global rate limits, and always
  rechecks payload bytes. Metadata is pruned after confirmed catalog removal.
  The original bencoded info dictionary is retained exactly; serializing the
  decoded Info struct would change the swarm hash in some cases.
- Bounded file-range exports use the existing TM, disk-read permits, and injected
  payload. Every covering piece must be verified. The website supports pasted
  magnets, URL parameters, torrent upload, saving, pause/resume, removal, and
  continued seeding while open.

### 5.3 Acceptance evidence

The maintained `web/tests/engine-contract.mjs` uses a local signaling tracker and
an independent browser client. Superseedr itself runs entirely in browser Wasm:
no native TM and no substitute JavaScript torrent engine. Generated payload size
is 2,097,189 bytes; its SHA-256 is
`b3908d818349c83ad2fc3c8c28b55ffd8945cb7a126729dcf2f0aa056d6e4664`.

Observed acceptance includes magnet metadata exchange, verified OPFS download and
range export, orderly shutdown, a **fresh worker** restoring and rechecking without
the original seed, and reseeding identical bytes to an independent downloader.
Both negotiation directions were exercised. The contract also checks task/clock
semantics, activation invalidation, resource permits, full storage admission,
cancellation-safe lease retention, and idempotent terminal close. Duplicate torrent
input, competing catalog ownership, and invalid export ranges are rejected.

The lifecycle review fixes add removal intent independent of the shutdown phase:
confirmed removal reconciles the catalog even when Stop overlaps cleanup, while
failed cleanup retains paused recovery settings. Already accepted removals receive
no duplicate shutdown command; missing acknowledgments still report failure.
The real browser contract passed bridge heartbeat expiry, a delayed host reply,
stale reply rejection, reseeding after recovery, and remove-plus-shutdown for both
payload retention and deletion. Three deterministic JavaScript contracts cover
bridge handshake expiry, old-peer cancellation, bounded retries, and recovery shutdown.
The shared demo's 109 real-Wasm contracts also passed.

This follow-up browser transfer run used `SUPERSEEDR_TEST_DISABLE_MDNS=1`:
a bare Chromium RTC pair failed with local `.local` ICE candidates and connected
with the override. The default-mode transfer timed out before metadata; this is
not evidence that the default mDNS path passed. The override belongs only to the
local acceptance harness and does not alter production RTC settings.

The built release page also passed restoration/export with a matching SHA-256,
pause/resume, duplicate upload rejection, confirmed deletion across restart,
URL magnet input, deletion before metadata arrives, and successful torrent upload.
Its contract completion marker is
`BUILT_PAGE_RESTORE_SAVE_PAUSE_RESUME_UPLOAD_REMOVE_PARAMETER_VERIFIED`.

Final native all-feature tests: **2,245 passed, 4 ignored**, run serially. The new
export rejection, actual pause-state, and delete-before-close tests passed. A
throughput assertion failed in an earlier run alongside compiler work; it passed
in the final isolated run. An existing full-event-queue test also caught a cleanup
join waiting on notification delivery; physical cleanup now joins independently
of backpressured app event delivery, and that regression passes.

Native no-default plus WebTorrent tests earlier in integration: 2,018 passed,
4 ignored. Final strict Clippy passed for native all-targets with all features,
no defaults plus WebTorrent, and no defaults without WebTorrent, and for the
browser client with its opt-in contracts. Rust formatting and diff checks passed. Existing live-demo browser
regressions: **56 passed**. OPFS contracts passed in sync-handle and writable-stream
modes with a peak of two handles.

The existing demo release size gate fails on the committed baseline as well:
2,544,014 bytes versus its 2,500,000-byte raw Wasm budget. Before this lifecycle
follow-up, the integration demo artifact measured 2,545,480 bytes, an increase of 1,466 bytes. The budget was not
raised. Typechecking and the static demo build succeed before that gate. The
separate WebTorrent release build succeeds and does not use the demo size budget.

### 5.4 Remaining product work and qualification

This establishes a real browser engine mode, not the entire production website.
Remaining accepted design work includes:

- Sequential playback and seek scheduling. The previously mentioned TM sequential
  mode is absent from this checkout; its branch/commit is pending clarification.
  No scheduling change was introduced under the state freeze.
- Media range/service-worker integration, codec/container capability handling,
  file-priority controls, and importing local content to start seeding.
- Full durable history/RSS and app integrity-probe services; currently the live
  host persists torrent/settings/metadata and surfaces payload availability faults.
  It does not claim native app service parity.
- Quota/eviction recovery UX, crash/worker-loss and interrupted-deletion recovery,
  catalog migrations, large-file/long-duration transfer qualification, and
  cross-browser/NAT/TURN deployment tests. The prior 755 MiB native-to-browser
  checkpoint is not evidence of a 755 MiB fully browser TM transfer.
- A release/deployment entry that presents both modes together. Static artifacts
  are separate and can be deployed alongside one another; no remote deployment
  or publication occurred.


## 6. Download-focused website scope (2026-09-05)

Sequential scheduling and its configuration were reverted in `cb6e3bfb`. The
browser retains the earlier WebRTC/OPFS engine, browser clock compatibility, and
seeding grace-period fix. Streaming/player/seek work is deferred.

The website displays manager-projected verified bytes for each manifest file.
Save is gated on that file's completion; manager verified-range admission still
checks every read. Saving copies data and retains the OPFS source for seeding.
The native reducer remains unchanged. See the production design's current release
scope and `web/README.md` for picker and OPFS-backed download behavior.

### 6.1 Verification and observed integration fixes

The browser app now preserves running/paused catalog intent when shutdown emits a
final paused metric. This race was present before the sequential feature. Manager
metrics continue while paused, so revalidation and already-admitted writes can
make completed files saveable without running reducer ticks or resuming transfers.
Skipped files are excluded from eligibility and export reads.

Validation for this website increment:

- Native all-feature library suite: 2,246 passed, 4 ignored before the final
  paused-observation change. After that change, the torrent-manager suite passed
  300 tests, 3 ignored. Final verified-export and per-file projection tests also
  passed, including skipped files, shared boundaries, rechecking, and pending writes.
- Real-Wasm app contracts: 110 passed, including shutdown catalog intent.
- Save/RTC host contracts: 7 passed. Large picker saves await bounded 1 MiB reads
  and writes; cancellation, failed/short reads, zero-length files, and fallback
  limits are covered. Picker control uses a test sink; native picker dialogs are
  not automated browser acceptance evidence.
- The final release page passed actual Chromium WebRTC download, browser-download
  save, and subsequent independent-peer reseeding. All three contents matched
  SHA-256 `b3908d818349c83ad2fc3c8c28b55ffd8945cb7a126729dcf2f0aa056d6e4664`
  for 2,097,189 generated bytes. The run also covered running and paused reload,
  bridge recovery, input by paste/URL/upload, incomplete Save gating, duplicate
  rejection, and removal racing shutdown. The existing local-only mDNS override
  (`SUPERSEEDR_TEST_DISABLE_MDNS=1`) was used; this does not qualify other browsers
  or the default mDNS path.
- The preserved demo passed 54 browser cases initially; two timing cases passed
  on targeted reruns with unchanged assertions. Its separate size gate still
  fails: 2,546,225 raw Wasm bytes against 2,500,000. An isolated build of untouched
  pre-sequential `1619252c` measured 2,546,807 bytes and failed the same limit.
  The budget was not increased. This remains a pre-existing demo release issue,
  independent of the passing production WebTorrent build.

Production acceptance commands: `npm run build:webtorrent`, followed by
`SUPERSEEDR_TEST_BUILT_UI=1 SUPERSEEDR_TEST_DISABLE_MDNS=1 npm run test:webtorrent`
from `web/`. The release page stays in `client-dist`; the test command separately
builds its contract-enabled worker module.


### 6.2 File-backed export integration (2026-09-06)

The previous 64 MiB in-memory fallback is replaced by a completed OPFS-backed
`File`. The Window requests `LiveClient.export_file`; TorrentManager admits the
file using the current committed-piece projection, then the payload backend
queues a flush/handle release and `getFile()`. The worker structured-clones the
File to the Window, which starts an object-URL download. Picker saves retain the
bounded verified-range path. Neither path removes the original seeding source.
`state.rs` still matches `1619252c` exactly.

The app reports **Download started** for the browser-managed path. URLs remain
alive for the document lifetime; there is no assumed completion timer. Users are
told to keep the page open and finish browser downloads before deleting the
source or reloading. Source mutation/removal can invalidate the File; there is
no immutable staging copy or promise of downloads surviving deletion. Stop
client retains the source. The existing removal confirmation explains the effect
on pending saves.

Production-backend tests ran against generated bytes, using a real download and
an independent streamed SHA-256 check. Each run also exercised exact cross-file
I/O in sync and writable modes, empty/skipped/padding exports, short physical
files, queued export before close/removal, late rejection, and retained reads.
The download itself holds a reopened sync reader handle, modeling upload access.
Temporary large payloads, downloads, and owned storage-test profiles are removed.

| Browser test build | 65 MiB + 37 byte save | 2 GiB save | Retained reads |
| --- | --- | --- | --- |
| Chromium 151.0.7922.34 | Passed | Passed | Passed |
| Firefox 153.0 | Passed | Passed | Passed |
| Playwright WebKit 26.5 | Passed | Passed | Passed |

WebKit requires a persistent test profile for OPFS in this harness; its ephemeral
context failed during storage initialization. Cross-file fallback tests then
reproduced an existing WebKit writable-stream problem with nonzero typed-array
byte offsets. Supplying a bounded copy of just that span, starting at offset
zero, fixed the byte mismatch. Both backend modes now pass across this matrix.
These tests qualify storage/export in these builds, not the entire client in
Firefox/WebKit or shipping Safari/iOS, private browsing, quotas, or background
operation. They do not establish a hard browser-process memory ceiling.

Additional integration validation:

- Chromium's real TorrentManager downloaded, verified, saved through the built
  page, and reseeded 68,157,477 bytes to an independent client. All hashes matched
  `88d77a58204761b2f3db8892cea7649f57f0d007660d4880583615436d3930a0`.
- The final release artifact was rebuilt after the WebKit fix and passed the
  complete 2,097,189-byte client/page suite again, including export racing
  shutdown, invalid and unverified export rejection, paused recheck, retained
  File readability after shutdown, bridge recovery, and all input/removal flows.
  The local-only mDNS override was used for both WebRTC runs.
- Nine save/RTC JavaScript contracts passed. Picker tests use an injected sink;
  real browser downloads exercise the file-backed path.
- Native TorrentManager tests: 300 passed, 3 ignored. Shared Wasm app tests:
  110 passed. The simulated demo explicitly rejects file export requests.
- Strict native all-target/all-feature Clippy and browser-client Wasm Clippy
  passed. The final production WebTorrent build and formatting checks passed.

Reproduce a large storage export with
`SUPERSEEDR_TEST_EXPORT_BYTES=2147483648 node tests/storage-contract.mjs` after
`npm run test:storage` from `web/`. Select an installed Playwright build using
`SUPERSEEDR_TEST_BROWSER=firefox` or `webkit` and, if installed separately,
`PLAYWRIGHT_BROWSERS_PATH`. Storage tests now use isolated persistent profiles.
