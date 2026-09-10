# Branch regression review — 2026-09-10

PR: #345, `web-torrent-production` into `develop`. GitHub head verified as `b38976bfa4e72dffad8a4dfa4ac7329a646b925c`; base `85a3e9913ecfef5a13e4cd8b3265f6c51329fc5d`. This follow-up includes fixes for all three findings described below.
Comparison: merge base `85a3e9913ecfef5a13e4cd8b3265f6c51329fc5d` through HEAD; 168 changed files.

## Result

The reviewed PR head had three confirmed findings: P1 native probe congestion is misreported as file unavailability (`src/torrent_manager/manager.rs:3567-3586` at PR head), P1 a full upload writer blocks peer receive processing (`src/networking/session.rs:723-727` at PR head), and P2 retained browser payload can survive a successful delete before metadata arrives (detailed below). All three are addressed by this follow-up. No further confirmed finding emerged from the follow-up full-PR review.

GitHub reports 168 changed files, 51,691 additions, and 25,413 deletions. Its raw diff endpoint rejects this PR as too large and the initial GraphQL file list stops at 100 files. All REST file pages were retrieved and matched exactly against the local base-to-head inventory below; review scope did not stop at that API truncation. Existing PR review-comment and review lists were empty when checked.

### Resolved: P2 — delete retained browser payload even before metadata arrives

Location: `src/web_integration/session/engine.rs:407-408`.

At the reviewed PR head, an active manager's delete request takes the `Accepted` path. When it is awaiting metadata, `TorrentState::Action::Delete` emits `DeletionComplete(Ok(()))` without a `DeleteFiles` effect (`src/torrent_manager/state.rs:2524-2546`). Unlike the stopped-manager path in `engine.rs`, this never removes the OPFS namespace. The deferred backend also treats terminal operations as successful when unopened (`src/persistence/payload/opfs.rs:240-242`).

A user can retain downloaded files, re-add the same magnet while metadata is unavailable, and choose “Remove and delete files.” The row and catalog entry disappear, but the old data still consumes browser storage and has no remaining catalog entry from which to delete it.

Chromium reproduction on an isolated origin: create and close a valid namespace containing a four-byte payload, add its matching trackerless magnet, wait for the awaiting-metadata row, call `remove(hash, true)`, wait for the row to disappear, and inspect OPFS. Result: `{ rows: 0, payloadNamespaceStillExists: true }`. The fixture initializes the retained storage state directly; it does not claim a public-swarm test.

Implemented correction: the metadata-free delete transition now emits the existing physical `DeleteFiles` effect with empty path lists, instead of reporting success itself. Native storage removes no paths. Deferred OPFS handles an unopened Remove by invoking namespace cleanup under the existing exclusive Web Lock; unopened Close continues to retain data. Physical failure therefore reaches the existing catalog recovery path before the entry can be forgotten.

The browser regression contract covers a four-byte retained payload, keep-files removal, re-add without metadata, deletion while another owner holds the payload lock, retained recovery settings across shutdown/reload, successful retry, absent-namespace deletion, and a final reload with no restored torrent. The existing reducer test now requires physical cleanup before reporting success.

Reproduction script and output: `/tmp/webtorrent-remove-repro.mjs`, `/tmp/webtorrent-remove-repro.log`.

## Fixed native regressions

- **False native data-unavailability under storage congestion.** Probe batches now retry `WouldBlock` admission failures instead of classifying them as file faults. Shutdown and the existing 30-second batch deadline stop retries without publishing a false result. Actual missing files remain reported. A paused-clock fixture proves congestion is retried and only the truly missing file becomes a problem entry.
- **PeerSession stops receiving while its upload writer is full.** The session retains one blocked writer message, pauses manager intake, and services incoming messages until writer capacity returns. The main and nested manager-backpressure loops can flush the retained message. A duplex-stream test fills the upload path, verifies Unchoke, Cancel, and a requested download block arrive before the remote reads uploads, then verifies all 1,100 queued upload blocks arrive in order. A separate test covers retained Upload, Have, and Bitfield messages.

