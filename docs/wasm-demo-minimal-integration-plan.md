# Superseedr Browser WASM Demo Integration Plan

Status: complete; this document records the first public browser-demo release qualification.

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

### Milestone 5: qualify and release Superseedr Web (complete)

1. Generate browser bindings with a pinned compatible `wasm-bindgen` CLI.
2. Type-check and bundle the static site.
3. Add separate CI jobs for native regression and the web build.
4. Add browser checks for all screens, representative reducers, resize, zoom, sustained animation,
   serialized writes, and background-tab recovery.
5. Publish only static assets; no application server is required.
6. Document supported browsers, controls, simulated limitations, bundle size, and local build steps.

Exit condition: the simulated demo is reproducibly deployable as Superseedr Web without changing
the native release workflow.

#### Milestone 5 implementation and validation record

Completed on 2026-08-30 from the committed Milestone 4 baseline `8a88cf24`.

Verified implementation boundaries and discoveries:

- The web build now requires `wasm-bindgen-cli 0.2.104`, compiles the standalone WASM crate with a
  size-oriented release profile, runs strict TypeScript checking, emits relative static URLs, and
  rejects a distribution containing server-side source or exceeding the recorded raw/gzip budgets.
  Playwright is locked exactly to `1.62.1`; the release CI environment uses Rust 1.95.0 and Node 24.
- Browser release diagnostics remain under `web`: a query-selected production screen, terminal
  fit/device-pixel-ratio counters, and peak concurrent-write count expose observable contracts
  without adding a root test hook or browser behavior to production TUI source.
- The bundled browser suite drives all eleven production modes and representative deeper
  reducers. It also verifies paste, pause/resume, confirmed deletion, viewport resizing, DPR zoom,
  sustained animation, one-at-a-time terminal writes, visibility-background recovery, and
  pagehide/pageshow recovery through the built static host.
- Browser automation confirmed two reducer details that the tests now preserve: a committed RSS
  search must be cleared through the production search reducer before `q` exits, and a committed
  file-browser search can remain in that mode, so the independent torrent-management scenario
  starts from a fresh browser session rather than bypassing either reducer.
- `.github/workflows/web.yml` defines separate native-regression and web/browser jobs for changes
  targeting either `develop` or `main`. The native job mirrors the documented format, feature,
  Clippy, and package gates; the web job uploads only `web/dist` and performs no deployment.
  `.github/workflows/rust.yml` and the native release/publish path are unchanged.
- Browser support, controls, simulation disclosures, local prerequisites, static-host behavior,
  bundle measurements, and browser-test steps are documented in `web/README.md`. The nested WASM
  README now describes the completed retained-session and production-dispatch integration instead
  of the Milestone 1-only boundary.
- No root/native compatibility seam, dependency, renderer, reducer, runtime, or application source
  changed in this milestone, so no new native characterization seam was required. The browser-only
  release profile, tests, diagnostics, build scripts, documentation, and CI remain under `web`.
- Native tests create an untracked zero-byte `test_torrent` in the repository root. Package-list
  review caught that Cargo would include it; the root artifact is now ignored and was removed
  before the final package was generated.

Verified gates:

- Native: formatting, the locked default suite (2,143 passed and one ignored), all targets with all
  features (2,164 passed and one ignored), all targets without default features (1,940 passed and
  one ignored), both strict Clippy matrices, package verification, CLI help/version/configuration
  inspection, an isolated real 120x40 PTY startup/Ctrl-C/cleanup run, and `git diff --check` passed.
  The explicitly deferred fuzz compilation gate was not run in this milestone.
- WASM: two host helpers, the locked target check, and strict all-target target Clippy passed.
  Nineteen production render/reducer contracts executed as WebAssembly under pinned
  `wasm-bindgen-test-runner 0.2.104`; all passed.
- The native pre-milestone and final `cargo tree -e features` outputs are byte-identical with
  SHA-256 `cdc2d9f5e8fa89c6bd13f40f8e402efdcbe068f0b02861e5895bde89e67fc215`. Native and WASM
  still resolve exactly one upstream `ratatui 0.30.2`; only native enables Crossterm.
- The final root package contains 369 files (8.8 MiB, 2.3 MiB compressed), excludes all `web/**`
  paths, and its unpacked root library passed a locked `wasm32-unknown-unknown` check. Release Node
  bindings selected all eleven production screens and returned a self-clearing frame for each.
- A clean `npm ci` installed 24 audited packages with no reported vulnerabilities. `npm run build`
  passed release WASM compilation, TypeScript, Vite bundling, and static-distribution checks. The
  measured assets are 2,289,836 bytes of WASM (819,346 gzip), 650,531 bytes of JavaScript (188,930
  gzip), and 1,010,034 gzip bytes total, all below their enforced budgets.
- Four pinned Chromium browser contracts passed against the bundled static site. The all-screen,
  reducer, lifecycle, and terminal-behavior scenarios completed without page or console errors.

Milestone 5 exit condition: satisfied. Superseedr Web is a reproducibly built and validated static
demo, the existing native release workflow is unchanged, and no WebTorrent, browser storage, broad
controller refactor, external deployment, or copyrighted fixture/title was added.

### Post-release architectural review correction record

Completed on 2026-08-30 from the committed Milestone 5 baseline `98c5533b`.

Verified implementation boundaries and discoveries:

- Native characterization tests were added before changing the seams. They exercise the top-level
  dispatcher and preserve the existing file-browser parent-fetch and `.torrent` confirmation
  command contracts.
- Native and WASM now use the same `events::handle_event`, mode dispatcher, and file-browser,
  delete-confirmation, and torrent-management interaction handlers. Target selection is confined
  to effect execution: native command sends retain their shutdown-aware Tokio task, while WASM uses
  nonblocking synchronous channel sends and starts no Tokio task or runtime.
- `.torrent` confirmation now validates the selected `RawNode<FileMetadata>` instead of consulting
  the host filesystem. Browser-owned virtual metadata therefore follows the production reducer and
  handler path without making `/simulated/**` real.
- The browser command service now fulfills every file-browser command in memory: tree fetches,
  torrent-file adds, and existing-torrent configuration updates. The mocks, virtual hierarchy, and
  browser behavior remain under `web`; no filesystem, networking, storage, or native-runtime mock
  was added to the root product.
- Real-WASM contracts reproduce `a` then Left without a runtime or crash, confirm a mocked
  `.torrent` file through the shared handler, and apply mocked existing-torrent file priorities.
  Bundled Chromium contracts repeat the first two end to end and require at least 38 frames over
  1.1 seconds, a tolerant approximately 35 FPS floor for the intended 60 FPS animation loop.
- Native release CI now installs the WASM target, extracts the generated `.crate`, finds its root
  manifest, and performs a locked root-library `wasm32-unknown-unknown` check. Repository-source
  WASM checks remain in the separate web job.
- No dependency manifest or lockfile changed. No WebTorrent, browser storage, broad `App` or
  controller refactor, external deployment, copyrighted fixture/title, or native behavioral change
  was introduced.

Verified gates:

- Native formatting and the locked default suite (2,145 passed and one ignored), all targets with
  all features (2,166 passed and one ignored), all targets without default features (1,942 passed
  and one ignored), and both strict Clippy matrices passed. CLI help, version, and shared-config
  inspection passed; an isolated real 120x40 PTY run entered and exited alternate screen, bracketed
  paste, and cursor modes cleanly after Ctrl-C. The separately deferred fuzz compilation gate was
  not run.
- WASM formatting, two host helpers, the locked target check, and strict all-target target Clippy
  passed. Twenty-two contracts executed as WebAssembly under pinned
  `wasm-bindgen-test-runner 0.2.104`; all passed.
- The native dependency-feature tree is byte-identical to the pre-review tree with SHA-256
  `cdc2d9f5e8fa89c6bd13f40f8e402efdcbe068f0b02861e5895bde89e67fc215`. Native and WASM
  still resolve exactly one upstream `ratatui 0.30.2`; only native resolves Crossterm.
