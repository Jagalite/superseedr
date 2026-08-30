# Superseedr Browser WASM Demo Integration Plan

Status: active; this document defines the first public browser-demo release only.

Reviewed baseline: `develop` at `ca1bb3cb`.

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
- Turning the simulated demo into a production browser torrent client or claiming that it performs
  real transfers.

These are follow-up projects and must not block the static demo release.

## Completed prerequisite: authoritative library root

The native-only authoritative-library-root refactor was merged in PR #335 at `ca1bb3cb`. It added
no WASM code, browser dependencies, mocks, target-selected application, or service suppression
flags. The merged arrangement is:

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

The merge preserved these guarantees:

- The existing startup/CLI implementation and its tests moved as one unit without widening the
  internal module graph into a public API.
- `#[tokio::main]` remains in the binary, preserving native Tokio runtime construction.
- Application modules remain private or `pub(crate)` by default; the binary does not require the
  complete graph to become public.
- The reduced fuzz facade's crate-wide `#![allow(dead_code, unused_imports)]` was removed rather
  than applied to the production graph.
- The existing `superseedr::fuzzing` public facade and all fuzz target names were preserved.
- The existing `superseedr` tracing target was preserved for logging emitted by the moved
  entrypoint.
- `App`, TUI, torrent, networking, disk, persistence, DHT, and terminal behavior were not changed.
- WASM target selection was deliberately deferred to the milestones below.

Continuing considerations:

- Fuzz builds now compile more of the production module graph than the former reduced library. This
  is a build-time cost, not a native runtime change; measure it before adding a special fuzz-only
  graph.
- Rust compilation proves module privacy, type identity, and feature compatibility. It does not by
  itself prove startup ordering, terminal cleanup, CLI output, logging metadata, or filesystem
  behavior, so the validation gates below remain required.

## Core decision: target-selected `App`

The production TUI event code currently accepts the concrete `crate::app::App` type. A separate
browser crate cannot provide an unrelated `MockApp` without either copying the event reducers or
refactoring their entire interface.

For this browser demo, `crate::app::App` will therefore be selected by compilation target:

```text
native target
  crate::app::App -> existing production App and services

wasm32 target
  crate::app::App -> root-owned reduced WebApp compatibility type
```

Conceptually, the shared application module exposes the selected name:

```rust
#[cfg(not(target_arch = "wasm32"))]
pub use native_runtime::App;

#[cfg(target_arch = "wasm32")]
pub use web_app::WebApp as App;
```

The compile-facing `WebApp` must live in the published root crate, preferably in
`src/app/web.rs`, and be selected at one obvious target boundary. Production reducers that name
`crate::app::App` compile inside the root crate, while `web/wasm` depends on that root crate.
Defining `WebApp` in `web/wasm` would require a dependency cycle or a source path that is omitted
from the packaged root crate. It must never be selected or compiled into a native build.

The browser behavior that drives `WebApp` remains owned by `web`: deterministic fixtures, lifecycle
simulation, command consumption, rendering cadence, input conversion, and browser integration do
not belong in the root compatibility type.

This is intentionally not a claim that the demo reuses the complete native `App`. It reuses the
production state, TUI, reducers, and command contracts with a WASM-only compatibility shell.

## Proposed repository layout

Keep one repository with separately built native and web applications:

```text
superseedr/
  src/
    app.rs                         shared models, native App and target-selected App export
    app/
      web.rs                       reduced WebApp compatibility type (wasm32 only)
    lib.rs                         library surface and target module selection
    native_entrypoint.rs           native startup, CLI implementation and related tests
    main.rs                        thin Tokio binary calling superseedr::run_native
    terminal_event.rs              target-selected terminal input data facade
    wasm_compat/                   data-only compatibility modules required by the root crate
    tui/                           unchanged production TUI source
    ...                            unchanged native services
  web/
    index.html
    package.json
    src/
      main.ts                      Ghostty Web and browser lifecycle
      style.css
    wasm/
      Cargo.toml                   package: superseedr-web
      src/
        lib.rs                     WASM exports and BrowserDemo harness
        ansi_backend.rs            Ratatui Backend -> ANSI
        mocks/                     mock command fulfillment, lifecycle and fixtures
        services/                  browser command consumers and future real backends
  docs/
```

`web` remains outside the default native Cargo workspace/build. Running normal native Cargo
commands must not require npm, Vite, wasm-bindgen, Ghostty Web, or browser packages.

### Ratatui and terminal-event boundary

Ratatui must remain the same upstream crates.io package and version on native and WASM. Do not use
a local crate that impersonates the `ratatui` package, place a browser Ratatui wrapper in the native
dependency graph, or rename a target-only path dependency to `ratatui-wasm`.

Those arrangements conflict with the publishable root crate:

- Cargo rejects one dependency name whose native and WASM targets use different sources.
- When packaging removes a path, native `ratatui` and a renamed `ratatui-wasm` dependency become
  the same registry package under different names, and package verification rejects the duplicate.
- A global path wrapper can package successfully, but makes normal native builds compile through
  browser compatibility code and violates the isolation boundary above.

The actual platform seam is terminal input, not rendering. Production modules currently reach
terminal event data through `ratatui::crossterm::event`; replace those imports with a small shared
`crate::terminal_event` facade:

```text
native
  crate::terminal_event -> re-export the real crossterm::event types

wasm32
  crate::terminal_event -> data-only compatible Event, KeyEvent, KeyCode, modifier, mouse,
                           paste, and resize types; no polling or terminal I/O
```

The root crate then depends on upstream Ratatui exactly once, with common render-only features for
all targets and the Crossterm backend feature added only for non-WASM targets. `web/wasm` also
depends directly on that exact upstream Ratatui version, so Cargo unifies the renderer types. The
local Ratatui and Crossterm shim crates are removed. Browser code remains responsible for converting
DOM/Ghostty input into the shared event data, while production reducers continue to consume the
same target-selected event surface.

`terminal_event` is shared platform compatibility code and belongs under `src`, not under `web`.
It contains no browser lifecycle, mock behavior, service implementation, or I/O. Keeping it in the
published root package also allows the packaged source to compile for WASM without reaching into a
directory omitted from the crate archive.

### Permanent browser-client naming

`web` is the permanent browser-client root, not a temporary demo directory. The product remains
Superseedr Web as its torrent backend evolves:

```text
WebApp
  -> DemoTorrentService       simulated first-release activity
  -> WebTorrentBackend        future WebRTC/WebTorrent activity
```

Adding WebTorrent must not rename the directory, crate, or product to `web-torrent`. WebTorrent is
a backend capability of Superseedr Web. The initial simulated service and future real backend may
coexist as explicitly labeled modes.

## Reduced `WebApp` compatibility type

`WebApp` is shared platform compatibility code, not the browser mock service. It should contain
only the surface required to compile and run production event reducers:

- Shared `AppState`.
- Shared `Settings`.
- An `AppCommand` sender and receiver.
- Any small command maps or shutdown sender directly referenced by existing reducers.
- Small application methods invoked by those reducers, implemented as shared state-only behavior
  or forwarding through existing `AppCommand` variants.

It must not contain:

- Native listeners or `PeerConnection` values.
- A `TorrentManager` registry.
- Native DHT, peer manager, resource manager, token buckets, or tuning controller.
- Filesystem watchers or persistence tasks.
- Native terminal ownership.
- Demo lifecycle simulation or fictional data that belongs in the mock service.
- Browser timers, DOM/Ghostty integration, or browser networking behavior.
- Direct fulfillment of service effects that the native application performs through commands.

The distinction is:

```text
WebApp
  -> shared state and reducer-facing command surface

Browser mock service
  -> fictional torrents, peers, telemetry and command fulfillment

BrowserDemo
  -> frame clock, rendering, input conversion and WASM exports
```

The root-owned compatibility type keeps the production reducer call graph intact. Keeping browser
behavior under `web` prevents the alternate `App` from becoming another application monolith.

Because `web/wasm` is a downstream crate, the root must expose a narrow WASM-gated integration
facade for constructing `WebApp`, dispatching terminal-event data, borrowing the presentation state,
draining commands, and applying mock-service updates. The exact methods should be compiler-driven;
do not make the complete `app` or `tui` module graph public merely to connect the browser harness.

## Browser command flow

The first release retains the production command boundary:

```text
Ghostty Web key or paste
  -> BrowserDemo converts input to the supported terminal event
  -> production tui::events::handle_event(event, &mut WebApp)
  -> production reducer mutates shared AppState and/or sends AppCommand
  -> browser mock service drains AppCommand RX
  -> mock service fulfills the request in memory
  -> shared AppState changes
  -> production tui::view::draw renders the next frame
```

Use the existing `AppCommand` channel contract if its synchronization-only features compile for
WASM. The browser frame loop may drain the receiver non-blockingly; this does not require starting
a Tokio runtime in the browser. If the existing channel type is not WASM-compatible, record that as
a compiler-proven blocker and introduce the smallest target-selected queue behind the same
`AppCommand` contract rather than adding a second reducer path.

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

## Compatibility testing strategy

The existing native test suite is the baseline, but compilation and existing unit tests alone do
not define the future TUI compatibility contract. Each compatibility seam must be characterized on
the native path before it is changed, then exercised through both the native and WASM-selected
paths after the seam exists.

Tests must call production entrypoints rather than copied reducers or browser-only shortcuts. The
core contract is:

```text
terminal event data
  -> production tui::events::handle_event
  -> production screen reducer
  -> target-selected App
  -> exact AppCommand payload and shared AppState transition
  -> production tui::view::draw
```