## Review method and coverage

Reviewed the complete base-to-head change set by subsystem, tracing changes through native and browser callers. The large application split was compared against its original function bodies: a whitespace/visibility-normalized helper matched 415 unchanged moved functions. This is a refactor-navigation aid, not an AST-equivalence proof. Review attention concentrated on changed behavior and ownership: startup/shutdown, checkpoint acknowledgements, manager incarnations, settings and cluster roles, previews, resource admission, storage cancellation/close/removal, TCP/uTP session flow, metadata negotiation, RTC policy/signaling, OPFS catalog recovery, and verified exports.

Synthetic workloads and their CLI/configuration are included as harness changes, not evidence of production throughput. Documentation, lockfiles, build features, Wasm entrypoints, and CI changes were also checked. The file inventory below records the full comparison scope; it does not imply that every line received equal scrutiny.

## Validation

The earlier review validation below covers the working tree with the two native fixes. It does not describe an untouched checkout of the remote PR. After the browser cleanup correction, the native-only suite passed again (2,256 passed, 2 ignored), Clippy passed with all features and tests, and the new Chromium retained-payload contract passed, including lock failure and reload recovery. Evidence: `/tmp/webtorrent-delete-native.log`, `/tmp/webtorrent-delete-clippy.log`, `/tmp/webtorrent-delete-browser.log`. The full browser engine suite also passed, as did the all-features native suite (2,300 passed, 5 ignored; `/tmp/webtorrent-delete-all.log`). Formatting and diff whitespace checks passed. All three corrections are included in this follow-up.

Native builds used `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0`, with locked offline dependencies to keep disk usage bounded. Socket/browser tests ran with localhost access.

| Check | Result | Local evidence |
| --- | --- | --- |
| Native-only `cargo test --lib --no-default-features --features dht,pex` | 2,256 passed; 2 ignored | `/tmp/webtorrent-final-native-tests.log` |
| All-features `cargo test --lib --all-features` | 2,300 passed; 5 ignored | `/tmp/webtorrent-final-tests.log` |
| Native manager ↔ Chromium interoperability | Both ignored transfer contracts explicitly run and passed | `/tmp/webtorrent-native-rtc-tests.log` |
| Clippy, all features and tests, `-D warnings` | Passed | `/tmp/webtorrent-final-clippy.log` |
| Formatting and `git diff --check` | Passed | Checked locally |
| TypeScript | Passed | `/tmp/webtorrent-typecheck.log` |
| Save contracts, Chromium | 18 Node tests plus folder/ZIP browser contracts passed; 65 MiB fixture | `/tmp/webtorrent-save-tests.log` |
| Save contracts, Firefox and WebKit | Folder/ZIP contents, retained sources, cleanup and 65 MiB export passed in both browsers | `/tmp/webtorrent-save-firefox.log`, `/tmp/webtorrent-save-webkit.log` |
| OPFS storage, Chromium | Sync and writable fallback contracts passed; 65 MiB export | `/tmp/webtorrent-storage-tests.log` |
| Browser engine | Download, export/shutdown, reload/recheck, reseed, bridge recovery, catalog scale and failed-manager/deletion recovery passed | `/tmp/webtorrent-engine-tests.log` |
| Release-built production page | Build and UI transfer/save/pause/resume/reload/removal contracts passed | `/tmp/webtorrent-release-build.log`, `/tmp/webtorrent-built-tests.log` |
| Demo browser suite | 56 passed | `/tmp/webtorrent-demo-tests.log` |
| Demo release build | Passed, including artifact size/static-content checks | `/tmp/webtorrent-demo-build.log` |

The all-features suite passed with the final production code; the later enhancement to the duplex test additionally verifies an incoming requested block and passed in the native-only suite. Clippy compiled that enhanced test with all features.

## Limits

