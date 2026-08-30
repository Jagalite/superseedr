# Superseedr Web WASM integration crate

Milestone 1 baseline: `ca1bb3cb` (`origin/develop`).

This standalone crate owns the browser ANSI backend, retained `BrowserDemo`, deterministic mock
service, and WASM exports. It invokes the production `tui::view::draw` entrypoint and production
event dispatcher/reducers through the root crate's narrow WASM integration facade. Ghostty Web and
page lifecycle remain in the parent `web` package. WebTorrent and browser storage are not included.
First and forced-refresh frames start with a full-terminal clear sequence; retained frames use
Ratatui's exact cell-diff behavior.

## Production renderer inventory

The public facade contains only `PresentationFixture`, `PresentationState`, fixture conversion,
resize support, and `presentation::draw`. Internally it retains the exact production types required
by `tui::view::draw`: `AppState`, `AppMode`, `TorrentDisplayState`, `Settings`, `DhtStatus`,
`DhtWaveTelemetry`, and `Theme`. Their private type graph pulls in the existing configuration,
peer-view, persistence-history, telemetry, torrent-display, layout, theme, and screen-renderer
modules. None of those production modules or models is made public.

Browser-owned code supplies:

- the Ratatui-to-ANSI backend;
- deterministic fictional state and in-memory command fulfillment;
- DOM-key translation into the root target-selected terminal-event facade; and
- the one-shot `renderDemoFrame` and retained `BrowserDemo` exports.

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

## Qualification

Host helper tests, real-WASM contracts, target checking, and strict WASM Clippy are documented in
the repository integration plan and run in the dedicated web CI job. The browser package builds
this crate with its size-optimized release profile and a pinned `wasm-bindgen 0.2.104` toolchain.
