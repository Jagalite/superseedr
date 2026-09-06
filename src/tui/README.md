# TUI Architecture (Current)

## Module Layout
- `src/tui/view.rs`: top-level draw dispatcher.
- `src/tui/events.rs`: top-level input dispatcher and cross-cutting key handling.
- `src/tui/effects.rs`: post-draw theme effect pass + effect activity speed helper.
- `src/tui/particles.rs`: theme-driven background/foreground particle rendering.
- `src/tui/screen_context.rs`: read-only draw context (`ScreenContext`, `AppViewModel`).
- `src/tui/screens/*.rs`: per-screen draw + event handling.
- `src/tui/layout.rs`: layout module root.
- `src/tui/layout/normal.rs`: normal screen layout planner (`calculate_layout`).
- `src/tui/layout/peers.rs`: responsive peer-management layout planner.
- `src/tui/layout/browser.rs`: browser screen layout planner (`calculate_file_browser_layout`).
- `src/tui/layout/common.rs`: shared table/column layout helpers.
- `src/tui/tree.rs`: tree navigation/filtering helpers.
- `src/tui/formatters.rs`: rendering format helpers.

## Runtime Flow
1. `App::run` receives events and manager updates.
2. Input is routed through `tui::events::handle_event(event, &mut app)`.
3. Draw loop ticks UI effects clock in `App`, then calls `tui::view::draw(f, &app_state, &settings)`.
4. For non-welcome screens, draw order is: optional particle background -> screen widgets -> theme color effects -> optional particle foreground.
5. In power-saving mode, drawing is gated by `app_state.ui.needs_redraw`.

## State Ownership Matrix
- `AppState` (domain/application core):
  - torrent/session/runtime metrics and histories
  - manager-facing state and persisted values
  - sorting configuration (`torrent_sort`, `peer_sort`)
  - error/warning and lifecycle flags (`should_quit`, `shutdown_progress`, etc.)
- `AppState.ui` (UI-owned transient state):
  - redraw/effects timing: `needs_redraw`, effect clocks
  - shared UI interaction state: selection + search
  - per-screen substates:
    - `config`
    - `delete_confirm`
    - `file_browser`
    - `peer_management`
- `AppMode`:
  - now acts as high-level route/screen id (`Normal`, `Config`, `FileBrowser`, etc.)
  - payload data has been migrated into `AppState.ui` substates.

## Current Transition Summary
- `Welcome`: `Esc` -> `Normal`.
- `Normal`:
  - `/` enters search.
  - `v` enters visualization focus mode when an eligible panel is visible; `Tab`/`Shift+Tab` moves between Peer Stream and DHT when those panels are visible, `Left`/`Right` (or `<`/`>`) cycles their retained visualization renderers, `u` restores the classic renderer, and `v` or `Esc` exits.
  - `z` -> `PowerSaving`.
  - `c` -> `Config`.
  - `a` -> `FileBrowser` (add torrent flow).
  - `d`/`D` -> `DeleteConfirm`.
  - `M` -> `TorrentManagement`.
  - `P` -> `PeerManagement`.
  - `Q` sets quit flag.
  - `Esc` clears `system_error` (stays in `Normal`).
- `TorrentManagement`:
  - `/` searches torrent names and paths.
  - `Tab` toggles search mode.
  - `x` toggles anonymized torrent names.
  - `h`/`l` or `←`/`→` moves between visible columns; `s` sorts by the focused column.
  - `Space` multi-selects the focused torrent; `A` selects all visible torrents.
  - `p`, `d`, and `D` queue pause/resume, remove, and purge actions while preserving the selected target set.
  - `Y` opens the draft-command confirmation when commands are queued; `Enter` submits from review.
  - `u` clears the current selection and any draft commands for that target set.
  - Held shortcuts reported as key repeats cannot toggle actions repeatedly; deliberate Press events remain reusable on terminals without key-release reporting.
  - Review mode supports `j`/`k`, arrows, Page Up/Down, Home, and End for scrolling large batches.
  - `Esc`/`q` returns to `Normal`.
- `PeerManagement`:
  - `Tab`/`Shift+Tab` cycles `All`, `Active`, `Recent`, and `Restricted` peers.
  - `/` searches peer addresses, endpoints, torrents, states, and restriction reasons; `Tab` toggles fuzzy/regex while search is active.
  - `h`/`l` or `←`/`→` moves between visible columns; `s` sorts by the focused column.
  - `Enter` toggles full peer details on compact layouts; `x` toggles privacy masking.
  - `Esc`/`q` returns to `Normal`.
- `PowerSaving`: `z` -> `Normal`.
- `Config`:
  - `Space` shifts boolean and choice settings immediately, opens value editing for the listen port and global rate limits, or opens a path browser; `Left`/`Right` (or `h`/`l`) moves backward/forward through choices; `r` opens reset confirmation for the focused setting.
  - In reset confirmation, `Y` restores the default and `Esc` cancels.
  - While editing a value, `Enter` applies it and `Esc` cancels the current edit.
  - Confirming a path in `FileBrowser` applies it and returns to Config.
  - `Esc`/`q` closes Config immediately.
  - In compact mode, `Space` opens the selected setting's details and activates its control; `Esc` returns to the settings list before closing Config.