This is broad regression review and fixture validation, not proof of regression absence. Native execution was on macOS; Linux and Windows were not run locally. Public trackers, TURN/NAT diversity, long-running real swarms, and sustained production performance were not qualified. The external-image acceptance test was not run. Remaining ignored native tests retain their existing scope. Browser transfer checks used local synthetic bytes and normal Chromium mDNS privacy settings.

## Changed-file inventory

### Builds, configuration, entrypoints, and test harnesses (43 files)

- `.github/workflows/rust.yml`
- `.gitignore`
- `Cargo.lock`
- `Cargo.toml`
- `src/config/mod.rs`
- `src/config/native.rs`
- `src/integrations/cli.rs`
- `src/lib.rs`
- `src/native/entrypoint.rs`
- `src/telemetry/ui_telemetry.rs`
- `src/tui/runtime/browser.rs`
- `src/tui/screens/config.rs`
- `web/client-wasm/Cargo.lock`
- `web/client-wasm/Cargo.toml`
- `web/client-wasm/src/lib.rs`
- `web/package-lock.json`
- `web/package.json`
- `web/scripts/build-client-wasm.sh`
- `web/scripts/build-wasm.sh`
- `web/scripts/check-dist.mjs`
- `web/scripts/prepare-peer-client.mjs`
- `web/scripts/test-engine.sh`
- `web/scripts/test-storage.sh`
- `web/storage-contract/Cargo.lock`
- `web/storage-contract/Cargo.toml`
- `web/storage-contract/image-worker.mjs`
- `web/storage-contract/src/lib.rs`
- `web/storage-contract/worker.mjs`
- `web/tests/engine-contract.mjs`
- `web/tests/engine-regressions.mjs`
- `web/tests/rtc-bridge-contract.mjs`
- `web/tests/rtc-image.mjs`
- `web/tests/rtc-peer.mjs`
- `web/tests/save-all-browser-contract.mjs`
- `web/tests/save-all-contract.mjs`
- `web/tests/save-all-engine-contract.mjs`
- `web/tests/save-file-contract.mjs`
- `web/tests/storage-contract.mjs`
- `web/vite.webtorrent.config.ts`
- `web/wasm/Cargo.lock`
- `web/wasm/Cargo.toml`
- `web/wasm/src/lib.rs`
- `web/wasm/src/simulation/mod.rs`

### Documentation (6 files)

- `docs/app-refactor.md`
- `docs/browser-tm-portability-audit.md`
- `docs/synthetic-benchmark.md`
- `docs/web-torrent-production-design.md`
- `docs/webtorrent-image-acceptance.md`
- `web/README.md`

### Synthetic workloads (4 files)

- `scripts/test-synthetic-connectivity.py`
- `src/native/synthetic_load.rs`
- `src/native/synthetic_load/rtc.rs`
- `src/native/synthetic_load/workload.rs`

### Application extraction and native host (51 files)

- `src/app/bootstrap.rs`
- `src/app/browser_model.rs`
- `src/app/checkpoint.rs`
- `src/app/commands.rs`
- `src/app/display_rate.rs`
- `src/app/file_preview.rs`
- `src/app/graph_model.rs`
- `src/app/ingest_policy.rs`
- `src/app/lifecycle.rs`
- `src/app/limits.rs`
- `src/app/manager_lifetime.rs`
- `src/app/mod.rs`
- `src/app/model.rs`
- `src/app/native.rs`
- `src/app/native/app_tests.rs`
- `src/app/native/bootstrap.rs`
- `src/app/native/cluster.rs`
- `src/app/native/control.rs`
- `src/app/native/ingest.rs`
- `src/app/native/integrity.rs`
- `src/app/native/listeners.rs`
- `src/app/native/manager_effects.rs`
- `src/app/native/network.rs`
- `src/app/native/persistence.rs`
- `src/app/native/presentation.rs`
- `src/app/native/preview.rs`
- `src/app/native/resources.rs`
- `src/app/native/rss.rs`
- `src/app/native/runtime.rs`
- `src/app/native/settings.rs`
- `src/app/native/status_output.rs`
- `src/app/native/torrent_runtime.rs`
- `src/app/native/version.rs`
- `src/app/native/watch_input.rs`
- `src/app/panels_model.rs`
- `src/app/presentation.rs`
- `src/app/reducer.rs`
- `src/app/reducer/health.rs`
- `src/app/reducer/metadata.rs`
- `src/app/reducer/preview.rs`
- `src/app/reducer/removal.rs`
- `src/app/reducer/services.rs`
- `src/app/resource_model.rs`
- `src/app/rss_model.rs`
- `src/app/settings_policy.rs`
- `src/app/throttle.rs`
- `src/app/torrent_helpers.rs`
- `src/app/torrent_manager_protocol.rs`
- `src/app/torrent_model.rs`
- `src/app/version_model.rs`
- `src/app/visualization_model.rs`

