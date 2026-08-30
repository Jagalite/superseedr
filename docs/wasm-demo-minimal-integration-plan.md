# WASM Demo Minimal Integration Plan

Status: proposed; this document defines the first public browser-demo release only.

Reviewed baseline: `develop` at `5457582c546222605164d2b254e8e44e030e4cc1`.

## Goal

Release a completely client-side Superseedr browser demo that renders and interacts through the
exact production TUI source while making the smallest practical change to the original native
codebase.

The browser demo may use a fully mocked application runtime. It does not need to reuse the native
`App` service runtime, `TorrentManager`, networking, disk, persistence, DHT, or RSS workers in its
first release.

The code-sharing requirement is narrower and explicit:

- Reuse the production `AppState`, settings, display models, and telemetry model types.
- Reuse the production `tui::view::draw` entrypoint and every production screen renderer.
- Reuse the production `tui::events::handle_event` path and screen reducers.
- Reuse `AppCommand` and the reducer-to-service command boundary.
- Do not copy, translate, or recreate TUI screens in the web application.

The native runtime must retain its existing behavior.

## Non-goals for the first release

- Refactoring the native `App` into a new controller/runtime architecture.
- Compiling or running the native `TorrentManager` in the browser.
- Adding a demo peer worker.
- Adding WebRTC or WebTorrent.
- Adding browser storage, OPFS, or File System Access support.
- Making native DHT, PEX, trackers, sockets, RSS, watch folders, or persistence run in WASM.
- Creating a second repository.
- Making the demo production-quality or claiming that it performs real transfers.

These are follow-up projects and must not block the static demo release.

## Preliminary standalone merge: authoritative library root

This native-only refactor may be merged and retained independently of the browser demo. It adds no
WASM code, browser dependencies, mocks, target-selected application, or service suppression flags.
Its purpose is to make `src/lib.rs` the single production module root before any browser seam is
introduced.

The reviewed baseline currently has two Rust crate roots:

- `src/main.rs` is the production crate root. It declares the complete application module graph and
  also contains native startup, CLI helpers, terminal ownership, and their tests.
- `src/lib.rs` is a deliberately reduced fuzzing facade. It declares only the parser/reducer modules
  needed by `superseedr::fuzzing`, including a small DHT service stand-in.

The standalone merge should use this arrangement:

```text
src/lib.rs
  -> authoritative private production module graph
  -> public superseedr::fuzzing facade
  -> private native_entrypoint module
  -> narrow public run_native() entrypoint

src/native_entrypoint.rs
  -> existing native startup and CLI implementation
  -> existing main.rs tests moved with that implementation

src/main.rs
  -> #[tokio::main]
  -> superseedr::run_native().await
```

Rules for this merge:

- Move the existing startup/CLI implementation and its tests as one unit; do not rewrite thousands
  of internal references as public `superseedr::...` imports.
- Keep `#[tokio::main]` in the binary so the native Tokio runtime construction remains unchanged.
- Keep application modules private or `pub(crate)` by default. Do not make the entire application
  graph public merely so the binary can reach it.
- Remove the reduced fuzz facade's crate-wide `#![allow(dead_code, unused_imports)]` when
  `src/lib.rs` becomes the production root. Do not suppress these warnings across the production
  graph; apply any genuinely necessary allowance at the narrowest item or module instead.
- Preserve the existing `superseedr::fuzzing` public facade and all fuzz target names.
- Preserve the current `superseedr` tracing target for logging emitted by the moved entrypoint.
  Add explicit tracing targets where the module move would otherwise change it to
  `superseedr::native_entrypoint`.
- Do not change `App`, TUI, torrent, networking, disk, persistence, DHT, or terminal behavior as
  part of this merge.
- Do not add WASM target selection yet. Later browser work should build on the library root in a
  separate merge.

Expected impact:

- Native runtime regression risk is low when the implementation moves without behavioral edits.
- Existing binary unit tests must move with the native entrypoint. Leaving them in the thin binary
  would prevent them from accessing private library internals and would change `cfg(test)` behavior.
- Fuzz builds may compile more of the production module graph than the current reduced library. This
  is a build-time cost, not a native runtime change; measure it before adding a special fuzz-only
  module graph.
- Default tracing targets emitted by moved entrypoint functions would acquire a module suffix.
  Preserve the existing `superseedr` target explicitly so log output and target-based filtering do
  not change as part of this refactor.
- Rust compilation proves module privacy, type identity, and feature compatibility. It does not by
  itself prove startup ordering, terminal cleanup, CLI output, logging metadata, or filesystem
  behavior, so the validation gates below remain required.

## Core decision: target-selected `App`

The production TUI event code currently accepts the concrete `crate::app::App` type. A separate
browser crate cannot provide an unrelated `MockApp` without either copying the event reducers or
refactoring their entire interface.