- The root package contains 369 files (8.8 MiB, 2.3 MiB compressed), excludes all `web/**` paths,
  and its unpacked root library passed the locked WASM target check. Pinned release Node bindings
  exported the one-shot renderer and retained browser session, selected all eleven production
  screens, and returned a self-clearing frame for each.
- `npm run build` passed release WASM compilation, TypeScript, Vite, and distribution checks. The
  measured assets are 2,276,880 bytes of WASM (815,367 gzip), 650,531 bytes of JavaScript (188,927
  gzip), and 1,006,053 gzip bytes total, all below their enforced budgets. All six pinned Chromium
  browser contracts passed without page or console errors.

Architectural review exit condition: satisfied. Shared dispatch and interaction behavior is now
single-sourced, browser-specific code executes effects against deterministic virtual services, the
packaged-source WASM contract is enforced in CI, and all requested non-fuzz validation gates pass.

#### Follow-up configuration and stale-file corrections

Completed on 2026-08-30 against the same uncommitted architectural-review tree.

- The browser command bridge now consumes `browser_mode` and `preserve_browser_mode` when a
  `FetchFileTree` command begins. It performs the same generation check, file-browser state reset,
  mode merge, and Config-to-FileBrowser transition before the browser-owned mock service returns
  virtual tree data; those command fields are no longer discarded.
- Confirming a configuration path in WASM now runs the production settings-item merge, applies the
  resulting settings to the retained browser session, refreshes the Config draft, invalidates the
  browser generation, and returns to Config. The deterministic diagnostic exposes the applied
  default download folder for browser-contract verification only.
- Production Config navigation exposes Default Download Folder after two visible-item Down actions
  from its initial selection. Nine Down actions select Layout in the current production UI, so the
  regression follows the actual path item without changing native navigation or item visibility.
- The shared reducer continues to trust virtual `RawNode<FileMetadata>` for browser selection.
  Native `ConfirmDecision::File` effect execution again requires `path.is_file()`, preserving the
  pre-review behavior when a file is deleted after its tree snapshot was loaded. A top-dispatcher
  native characterization creates that stale snapshot, deletes the file, and verifies the browser
  remains open without queuing `AddTorrentFromFile`.
- Final native gates passed: formatting; 2,146 default tests, 2,167 all-feature tests, and 1,943
  no-default-feature tests with one existing ignored case in each matrix; and both strict Clippy
  configurations. The dependency-feature tree remains byte-identical with SHA-256
  `cdc2d9f5e8fa89c6bd13f40f8e402efdcbe068f0b02861e5895bde89e67fc215`.
- Final WASM/browser gates passed: two host helpers, locked wasm32 check, strict target Clippy, 23
  real-WASM contracts, optimized TypeScript/Vite build, and seven Chromium contracts. The measured
  assets are 2,281,242 bytes of WASM (817,156 gzip), 650,874 bytes of JavaScript (188,961 gzip),
  and 1,007,877 gzip bytes total, all below their enforced budgets.
- The final 369-file root package remains 8.8 MiB (2.3 MiB compressed), excludes `web/**`, and its
  freshly unpacked root library passes the locked `wasm32-unknown-unknown` check. No dependency,
  native runtime, WebTorrent, browser storage, broad controller, deployment, or fixture scope was
  added; the fuzz gate remains separately deferred.

Follow-up exit condition: satisfied. Configuration path browsing and application execute through
the retained production reducer path, while native stale-file behavior is restored exclusively at
the native effect boundary.

### Post-release dynamic simulation parity milestone (complete)

Completed on 2026-08-30 from the committed architectural-review baseline `d24ab96c`.

Verified implementation boundaries and discoveries:

- `web/wasm` now owns a deterministic `MockTorrentSession` state machine covering metadata
  discovery, progressive peer discovery, downloading, repeatable peer and disk-backoff stalls,
  piece checking, and seeding. The earlier proof-of-concept state machine informed the lifecycle
  vocabulary only; the implementation was rebuilt around the retained `BrowserSession` boundary
  and does not import or copy proof-of-concept architecture.
- Each simulated session derives its size, piece count, peer goal, rate variation, reserved test
  addresses, and timing from its info hash. Time is advanced in bounded fixed steps, so the same
  input and elapsed duration produce the same phase, progress, peers, stalls, and totals without
  network, filesystem, timer, or random-number dependencies.
- The browser service publishes complete production-shaped torrent updates: piece and byte
  progress, connected peer records and bitfields, per-peer and session totals, block and transfer
  histories, file metadata and direction, disk rates and operations, DHT activity, aggregate
  histories, peer-manager evidence, and seeding state. The existing production renderers consume
  those fields unchanged through `BrowserSession`; no alternate visualization renderer was added.
- Runtime telemetry has a separate browser-session update path from the one-time virtual
  filesystem, journal, and RSS fixtures. This prevents a simulation frame from resetting active
  file-browser state while still keeping the production dashboard and component visualizations
  live. The animation clock receives the browser frame delta, while heavier torrent snapshots are
  published at a bounded cadence.
- The shared dispatcher remains the only input path. Add-magnet and mocked `.torrent` commands
  create dynamic sessions, pause publishes an immediate frozen snapshot, resume continues the same
  phase and byte position, and confirmed deletion removes both simulator state and the production
  display record. Immediate pause publication was required to avoid revealing an already accrued
  sub-publish interval after the UI reported the torrent paused.
- Browser frame advancement caps foreground deltas and substitutes one frame interval after a
  background jump. The existing serialized Ghostty writer, visibility recovery, and intended
  60-FPS rendering contract therefore remain bounded while the simulation does not fast-forward
  after a suspended tab.
- All lifecycle implementation, fictional session data, browser diagnostics, and Chromium
  behavior remain under `web`. The only root additions are WASM-selected `BrowserSession` data
  transfer, runtime update, visualization-clock, and read-only contract snapshots in the already
  target-gated integration module. No simulator, mock service, or browser scheduler enters the
  native runtime.
- No manifest, lockfile, dependency, native dispatcher, reducer, effect executor, renderer, or
  release workflow changed. No WebTorrent, browser storage, broad `App` or controller refactor,
  external deployment, copyrighted title or brand, or native simulation behavior was added.

Verified contracts and gates:

- Three new real-WASM contracts prove the full metadata-to-seeding lifecycle, both deterministic
  stall modes, coherent production metric/visualization inputs, pause/resume continuity, and
  confirmed deletion. The complete suite executed 26 contracts under pinned
  `wasm-bindgen-test-runner 0.2.104`; all passed. The two host helpers, locked wasm32 check, and
  strict all-target wasm32 Clippy also passed.
- Two new Chromium contracts observe every dynamic phase and both stalls through the optimized
  bundle, verify monotonic bounded byte progress, active peers and rates, final seeding totals, and
  advancing visualization time, then independently pause, resume, and delete a selected dynamic
  torrent through production key handling. All nine browser contracts passed without page or
  console errors.
- Native formatting, 2,146 default tests, 2,167 all-feature tests, and 1,943 no-default-feature
  tests passed with one existing ignored case in each matrix. Both strict native Clippy
  configurations passed. CLI help/version and effective configuration inspection passed; an
  isolated real 120x40 PTY entered and cleaned up alternate-screen, bracketed-paste, keyboard, and
  cursor modes after Ctrl-C. The separately deferred fuzz compilation gate was not run.
- The native dependency-feature tree remains byte-identical with SHA-256
  `cdc2d9f5e8fa89c6bd13f40f8e402efdcbe068f0b02861e5895bde89e67fc215`. Native and WASM
  still resolve exactly one upstream `ratatui 0.30.2`; only native resolves Crossterm.
- The generated root package contains 369 files (8.8 MiB, 2.3 MiB compressed), excludes all
  `web/**` paths, verifies normally, and passes a locked root-library wasm32 check after fresh
  extraction. Release Node bindings returned self-clearing frames for all eleven production
  screens and advanced the new dynamic runtime export.