- `FileBrowser`:
  - `Y` confirms current action.
  - `Esc` returns to `Normal` or `Config` depending on browser mode.
  - `/` enters browser search for the active pane; while the search bar is active, `Tab` toggles fuzzy/regex instead of switching panes.
  - Torrent preview pane: `Space` or `p` cycles the selected file/folder priority, `P` cycles all file priorities, `e` expands all, and `c` closes all.
- `DeleteConfirm`: `Y` confirms and returns to `Normal`; `Esc` cancels.

## Navigation Contract (Minimal)
This contract formalizes top-level screen transitions. Any transition behavior change should update this table and the transition tests.

| From Mode | Input/Event | To Mode | Notes |
| --- | --- | --- | --- |
| `Welcome` | `Esc` press | `Normal` | Entry handoff |
| `Normal` | `m` | `Help` | Manual/help route |
| `Help` | `Esc`, `m`, or `q` | `Normal` | Close help when search is not active |
| `Normal` | `z` | `PowerSaving` | Zen mode |
| `PowerSaving` | `z` | `Normal` | Return from zen |
| `Normal` | `c` | `Config` | Open settings |
| `Config` | completed control change | `Config` | Apply toggles, choices, confirmed resets, and value edits immediately |
| `Config` | `Esc` or `q` | `Normal` or `Config` | Close immediately; compact details first returns to the settings list |
| `Normal` | `M` | `TorrentManagement` | Batch torrent management |
| `TorrentManagement` | `Esc` or `q` | `Normal` | Close management |
| `Normal` | `P` | `PeerManagement` | Inspect tracked peers and restrictions |
| `PeerManagement` | `Esc` or `q` | `Normal` | Close peer management |
| `Normal` | `d`/`D` | `DeleteConfirm` | Selected torrent only |
| `DeleteConfirm` | `Y` or `Esc` | `Normal` | Confirm/cancel dialog |
| `Normal` | `a` | `FileBrowser` | Add torrent path flow |
| `Config` | `Space` on path item | `FileBrowser` | Path picker flow; compact mode opens details |
| `FileBrowser` config path picker | `Y` | `Config` | Apply the confirmed path immediately |
| `FileBrowser` | `Esc` | `Normal` or `Config` | Depends on browser sub-mode |

### Forbidden/No-op examples
- `Help` + unrelated keys (e.g. `c`, `a`, `d`) must stay in `Help`.
- `PowerSaving` + non-`z` keys must stay in `PowerSaving`.
- `Welcome` + non-`Esc` keys must stay in `Welcome`.

### Executable Transition Table (Tests)
- Treat this matrix as an executable contract via focused tests:
  - mode-local handler tests for `Welcome`, `Help`, and `PowerSaving`
  - existing reducer/effect tests for `Normal`, `Config`, `DeleteConfirm`, and `FileBrowser`
  - existing event-layer debounce tests in `tui/events.rs`
- If a transition behavior changes, update both this table and the corresponding tests.

### Future Full-System Trigger
Keep the current lightweight contract unless one or more of these happen:
1. Top-level modes/submodes grow enough that distributed handler logic becomes hard to reason about.
2. Navigation regressions continue despite transition contract tests.
3. Multiple parallel features frequently modify navigation and produce conflicts.
4. Guarded/conditional transitions become complex enough to justify a centralized runtime FSM.

## Help Overlay
- Help now uses dedicated route mode: `AppMode::Help`.
- Windows: `m` press toggles between `Normal` and `Help`.
- Non-Windows: `m` press opens help from `Normal`; `m` release or `Esc` closes to `Normal`.
- Help content is sectioned (`General`, `Torrents`, `Graphs`, `Legends`, `Screens`, `Paths`, `Build`) and scrolls with `Up`/`Down` or `k`/`j`.
- `Tab`/`Shift+Tab` or `h`/`l` moves between sections.
- `/` opens a prompt-panel search across all help contents, including path and build rows; typed characters filter live, `Tab` toggles fuzzy/regex matching, `Enter` keeps results, and `Esc` clears search.
- Help keeps the section tabs above the bordered panel and the command footer below it. Wider layouts use the classic full tab strip; narrower layouts tighten the spacing or show neighboring sections while preserving the grouped command index.
- Help geometry is planned once and shared by rendering and scroll clamping so fixed chrome, search, and warning rows cannot make reachable content diverge from what is visible.

## Invariants
- Reducers are deterministic and side-effect free; side effects execute via effect runners.
- Screen reducers do not mutate `app_state.mode` directly; route transitions are emitted as effects and applied in effect executors.
- `events.rs` stays staged and thin: resize handling, Esc debounce, global hooks, then mode dispatch.
- Screen `handle_event` entrypoints stay thin and delegate to per-screen reducer/mapping helpers.
- Layout planners are pure functions from geometry/context to `LayoutPlan` values.
- Draw functions read from state and context, and do not mutate core app/domain state.

## Extension Guide (New Screen)
1. Add `src/tui/screens/<screen>.rs` with `draw` and `handle_event` entrypoints.
2. Keep `handle_event` as staged dispatch: `map input -> reduce action -> execute effects`.
3. Add a per-screen layout planner under `src/tui/layout/<screen>.rs` if layout is non-trivial.
4. Keep reusable table/column logic in `src/tui/layout/common.rs`.
5. Wire dispatch in `src/tui/events.rs` and rendering in `src/tui/view.rs`.
6. Add reducer/mapping unit tests and at least one transition/behavior regression test.