Use these layers:

1. Keep the existing pure reducer tests. They are the target-independent specification for state
   transitions, selection rules, confirmation requirements, and emitted effects.
2. Before changing an interaction seam, add native characterization tests through the highest
   practical production entrypoint. These tests establish current behavior rather than behavior
   invented for the browser.
3. Add a native harness and a WASM `WebApp` harness for the same scenario and expected outcome.
   Construction and command-drain adapters may be target-specific, but event input, reducer path,
   expected `AppCommand`, and expected state transition must match.
4. Execute WASM tests under a real WASM test runner. `cargo check` and Clippy prove compilation but
   do not execute target-selected code.
5. Render every supported `AppMode` with the production draw entrypoint at representative normal,
   narrow, short, and minimum-safe terminal sizes. Prefer semantic cell/text assertions and
   no-panic/layout invariants over brittle full ANSI snapshots.
6. Keep CLI, package, native dependency-feature, and real PTY startup/cleanup checks as separate
   regression gates. Unit tests cannot prove process startup ordering, terminal ownership, logging,
   or packaged-source completeness.

The initial event contract suite should cover at least:

- Explicit paste and paste-burst magnet input produce the expected add command.
- Pause and resume target the selected info hash, update the expected control state, and preserve
  command ordering.
- A delete request opens the production confirmation mode and emits no delete command early.
- Delete cancellation returns to the normal mode without a delete command.
- Confirmed deletion preserves the selected hash and delete-files flag, marks the torrent deleting,
  and emits exactly one delete request.
- Missing or stale selections do not act on another torrent.
- Command draining is nonblocking and preserves FIFO order.
- Key modifiers and press, repeat, release, paste, resize, and later mouse data retain the semantics
  expected by production reducers.

Do not introduce a broad production trait or controller abstraction solely to share test setup. A
small test harness around the real target-selected types is sufficient.

## Permitted changes to original Superseedr source

Every original-source change must fit one of the following categories.

### 1. Library exposure

- Expose the existing application models, configuration, TUI, theme, telemetry models, and event
  dispatcher needed by the browser build.
- Preserve existing native module paths through re-exports where possible.

### 2. One target-selection boundary

- Select the production `App` for native targets.
- Select the root-owned reduced `WebApp` compatibility type for `wasm32`.
- Select real native-only modules or root-owned, data-only WASM compatibility modules at the
  library root.
- Avoid scattering target decisions throughout screen renderers.
- Do not reach from the published root crate into `web/wasm` with a filesystem `path` attribute.

### 3. Target-specific dependencies

- Use the same upstream Ratatui package and exact version on native and WASM. Enable its renderer
  features for both targets and its Crossterm backend feature only on native.
- Keep native Crossterm, full Tokio, Notify, Socket2, Sysinfo, Rlimit, and other native dependencies
  on non-WASM targets.
- Use only the browser-compatible dependency features required to compile shared state and TUI code
  for `wasm32`.
- Keep terminal event compatibility behind `crate::terminal_event`; do not expose it by replacing
  the Ratatui or Crossterm packages with browser path crates.
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

- Fictional torrents, peers, file trees, RSS entries, journal entries, and names.
- Mock torrent lifecycle and telemetry generation.
- Mock `AppCommand` consumption and effect fulfillment.
- Browser service adapters that update the root-owned `WebApp` state.
- Browser implementations of networking or service capabilities.
- The ANSI Ratatui backend.
- Ghostty Web initialization and terminal write serialization.
- `requestAnimationFrame`, 60 FPS timing, resize, zoom, and page lifecycle handling.
- TypeScript, CSS, Vite configuration, npm dependencies, and static-site assets.
- Future demo-only diagnostics and browser test hooks.

No real copyrighted titles or brands may appear in fixtures, tests, screenshots, or mock UI text.

Conversely, any compatibility type or module compiled as part of the root crate must live under
`src`, be target-gated, and contain no mock data or browser I/O. This includes `WebApp`,
`terminal_event`, and any data-only networking types needed to compile shared models. The root crate
must not import source from the nested `web/wasm` package with `#[path]`.

## First-release implementation plan

### Completed prerequisite: authoritative library root

PR #335 made `src/lib.rs` authoritative, moved native startup into
`src/native_entrypoint.rs`, and reduced `src/main.rs` to a Tokio wrapper. No further preliminary
native refactor is required before Milestone 1.

### Milestone 1: compile the exact production renderer for WASM (complete)

1. Start from the current merged `develop` tip and record its exact commit.
2. Treat the legacy browser experiment only as reference material; do not merge its
   original-source changes wholesale.
3. Create `web/wasm` with package name `superseedr-web`, outside the default native workspace.
4. Inventory the exact shared types and modules required by `tui::view::draw`.
5. Expose a narrow presentation facade for the production draw entrypoint and required display
   models; do not make the complete module graph public.
