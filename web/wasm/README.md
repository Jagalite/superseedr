# Superseedr Web WASM renderer

Milestone 1 baseline: `ca1bb3cb` (`origin/develop`).

This standalone crate owns the browser ANSI backend and a single exported renderer function. The
export constructs deterministic fictional presentation state through Superseedr's narrow
`presentation` facade and invokes the production `tui::view::draw` entrypoint. It does not include
`WebApp`, input reducers, browser services, Ghostty Web, WebTorrent, or browser storage.
Every returned frame starts with a full-terminal clear sequence so repeated writes cannot retain
stale cells from an earlier frame.

## Production renderer inventory

The public facade contains only `PresentationFixture`, `PresentationState`, fixture conversion,
resize support, and `presentation::draw`. Internally it retains the exact production types required
by `tui::view::draw`: `AppState`, `AppMode`, `TorrentDisplayState`, `Settings`, `DhtStatus`,
`DhtWaveTelemetry`, and `Theme`. Their private type graph pulls in the existing configuration,
peer-view, persistence-history, telemetry, torrent-display, layout, theme, and screen-renderer
modules. None of those production modules or models is made public.

Browser-owned code supplies only:

- the Ratatui-to-ANSI backend;
- the deterministic fictional fixture values;
- the shared target-selected terminal-event data facade and browser-only networking data shim; and
- the `renderDemoFrame` WASM export.

No production TUI renderer is copied or translated.

## Original-source change classification

- `Cargo.toml`: target-specific dependency/workspace boundary; keeps `web/wasm` outside the native
  default workspace, keeps native terminal/runtime/system dependencies off `wasm32`, and uses one
  exact upstream Ratatui version with renderer-only features on WASM.
- `src/lib.rs`: library exposure and target module selection; exports only the presentation facade,
  excludes native services on `wasm32`, and applies target-only lint scope to private support
  modules that are intentionally unreachable through the facade.
- `src/terminal_event.rs`: platform-safe shared compatibility; re-exports Crossterm event types on
  native and provides matching data-only event types without I/O on WASM.
- `src/presentation.rs`: library exposure; encapsulates the production display models and exact
  draw entrypoint behind a renderer-only API.
- `src/app.rs`, `src/integrations/mod.rs`, `src/torrent_manager/mod.rs`, and `src/tracker/mod.rs`:
  target selection; retain shared display and command model types while excluding native service
  runtimes from the renderer-only target.
- `src/tui/mod.rs` and the browser, delete-confirm, normal, and torrent screen modules: target
  selection; retain every production draw function while excluding reducer entrypoints that require
  the native `App`. Reducer integration remains Milestone 2 work.
- `src/config.rs`, `src/fs_atomic.rs`, the telemetry modules, `src/peer_manager.rs`, and the normal,
  peers, power, RSS, and welcome screens: platform-safe helpers; preserve native behavior while
  using browser-safe clocks or explicit unavailable results on `wasm32`.

## Remaining blockers

There are no remaining Milestone 1 compiler blockers. `WebApp`, production reducer input, full mock
services, browser bindings/shell, and all-screen qualification are intentionally deferred to later
milestones; they are not part of the renderer-only exit condition.