- The final optimized distribution contains 2,302,293 bytes of WASM (824,735 gzip), 652,942 bytes
  of JavaScript (189,246 gzip), and 1,015,740 gzip bytes total. TypeScript, Vite, static-content
  inspection, and every enforced raw/gzip budget passed. `git diff --check` also passed.

Dynamic simulation parity exit condition: satisfied. The browser demo now provides a coherent,
controllable torrent lifecycle through the shared production TUI boundary without expanding the
native runtime or the published crate's browser scope.

### Post-release production visualization telemetry correction (complete)

Completed on 2026-08-30 from the committed dynamic-simulation baseline `886271a4`.

Review findings and implementation boundaries:

- The simulator previously populated several display histories directly. That made transfer rows
  look active, but Peer Stream received peer-population snapshots instead of discrete lifecycle
  events, the persisted Network and Activity graph tiers contained only their fixture placeholder,
  and DHT, swarm, block, disk, and file-activity effects did not all pass through their production
  aggregation paths.
- A native characterization was added before changing the telemetry seam. It proves that the real
  manager-event accumulator commits discovered, connected, disconnected, received-block,
  sent-block, and completed-write data on the production one-second tick, then clears its live
  counters. The existing DHT-wave and disk-phase characterizations continue to cover the exact
  renderer inputs moved behind the shared frame-effects boundary.
- Browser torrent snapshots now enter through `UiTelemetry::on_metrics`. Discrete simulated peer,
  block, disk-operation, and disk-backoff signals enter through
  `UiTelemetry::on_manager_event_metrics`; each simulated second then runs the same UI, Network
  History, and Activity History reducers as native. Browser code supplies data-only system metrics
  instead of invoking native process inspection.
- The virtual service seeds 120 deterministic seconds of rate and resource history, and the
  browser bridge rebuilds a short presentation placeholder through the production rollup types.
  The 10-minute Network Graph and Activity tiers are therefore populated immediately and continue
  to advance through the normal second-tick contracts.
- The DHT wave, file-activity phases, swarm flash, and disk-health phase now share one production
  elapsed-frame function. DHT and disk algorithms were extracted without changing their formulas
  into `src/app/ui_effects.rs`, so native and WASM execute the same implementation rather than
  target copies. Native still obtains wall time and live service snapshots; the browser supplies
  its deterministic elapsed delta and virtual DHT snapshot at the effect boundary.
- Peer histories now contain sparse event counts rather than repeating the connected population
  every publish. Block, disk, recent-file, swarm-availability, and DHT-wave inputs remain coherent
  with each session's phase, and pause, resume, and delete continue to control the same retained
  simulated session through production commands.
- Browser simulation advancement is no longer serialized behind Ghostty's asynchronous writer.
  Animation frames continue to advance the deterministic service and production effects while a
  terminal write is pending; rendering coalesces to the latest state and still permits only one
  writer callback at a time. A browser-only delayed-writer hook exists solely for this regression
  contract.
- Torrent lifecycle and rate advancement now use a retained 100 ms fixed-step accumulator. Ten
  100 ms calls and sixty 60-FPS calls over the same downloading interval produce identical phase,
  stall, byte, rate, peer, upload, and history snapshots; caller refresh cadence can no longer
  alter completion timing or transfer totals. Presentation effects still consume elapsed time
  directly so animation remains smooth between model publications.
- Browser presentation fixtures now inherit `Settings::default()` instead of overriding the theme
  with Andromeda. Both the one-shot renderer and retained browser session therefore start with the
  same Catppuccin Mocha default as native Superseedr, covered by real-WASM and Chromium contracts.
- All lifecycle generation, virtual histories, command effects, diagnostics, and scheduling remain
  under `web`. Root changes are limited to native-characterized telemetry/effects seams and the
  existing wasm32 integration facade. No browser simulator entered the native runtime, and no
  WebTorrent, browser storage, broad controller refactor, external deployment, copyrighted test
  title or brand, manifest, or lockfile change was added.

Verified contracts and gates:

- Native formatting and locked tests passed: 2,147 default, 2,168 all-feature, and 1,944
  no-default-feature tests, with one existing ignored case in each matrix. Both strict native
  Clippy configurations passed with warnings denied. CLI help, version, and effective-config
  inspection passed; a real PTY entered and cleaned up alternate-screen, bracketed-paste,
  keyboard-enhancement, cursor, and raw-terminal modes after Ctrl-C. The separately deferred fuzz
  compilation gate was not run.
- WASM host helpers, the locked wasm32 check, and strict all-target wasm32 Clippy passed. All 28
  contracts executed as WebAssembly under the pinned `wasm-bindgen-test-runner 0.2.104`; the
  lifecycle contract now asserts populated production Network and Activity tiers, bounded event
  counts, recent file activity, swarm availability, DHT initialization, coherent rates, the native
  default theme, and equal-duration partition invariance.
- The native dependency-feature tree remains byte-identical with SHA-256
  `cdc2d9f5e8fa89c6bd13f40f8e402efdcbe068f0b02861e5895bde89e67fc215`. Native and WASM
  resolve exactly one upstream `ratatui 0.30.2`; only native enables Crossterm.
- The verified root package contains 370 files (8.8 MiB, 2.3 MiB compressed), includes the shared
  effects source, excludes `web/**`, and its freshly unpacked root library passes the locked
  wasm32 check. Pinned release Node bindings returned self-clearing frames for all eleven
  production screens and advanced simulation, production visualization time, 121-sample Network
  and Activity tiers, and the DHT wave.
- The final optimized distribution contains 2,353,903 bytes of WASM (840,781 gzip), 654,792 bytes
  of JavaScript (189,564 gzip), and 1,032,103 gzip bytes total. TypeScript, Vite, static-content
  inspection, and every raw/gzip budget passed. All eleven Chromium contracts passed without page or
  console errors, including full lifecycle visualization assertions and a 250 ms writer-delay
  scenario that sustained at least 30 simulation ticks over 1.1 seconds while writes stayed
  serialized.
- A live collaborative-browser inspection showed populated 10-minute Network Graph, Peer Stream,
  Blocks, Swarm Availability, DHT, and disk panels with no console errors. `git diff --check` also
  passed.

Production visualization telemetry correction exit condition: satisfied. The simulated service
now drives the exact production metric reducers and visualization effects, terminal backpressure
cannot throttle simulation time, native behavior and dependency resolution remain characterized,
and every requested non-fuzz validation gate passes.

### Browser scenario presets (complete)

Completed on 2026-08-30 against branch baseline `31093556`. The browser demo now exposes eight
repeatable, declarative presets through `?scenario=`: `downloading`, `seeding`, `mixed` (the
default), `swarm`, `missing-pieces`, `disk-pressure`, `disk-error`, and `recovery`. Unknown scenario
names fail during browser initialization instead of silently selecting a different profile.

Implementation discoveries and boundaries:

- `web/wasm/src/scenarios.rs` owns the complete data-only catalog. Profiles describe fictional
  sessions, initial lifecycle/control state, progress, peer goals, rate and upload scaling, piece
  availability, disk schedules, and typed journal entries. The fixtures contain no external titles,
  products, or peer-client brands.
- `MockTorrentSession` remains the single lifecycle interpreter. Scenario sessions, user-added
  magnets, pauses, resumes, and confirmed deletes all flow through `DemoCommandService`,
  `BrowserSession`, the top-level production dispatcher, and the existing production reducers.
  The production TUI and browser ANSI backend are unchanged.
- The retained 100 ms fixed-step accumulator advances rate variation, bytes, lifecycle phases,
  peer events, missing-piece arrivals, and disk transitions. Every preset produces identical phase,
  byte, peer, stall, availability, and event observations when five seconds is partitioned as 50
  100 ms deltas or 300 60-FPS deltas.