6. Add the shared `terminal_event` facade, mechanically route production event imports through it,
   and keep the native side as a re-export of the real Crossterm event types.
7. Depend directly on the same exact upstream Ratatui version from the root and browser crates;
   remove local Ratatui/Crossterm package shims and keep Crossterm backend features native-only.
8. Add only the remaining target-specific dependency and module selection proven necessary by a
   `wasm32-unknown-unknown` compile.
9. Add a browser-owned ANSI Ratatui backend and deterministic fictional display state.
10. Export a WASM function that invokes the exact production draw function and returns a non-empty
   ANSI frame.
11. Classify every original-source change using the permitted categories above and record remaining
   compiler blockers.

Milestone 1 does not include `WebApp`, production event reducers, complete mocks, Ghostty Web,
WebTorrent, browser storage, or all-screen qualification.

Exit condition: `web/wasm` compiles for `wasm32-unknown-unknown`, a WASM export returns ANSI emitted
by the production renderer, no TUI renderer is copied, and the native feature matrix still passes.

### Milestone 2: add `WebApp` and production reducer input (complete)

The first reducer scope is deliberately narrow: paste/add a magnet, pause/resume a selected
torrent, request deletion, cancel deletion, and confirm deletion. Broader screens and interactions
remain Milestone 4 work.

1. Before changing `App` selection, add native characterization tests for every scoped interaction
   through the production event dispatcher. Record the exact commands and state transitions.
2. Trace the production call graph for those interactions from `tui::events::handle_event` through
   the relevant screen reducers.
3. Inventory every `App` field and method reached by that call graph. Classify each dependency as
   shared state, a state-only method, an existing `AppCommand` effect, or native-only behavior.
4. Add `src/app/web.rs` to the published root crate. Define the smallest `WebApp` compatibility
   type proven necessary by the inventory and alias it to `crate::app::App` only under `wasm32`.
5. Implement reducer-facing `WebApp` methods only as shared state changes or sends of existing
   `AppCommand` variants. Do not fulfill service effects or generate demo data in this type.
6. Leave the native `App` fields, construction, methods, services, and run loop unchanged. The
   native target must continue selecting the existing concrete type.
7. Compile the production top-level event dispatcher and only the screen reducers required by the
   initial interaction scope. Do not copy reducers or introduce a browser-specific event path.
8. Under `web/wasm`, add the minimal command consumer needed to fulfill the scoped commands in
   memory and update shared state using deterministic fictional data.
9. Preserve existing `AppCommand` variants. Any proposed browser-only variant must be separately
   justified in this document before implementation.
10. Run the same scoped scenarios through the WASM-selected `WebApp` harness. Assert the exact
    command payloads, FIFO ordering, state transitions, stale-selection behavior, and nonblocking
    drain behavior. Explicitly prove that deletion cannot bypass the production confirmation step.
11. Add WASM terminal-event tests for key modifiers, press/repeat/release, paste, and resize data
    used by the scoped reducers.
12. Execute the WASM contract tests with the configured WASM test runner; a successful target check
    is not an execution result.
13. Run the complete native feature, Clippy, package, CLI, and PTY regression gates. Package
    verification must prove that `WebApp` source is included in the root crate archive without
    reaching into `web`.

Exit condition: the native characterization scenarios still pass; the same behavioral contracts
pass through the production event dispatcher with the root-owned `WebApp`; browser-owned mocks
fulfill commands in memory; confirmation and selection semantics are preserved; the packaged root
crate builds for WASM from its archive; and native behavior remains unchanged.

#### Milestone 2 implementation and validation record

Completed on 2026-08-30 from the committed Milestone 1 baseline `21480c7a`.

Verified implementation boundaries:

- Six native characterization tests were added and passed through
  `tui::events::handle_event` before changing the `App` selection seam. They record explicit paste,
  paste-burst input, pause/resume ordering and state, delete confirmation/cancellation, confirmed
  delete payload and state, and missing-selection behavior.
- The WASM-selected `WebApp` is root-owned at `src/app/web.rs` and contains only shared
  `AppState`, `Settings`, and the existing `AppCommand` sender/receiver. It starts no runtime or
  service and contains no fixture, lifecycle, browser, or I/O behavior.
- The scoped WASM dispatcher invokes the production top-level event translator and the exact
  normal/delete-confirm reducers. It supports only magnet paste, pause/resume, delete request,
  cancellation, and confirmation in this milestone; broader modes remain gated on Milestone 4.
- The root exposes one WASM-only integration facade instead of widening the private application or
  TUI module graph. Deterministic command fulfillment and fictional torrent creation remain under
  `web/wasm`.
