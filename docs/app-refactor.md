# Application refactor

## 1. Scope

This change refactors the existing application layer on `web-torrent-production`: native application orchestration, shared application models and transitions, browser session execution, and application behavior previously embedded in the browser UI adapter. It preserves the public native `App` entry point and browser session methods.

The production WebTorrent design remains the source of the broader delivery scope. WebRTC transport, durable browser payload/catalog adapters, workers, playback, export, and the website are subsequent workstreams. The browser ports described here make that work explicit; an empty browser host or a passing Wasm contract is not evidence of a working browser torrent client.

`src/torrent_manager/state.rs` is unchanged. `TorrentState` continues to decide peer admission/removal and logical torrent behavior. App effects request work and interpret observations; they do not admit peers, schedule pieces, establish data integrity, or replace existing service algorithms.

## 2. Responsibility map

| Responsibility | Shared application owner | Native/browser execution |
| --- | --- | --- |
| Models and presentation | `app/model.rs`, `torrent_model.rs`, `commands.rs`, and the focused browser/panel/visualization models; `app/mod.rs` provides compatible re-exports | Hosts own their handles and expose the composed `AppState` to the existing renderer |
| Initial hydration | `app/bootstrap.rs::initial_app_state` initializes configuration, presentation, RSS and journal facts | `app/native/bootstrap.rs` constructs services and retains existing paced startup/promotion; `web_integration/session/bootstrap.rs` supports explicit fixture and empty-host construction |
| Ingest and role policy | `app/ingest_policy.rs` decides prompting, direct adds and forwarding after acquisition; existing control/ingest types preserve origins and correlations | Native `ingest`, `control`, `cluster`, and `watch_input` modules retain staging, locks, archive, shared-inbox and CLI/control integration; browser `control` normalizes paste and executes browser/manager requests |
| Settings and catalog | `app/settings_policy.rs` tracks requested settings, filters stale outcomes, projects effective settings and plans catalog effects | Native `settings` and `torrent_runtime` execute service changes and manager requests, retaining port rollback; browser `settings` and `managers` use the shared projection and report failed delivery |
| Metadata and previews | `app/reducer/metadata.rs` and `preview.rs` own metadata hydration, preview identity, fetch/preview results, sorting and cursor reconciliation | Native `preview` acquires filesystem data; browser `preview` adapts selected/virtual file data and queues acquisition requests |
| Manager observations | `app/reducer.rs`, `removal.rs`, and `manager_lifetime.rs` handle projections, cleanup outcomes and stale registration tokens | Native `manager_effects` and browser `managers` execute resource cleanup; each host retains one set of manager channels and registration lifetimes |
| Network and resource services | Shared projections and existing resource/throttle models | Native `network`, `listeners`, `resources`, and `integrity` preserve existing network scopes, peer policy, resource/tuning controllers, integrity scheduler and inbound routing ownership |
| Integrity observations | `app/reducer/health.rs` turns freshness-checked sweep outcomes into projection changes and requested follow-up work | The existing integrity scheduler retains epochs, sweep assembly and scheduling; the manager retains recovery/integrity decisions |
| RSS and history observations | `app/reducer/services.rs` interprets RSS and history restore results; existing telemetry modules retain history algorithms | Native `rss` and `persistence` execute integration work; browser `rss_results`, `telemetry` and the effect port expose browser results/work |
| Checkpoints and recovery | `app/checkpoint.rs` prepares snapshots from an explicit time observation and tracks requested/committed revisions | Native `persistence` owns ordered writers and drains acknowledgements during flush; browser `checkpoint` separates snapshot preparation from asynchronous durable completion |
| Journal and external output | Existing event-journal/status models and formats | Native `persistence`, `status_output`, and `version` preserve journal origins, snapshots, status files and version checks; browser observations stay in its host modules |
| Shutdown | `app/lifecycle.rs` tracks distinct manager acknowledgements, failures and checkpoint readiness | Native `runtime` owns task sets and teardown independently of rendering; browser `runtime` exposes shutdown plus an explicit checkpoint/result contract |
| Future browser media/export/seeding UI | The website and verified-read contracts in the production design | These features still require their browser adapters, WebTorrent managers and payload storage implementation |

The native and browser facade files now contain composition fields, imports, module declarations and compatible entry points. Existing native integration tests remain under the same Rust test namespace, in `app/native/app_tests.rs`; focused new tests sit beside the shared behavior they qualify.

## 3. Application contracts

### 3.1 Actions, effects and service ownership

`AppAction` handles manager facts, service observations, completion transitions and checkpoint results. Focused shared functions handle settings/catalog planning and presentation operations where an additional enum round trip would add no useful boundary. `AppEffect` describes metadata work, resource cleanup, data-health follow-up, RSS projection refresh and requested checkpoints.