For this POC, `crate::app::App` will therefore be selected by compilation target:

```text
native target
  crate::app::App -> existing production App and services

wasm32 target
  crate::app::App -> web-poc-owned WebDemoApp
```

Conceptually, the shared application module exposes the selected name:

```rust
#[cfg(not(target_arch = "wasm32"))]
pub use native_runtime::App;

#[cfg(target_arch = "wasm32")]
pub use web_demo_app::WebDemoApp as App;
```

The exact module arrangement can differ if Rust path or Cargo packaging constraints require it.
The architectural rule is that the alternate implementation is owned by `web-poc`, selected at one
obvious boundary, and never used by a native build.

This is intentionally not a claim that the demo reuses the complete native `App`. It reuses the
production state, TUI, reducers, and command contracts with a browser-specific application shell.

## Proposed repository layout

Keep one repository with separately built native and web applications:

```text
superseedr/
  src/
    app.rs                         shared models plus native App selection
    lib.rs                         library surface and target module selection
    native_entrypoint.rs           native startup, CLI implementation and related tests
    main.rs                        thin Tokio binary calling superseedr::run_native
    tui/                           unchanged production TUI source
    ...                            unchanged native services
  web-poc/
    index.html
    package.json
    src/
      main.ts                      Ghostty Web and browser lifecycle
      style.css
    wasm/
      Cargo.toml
      src/
        lib.rs                     WASM exports and BrowserDemo harness
        app.rs                     alternate fully mocked WebDemoApp
        ansi_backend.rs            Ratatui Backend -> ANSI
        mocks/                     mock service fulfillment and fixtures
        shims/                     browser-only compatibility modules if needed
  docs/
```

`web-poc` remains outside the default native Cargo workspace/build. Running normal native Cargo
commands must not require npm, Vite, wasm-bindgen, Ghostty Web, or browser packages.

## Alternate application responsibilities

`WebDemoApp` should contain only the surface required by production event reducers:

- Shared `AppState`.
- Shared `Settings`.
- An `AppCommand` sender and receiver.
- Any small command maps or shutdown sender directly referenced by existing reducers.
- Small application methods invoked by those reducers, implemented with mock/browser semantics.

It must not contain:

- Native listeners or `PeerConnection` values.
- A `TorrentManager` registry.
- Native DHT, peer manager, resource manager, token buckets, or tuning controller.
- Filesystem watchers or persistence tasks.
- Native terminal ownership.
- Demo lifecycle simulation or fictional data that belongs in the mock service.

The distinction is:

```text
WebDemoApp
  -> shared state and reducer-facing command surface

Browser mock service
  -> fictional torrents, peers, telemetry and command fulfillment

BrowserDemo
  -> frame clock, rendering, input conversion and WASM exports
```

Keeping those responsibilities separate prevents the alternate `App` from becoming another
monolith.

## Browser command flow

The first release retains the production command boundary:

```text
Ghostty Web key or paste
  -> BrowserDemo converts input to the supported terminal event
  -> production tui::events::handle_event(event, &mut WebDemoApp)
  -> production reducer mutates shared AppState and/or sends AppCommand
  -> browser mock service drains AppCommand RX
  -> mock service fulfills the request in memory
  -> shared AppState changes
  -> production tui::view::draw renders the next frame
```

Pause, resume, delete, file selection, searches, navigation, configuration editing, and other
interactions must reach the same reducer and `AppCommand` path used by native Superseedr wherever
the existing reducer permits it.

## Browser render flow

```text
shared AppState and Settings
  -> production tui::view::draw
  -> Ratatui frame and retained cell diff
  -> browser ANSI backend
  -> ANSI string across the WASM boundary
  -> Ghostty Web Terminal.write
  -> browser terminal surface
```

The web application may supply a different Ratatui backend. It may not supply different screen
renderers.

## Permitted changes to original Superseedr source

Every original-source change must fit one of the following categories.

### 1. Library exposure

- Expose the existing application models, configuration, TUI, theme, telemetry models, and event
  dispatcher needed by the browser build.
- Preserve existing native module paths through re-exports where possible.

### 2. One target-selection boundary

- Select the production `App` for native targets.
- Select `WebDemoApp` for `wasm32`.
- Select real native-only modules or browser compatibility modules at the library root.
- Avoid scattering target decisions throughout screen renderers.

### 3. Target-specific dependencies

- Keep native Crossterm/Ratatui, full Tokio, Notify, Socket2, Sysinfo, Rlimit, and other native
  dependencies on non-WASM targets.
- Use only the browser-compatible dependency features required to compile shared state and TUI code
  for `wasm32`.