- `missing-pieces` publishes coherent peer bitfields with four unavailable pieces, caps completion,
  and introduces one scheduled supplier peer before transfer resumes. The swarm presets are bounded
  to 20 peers so production Peer Stream and Swarm Availability remain useful without unbounded
  event volume.
- Browser review exposed two overly uniform inputs: peer rates were divided equally and local
  seeding marked every remote peer complete, which the production heatmap intentionally omits.
  Deterministic weighted shares now sum back to the torrent totals while varying by peer, and stable
  peer bitfields provide low, medium, and high rarity bands plus monotonic scheduled acquisitions.
  Remote peers remain independently incomplete while the local torrent seeds, so the unchanged
  heatmap stays populated and its existing new-piece flash path receives real transitions.
- A follow-up comparison with the native torrent manager found that production does not publish
  each interval's instantaneous UL/DL rate directly. It applies the same time-aware exponential
  moving average to global and per-peer interval byte counts with a five-second smoothing period.
  Browser transfer progress still uses the raw deterministic targets. A later frame-cadence review
  corrected the initial assumption that production always applies the formula at one-second
  intervals: the torrent manager uses the configured `DataRate`, so `Rate60s` applies the
  time-aware EMA on every approximately 17 ms tick. The browser now does the same. Pausing clears
  the averages immediately, stalls decay naturally, peer rows retain heterogeneous but slow-moving
  shares, and seeded graph history follows the same five-second response instead of a sample-by-sample
  sawtooth. Raw transfer and byte accumulation are phase-gated, while published averages remain
  visible long enough to decay naturally through piece checking and into seeding instead of snapping
  to zero when a torrent reaches 100%.
- Disk profiles use browser-only `healthy`, `pressure`, `error`, and `recovering` states to throttle
  rates, drive production disk/backoff telemetry, emit warnings, and recover deterministically.
  The only root change is a typed, wasm32-gated journal classification that maps browser data into
  the existing production lifecycle and data-health event models; no scenario state or filesystem
  behavior entered the native runtime.
- The browser shell recreates only its `BrowserSession` and mock service when loading a URL preset,
  then continues using the same input dispatcher, production renderer, animation clock, and
  serialized terminal writer. Browser-owned diagnostic attributes make semantic Chromium
  assertions possible without coupling tests to canvas pixels.

Verified contracts and gates:

- Native formatting and locked tests passed: 2,147 default, 2,168 all-feature, and 1,944
  no-default-feature tests, with one existing ignored case in each matrix. Both strict native
  Clippy configurations passed with warnings denied. CLI help, version, and effective-config
  inspection passed; an isolated real 120x40 PTY entered and cleaned up alternate-screen,
  bracketed-paste, keyboard-enhancement, cursor, and raw-terminal modes after Ctrl-C. The separately
  deferred fuzz compilation gate was not run.
- The two host helpers, locked wasm32 check, and strict all-target wasm32 Clippy passed. All 35
  contracts executed as WebAssembly under pinned `wasm-bindgen-test-runner 0.2.104`, covering every
  catalog entry, equal-time partition invariance, missing-piece supplier arrival, disk warning and
  recovery presentation, shared add/pause/resume/confirmed-delete behavior, heterogeneous peer
  rates, exact aggregate-rate coherence, the native five-second torrent/peer rate average, evolving
  availability, and seeding-time remote rarity.
- The native dependency-feature tree remains byte-identical with SHA-256
  `cdc2d9f5e8fa89c6bd13f40f8e402efdcbe068f0b02861e5895bde89e67fc215`. Native and WASM continue
  to resolve exactly one upstream `ratatui 0.30.2`; only native enables Crossterm.
- The verified root package contains 370 files (8.8 MiB, 2.3 MiB compressed), includes
  `src/web_integration.rs`, excludes all `web/**` paths, and its freshly unpacked root library passes
  the locked wasm32 check.
- The optimized distribution contains 2,381,139 bytes of WASM (851,347 gzip), 657,840 bytes of
  JavaScript (189,970 gzip), and 1,043,074 gzip bytes total. TypeScript, Vite, static-content
  inspection, and every raw/gzip budget passed. All 14 Chromium contracts passed without page or
  console errors, including all URL presets, failure/recovery transitions, shared controls, every
  production screen, full lifecycle telemetry, resize, zoom, visibility, writer serialization, and
  sustained-animation coverage. The lifecycle contract also bounds consecutive published download
  changes so the browser cannot regress to raw 100 ms rate jumps. `git diff --check` also passed.

Scenario-preset exit condition: satisfied. Every preset is elapsed-time deterministic, selectable
from the browser shell, visibly exercises the intended production TUI components, remains fully
browser-owned, preserves interactive torrent controls, and passes every requested non-fuzz native,
WASM, package, optimized-build, distribution-budget, and Chromium gate.

### Browser resize robustness (complete)

Completed on 2026-08-30 against branch baseline `eb1b5398`, using the POC browser host only as a
behavioral reference. Browser startup now waits for `document.fonts.ready`, performs an initial
font-ready fit, waits for layout settlement, and performs a second fit before constructing
`BrowserDemo` with the resulting terminal geometry.

Implementation discoveries and boundaries:

- `web/src/main.ts` owns all resize detection and settlement policy. A `ResizeObserver` watches the
  terminal container itself, while window and visual-viewport resize listeners cover viewport
  changes. Device-pixel-ratio media-query observation covers zoom, and a bounded 200 ms
  geometry/DPR poll provides a fallback when a browser omits or coalesces those notifications.
- Every detected change fits immediately and once more after 120 ms so late font, canvas, zoom, and
  flex-layout changes converge without making ordinary resize feedback feel delayed. Duplicate
  fitted cell dimensions are suppressed before entering the serialized asynchronous writer queue.
- The Rust seam is unchanged: the browser calls `BrowserDemo::resize`, which resizes the retained
  ANSI terminal and dispatches production `Event::Resize` through `BrowserSession` and the shared
  top-level TUI path. No detection, DOM state, timer, or browser policy was added to the native
  runtime.
- The existing 60-FPS simulation/render scheduler, serialized terminal writer, full-frame refresh,
  and page visibility/pagehide/pageshow lifecycle behavior remain intact. Teardown now also
  disconnects the observer, DPR query, listeners, timers, and fit addon.
- Capped browser-only query delays allow Chromium to reproduce font and layout settlement without
  changing production Rust or fixture state. Diagnostic attributes expose fit source/count,
  observer activity, and DPR for semantic assertions without coupling tests to canvas pixels.

Verified contracts and gates:

- Native formatting and locked tests passed: 2,147 default, 2,168 all-feature, and 1,944
  no-default-feature tests, with one existing ignored case in each matrix. Both strict native
  Clippy configurations passed with warnings denied. CLI help, version, effective-config
  inspection, and isolated PTY startup/cleanup passed. The separately deferred fuzz compilation
  gate was not run.
- The two standalone host helpers, locked wasm32 check, and strict all-target wasm32 Clippy passed.
  All 35 contracts executed as WebAssembly under pinned `wasm-bindgen-test-runner 0.2.104`.
- The native dependency-feature tree remains byte-identical with SHA-256
  `cdc2d9f5e8fa89c6bd13f40f8e402efdcbe068f0b02861e5895bde89e67fc215`; native and WASM continue
  to resolve exactly one upstream `ratatui 0.30.2`.
- The root package contains 370 files (8.8 MiB, 2.3 MiB compressed), includes the shared WASM
  compatibility sources, excludes `web/**`, and its freshly unpacked root library passes the locked
  wasm32 check.
- The optimized distribution contains 2,381,139 bytes of WASM (851,347 gzip), 659,384 bytes of
  JavaScript (190,493 gzip), and 1,043,600 gzip bytes total. TypeScript, Vite, static-content
  inspection, and every raw/gzip budget passed.
- All 18 Chromium contracts passed without page or console errors. Dedicated cases cover delayed
  font/layout settlement, terminal-container-only resizing, viewport resizing, and DPR zoom; each
  runtime change verifies the immediate and settled fits while the existing scheduler, serialized
  writer, and page lifecycle contract stays bounded.