- The existing Tokio MPSC channel compiled and executed under WASM without starting a Tokio
  runtime. Nonblocking drains preserved FIFO order, so no alternate queue or `AppCommand` variant
  was required.
- A real WASM execution initially proved that `std::time::Instant::now()` panics on
  `wasm32-unknown-unknown`. Event and paste-burst timing now use the existing `web_time::Instant`;
  the native characterization suite and complete native matrices remained unchanged and green.
- The native manifest and lockfile did not change in this milestone. Native dependency features
  still resolve one `ratatui 0.30.2` with Crossterm enabled, while WASM resolves the same single
  version without the Crossterm backend feature.

Verified gates:

- Native: formatting, the locked default suite (2,141 passed and one ignored), all targets with all
  features (2,162 passed and one ignored), all targets without default features (1,938 passed and
  one ignored), both strict Clippy matrices, package verification, and `git diff --check` passed.
- WASM: the documented host test, target check, and strict target Clippy gates passed from the
  standalone locked workspace. Eight contracts executed as WebAssembly under the pinned
  `wasm-bindgen-test-runner 0.2.104` and Node; all passed.
- Contract coverage proves explicit paste, paste-burst translation, exact add/pause/resume/delete
  commands, FIFO and nonblocking drain behavior, reducer state transitions, delete confirmation,
  cancel safety, delete-files preservation, stale selection, modifiers, press/repeat/release,
  resize data, browser-owned in-memory fulfillment, and production rendering from `WebApp`.
- `cargo package --list` includes `src/app/web.rs`, `src/web_integration.rs`, the terminal-event
  facade, dispatcher, and scoped reducers. The generated crate archive was extracted into an
  isolated directory and its root library passed a locked `wasm32-unknown-unknown` check.
- Pinned `wasm-bindgen 0.2.104` Node bindings exported `renderDemoFrame`; invoking it returned a
  10,573-byte self-clearing ANSI frame containing the fictional production fixture.
- Native CLI help, version, shared-config inspection, and structured config inspection passed. An
  isolated real PTY run entered raw/alternate-screen mode, rendered the production TUI, accepted
  Ctrl-C, and emitted bracketed-paste, keyboard-enhancement, cursor, and alternate-screen cleanup
  sequences on exit.

Milestone 2 exit condition: satisfied. No WebTorrent, browser storage, broad controller refactor,
browser deployment, or copyrighted fixture/title was added.

### Milestone 3: integrate the permanent browser shell (complete)

1. Initialize Ghostty Web and feed it serialized ANSI writes.
2. Drive rendering with `requestAnimationFrame` at a 60 FPS target without allowing frame backlog.
3. Forward terminal resize, browser zoom, viewport, and device-pixel-ratio changes into the Ratatui
   backend and shared screen area.
4. Handle background-tab elapsed-time jumps and page lifecycle transitions safely.
5. Display an unambiguous simulated-mode notice outside the TUI.

Exit condition: the production normal screen runs interactively in Ghostty Web at the intended
cadence and responds correctly to resizing and zooming.

#### Milestone 3 implementation and validation record

Completed on 2026-08-30 from the committed Milestone 2 baseline `4ea54cb9`.

Verified implementation boundaries:

- One native characterization test was added and passed through
  `tui::events::handle_event` before the browser resize seam changed. It proves that a resize
  updates the shared screen area, requests a redraw, and emits no service command.
- `web/wasm::BrowserDemo` retains one Ratatui `Terminal<AnsiBackend>`, the Milestone 2
  `BrowserSession`, and the browser-owned command mock. Its first and forced-refresh frames clear
  the terminal, while ordinary frames preserve Ratatui's retained diff state. Resize updates both
  the ANSI backend and the production reducer-selected screen area.
- The static host under `web` initializes pinned `ghostty-web 0.4.0`, owns the sole serialized ANSI
  write path, and schedules a new production frame only when the preceding terminal write and any
  reducer operation have completed. The animation loop targets 60 FPS without accumulating a
  frame queue.
- Ghostty Web's `FitAddon`, a container observer, window and visual-viewport resize listeners, and
  a device-pixel-ratio media query forward viewport, zoom, and display-scale changes. The browser
  input adapter preserves key kind and modifier data, explicit paste, and the production
  paste-burst flush boundary.
- Page visibility pauses drawing. Page hide cancels the outstanding animation callback, page show
  starts exactly one callback chain and forces a self-contained refresh, and unload stops drawing
  before freeing WASM and terminal state. Live testing found and removed an initial unload race and
  duplicate-resume-loop risk before this milestone was accepted.
- The simulated-mode notice is outside the terminal and explicitly says that no network or disk
  activity occurs. All fixture and mock labels are fictional, and the browser fixture selects the
  generic built-in Andromeda theme without changing the native default or runtime.