The existing native service owners continue to implement physical I/O. Browser composition obtains remaining physical work through `BrowserSession::drain_effects()`, and user/acquisition requests through the existing `drain_commands()` interface. The manager-event effect queue applies backpressure at 1,000 pending effects; checkpoint requests are coalesced separately. Browser command overflow and unavailable RSS/network-interface operations surface errors. Lifecycle/metadata publication returns the original event on full or closed channels, allowing the producer to retry; the demo caller follows that contract.

`BrowserSession::from_fixture` explicitly selects demo behavior. `from_settings` starts without presentation fixtures or simulated managers, and exposes capability facts through `capabilities()`. It does not claim durable storage, native paths, shared locks, RSS, or OS telemetry. The existing web entry point still selects the demo; installing actual browser capabilities belongs to browser production composition.

### 3.2 Asynchronous identity

Each manager registration owns a lifetime token. Its producers hold a source token, and queued events are checked again when consumed. Removing or replacing the registration invalidates old metadata/deletion events; browser telemetry batches carry the same source identity. Native manager protocols are unchanged: a host-owned forwarder adds the application source envelope.

File preview/fetch results keep their existing request and browser-generation checks. Settings and checkpoint results carry revisions. These identities have distinct purposes; they are not a global transaction sequence and do not grant torrent or peer authority.

### 3.3 Settings and persistence

`client_configs` remains the canonical effective app configuration. `SettingsApplication` records the requested configuration, pending state and application error. Port-rebind rollback remains in the native executor. Successful queueing is not manager completion or durable storage completion, and a settings result cannot hide an unrelated checkpoint failure.

A checkpoint contains a revision and the existing settings/RSS/history snapshots. Preparation receives an observed timestamp; it does not perform physical I/O. Completion advances only the acknowledged revision, ignores unissued/stale completions, and cannot clear a newer failed save. Existing persistence formats are retained.

Browser checkpoint preparation preserves an explicit set of catalog entries awaiting manager restoration, initialized during hydration and retired when a projection arrives or the torrent is removed. A missing runtime alone never implies pending restoration. Successful browser removal clears the catalog entry; failed cleanup carries a paused, unvalidated recovery entry captured before the display is removed, including torrents that have not reached their first checkpoint. A browser backend must serialize and durably commit the snapshot before calling `complete_checkpoint`; the memory backend does not fulfill that production durability requirement.

## 4. Deliberate behavior corrections

These changes are separate from the mechanical module moves:

- Failed payload deletion releases the terminated runtime but retains a visible cleanup error. Its catalog entry is retained or reconstructed paused and unvalidated, so restart does not silently forget the payload or assert that partially deleted data is valid.
- Stale manager events cannot remove or overwrite a replacement registration of the same torrent.
- Shutdown counts distinct manager acknowledgements; duplicates and unknown hashes cannot finish another manager's shutdown.
- Native app-created background and manager tasks have explicit owners. Teardown drains them before stopping dependent services, and does not require a terminal renderer. Cleanup also runs when the event loop returns a rendering error.
- Persistence flush continues consuming acknowledgements after the normal app loop stops, avoiding a writer blocked on its result channel.
- Browser pause/delete/configuration requests report failed delivery and avoid updating their requested projection when the command was not queued.
- Browser shutdown preserves the catalog and requires both manager termination and checkpoint completion before reporting completion. `drain_manager_messages()` retries shutdown commands after transient queue backpressure; acknowledged commands are not resent. Closed channels without a terminal acknowledgement produce a failed teardown, exposed through `shutdown_failed()`. Queued acknowledgements are consumed before checking for closed channels, including those deferred by a full effects queue.
- Browser manager registration returns `Result` and rejects new or replacement registrations once shutdown starts. Callers must register successfully before starting or publishing a runtime; the demo composition follows this contract.

The cursor regression fixture now uses a deterministic virtual ancestor path. Filesystem fuzzy matching intentionally includes ancestors; a random temporary directory could accidentally match the test query. Production search behavior is unchanged.

## 5. Validation

The final native library run passed **2,221 tests**, with one existing ignored test. The real Wasm suite under Node passed **109 contracts**. Strict native Clippy passed for all targets with all features and with no default features. Strict all-target Wasm Clippy also passed. Formatting, whitespace and the frozen `state.rs` comparison passed.

The native suite includes the repository's networking-construction architecture guard. Focused contracts additionally cover stale callbacks, checkpoint ordering, cleanup recovery before and after the first checkpoint, settings-result isolation, explicit browser restoration, renderer-independent shutdown, shutdown command backpressure, closed-channel acknowledgement ordering, rejected late registration and retryable lifecycle event delivery.

This does not substitute for browser-worker/storage lifecycle testing, Chromium-based product testing, public-swarm interoperability or media/export acceptance. Those belong to the corresponding production workstreams.