Browser resize robustness exit condition: satisfied. Browser geometry reliably converges after
fonts, container layout, viewport changes, and zoom while every resize still enters the unchanged
production `Event::Resize` path and all browser detection remains under `web`.

### Browser frame-rate metrics and terminal scroll containment (complete)

Completed on 2026-08-30 against branch baseline `ebb50827`. The browser simulation now advances
and publishes live torrent state on a deterministic 1/60-second fixed step instead of the previous
100 ms step. The declarative scenario schedule retains its original 100 ms tick semantics, so peer
arrival, disk pressure/error/recovery, rate variation, and fixture phase offsets keep their intended
timing while progress, piece counts, peer state, and session totals can update with every display
frame.

Implementation discoveries and boundaries:

- The browser page already requested animation frames continuously, but its explicit
  `elapsed >= 1000 / 60` check skipped alternating Chromium frames when timestamps landed just
  below that threshold. Every visible animation callback now forwards its elapsed time; the
  browser-owned Rust fixed-step accumulator remains the sole 60-Hz limiter and preserves
  equal-duration partition invariance.
- Torrent updates still enter `BrowserSession::upsert_mock_torrent` and production
  `UiTelemetry::on_metrics`; no parallel display state or renderer path was introduced. Torrent and
  peer rates now apply the native time-aware five-second EMA on every deterministic 1/60-second
  step. UL/DL values therefore change with the rows at frame cadence while retaining their slower
  production-style movement. Piece checking stops raw UL/DL samples and byte accumulation without
  hiding the accumulated averages, which decay naturally through checking and into seeding.
- A follow-up browser review exposed a separate production-state gate: unlike the POC,
  `BrowserSession::from_fixture` still inherited `DataRate::Rate1s`, so the production torrent list
  advertised and applied a one-FPS target even while the outer browser scheduler and mock service
  ran faster. The WASM-only session now initializes both `Settings.ui_refresh_rate` and
  `AppState.data_rate` to `DataRate::Rate60s`, matching the POC without changing the native default.
- Browser telemetry initially stopped after the shared one-second telemetry reducers and omitted the
  native runtime's following autosort refresh. The browser effect boundary now executes the same
  `align_unpinned_sort_with_visible_activity` and `refresh_autosort_after_stats` sequence after each
  synthetic second tick. Active downloads therefore reorder by DL, seeding activity reorders by UL,
  and the unchanged production weighted-activity tie-breaker applies when primary rates match. A
  browser-only `is_seeding` shortcut was also removed: it classified any session containing one
  complete torrent as seeding, while production classifies the objective as seeding only when no
  torrent remains incomplete.
- The earlier FPS diagnostic used the reciprocal of each animation delta, so harmless scheduling
  variance made the production footer alternate below 60. BrowserSession now measures callbacks
  over the same one-second window used by the native UI counter. The footer starts at its configured
  `60 fps` target and updates once per completed measurement window rather than oscillating every
  frame.
- Mock history capture and second-tick manager/runtime telemetry retain their existing intervals.
  Visualization effects continue to consume elapsed time independently, and writer backpressure
  still cannot stop the simulation clock.
- `scrollback: 0` remains configured on Ghostty. Browser-owned CSS now disables overscroll and
  touch panning on the terminal host, while non-passive wheel and touch-move handlers cancel scroll
  gestures over the terminal. Page content outside the terminal remains unaffected; no scroll
  policy entered Rust or the native runtime.

Verified contracts and gates:

- Standalone host helpers, locked wasm32 compilation, and strict all-target wasm32 Clippy passed.
  All 38 contracts executed as WebAssembly, including proofs that the production browser session
  starts with a stable `60 fps` production label and that consecutive 1/60-second advances publish
  strictly increasing torrent-progress snapshots and continuously updated native-style rate EMAs
  while the lifecycle contract enforces zero raw transfer during 100%-complete piece checking and
  natural decay of the displayed averages, browser stats select and dynamically order the proper
  DL/UL activity column, and all prior
  partition-invariance, scenario, and shared-control contracts remain green.
- The optimized distribution contains 2,383,772 bytes of WASM (852,041 gzip), 662,460 bytes of
  JavaScript (190,808 gzip), and 1,044,631 gzip bytes total. TypeScript, Vite, static-content
  inspection, and every raw/gzip budget passed.
- All 21 Chromium contracts passed without page or console errors. The cadence contract observes at
  least twelve distinct progress and displayed-rate snapshots over 400 ms while the production FPS
  label remains `60 fps`, separating the 60-Hz behavior from the former one-second row-rate updates.
  The autosort contract proves that downloading and seeding scenarios select and maintain descending
  DL and UL order respectively. The scroll contract verifies that real wheel input leaves page
  position unchanged, synthetic wheel input is canceled, and touch panning is disabled.

Browser frame-rate and scroll-containment exit condition: satisfied. Live torrent state follows the
60-Hz browser scheduler without changing native rate smoothing or scenario determinism, and the
terminal no longer accepts wheel, overscroll, or touch-pan scrolling.

### Browser swarm churn and virtual torrent-file preview (complete)

Completed on 2026-08-30 against branch baseline `ebb50827`. The browser-owned simulation now
models deterministic peer arrivals and departures, multi-second transfer lulls, narrow bursts, and
periods with connected peers but no interested upload recipient. The `[a]` file browser now also
loads a fictional in-memory `.torrent` preview through the existing production preview state and
unchanged TUI renderer.

Implementation discoveries and boundaries:

- Peer membership is derived from accumulated simulated time, the torrent seed, and fixed epochs.
  The active roster rotates through a larger deterministic peer pool while its connected count
  falls and recovers; manager telemetry compares peer identities rather than counts so equal-sized
  leave/join swaps still feed production connect/disconnect histories.
- Download and upload sources combine slow and fine waves with narrow deterministic bursts and
  explicit multi-second lulls. Connected-peer and interested-recipient factors bound the available
  throughput. The existing five-second native-style EMA still controls displayed movement. Zero-peer
  and zero-recipient intervals stop raw transfer immediately while the published average falls
  naturally, matching the distinction between current availability and recent throughput.
- A completion review found the same raw-versus-average distinction at the phase boundary. Reaching
  100% now fixes downloaded bytes at the total and supplies zero raw download samples during checking
  and seeding, but the prior five-second DL/UL averages remain published while they decay. This is a
  browser simulation correction only; the production telemetry reducers and TUI are unchanged.
- Torrent totals remain coherent with the visible peer rows: per-peer rates are heterogeneous and
  normalized back to the exact production aggregate on every 60-Hz publication. Pause freezes
  elapsed simulation and clears live rates, resume continues the same schedule, and delete removes
  the session and its telemetry exactly as before.
- The POC was used only to compare its broad congestion and peer-factor behavior. All lifecycle,
  roster, rate, and fixture logic remains under `web/wasm`; no simulation behavior entered the
  native runtime.
- The production file-browser renderer already supported cached torrent metadata. The missing
  browser effect now appears as `FetchTorrentPreview` at the narrow `BrowserSession` boundary; the
  browser mock service returns fictional virtual file metadata, and `BrowserSession` applies it to
  the existing `TorrentFilePreviewState` after validating generation, request id, and selected
  path. The shared dispatcher and original TUI code are unchanged.
- Chromium exposed that a single `[a]` key is replayed by the production paste-burst translator
  during its delayed flush. Preview synchronization therefore runs after both direct shared events
  and `flush_pending_paste_burst`, preserving the same dispatcher path instead of adding a browser
  shortcut.

Verified contracts and gates:

- Native formatting and locked tests passed: 2,147 default, 2,168 all-feature, and 1,944
  no-default-feature tests, with one existing ignored case in each matrix. Both strict native
  Clippy configurations passed with warnings denied. The separately deferred fuzz compilation gate
  was not run.