- Browser dependencies, lifecycle behavior, diagnostics, fixtures, and command fulfillment remain
  under `web`. Root changes are limited to the pre-characterized resize test, the presentation-only
  fixture theme, and a package-only `web/**` exclusion. No native dependency or lock resolution
  changed.
- Package-list validation discovered that newly tracked frontend files would otherwise enter the
  crates.io archive. The package-only exclusion keeps the browser client out of the native release
  artifact while retaining all root-owned WASM compatibility sources.

Verified gates:

- Native: formatting, the locked default suite (2,142 passed and one ignored), all targets with all
  features (2,163 passed and one ignored), all targets without default features (1,939 passed and
  one ignored), both strict Clippy matrices, package verification, CLI smoke checks, a real isolated
  120x40 PTY startup/Ctrl-C/cleanup run, and `git diff --check` passed.
- WASM: the host helpers, locked target check, and strict target Clippy passed. Eleven contracts
  executed as WebAssembly under `wasm-bindgen-test-runner 0.2.104` and Node, including retained
  terminal refresh/resize, production pause dispatch, key names, key kinds, reducers, command
  ordering, confirmation, selection, and in-memory fulfillment.
- The regenerated root package contains 368 files (8.7 MiB), excludes all `web/**` paths, and its
  unpacked root library passed a locked `wasm32-unknown-unknown` check. Native and WASM dependency
  trees still resolve exactly one upstream `ratatui 0.30.2`; only native enables Crossterm.
- Pinned Node bindings exposed `renderDemoFrame` and the retained `BrowserDemo` render, refresh,
  resize, and input methods. The one-shot and retained first frames were self-clearing and measured
  9,115 and 4,817 bytes respectively in the runtime-export check.
- `npm run build` regenerated the pinned bindings, passed strict TypeScript 7.0.2 checking, and
  bundled with Vite 8.2.2. The unoptimized milestone WASM bundle is about 9.32 MB raw (2.45 MB
  gzip); release optimization and the published bundle-size budget remain Milestone 5 work.
- In a clean live browser tab, the production normal screen rendered continuously with no console
  errors or concurrent write backlog. The page paused while hidden and resumed with a full frame;
  pause and paste traversed the browser adapter and production reducers, with pause state changing
  and a fictional pasted magnet increasing the in-memory torrent count.
- Live container resize changed the shared terminal from 162x44 to 99x31 and back. Exercising the
  visual-viewport zoom/resize route changed it from 162x45 to 107x34 and restored 162x44. The
  collaborative preview's own viewport-resize control timed out independently of the page, so
  reproducible real viewport and browser-zoom automation remains an explicit Milestone 5 gate.

Milestone 3 exit condition: satisfied. The production normal screen is interactive in Ghostty Web,
uses serialized retained ANSI rendering at the requested cadence, and responds through the shared
resize and zoom routes. No WebTorrent, browser storage, broad controller refactor, deployment, or
copyrighted mock/title was added.

### Milestone 4: complete the simulated browser experience (complete)

1. Render every production `AppMode` through the same `tui::view::draw` entrypoint and add semantic
   render tests at representative normal, narrow, short, and minimum-safe terminal sizes.
2. Populate every screen with deterministic fictional data owned by the browser mock service.
3. Feed coherent torrent, peer, file, block, disk, DHT, history, heatmap, and system telemetry into
   production display models.
4. Complete paste, pause/resume, delete, configuration, file-browser, RSS, journal,
   peer-management, and torrent-management interactions through production reducers.
5. Exercise metadata discovery, downloading, stalls, piece checking, seeding, and deletion without
   implying that simulated activity is real.

Exit condition: all production screens and representative deeper interactions work without copied
renderers or a second browser-specific UI logic path.

#### Milestone 4 implementation and validation record

Completed on 2026-08-30 from the committed Milestone 3 baseline `ee0766e7`.

Verified implementation boundaries:

- A native characterization test was added and passed through the production top-level event
  dispatcher before widening the WASM reducer seam. It proves that all eleven `AppMode` values use
  the shared dispatcher and that native mode transitions continue to emit no unexpected service
  commands.
- Native and WASM now call the same normal, file-browser, torrent-management, configuration, RSS,
  journal, peer-management, power-saving, help, welcome, and delete-confirm reducers. Target
  selection is limited to service-effect execution and the root-owned compatibility surface; no
  renderer, reducer, or browser-specific UI path was copied.
- The root WASM facade exposes only narrow data transfer types and maps them into the existing
  private production display models. Deterministic fictional torrents, peer addresses, file trees,
  RSS entries, journal entries, lifecycle labels, and telemetry remain under `web/wasm`.
- The simulated data covers metadata discovery, active downloading, a peer stall, piece checking,
  seeding, and pending deletion. It coherently populates torrent, peer, file, block, disk, DHT,
  activity-history, swarm-availability, heatmap, RSS, journal, and system-telemetry views without
  claiming network or disk activity.