- Preserve the regular public-build DHT/PEX feature identity while replacing native service modules
  at the WASM target boundary.

### 4. Platform-safe shared helpers

- Use browser-safe clocks where production TUI animation calls time directly.
- Put environment/path/filesystem availability checks behind narrow helper functions.
- Return an explicit unsupported/unavailable result in the browser rather than attempting native
  I/O.
- Keep native helper behavior unchanged.

### 5. Minimal reducer compatibility

- Make only the smallest changes required for a reducer to operate against both selected `App`
  implementations.
- Prefer a method already meaningful to both applications over an inline `cfg` in a screen.
- Do not add fictional demo behavior to production reducers.

Any change outside these categories requires separate justification and is not part of the first
release.

## Code that must remain web-owned

- `WebDemoApp` and its reducer-facing mock methods.
- Fictional torrents, peers, file trees, RSS entries, journal entries, and names.
- Mock torrent lifecycle and telemetry generation.
- Mock `AppCommand` consumption.
- Browser-only networking type shims.
- The ANSI Ratatui backend.
- Ghostty Web initialization and terminal write serialization.
- `requestAnimationFrame`, 60 FPS timing, resize, zoom, and page lifecycle handling.
- TypeScript, CSS, Vite configuration, npm dependencies, and static-site assets.
- Future demo-only diagnostics and browser test hooks.

No real copyrighted titles or brands may appear in fixtures, tests, screenshots, or mock UI text.

## First-release implementation plan

### Preliminary phase: merge the native library root independently

1. Make `src/lib.rs` the authoritative production module root.
2. Move the current native startup/CLI implementation and its tests into
   `src/native_entrypoint.rs` without behavioral edits.
3. Reduce `src/main.rs` to the Tokio wrapper that calls `superseedr::run_native().await`.
4. Preserve `superseedr::fuzzing` and compile every existing fuzz target; listing target names is
   not sufficient validation.
5. Run the native feature-matrix, CLI, and PTY smoke checks described below.
6. Merge this refactor without any WASM, browser, mock, or target-selection code.

Exit condition: native Superseedr runs through the library crate, all native and fuzz validation
passes, and the commit remains useful even if the browser demo is postponed indefinitely.

### Phase 0: freeze the acceptance boundary

1. Record the clean native baseline from `main`.
2. Record the current POC's shared-source and browser-owned diffs separately.
3. Classify every original-source hunk using the permitted-change categories above.
4. Remove or relocate any browser fixture, mock service, or browser lifecycle logic found in
   original production modules.
5. Treat exact renderer and reducer reuse as a regression gate.

Exit condition: every surviving original-source change is necessary to expose, select, or safely
compile shared production code.

### Phase 1: isolate the alternate application

1. Move the reduced WASM `App` definition and mock-only methods into
   `web-poc/wasm/src/app.rs`.
2. Name the concrete type `WebDemoApp`; alias it to `crate::app::App` only under `wasm32` so existing
   reducers remain unchanged.
3. Leave the native `App` fields, constructor, methods, and run loop behavior unchanged.
4. Keep shared application models in the original crate rather than duplicating them in the web
   crate.
5. Move all lifecycle simulation and fixture construction out of `WebDemoApp` into a browser mock
   service module.

Exit condition: there is one production `App`, one browser `WebDemoApp`, and no duplicated TUI or
state-model source.

### Phase 2: consolidate target boundaries

1. Keep target-specific dependency selection in `Cargo.toml`.
2. Keep module selection in `src/lib.rs` and the application module root.
3. Relocate browser networking shims under `web-poc` when path/module selection permits it.
4. Replace repeated screen-level target gates with narrow platform capability helpers where that
   reduces original-source churn.
5. Confirm the regular DHT/PEX build identity remains visible while no native socket service is
   linked into the WASM demo.

Exit condition: target-specific logic is concentrated at dependency, module, and capability
boundaries rather than spread through TUI code.

### Phase 3: retain production reducer behavior

1. Inventory every `App` field and method accessed by `tui::events` and individual screen reducers.
2. Implement only that surface on `WebDemoApp`.
3. Ensure production reducers continue to emit the existing `AppCommand` variants.
4. Fulfill commands in the browser mock service without bypassing reducer confirmation or selection
   rules.
5. Add tests proving paste, pause/resume, confirmed delete, configuration, file-browser, RSS,
   journal, peer-management, and torrent-management flows reach the shared reducer boundary.

Exit condition: browser interactions do not mutate state through a second browser-specific UI
logic path.

### Phase 4: qualify the demo

1. Render all production `AppMode` variants through `tui::view::draw`.
2. Populate every screen using fictional deterministic data.
3. Verify normal-screen graphs, heatmaps, DHT, disk, peer, file, and torrent visualizations evolve
   coherently.