- The two standalone host helpers, locked wasm32 check, and strict all-target wasm32 Clippy passed.
  All 39 contracts executed as real WebAssembly. New contracts prove peer-count and peer-identity
  churn, zero-recipient upload windows with naturally decaying published rates, burst/lull
  variation, aggregate-to-peer rate equality, zero raw transfer with declining averages after
  completion, equal-elapsed partition invariance, and successful virtual `.torrent` preview
  fulfillment.
- Cargo manifests and lockfiles are unchanged. Native and WASM continue to resolve exactly one
  upstream `ratatui 0.30.2`; the native Crossterm path remains native-only.
- `cargo package` produced 370 files (8.8 MiB, 2.3 MiB compressed), includes the root WASM
  compatibility sources, and its freshly extracted root library passed the locked wasm32 check.
- The optimized distribution contains 2,392,035 bytes of WASM (855,185 gzip), 663,289 bytes of
  JavaScript (190,887 gzip), and 1,047,854 gzip bytes total. TypeScript, Vite, static-content
  inspection, and every raw/gzip budget passed.
- All 22 Chromium contracts passed without page or console errors. New cases verify that `[a]`
  reaches a ready three-file virtual preview before confirmation and that a live seeding swarm
  changes peer counts, records both joins and departures, varies upload rates, reaches a
  connected-but-zero-recipient interval while its displayed average decays, and resumes raw upload.
  The lifecycle case also observes nonzero, falling DL/UL averages throughout checking after bytes
  have stopped at the completed total.

Browser swarm and preview exit condition: satisfied. The browser presents time-partition-invariant
swarm churn and realistic transfer availability through production telemetry, while virtual
torrent preview uses the shared file-browser state and unchanged TUI with all target-specific
fixtures and effects kept under `web`.

### Browser remote-peer download lifecycle correction (complete)

Completed on 2026-08-30 after browser review of the seeding peer rows. The prior simulation keyed
peer rate averages by the peer's current row index and synthesized every bitfield from global
scenario time. A newly rotated identity could therefore inherit another peer's rate and appear with
an already mature piece map.

- Browser-owned rate state is now keyed by stable peer-pool identity. A newly connected identity
  spends a short deterministic handshake interval at zero rate, while reconnecting identities keep
  their lifetime byte totals and piece progress.
- In seeding sessions, each peer's bitfield now derives from the bytes actually uploaded to that
  peer. New peers enter at zero percent, acquire pieces monotonically after transfer begins, stop
  requesting after completing their copy, and preserve progress across reconnection.
- Remote peer download capacities vary deterministically by identity and elapsed simulated time,
  with a strict 2,000,000,000-bit-per-second per-peer ceiling. The production-facing torrent upload
  rate remains the exact sum of the visible peer rows and still uses the native five-second EMA.
- Recipient selection now assigns bytes and rates only to interested, ready, incomplete peers.
  Zero-recipient lulls, churn, pause, resume, deletion, fixed-step partition invariance, and the
  unchanged production renderer/reducers remain intact. All new lifecycle and diagnostic state is
  confined to `web`.
- Two host helper tests, strict all-target wasm32 Clippy, all 42 real-WASM contracts, the optimized
  build and size budgets, and all 25 Chromium contracts passed. Chromium directly observes a
  zero-progress arrival followed by a peer download start and enforces the 2 Gbps ceiling while the
  existing full-lifecycle, 60 FPS, resize, and zoom contracts remain green.

Remote-peer lifecycle exit condition: satisfied. Seeding peer completion and rates now describe
the same transferred bytes, newcomers visibly begin at zero, and randomized per-peer throughput can
reach high-speed swarm values without exceeding 2 Gbps or bypassing production telemetry.

### Post-release production metrics pipeline parity correction (complete)

Completed on 2026-08-30 against branch baseline `e1aae925`. This correction audited the browser
producer from every production `TorrentMetrics`, peer, manager-event, disk, history, and
visualization consumer back to the native torrent-manager and UI-telemetry producers. The native
runtime, production reducers, and TUI renderers are unchanged.

Implementation discoveries and boundaries:

- Native transfer rates are bits per second, while interval counters, peer lifetime totals, disk
  operations, and session totals are bytes. The browser had advanced byte totals directly from the
  bit rate, making progress, completion, and block activity eight times too fast. Fixed-step
  transfer accumulation now performs the native bit-to-byte conversion; ETA uses remaining bytes
  and the five-second bit-rate average exactly as the native manager does.
- The browser boundary previously dropped ETA, tracker countdown, interval byte counters, peer
  choke/interest flags, connection histories, transport identity, and file-touch direction. The
  data-only boundary now carries those native metric semantics into the existing
  `UiTelemetry::on_metrics` reducer. TCP/uTP and beneficial-peer totals are derived using the native
  lifetime-payload rule, so Peer Stream quality and transport totals no longer treat every peer as
  beneficial TCP.
- Block Stream and file activity were synthesized from averaged display rates. They now come from
  actual fixed-step byte deltas: completed 16 KiB blocks enter
  `UiTelemetry::on_manager_event_metrics`, and download and upload file touches can be emitted
  simultaneously only when bytes actually moved. Historical block fixtures use the same 16 KiB
  production unit.
- Replaying an entire second of disk and block events in one browser frame caused a visible frame
  spike. The browser simulation now accumulates exact bytes on every 60 Hz fixed step and executes
  coalesced block and disk effects at the existing 4 Hz detail boundary. Production one-second
  reducers still own block histories, disk rates, IOPS, backoff, and network/activity rollups.
  Deterministic mock disk latency samples fill the timing values that synchronous browser effects
  cannot measure through wall-clock I/O.
- Peer lifetime totals are now monotonic per stable peer identity instead of being redistributed
  across the current roster. Departed peers remain in the production `PeerManagerView`, joins and
  departures advance connection counters, idle connected peers remain active manager entries, and
  pause immediately disconnects the virtual roster. Resume and delete continue through the shared
  production commands and reducers.
- Runtime history publication had run at 10 Hz and once again at exact second boundaries, repeatedly
  reseeding production one-second rollups. Torrent display samples remain responsive, while system,
  disk, Network History, and Activity History publication now occurs once per simulated second with
  no duplicate boundary sample.
- Browser requestAnimationFrame sampling can straddle one callback boundary and report 59 for a
  healthy nominal-60 scheduler. The browser integration clamps only measurements within two percent
  of the configured target, keeping genuine misses visible without changing the production footer
  formatter or any TUI code.
- All simulation state, peer identity, tracker timing, disk timing, fixture sizing, and diagnostic
  hooks remain under `web` or the narrow wasm32-only integration boundary. No browser simulation,
  fictional data, or target-specific behavior entered the native runtime.

Verified contracts and gates:

- Real-WASM contracts cover bit/byte accounting, ETA and tracker countdown, TCP/uTP and beneficial
  counts, actual 16 KiB block events, directional file activity, disk IOPS and latency, monotonic
  per-peer lifetime totals, reconnect counters, departed-peer retention, pause disconnection,
  equal-elapsed partition invariance, lifecycle completion, and the unchanged production screens.
- Chromium covers the same live telemetry at the DOM boundary, production pause/resume/delete,
  lifecycle decay, 60 Hz progress/rate publication, stable target-FPS reporting, and the existing
  resize, zoom, serialization, visibility, and no-console-error contracts.
- Cargo manifests and lockfiles are unchanged. The browser continues to use the exact production
  Ratatui renderer and reducers; all target-specific fixtures and effect execution remain removable
  with `web`.
- Final native validation passed: the default matrix ran 2,147 tests with one existing ignored
  case, the all-feature matrix ran 2,168 tests with one existing ignored case, and the
  no-default-feature matrix ran 1,944 tests with one existing ignored case. Both strict native
  Clippy matrices, formatting, package verification, and `git diff --check` passed.