- Rendering every mode discovered that `std::env::vars_os()` traps on `wasm32-unknown-unknown`.
  Environment and additional-watch-path discovery now use narrow target-safe helpers: native keeps
  its existing environment/path behavior, while WASM returns no environment override or native
  watch path. Existing native characterization tests for both helpers remain green.
- The all-features native gate exposed an existing test that searched the entire persisted event
  journal and could select an older matching event. The test now identifies the event allocated by
  its own scenario using the new entry ID; production journal behavior was not changed.
- Browser key conversion now maps Shift+Tab to `BackTab`, preserving the production reducer's
  terminal semantics. Browser lifecycle, mock fulfillment, animation, ANSI rendering, and
  diagnostics remain web-owned. No WebTorrent, browser storage, or broad application/controller
  refactor was introduced.

Verified gates:

- Native: formatting, the locked default suite (2,143 passed and one ignored), all targets with all
  features (2,164 passed and one ignored), all targets without default features (1,940 passed and
  one ignored), both strict Clippy matrices, package verification, CLI help/version/config smoke
  checks, a real 120x40 PTY startup/Ctrl-C/cleanup run, and `git diff --check` passed.
- WASM: the documented host tests, locked target check, and strict all-target target Clippy passed.
  Nineteen contracts executed as WebAssembly under pinned `wasm-bindgen-test-runner 0.2.104`; all
  passed.
- The semantic renderer contract draws all eleven production modes through `tui::view::draw` at
  120x40, 58x32, 100x14, and 32x10. Every frame is self-clearing and each normal-size frame contains
  screen-specific production content from the browser-owned fixture.
- Reducer contracts cover paste, pause/resume, delete/cancel/confirm, configuration editing,
  file-browser search, RSS search/navigation, journal selection, peer details, and
  torrent-management review/submission. Live Ghostty Web checks exercised the same browser adapter
  and production paths, including a six-to-seven-to-six torrent add/delete sequence.
- The native pre-seam and final `cargo tree -e features` outputs are byte-identical with SHA-256
  `cdc2d9f5e8fa89c6bd13f40f8e402efdcbe068f0b02861e5895bde89e67fc215`. Native and WASM each
  resolve exactly one upstream `ratatui 0.30.2`; Crossterm remains native-only.
- The root package contains 369 files (8.7 MiB), excludes `web/**`, and its unpacked root library
  passed a locked `wasm32-unknown-unknown` check. Pinned `wasm-bindgen 0.2.104` Node bindings invoked
  the one-shot renderer and retained `BrowserDemo`, selected all eleven screens, and received a
  self-clearing frame for each.
- `npm run build` regenerated bindings, passed TypeScript checking, and bundled the static site.
  The unoptimized milestone bundle is about 10.45 MB raw (2.71 MB gzip); release optimization and
  the enforceable budget remain Milestone 5 work.
- In the live browser, the production terminal changed from 162x44 to 89x44 and back through the
  page's resize path. A sustained 1.1-second sample produced 46 serialized frames with no backlog;
  pagehide produced zero frames and pageshow resumed with 15 frames in 350 ms. All screens and
  representative deeper interactions rendered without page or console errors. The collaborative
  preview's viewport-resize control still times out independently of the page, so reproducible
  viewport and zoom coverage remains a Milestone 5 automated-browser gate.

Milestone 4 exit condition: satisfied. Every production screen and representative deeper
interaction runs through the exact production renderer, event dispatcher, reducers, state, and
command boundary with browser-owned fictional data and no second UI logic path.

### Milestone 5: qualify and release Superseedr Web

1. Generate browser bindings with a pinned compatible `wasm-bindgen` CLI.
2. Type-check and bundle the static site.
3. Add separate CI jobs for native regression and the web build.
4. Add browser checks for all screens, representative reducers, resize, zoom, sustained animation,
   serialized writes, and background-tab recovery.
5. Publish only static assets; no application server is required.
6. Document supported browsers, controls, simulated limitations, bundle size, and local build steps.

Exit condition: the simulated demo is reproducibly deployable as Superseedr Web without changing
the native release workflow.

## Validation gates

### Original native application

```text
cargo fmt --all -- --check
cargo test --locked
cargo test --all-targets --all-features --locked
cargo test --all-targets --no-default-features --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo clippy --all-targets --no-default-features --locked -- -D warnings
cargo package --allow-dirty --locked
git diff --check
```

Native tests that require localhost TCP/UDP must run with the needed network permissions. A sandbox
denial is not evidence that the source failed.

Before and after each compatibility-seam change, record the native `cargo tree -e features` output
for the supported host target and review any difference. The normal resolved native Ratatui,
Crossterm, Tokio, clock, and service dependency behavior must not change accidentally.

The completed library-root prerequisite was additionally qualified by:

- Compile all ten existing `cargo-fuzz` targets and confirm the `superseedr::fuzzing` facade and
  target names are unchanged. `cargo fuzz list` may verify the inventory, but it does not satisfy
  the compile gate by itself.
- Smoke-test CLI help, configuration inspection, and representative command handling.
- Launch and exit the real application through a PTY to verify raw mode, alternate-screen entry,
  panic/exit cleanup, and the production TUI startup path.
- Confirm `src/main.rs` declares no duplicate production modules and contains no native application
  implementation beyond the Tokio wrapper.

### WASM validation for every milestone

```text
cargo test --manifest-path web/wasm/Cargo.toml --locked
cargo check --manifest-path web/wasm/Cargo.toml --target wasm32-unknown-unknown --locked
cargo clippy --manifest-path web/wasm/Cargo.toml \
  --target wasm32-unknown-unknown --all-targets --locked -- -D warnings
```

The host `cargo test` command runs fast target-independent helper tests; it does not replace actual
WASM execution.

Milestone 1 must additionally prove that its exported renderer returns a non-empty ANSI frame
produced by the production draw entrypoint.

### Milestone 2 and later WASM contract execution

Run the target-selected contract suite as WASM, initially with a Node runner because these tests
exercise Rust state, reducers, and command channels without requiring DOM APIs:

```text
cd web/wasm
wasm-pack test --node
```

This command, or an equivalent pinned `wasm-bindgen-test-runner` invocation, must run in CI. The
suite must fail if it accidentally exercises the native `App`, copies a reducer, bypasses deletion
confirmation, or fails to observe the expected command/state contract.

### Packaged-source target verification

For every milestone that adds a root-owned WASM compatibility source, inspect the output of
`cargo package --list`, build the normal package, unpack the generated crate archive into a
temporary directory, and run the root library's `wasm32-unknown-unknown` check from that archive.
Host-only package verification is insufficient because it can omit a WASM-only source without
compiling that path.

### Milestones 3 through 5 browser validation

```text
cd web && npm run build
```

Live browser checks must cover all screens, representative reducer interactions, paste, pause,
resume, deletion confirmation, resize, zoom, and a sustained animation interval.

## Minimal-change review checklist

Before merging the first release, review the diff using these questions:

- Does any original TUI renderer contain browser-specific presentation logic?
- Does any original module contain fictional fixture data or lifecycle simulation?
- Was native code behavior changed when a target gate or re-export would have sufficed?
- Is every browser-only dependency excluded from the native target?
- Does native `cargo tree` resolve Ratatui and Crossterm only from crates.io, with no dependency
  under `web`?
- Does the packaged crate verify without duplicate Ratatui dependency names or omitted path files?
- Does the compile-facing `WebApp` live in the published root crate, compile only for WASM, and
  contain no mock lifecycle or browser integration behavior?
- Are mock command fulfillment and browser service behavior still owned by `web`?
- Does the browser still call the production top-level draw and event functions directly?
- Were native characterization tests added before changing each reducer-facing seam?
- Do native and WASM harnesses assert the same event, command payload, ordering, selection,
  confirmation, and state-transition contracts?
- Did the WASM contract tests execute under a WASM runner in CI rather than only compile?
- Do semantic render tests cover every supported `AppMode` at representative terminal sizes?
- Could a native-only contributor build and test without installing browser tooling?
- Are target gates concentrated enough that removing the browser client later would be
  understandable?
- Does the UI clearly disclose that torrent activity is simulated?
- Did the completed library-root prerequisite keep internal modules private rather than widening
  the public API?
- Did native entrypoint tests move with the implementation so their `cfg(test)` behavior is
  preserved?
- Does the fuzz facade still compile with the same public paths and target names?

## Known tradeoff

The target-selected alternate `App` is a deliberate browser-demo integration compromise. It is a
small WASM-only compatibility type in the root crate, not a second product runtime. It avoids a
broad native refactor before release but creates a reducer-facing surface that must stay compatible
with the production `App`.

Control that risk by:

- Keeping `WebApp` small.
- Testing every shared reducer used by the demo.
- Keeping mocks outside `WebApp`.
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
- Native characterization and WASM contract suites pass for every supported interaction, and all
  supported `AppMode` values pass semantic render tests at representative terminal sizes.
- The simulated browser harness, fixtures, command fulfillment, and lifecycle live under `web` and
  start no native services; only the reduced compile-facing `WebApp` type lives in the root crate.
- Original-source changes are limited to library exposure, target selection, dependency selection,
  platform-safe helpers, and minimal reducer compatibility.
- Native behavior and native release tooling remain unchanged.
- The demo builds to static client-side assets and clearly labels all activity as simulated.
- No App refactor, demo worker, WebTorrent, or browser storage implementation has been pulled into
  the first release.