### Storage, execution, and resource ownership (14 files)

- `src/execution/browser.rs`
- `src/execution/browser_contract.rs`
- `src/execution/mod.rs`
- `src/persistence/mod.rs`
- `src/persistence/payload.rs`
- `src/persistence/payload/capability.rs`
- `src/persistence/payload/capability_tests.rs`
- `src/persistence/payload/native_backend.rs`
- `src/persistence/payload/opfs.js`
- `src/persistence/payload/opfs.rs`
- `src/persistence/payload/spans.rs`
- `src/resource/mod.rs`
- `src/resource/native.rs`
- `src/token_bucket.rs`

### Torrent manager and networking (27 files)

- `src/networking/activation.rs`
- `src/networking/mod.rs`
- `src/networking/model.rs`
- `src/networking/runtime.rs`
- `src/networking/session.rs`
- `src/networking/session_metadata.rs`
- `src/networking/webtorrent/browser.js`
- `src/networking/webtorrent/browser.rs`
- `src/networking/webtorrent/diagnostics.rs`
- `src/networking/webtorrent/mod.rs`
- `src/networking/webtorrent/native.rs`
- `src/networking/webtorrent/tracker.rs`
- `src/networking/webtorrent/wire.rs`
- `src/peer_manager/data.rs`
- `src/peer_manager/native.rs`
- `src/torrent_manager/command.rs`
- `src/torrent_manager/file_progress.rs`
- `src/torrent_manager/integrity_scheduler.rs`
- `src/torrent_manager/manager.rs`
- `src/torrent_manager/mod.rs`
- `src/torrent_manager/payload_contract_tests.rs`
- `src/torrent_manager/rtc.rs`
- `src/torrent_manager/rtc_contract_tests.rs`
- `src/torrent_manager/rtc_image_contract_tests.rs`
- `src/torrent_manager/state.rs`
- `src/torrent_manager/tracker_execution.rs`
- `src/tracker/mod.rs`

### Browser host, catalog, and exports (23 files)

- `src/web_integration/mod.rs`
- `src/web_integration/session.rs`
- `src/web_integration/session/bootstrap.rs`
- `src/web_integration/session/catalog.js`
- `src/web_integration/session/checkpoint.rs`
- `src/web_integration/session/control.rs`
- `src/web_integration/session/engine.rs`
- `src/web_integration/session/manager_lifecycle.rs`
- `src/web_integration/session/managers.rs`
- `src/web_integration/session/preview.rs`
- `src/web_integration/session/rss_results.rs`
- `src/web_integration/session/runtime.rs`
- `src/web_integration/session/settings.rs`
- `src/web_integration/session/telemetry.rs`
- `src/web_integration/session/view.rs`
- `web/src/engine-worker.js`
- `web/src/rtc-host.js`
- `web/src/save-all.js`
- `web/src/save-archive.js`
- `web/src/save-file.js`
- `web/src/webtorrent.css`
- `web/src/webtorrent.js`
- `web/webtorrent.html`