- The release package contains 371 files (8.8 MiB unpacked, 2.3 MiB compressed). Its freshly
  extracted root library passed the locked `wasm32-unknown-unknown` check. Native and standalone
  WASM dependency trees each resolve exactly one upstream `ratatui 0.30.2`.
- Final WASM validation passed: two host helpers, the locked target check, strict all-target target
  Clippy, and all 41 contracts executed as real WebAssembly under the pinned
  `wasm-bindgen-test-runner 0.2.104`.
- The final optimized distribution contains 2,417,847 bytes of WASM (862,667 gzip), 665,808 bytes
  of JavaScript (191,275 gzip), and 1,055,723 gzip bytes total. TypeScript, Vite, static-content
  inspection, and every raw/gzip budget passed. All 23 Chromium contracts passed.

Production metrics pipeline parity exit condition: satisfied. Browser data now preserves native
units and event meaning through the production telemetry reducers without modifying native runtime
behavior or the production TUI.

### Browser fixture scale, shared link, and visualization activity correction (complete)

Completed on 2026-08-30 after local review of the browser demo fixture scale.

Implementation discoveries and boundaries:

- The default mixed scenario now contains fifteen fictional, unbranded Linux ISO fixtures. Every name
  is browser-owned, ends in `.iso`, and avoids real distributions, copyrighted titles, and brands.
  Every fixture participates in the simulated lifecycle instead of being held in a browser-only
  idle state. None impersonates a paused or deleting production state; metadata discovery, peer
  discovery, eight concurrent downloads, two checks, and three seeds continue through the shared
  production view.
- Default scenario torrents use a deterministic 1.5 GiB size. Interactive
  magnet and virtual `.torrent` additions use a deliberately short 96-128 MiB lifecycle fixture so
  browser contracts and manual demos can still reach checking and seeding.
- Raw torrent and peer demand shares one deterministic bidirectional link that varies from 264 to
  330 Mbps around a 300 Mbps target. When both directions are saturated, downloads receive a 70%
  priority share; unused capacity in either direction is immediately available to the other.
  Per-torrent and per-peer rates remain heterogeneous, production five-second averages rise and fall naturally,
  and peer churn, recipient lulls, stalls, pause, resume, and delete continue to affect the shared
  totals through `MockTorrentSession` and `BrowserSession`. The mixed fixtures use deterministic
  demand weights from 18% through 290%, distinct peer populations, and staggered transfer waves;
  real-WASM and Chromium contracts require at least an 8x difference between active per-torrent DL
  and UL shares while the combined aggregate remains capped at 330 Mbps and spends most samples
  near the 300 Mbps target. Completed torrents use deterministic 800 ms eligibility epochs with a
  15% active-upload duty cycle, producing many zero-recipient periods without pausing torrents or
  bypassing production seeding state.
- Peer Stream inputs use the production manager-event telemetry path but publish browser-owned,
  deterministic discovery, connection, and disconnection events. The bounded probability model
  restores the original low event-volume envelope with 60% single-event trickles, 15% burst starts,
  and 10% lulls. Each burst tapers as two, one, and one events across three adjacent telemetry
  samples instead of clustering a whole batch into one column. The selected torrent therefore
  accumulates tens of irregular visible events instead of forcing hundreds of per-event telemetry
  reducer calls. The detailed peer roster stays intentionally bounded for usable peer-management
  screens and stable 60-FPS rendering, while DHT peer discoveries remain in the thousands.
- DHT fixtures now operate at the scale expected by the production query-load and peer-yield
  normalization instead of publishing only one or two active lookups. The unchanged production DHT
  visualization receives a changing active-query count, discovered-peer count, and node population.
- Disk load selection is deterministic by elapsed two-second epoch with a 70% Busy, 25% Strain,
  and 5% Chaos weighting. Throughput, sequential or chaotic seek history, latency, and backoff flow
  through the production telemetry scorer and hysteresis, which now reaches all three non-idle Disk
  Orb states. Moving disk-byte accumulation to each fixed step corrected the under-reporting that
  had been hidden by coalesced browser effect execution.
- Every torrent integrates bytes, progress, aggregate five-second rate averages, ETA, control, and
  activity at 60 Hz. The selected and newly added torrents publish on every display frame; static
  ANSI work for background rows is bounded through deterministic cohorts in addition to their 4 Hz
  full-detail publication. Swarm inputs and peer EMA/lifetime accounting use the deterministic
  100 ms model boundary, while block events, file activity, and disk effects retain exact
  accumulated totals.
- The selected torrent's peer-table DL/UL values now use a lightweight display-frame update that
  changes only the production peer snapshot's rate fields. Full peer metadata, bitfields, lifetime
  evidence, sorting, and the production `PeerManagerView` remain on the 4 Hz detail boundary, so
  the smoother table does not introduce a 60 Hz peer-manager rebuild. Real-WASM and Chromium
  contracts require at least twelve distinct selected-peer rate frames over 400 ms while allowing
  no more than two full peer-manager metric publications.
- Browser profiling at the 9 px, 216x66 terminal grid measured Rust simulation at approximately
  0.13 ms per frame and production Ratatui-to-ANSI rendering at approximately 2.18 ms. Ghostty's
  canvas renderer was the actual bottleneck: it averaged 12.13 ms and repainted about 33 complete
  rows per frame because it expands every dirty row to both neighbors and draws blank-space glyphs.
- The browser writer no longer treats Ghostty's next-animation-frame callback as asynchronous
  parsing completion. Normal ANSI writes finish synchronously, as Ghostty 0.4 implements them,
  while the explicit delayed-writer contract retains serialized backpressure. Test-only DOM
  diagnostics publish their display-cadence subset every frame and their full snapshot at 10 Hz.
- A browser-only adapter for the pinned Ghostty 0.4 fixed-cell renderer retains every genuinely
  dirty production TUI row, full redraw, resize, scrollback, cursor, selection, and hover behavior,
  while avoiding redundant neighbor rows for the demo's fixed-cell output. It also skips only
  undecorated blank glyphs; backgrounds, selections, text decoration, hyperlinks, and complex
  graphemes continue through the upstream renderer. The unchanged TUI, simulation, and terminal
  write cadence measured 59.93 FPS at 216x66 after the correction.
- Zoom review found that eagerly resizing Ghostty's backing canvas for a DPR change exposed its
  cleared background until the next animation frame. DPR synchronization now leaves the painted
  canvas intact until Ghostty can resize and fully repaint it synchronously in one render task.
  The fixed-cell adapter recognizes backing-size changes as mandatory full redraws, and concurrent
  window, visual-viewport, DPR, observer, and geometry notifications coalesce into one immediate
  fit followed by the settled fit. A Chromium canvas-paint contract rejects any cleared frame
  during DPR zoom while retaining the 10 px terminal.
- The terminal frame uses its active border continuously. Fine-pointer browsers start with terminal
  input focused, and clicking non-interactive frame chrome restores that focus, while the repository
  link keeps real DOM focus when explicitly selected. This removes the misleading inactive visual
  state without routing browser shortcuts or link interaction into production reducers.
- Ghostty's retained terminal cursor is hidden with the standard DEC private-mode sequence because
  the dashboard is not a command-line prompt. Fine-pointer browsers retain automatic keyboard
  focus; coarse-pointer devices immediately release Ghostty's startup focus and wait for an
  intentional terminal tap, preventing page readiness from requesting a phone keyboard.
- The supplemental mode heading, simulation notice, and focus-instruction copy were removed from
  the page shell. The browser still renders the unchanged production TUI, and the simulation
  boundary remains explicit in browser source, tests, and this living document.
- No production TUI component, reducer, native service, native dependency, or native runtime
  behavior changed. Browser fixture generation, link shaping, DHT values, disk profiles, and
  diagnostics remain under `web`, apart from the existing narrow WASM effect boundary.

Verified contracts and gates:

- Native validation passed with 2,147 default tests, 2,168 all-feature tests, and 1,944
  no-default-feature tests, with one existing ignored test in each matrix. Both strict native
  Clippy matrices, formatting, and `git diff --check` passed.