4. Verify resizing and browser zoom in both shrinking and growing directions.
5. Serialize Ghostty Web writes and prevent animation-frame backlog.
6. Measure sustained frame behavior and cap elapsed-time jumps after background-tab pauses.
7. Display an unambiguous simulated-demo notice outside the TUI.

Exit condition: the static demo behaves coherently without implying real network or disk activity.

### Phase 5: release packaging

1. Build Rust to `wasm32-unknown-unknown`.
2. Generate browser bindings with a pinned compatible `wasm-bindgen` CLI.
3. Type-check and bundle the static site.
4. Add separate CI jobs for native regression and the optional web build.
5. Publish only static assets; no application server is required.
6. Document supported browsers, controls, simulated limitations, bundle size, and local build steps.

Exit condition: the demo is reproducibly deployable from the repository without changing the
native release workflow.

## Validation gates

### Original native application

```text
cargo fmt --all -- --check
cargo test --locked
cargo test --all-targets --all-features --locked
cargo test --all-targets --no-default-features --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
git diff --check
```

Native tests that require localhost TCP/UDP must run with the needed network permissions. A sandbox
denial is not evidence that the source failed.

For the standalone library-root merge, also:

- Compile all ten existing `cargo-fuzz` targets and confirm the `superseedr::fuzzing` facade and
  target names are unchanged. `cargo fuzz list` may verify the inventory, but it does not satisfy
  the compile gate by itself.
- Smoke-test CLI help, configuration inspection, and representative command handling.
- Launch and exit the real application through a PTY to verify raw mode, alternate-screen entry,
  panic/exit cleanup, and the production TUI startup path.
- Confirm `src/main.rs` declares no duplicate production modules and contains no native application
  implementation beyond the Tokio wrapper.

### Browser application

```text
cargo check --manifest-path web-poc/wasm/Cargo.toml --target wasm32-unknown-unknown
cargo clippy --manifest-path web-poc/wasm/Cargo.toml --target wasm32-unknown-unknown --all-targets -- -D warnings
cd web-poc && npm run build
```

Live browser checks must cover all screens, representative reducer interactions, paste, pause,
resume, deletion confirmation, resize, zoom, and a sustained animation interval.

## Minimal-change review checklist

Before merging the first release, review the diff using these questions:

- Does any original TUI renderer contain browser-specific presentation logic?
- Does any original module contain fictional fixture data or lifecycle simulation?
- Was native code behavior changed when a target gate or re-export would have sufficed?
- Is every browser-only dependency excluded from the native target?
- Is the alternate `App` implementation clearly owned by `web-poc`?
- Does the browser still call the production top-level draw and event functions directly?
- Could a native-only contributor build and test without installing browser tooling?
- Are target gates concentrated enough that removing `web-poc` later would be understandable?
- Does the UI clearly disclose that torrent activity is simulated?
- Did the preliminary native merge keep internal modules private rather than widening the public
  API?
- Did native entrypoint tests move with the implementation so their `cfg(test)` behavior is
  preserved?
- Does the fuzz facade still compile with the same public paths and target names?

## Known tradeoff

The target-selected alternate `App` is a deliberate POC compromise. It avoids a broad native
refactor before release but creates a reducer-facing surface that must stay compatible with the
production `App`.

Control that risk by:

- Keeping `WebDemoApp` small.
- Testing every shared reducer used by the demo.
- Keeping mocks outside `WebDemoApp`.
- Treating compilation failures after native `App` changes as useful contract feedback.
- Avoiding promises that the complete native runtime is shared.

Do not solve this temporary compatibility cost with a large controller/runtime refactor before the
first demo release.

## Deferred roadmap

After the static demo is released and its constraints are understood:

1. Reassess whether the alternate `App` surface is stable enough to formalize as a shared
   controller.
2. Consider a deterministic demo peer worker that exercises the real `TorrentManager`.
3. Generalize peer identity and the manager-to-peer worker contract.
4. Add a WebRTC peer implementation while retaining the existing torrent state machine.
5. Add browser-compatible storage separately.
6. Replace simulated lifecycle data incrementally rather than rewriting the TUI.

Those decisions must be informed by the released demo. They are not acceptance requirements for
this plan.

## Completion criteria

The first-release integration is complete when:

- The browser directly renders every production screen from shared TUI source.
- Browser input reaches production event reducers.
- The fully mocked alternate application lives under `web-poc` and starts no native services.
- Original-source changes are limited to library exposure, target selection, dependency selection,
  platform-safe helpers, and minimal reducer compatibility.
- Native behavior and native release tooling remain unchanged.
- The demo builds to static client-side assets and clearly labels all activity as simulated.
- No App refactor, demo worker, WebTorrent, or browser storage implementation has been pulled into
  the first release.