- The standalone browser crate passed its two host helpers, locked `wasm32-unknown-unknown` check,
  strict all-target WASM Clippy, and all 52 contracts as real WebAssembly under the pinned
  `wasm-bindgen-test-runner`.
- The release package contains 371 files (8.9 MiB unpacked, 2.3 MiB compressed), passed native
  package verification, and its freshly extracted root library passed a locked WASM check. Native
  and standalone WASM dependency trees each resolve exactly one upstream `ratatui 0.30.2`.
- The optimized distribution contains 2,437,959 bytes of WASM (870,204 gzip), 670,969 bytes of
  JavaScript (192,608 gzip), and 1,064,220 gzip bytes total. TypeScript, Vite, relative assets,
  static-content inspection, and every raw/gzip budget passed.
- All 35 Chromium contracts passed, including concurrent default downloads, the fifteen-row catalog,
  the shared download and upload budgets, active DHT, Busy/Strain/Chaos Disk Orb states, lifecycle
  completion, display-cadence metrics, scrolling suppression, resize, zoom without a cleared-canvas
  frame, serialization, visibility, and no-console-error gates.

Browser fixture scale and visualization activity exit condition: satisfied. The fifteen-torrent
default remains deterministic, production-shaped, browser-owned, native-safe, and retains the
10 px terminal while rejecting the prior sustained 49 FPS regression with a minimum of 60 completed
frames over the 1.1-second Chromium animation gate.

### Pull-request interaction and effect corrections (complete)

Completed on 2026-08-31 after validating each reported PR #338 interaction finding against the
browser boundary and production reducers.

Implementation discoveries and boundaries:

- The browser effect executor now applies shared RSS configuration updates, deterministic sync
  completion, and preview downloads instead of discarding those production commands. Feed add,
  enable/disable, delete, sync, history, and download state remain browser-owned and in-memory.
- WASM batch dispatch now sends one data-only batch envelope through the bounded command channel.
  Draining recursively expands that envelope in original order, so a management action involving
  more than 32 torrents is no longer partially dropped at the channel boundary. Native Tokio batch
  execution is unchanged.
- Existing-torrent file priorities use replacement semantics: the mock effect clears the previous
  override map before applying the submitted non-Normal overrides. Returning every file to Normal
  therefore removes stale Skip or High entries.
- Browser magnet effects delegate identity parsing to the production canonical magnet helper.
  Malformed or hashless values now surface an error without creating a torrent, while hexadecimal,
  base32, and case-insensitive BTIH forms share one stable identity.
- Re-adding a canonical magnet identity is now idempotent at the browser effect boundary. The
  existing mock session, lifecycle phase, progress, peers, and transfer averages remain intact
  instead of being replaced by a fresh metadata session.
- Applied global download and upload limits now bound the browser simulator's aggregate transfer
  work before shared-link allocation. Browser contracts measure exact session-byte deltas over the
  elapsed interval; production five-second rate averages retain their natural decay after a limit
  change instead of being cleared or clamped artificially.
- Confirming a virtual `.torrent` file retains the metadata-preview display name when the mock
  session is created rather than falling back to the fixture path's filename stem.
- Virtual `.torrent` identity is now derived deterministically from the mocked path and metainfo.
  Confirming the same file twice preserves the running session rather than creating a second
  torrent, and new file sessions use the configured download folder with the simulated folder only
  as a fallback.
- Opening the add-file browser now refetches its virtual tree whenever the configured root differs
  from the retained browser root. Existing matching virtual rows remain reusable, avoiding an
  unnecessary fetch while preventing stale `/simulated` entries from appearing under another path.
- The browser supplies one fictional selectable network interface and fulfills refresh commands in
  memory. The WASM compatibility capability permits the unchanged configuration reducer to select
  that virtual interface without adding network I/O or changing native interface discovery.
- `always_show_add_location_prompt` now routes pasted magnets through the shared download-location
  dialog in the browser. Confirmation emits the normal add control request with the chosen path,
  container, and priorities; direct add remains unchanged when the option is disabled.
- Repeating an RSS preview download preserves the existing torrent session, progress, averages,
  peers, and lifecycle phase while retaining the production RSS history/deduplication behavior.
- Browser publication and rendering now follow the production `DataRate` selected by `[` and `]`.
  The 60-FPS default still advances on each display frame, slower targets accumulate real elapsed
  time, and background resume remains bounded rather than replaying a hidden interval.
- Focused terminal input leaves standard browser zoom shortcuts untouched on every desktop
  platform. Committed composition text is forwarded as a production paste event, enabling IME text
  in existing configuration and RSS editors without introducing a parallel input reducer.
- Browser configuration updates now rebuild the rendered production theme at the effect boundary,
  keeping the active palette and configured theme identity synchronized.
- Virtual `.torrent` confirmation now honors `always_show_add_location_prompt`. The first
  confirmation opens the shared download-location and priority dialog from the already loaded
  virtual metadata; the second emits the normal file-add control payload with the selected path,
  container, validation flag, and priority overrides.
- New RSS preview sessions inherit the configured default download folder, matching direct magnet
  and virtual `.torrent` additions while preserving duplicate-session state.
- Focused terminal input also leaves `Ctrl+V` to the browser so the existing paste event listener
  receives clipboard text on Windows and Linux; no alternate clipboard or reducer path was added.
- The page-shell repository link uses generic source wording so tests and mock UI contain no
  third-party brand text. The destination remains the real project source URL.
- The default-theme browser contract now compares against the production default identity without
  embedding its display name in browser fixture or test text.
- Remaining semantic and runtime test names now use generic feature descriptions rather than
  embedding third-party license or asynchronous-runtime brands.
- All target-specific execution and diagnostics remain within the existing WASM integration and
  `web` boundary. Native runtime behavior, production reducers, and the production TUI are unchanged.

Verified contracts and gates:

- Native formatting, `git diff --check`, all 2,147 default tests, all 2,168 all-feature tests, and
  all 1,944 no-default-feature tests passed, with one existing ignored case in each matrix. Both
  strict native Clippy matrices, CLI help/version/config inspection, and isolated 120x40 PTY
  startup/Ctrl-C cleanup passed.
- The standalone browser crate passed both host helpers, the locked WASM target check, strict
  all-target WASM Clippy, and all 57 real-WASM contracts. New contracts cover RSS effects,
  canonical magnet rejection and duplicate preservation, configured aggregate transfer limits, a
  35-command management batch, preview-name retention, replacement of file priority overrides,
  deterministic virtual-file identity and destination, the add-location prompt, and virtual
  network-interface refresh. The final review contracts additionally cover rendered-theme updates,
  configured `.torrent` confirmation, and configured RSS destinations.
- The optimized browser build passed TypeScript, Vite, relative-asset inspection, and every size
  budget. All 41 Chromium contracts passed, including live RSS configuration/download behavior,
  duplicate RSS and virtual-file preservation, base32 magnet handling, configured transfer limits,
  virtual `.torrent` metadata naming, refresh-rate scheduling, zoom-shortcut passthrough, virtual
  interface availability, committed IME text, and focused-terminal `Ctrl+V` passthrough.
- The native package contains 371 files (8.9 MiB unpacked, 2.3 MiB compressed), passed Cargo's
  package verification, and its freshly extracted root library passed the locked WASM check. Native
  and standalone WASM dependency trees each resolve exactly one upstream `ratatui 0.30.2`.

Pull-request correction exit condition: satisfied. Every confirmed browser interaction finding has
an executed real-WASM and/or Chromium regression contract, and native behavior remains qualified by
the complete documented gate matrix.

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
- Is the simulated-data boundary explicit in browser source, tests, and documentation without
  changing the production TUI?
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
- The demo builds to static client-side assets, while its simulated-data boundary remains
  browser-owned and documented without requiring supplemental page-shell copy.
- No App refactor, demo worker, WebTorrent, or browser storage implementation has been pulled into
  the first release.
