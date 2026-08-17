# Release Candidate TUI And CLI Test Runbook

## Purpose

Use this runbook for the final manual release-candidate sweep. It exercises the
checked-out Superseedr binary against real, legally redistributable Linux ISO
torrents, drives the TUI with live keyboard input, and verifies the CLI in
standalone and shared-config modes.

This is a release gate, not a source-reading checklist. A feature passes only
when its visible result or persisted effect is observed in a live run.

The run is complete when:

- every required test has a `PASS`, `FAIL`, `BLOCKED`, or justified `N/A` result;
- every `PASS` has the evidence requested by that test;
- no open failure is classified as release-blocking;
- downloaded payload checksums match their publishers' checksums;
- restart and cleanup checks complete without corrupting or deleting unrelated data.

## Operator Rules

1. Test the checked-out release candidate, not a globally installed binary.
2. Use a new scratch root, isolated home directories, and a dedicated shared
   root for every run.
3. Use only public, legally redistributable test content approved by the human
   operator. Do not commit torrent metadata, downloaded payloads, feed content,
   screenshots containing third-party titles, or generated config under `tmp/`.
4. Never point `purge`, `move`, path selection, or shared-config tests at a
   production library. Destructive tests use disposable copies only.
5. Record literal keys sent and the before/after state. Source-level confidence
   is not a manual test result.
6. Do not silently repair a failed test and mark it passed. Record the failure,
   the repair, and the successful rerun as separate observations.
7. External tracker, peer, feed, and network availability can be `BLOCKED`, but
   the local UI/CLI response must still be recorded.
8. Keep preview-only fixtures physically separate from every configured watch
   folder. Before copying inputs, resolve the effective watch paths with
   `show-configs` and confirm none contains `MULTIFILE_FIXTURE`.

## Result And Evidence Format

Use these result values:

- `PASS`: the expected result was directly observed.
- `FAIL`: Superseedr produced the wrong result, crashed, hung, corrupted state,
  or made an unsafe change.
- `BLOCKED`: an external dependency prevented the observation. State the exact
  dependency and error.
- `N/A`: the feature is intentionally absent from this build or platform. State why.

For each test, record:

```text
ID:
Result: PASS | FAIL | BLOCKED | N/A
Build commit:
Command or literal keys:
Expected:
Observed:
Evidence path:
Issue link, if any:
```

Acceptable evidence includes terminal transcripts, redacted TUI screenshots,
JSON output, config/status/journal snapshots, payload checksums, and exact file
paths under the run's evidence directory. Redact personal paths, addresses, and
unrelated torrent names before sharing evidence.

## Required Coverage

### Platforms And Terminals

Run the full sweep on the primary release platform. Before a public release,
also run the smoke subset on every shipped platform and at least two terminal
emulators when practical.

The smoke subset is: `SET-01` through `SET-05`, `TUI-01`, `TUI-03`, `TUI-05`,
`TUI-07`, `TUI-09`, `TUI-16`, `CLI-01`, `CLI-03`, `CLI-06`, `CLI-08`,
`CLI-10`, `PER-01`, and `SHR-01`.

Record:

| Field | Value |
| --- | --- |
| Commit | |
| Version | |
| Build features | default / no-default-features / other |
| OS and architecture | |
| Terminal and version | |
| Shell | |
| Terminal sizes tested | narrow / normal / wide |
| Network path | direct / VPN / container |
| Peer transport | all / tcp / utp |
| Start time and duration | |

### Test Inputs

The human operator supplies and approves these inputs before the run:

| Variable | Requirement | Coverage |
| --- | --- | --- |
| `ISO_TORRENT_A` | Public Linux ISO `.torrent` file | file add, preview, real download |
| `ISO_MAGNET_B` | Public Linux ISO magnet | paste, CLI magnet, metadata arrival |
| `MULTIFILE_FIXTURE` | One repository fixture from `integration_tests/torrents/{v1,v2,hybrid}/multi_file.torrent` | add-review screens, folder expansion, and priority toggles only |
| `V2_OR_HYBRID_TORRENT` | Legal v2 or hybrid torrent, published or locally generated | v2 metadata and Merkle verification |
| `ISO_CHECKSUM_A` | Publisher checksum for the first ISO | end-to-end integrity |
| `RSS_SETUP_URL` | Reserved non-resolving HTTPS URL such as `https://rss.invalid/feed.xml` | required RSS form, persistence, error, and cleanup checks |
| `RSS_FEED_URL` | Optional operator-approved HTTPS torrent feed | optional live sync and Explorer contents |

A single-file ISO does not cover folder priority behavior. Use one repository
multi-file fixture to exercise that UI, then cancel the add review without
submitting it. The fixture must never enter the catalog or start a download. If
the live ISO inputs are v1-only, the v2/hybrid input is also required for
protocol coverage.

### Agent Execution Guidance

An agent running this document should:

- keep the TUI in a controllable real terminal session and use a second shell
  for CLI/status observations;
- capture screen text or a redacted screenshot before and after each state change;
- send literal keys one at a time unless the test is specifically about held,
  repeated, or pasted input;
- with terminal automation, send arrows, `Esc`, `Tab`, `Enter`, and other
  non-printing keys as named key events; send uppercase confirmations and
  punctuation as literal text, and verify the screen after each one;
- do not assume one synthetic repeated-key command produces multiple events;
  send navigation keys individually unless repeat behavior is the subject of the test;
- re-read the active screen footer and Help before declaring a documented key
  stale; record documentation drift as a failure instead of silently substituting a key;
- checkpoint the report and evidence after each numbered section so a terminal
  crash or restart does not erase the run history;
- pause and request human approval before acquiring new external test inputs or
  performing cleanup outside the already approved scratch root.

## SET: Isolated Setup

Run build commands with the normal development home first. Then launch the
release candidate with isolated homes so launcher sidecars and standalone state
cannot affect the user's real configuration.

```bash
git status --short --branch
git rev-parse HEAD
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
cargo test --all-targets --all-features
cargo test --all-targets --no-default-features
cargo build --release
```

These automated checks are prerequisites, not substitutes for the live sweep.
They may run on approved offloaded compute when the local machine cannot sustain
them; record the exact commit and returned logs/artifacts in that case.

Create one run root:

```bash
export REPO_ROOT="$(pwd)"
export RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)"
export RC_ROOT="$REPO_ROOT/tmp/release-candidate/$RUN_ID"
export LOCAL_HOME="$RC_ROOT/home-local"
export HOST_A_HOME="$RC_ROOT/home-host-a"
export HOST_B_HOME="$RC_ROOT/home-host-b"
export SHARED_ROOT="$RC_ROOT/shared-root"
export DOWNLOAD_ROOT="$SHARED_ROOT/downloads"
export MOVE_ROOT="$SHARED_ROOT/move-destination"
export EVIDENCE_ROOT="$RC_ROOT/evidence"
export PREVIEW_ROOT="$HOST_A_HOME/preview-fixtures"
export WATCH_A_ROOT="$HOST_A_HOME/watch-input"
export WATCH_B_ROOT="$HOST_B_HOME/watch-input"
export BIN="$REPO_ROOT/target/release/superseedr"
mkdir -p "$LOCAL_HOME" "$HOST_A_HOME" "$HOST_B_HOME"
mkdir -p "$SHARED_ROOT/fixtures" "$DOWNLOAD_ROOT" "$MOVE_ROOT" "$EVIDENCE_ROOT"
mkdir -p "$PREVIEW_ROOT" "$WATCH_A_ROOT" "$WATCH_B_ROOT"
```

Copy operator-approved `.torrent` inputs into `$SHARED_ROOT/fixtures/`. Keep the
original payload location outside any purge target. Record source URLs and
publisher checksums in the private test report, not in repository fixtures.
Copy `MULTIFILE_FIXTURE` only to `$PREVIEW_ROOT`, never to an active watch
folder or configured download target. Configure and verify `$WATCH_A_ROOT` and
`$WATCH_B_ROOT` before staging any watch-folder input. Isolated homes do not
need a conventional `~/Downloads` directory; use the explicit paths under
`$RC_ROOT` throughout this run.

Use these launch shapes throughout the run:

```bash
# Isolated standalone client or CLI
HOME="$LOCAL_HOME" "$BIN"
HOME="$LOCAL_HOME" "$BIN" --json show-configs

# Isolated shared host A
HOME="$HOST_A_HOME" \
SUPERSEEDR_SHARED_CONFIG_DIR="$SHARED_ROOT" \
SUPERSEEDR_SHARED_HOST_ID="rc-host-a" \
"$BIN"

# Isolated shared host B
HOME="$HOST_B_HOME" \
SUPERSEEDR_SHARED_CONFIG_DIR="$SHARED_ROOT" \
SUPERSEEDR_SHARED_HOST_ID="rc-host-b" \
"$BIN"
```

If an isolated `HOME` causes the Rust toolchain to be used after the build, also
preserve `RUSTUP_HOME`, `CARGO_HOME`, and set `RUSTUP_SELF_UPDATE=disable`.
If the environment blocks UDP or listener creation, record the failure and
repeat the affected smoke test with `SUPERSEEDR_PEER_TRANSPORT=tcp`. Do not
replace the required `all` and `utp` transport passes with the TCP result.

| ID | Test | Expected |
| --- | --- | --- |
| `SET-01` | Record `git status`, commit, `"$BIN" --version`, and `"$BIN" --help`. | Binary and source revision are unambiguous; help lists every intended command. |
| `SET-02` | Run `HOME="$LOCAL_HOME" "$BIN" --json show-configs`. | All effective paths resolve under the isolated home; JSON parses. |
| `SET-03` | Run shared `show-shared-config`, `show-host-id`, and `--json show-configs` as host A. | Source is `env`, host is `rc-host-a`, and the config root is `$SHARED_ROOT/superseedr-config`. |
| `SET-04` | Verify the shared root and all input/destination directories are writable. | No production path is inside the test target set. |
| `SET-05` | Start and stop a client once, then inspect logs and the terminal. | No panic, raw-mode leak, stale lock, or terminal corruption. |

## TUI: Full Live Interaction Sweep

Run `TUI-01` through `TUI-15` in one shared host-A session where possible. Use
a real terminal or a terminal-control tool that can send literal key events and
read the rendered screen. Keep an independent shell open for CLI observations.

The recommended execution order is `TUI-01`, `TUI-09`, `TUI-06`, `TUI-07`,
`TUI-08`, `TUI-02` through `TUI-05`, then `TUI-10` through `TUI-16`. This makes
the add-location setting explicit, moves each host to its dedicated empty watch
folder before the preview fixture is staged, and populates the live dashboard
before its table, search, peer-management, and telemetry tests.

Current top-level route coverage:

| `AppMode` | Covered by |
| --- | --- |
| `Welcome` | `TUI-01` |
| `Normal` | `TUI-01` through `TUI-04`, `TUI-15` |
| `Help` | `TUI-05` |
| `FileBrowser` | `TUI-06` through `TUI-10`, `TUI-13` |
| `Config` | `TUI-09` |
| `Rss` | `TUI-11` |
| `Journal` | `TUI-12` |
| `PeerManagement` | `TUI-16` |
| `TorrentManagement` | `TUI-13` |
| `DeleteConfirm` | `TUI-14` |
| `PowerSaving` | `TUI-14` |

### `TUI-01` Welcome, Baseline, And Resize

1. Launch a fresh client and record the welcome screen.
2. Press an unrelated key; confirm the screen does not change mode.
3. Press `Esc`; confirm the normal dashboard appears.
4. Resize through narrow, normal, and wide layouts, including the smallest
   practical supported size.
5. Confirm no panic, overlapping critical labels, stuck blank screen, or lost selection.

### `TUI-02` Normal Dashboard Navigation

With two or more torrents and at least one active peer if available:

1. Navigate rows with arrows and `j`/`k`; test `PageUp`, `PageDown`, `Home`, and `End`.
2. Navigate focused columns with `Left`/`Right` and `h`/`l`.
3. Press `s` twice on multiple columns; confirm ascending/descending behavior
   and stable row selection. Press `S`; confirm automatic sorting resumes.
4. Press `p` twice on a disposable torrent; confirm pause and resume in the UI,
   `status`, and journal.
5. Press `x` twice; confirm anonymization hides and restores names without
   changing torrent identity or selection.
6. Leave a torrent selected while its live metrics change; confirm selection
   does not jump unexpectedly.

### `TUI-03` Normal Search

1. Press `/`, enter a substring, and confirm the torrent list filters live.
2. Use `Backspace`; confirm the result set updates.
3. Press `Enter`; confirm the search closes while the applied query remains.
4. Reopen search and press `Esc`; confirm the query and filter clear.
5. Search for a missing value; confirm an empty state rather than stale rows or a crash.

### `TUI-04` Graphs, Themes, Rate, And Live Telemetry

1. Press `t`/`T` through all graph time scales.
2. Press `g`/`G` through every chart panel.
3. Press `[`/`]` and `{`/`}`; confirm refresh-rate changes apply without
   freezing input or making rendering unusable.
4. Press `<`/`>` through representative dark, light, and high-contrast themes.
5. Confirm download/upload graphs, peer flags, disk activity, DHT state, tuning
   state, and transport/listener status update when relevant activity exists.
6. Restart later in `PER-01` and confirm the final chosen theme persists by name.

### `TUI-05` Help Screen

1. Press `m`; confirm Help opens and unrelated route keys do not leave Help.
2. Cycle all sections with `Tab`, `Shift+Tab`, `h`, and `l`.
3. Scroll with arrows and `j`/`k`; inspect General, Torrents, Graphs, Legends,
   Screens, Paths, and Build.
4. Press `/`, search for a key or path, and confirm all-help search.
5. While search is active, press `Tab`; confirm fuzzy/regex mode changes rather
   than the section. Test a valid regex and an invalid regex.
6. Confirm `Enter` keeps results, `Esc` clears search, and `q`, `m`, or `Esc`
   closes Help when search is inactive.
7. Confirm Paths match `show-configs` and Build accurately reports DHT, PEX,
   and private/public feature state.

### `TUI-06` Add Browser And Torrent Preview

1. Press `a`; navigate directories with arrows, `Enter`/`Right`, and
   `Backspace`/`Left`/`u`.
2. Confirm only appropriate selectable inputs can be added.
3. Search the filesystem pane with `/`; while the prompt is open, use `Tab` to
   toggle fuzzy/regex mode. Test a matching query, missing query, valid regex,
   invalid regex, `Enter`, and `Esc`.
4. Select `ISO_TORRENT_A`; confirm its preview name, protocol, total size, and
   file tree are plausible before adding.
5. Press `Esc`; confirm cancellation leaves no torrent or stale pending preview.
6. Repeat and press `Y`; confirm the add appears in the dashboard, CLI `torrents`,
   `status`, and journal exactly once.
7. Repeat the add; confirm duplicate handling is explicit and does not create a
   second runtime torrent.

### `TUI-07` Paste And Magnet Metadata

1. Paste `ISO_MAGNET_B` using the terminal's bracketed-paste path.
2. Repeat with the platform's normal paste shortcut if it differs.
3. Confirm pasted text is treated as one magnet input rather than live shortcuts.
4. With add-location confirmation enabled, observe the pending magnet state,
   wait for metadata, choose a location/priority, and confirm.
5. Cancel a second pending magnet before metadata arrives; confirm its preview
   runtime is cleaned up and it does not reappear after restart.
6. Paste an invalid magnet and a nonexistent `.torrent` path; confirm clear,
   non-destructive errors and continued keyboard responsiveness.

### `TUI-08` Multi-File Preview, Priorities, And Existing-Torrent Location

Use `MULTIFILE_FIXTURE` only inside the add-review flow. Do not submit it.
Before starting, confirm its staged path is under `$PREVIEW_ROOT`, is outside
every path in `runtime_watch_dirs`, and is not already present in `torrents` or
the event journal.

1. Press `a`, select `MULTIFILE_FIXTURE`, and enter its add-review/location
   screen. Do not send the final `Y` that commits the add.
2. Confirm the fixture's protocol, total size, folder structure, and files are
   rendered plausibly. Use `Tab` to switch between filesystem and
   torrent-preview panes when search and name editing are inactive.
3. In the preview, navigate folders/files; use `Space` or `p` to cycle
   Normal/Skip/High, and confirm folder mixed state is correct.
4. Use `P` to cycle all priorities, `e` to expand all, and `c` to collapse all.
5. Search the active filesystem pane, then the active preview pane. Confirm the
   result follows focus and `Tab` changes search mode while the prompt is active.
6. Toggle container use with `x`. Edit its name with `r`; test cursor movement,
   deletion, `Esc` restoration, and `Enter` commit.
7. Press `Esc`; confirm the fixture was not added to the dashboard, catalog,
   `torrents`, status, recovery candidates, or journal, and no payload directory
   was created.
8. Select the submitted live ISO and press `f`; confirm the existing-torrent
   file/location editor opens. Exercise its single-file priority toggle, then
   cancel and confirm no change was persisted.
9. On a disposable live ISO path, reopen with `f`, choose a new disposable
   location, and confirm with `Y`. Verify `info`, restart, and checksums reflect
   the path change. This submission applies only to the live ISO, never the
   multi-file fixture.

### `TUI-09` Config Screen

Exercise every current setting: Listen Port, Default Download Folder, Torrent
Watch Folder, Layout, Confirm Add Priority And Location, Global Download Limit,
and Global Upload Limit.

For each setting:

1. Move with arrows and `j`/`k`; verify the details pane describes the selected item.
2. Use `Space`, `h`/`l`, or `t`/`f` as appropriate and confirm immediate apply.
3. For editable values, test cursor movement, `Home`, `End`, `Backspace`,
   `Delete`, valid input with `Enter`, and cancellation with `Esc`.
4. Attempt an invalid or boundary value and confirm a useful error without
   losing the previously applied value.
5. Press `r`, cancel reset with `Esc`, then repeat and confirm with `Y`.
6. For each unlocked path setting in the current mode, open the path picker,
   cancel once, then select a disposable path with `Y` and confirm the runtime
   and `show-configs` agree.
7. Change Layout through Auto, Horizontal, Vertical, and Square. Resize to force
   Wide, Stacked, and Compact Config presentations. In Compact, confirm `Space`
   opens details and `Esc` returns to the settings list before closing Config.
8. Change the listen port while running. Confirm listener/status updates and the
   transport-seen matrix resets for the new listener.
9. Press `q` or `Esc`; confirm Config closes without an extra save step.

For rate editors, use a documented valid value such as `25 Mbps`; byte-oriented
forms such as `MiB/s` are not valid unless Help explicitly says otherwise. An
invalid entry must remain uncommitted and show an actionable explanation. In a
path picker, verify the header before pressing `Y`: confirmation applies to the
current directory, not merely a visually nearby child row.

The Default Download Folder is selected manually during each add in shared
mode, so exercise that persisted setting in standalone mode and confirm it is
visibly locked in shared mode. For path-picker steps, exercise every unlocked
path in the current mode. Repeat Config on a follower and confirm cluster-owned
or locked settings are visibly locked and cannot be changed locally.

### `TUI-10` Watch Folder

Never use `MULTIFILE_FIXTURE` in this section. Reconfirm that the configured
watch folder is the dedicated empty `$WATCH_A_ROOT` or `$WATCH_B_ROOT`, and use
a separate disposable copy of an operator-approved live input.

1. Copy an approved `.torrent` into the configured host watch folder.
2. Confirm it is processed once, appears in TUI/CLI/journal, and the source file
   follows the documented processed-file behavior.
3. Repeat with a `.magnet` file and, in shared mode, a portable `.path` file
   whose target lies on the shared root.
4. Test an invalid input and a `.path` outside the shared root. Confirm clear
   rejection, no catalog corruption, and no repeated hot loop.

### `TUI-11` RSS Setup And Optional Live Sync

The required release gate covers safe setup and UI behavior. Live feed contents
and downloading an RSS item are optional unless the operator approved that
exact feed and item.

1. Press `r`; confirm RSS opens and `Tab` cycles Links, Filters, and Explorer.
2. In Links, press `a`, type `RSS_SETUP_URL`, cancel once, then add it with
   `Enter`. The reserved URL should fail resolution clearly without freezing or
   entering an unbounded retry loop.
3. Press `Space` to disable/enable the feed; confirm visual and persisted state.
4. Press `s`; confirm sync starts without freezing input. Record success or the
   exact external network/feed failure.
5. In Filters, press `a`; test filter text and use `Tab` while editing to toggle
   its mode. Confirm preview ordering/dimming responds to the filter.
6. In Explorer, press `/`; test search entry, backspace, `Enter`, and `Esc`.
7. Press `h`; confirm History toggles, navigation works, and `h` returns.
8. Press `D` on a disposable feed/filter, cancel, repeat, and confirm with `Y`.
9. Delete the setup-only URL and confirm it is absent from persistence. If a
   live `RSS_FEED_URL` was explicitly approved, add it and verify one sync.
10. If explicitly approved, press `Y` on one authorized, not-yet-downloaded
    Explorer item and confirm it enters normal ingest and History marks it.
11. Press `q` or `Esc`; restart later and confirm the intended RSS configuration persists.

### `TUI-12` Event Journal

1. Press `J`; confirm recent add, pause/resume, watch, RSS, and error events are present.
2. Cycle All, Queue, Commands, and Health with `Tab`/`Shift+Tab`.
3. Navigate with arrows and `j`/`k`; confirm details follow selection.
4. On an operator-approved archived add source, press `Y` and confirm replay is
   queued/applied once. On a non-replayable event, confirm `Y` is safely rejected.
5. Press `q` or `Esc`; confirm normal mode returns.

### `TUI-13` Torrent Management

1. Press `M`; navigate rows, pages, first/last, and columns.
2. Sort with `s`; search with `/`; use `Tab` for fuzzy/regex search; test `x` anonymization.
3. Select one row with `Space`, multiple rows, and all visible rows with `A`.
4. Press `f`; confirm the highlighted torrent's file/location editor opens and
   returns to Torrent Management on cancel/confirm.
5. Queue pause/resume with `p`; press `Y` to review, scroll a large review if
   available, cancel once, then submit with `Enter`.
6. Queue non-destructive remove with `d` on a disposable catalog entry and
   confirm the exact selected target set is preserved through review.
7. Queue purge with `D` only for a disposable copied payload. Confirm unrelated
   data remains untouched.
8. Press `u`; confirm selection and draft commands for the target set clear.
9. Hold or repeat destructive/action keys; confirm one physical hold does not
   toggle or queue the action repeatedly.
10. Press `q` or `Esc`; confirm pending drafts do not leak into normal mode.

### `TUI-14` Delete Dialog And Zen Mode

1. In Normal, press `d`; confirm the dialog identifies remove-without-files.
   Press an unrelated key, then `Esc`; confirm nothing changes.
2. Repeat `d`, press `Y`, and confirm only the disposable catalog entry is removed.
3. On a disposable copied payload, press `D`; cancel once, then confirm with
   `Y`. Verify only the expected payload path is deleted.
4. Press `z`; confirm Zen/Power Saving renders and unrelated route keys do not
   leave the mode. Confirm reduced redraw activity if observable.
5. Press `z`; confirm normal mode returns with state intact.

### `TUI-15` Quit, Shutdown, And Terminal Restoration

1. During active download and while paused, test `Q` in separate runs.
2. Test `Ctrl+C` in a separate run.
3. Confirm graceful shutdown progress completes, state is flushed, the lock is
   released, and the shell prompt/echo/cursor are restored.
4. Restart immediately. Confirm no duplicate torrents, stale pending action,
   partial config, corrupt persistence warning, or forced recheck unless expected.

### `TUI-16` Global Peer Management

Run this with at least one active peer and retain recently disconnected peer
evidence long enough to cover both live and historical rows.

1. Press `P`; confirm Peer Management opens without disturbing the selected
   torrent or live transfer.
2. Navigate with arrows, `j`/`k`, `PageUp`, `PageDown`, `Home`, and `End`.
   Move across columns with `h`/`l` or arrows and sort representative columns
   with `s`; confirm row selection remains stable as telemetry updates.
3. Cycle All, Active, Recent, and Restricted with `Tab`/`Shift+Tab`. Confirm
   active and recently disconnected peers appear in the correct filters and a
   restricted peer never appears as an unrestricted active row.
4. Search with `/` across an address fragment, endpoint, torrent label, state,
   and restriction reason. Toggle fuzzy/regex mode with `Tab`; test a valid
   regex, invalid regex, missing query, `Enter`, and `Esc`.
5. Press `x`; confirm peer addresses and torrent identities are masked while
   row identity, selection, rates, evidence, and restriction state remain usable.
6. Resize through wide, stacked, and compact layouts. Where compact layout uses
   a details overlay, open it with `Enter`, scroll it, search within details,
   then close it without losing the selected row.
7. Confirm tracked transfer totals, reconnect evidence, last-seen age, active
   transport, and restriction countdown update without sustained idle redraw or
   input lag. Compare an active row with the normal peer table when available.
8. Press `q` or `Esc`; confirm Normal returns with its prior selection intact.

## CLI: Command Surface Sweep

For commands supporting `--json`, run both text and JSON forms. Parse JSON with
an available JSON parser rather than judging it visually. Capture stdout,
stderr, and exit status separately.

### `CLI-01` Parser, Version, And Error Contract

Run:

```bash
HOME="$LOCAL_HOME" "$BIN" --help
HOME="$LOCAL_HOME" "$BIN" --version
HOME="$LOCAL_HOME" "$BIN" help add
HOME="$LOCAL_HOME" "$BIN" help status
HOME="$LOCAL_HOME" "$BIN" help priority
HOME="$LOCAL_HOME" "$BIN" help move
```

For every subcommand, verify `--help` works. Test an unknown option as a parser
error, plus missing required arguments, conflicting priority selectors, and an
invalid priority value. A bare unrecognized token is intentionally parsed as
the positional direct-add `INPUT`; test it as a nonexistent input path, not as
an unknown subcommand. Expected: no panic; nonzero exit for invalid input;
concise, actionable stderr; no state change.

### `CLI-02` Launcher Selection And Precedence

Using only the isolated homes:

1. Test `show-shared-config`, `set-shared-config`, and `clear-shared-config`.
2. Test mount-root and explicit `superseedr-config` input normalization.
3. Test `show-host-id`, `set-host-id`, and `clear-host-id`.
4. Confirm environment variables override persisted launcher sidecars.
5. Confirm `SUPERSEEDR_SHARED_HOST_ID` is reported as the canonical env source.
6. Test missing, relative, and non-writable roots; expect explicit safe failure.

### `CLI-03` Effective Paths

Run `show-configs`, `show-configs --all`, and their JSON forms in standalone,
shared host-A, and shared host-B contexts. Expected:

- effective paths are absolute and match actual files;
- host-local paths differ between host IDs where intended;
- shared catalog/settings/inbox paths agree across hosts;
- descriptions exist in JSON;
- no path escapes the isolated run root unexpectedly.

On a never-started home or host, `show-configs` may report paths together with a
`settings_load_error` explaining that the client has not started. Record that
bootstrap result, start and stop the client once, then rerun and require loaded
settings with no error.

### `CLI-04` Add Variants

Exercise all supported forms against disposable entries:

```text
superseedr <INPUT>
superseedr add <TORRENT_PATH>
superseedr add <MAGNET>
superseedr add <INPUT_A> <INPUT_B>
superseedr add '<MAGNET_A>,<MAGNET_B>'
superseedr add --path <EXISTING_DIRECTORY> <INPUT>
superseedr add --validated <INPUT>
```

Run representative forms online and offline, standalone and shared. Confirm
accepted/routed/queued/applied/observed levels separately. In shared mode, test
a `.torrent` under the shared root and reject a non-portable cross-host local path.

### `CLI-05` Read Commands And Target Resolution

Run `torrents`, `info`, `files`, `status`, and `journal` in text and JSON.

For `info` and `files`, resolve the same torrent by:

- full info hash;
- unique payload file path;
- nonexistent target;
- ambiguous path if a safe fixture can create one.

Confirm text/JSON describe the same torrent and file priorities. Run
`journal --catalog-recovery` and confirm it analyzes recovery candidates without
unexpectedly mutating the live catalog.

### `CLI-06` Status Modes

1. Run one-shot `status` online and offline.
2. Run `status --follow`, observe at least two changed snapshots, and interrupt it.
3. In standalone mode, test `status --interval <SECONDS>` and `status --stop`.
4. In shared mode, confirm `--follow` reads leader state and interval/stop
   controls fail with the documented explanation.
5. Confirm listener addresses, transport state, torrent counts, rates, and paths
   agree with the TUI and actual test state.

### `CLI-07` Pause, Resume, Remove, And Purge

For each command, test a single info hash, multiple targets, and a unique file
path where supported. Repeat representative operations:

- standalone online;
- standalone offline;
- shared online through the leader inbox;
- shared offline with no leader.

Confirm `remove` preserves payload data. Run `purge` only on a disposable copy;
confirm it deletes exactly the safely resolved payload and preserves unrelated
files. Mixed valid/invalid target batches must have explicit, reviewable semantics.

### `CLI-08` File Priority

Use the submitted live ISO's single file:

```text
superseedr priority <ISO_TARGET> --file-index 0 high
superseedr priority <ISO_TARGET> --file-path <ISO_RELATIVE_PATH> skip
superseedr priority <ISO_TARGET> --file-index 0 normal
```

Run online and offline in standalone and shared contexts. Confirm each change in
`files`, TUI, persistence, and after restart. Test out-of-range index, missing
path, both selectors, neither selector, and a path outside the torrent. Expected:
no parser panic, no change to another file, and actionable failure.

### `CLI-09` Offline Move

Use a completed disposable payload and record checksums before the test.

1. While the client is running, run `move <INFO_HASH> <MOVE_ROOT>`. Expect
   rejection and no source or catalog mutation.
2. Stop the client and repeat. Expect one successful move.
3. Confirm source/destination state, payload checksum, and persisted path.
4. Restart and confirm the torrent loads complete at the new path.
5. Test invalid hash, nonexistent destination, unsafe overlapping destination,
   and conflicting destination content. Expect no partial move.

### `CLI-10` Stop Client

Run `stop-client` against a standalone client and a shared leader. Confirm the
right process stops gracefully and followers are not incorrectly terminated.
Run it with no client and confirm a clear, non-panicking response.

### `CLI-11` Standalone And Shared Conversion

Using a disposable standalone catalog:

1. Record `show-configs`, torrents, settings, priorities, and checksums.
2. Run `to-shared <SHARED_ROOT>` and inspect the layered files.
3. Launch host A and confirm equivalent runtime behavior.
4. Stop all shared clients and run `to-standalone` under a fresh isolated home.
5. Confirm catalog, settings, paths, and priorities survive the round trip.
6. Confirm conversion does not silently change launcher sidecars.

### `CLI-12` Feature-Gated Engineering Commands

The public default binary does not expose the synthetic benchmark commands. For
an engineering build compiled with `synthetic-load`, follow
[Synthetic Benchmark](synthetic-benchmark.md) with a small explicit disk budget
and scratch output under `$RC_ROOT`. Verify `benchmark --help`, a bounded smoke
scenario, JSON/sample artifacts, cancellation, and cleanup. Also verify the
normal public release artifact does not accidentally expose hidden engineering
commands. Mark this section `N/A` for a default-only release after confirming
the feature is absent.

## SHR: Two-Node Shared Cluster

Run two clients with the same `$SHARED_ROOT`, distinct homes, and distinct host
IDs. This section is required when cluster mode is part of the release.

Give the two hosts distinct host-local listen ports before starting them
simultaneously. If both bootstrap to the same default, start each alone, change
and verify its Listen Port in Config, stop it, and only then launch both clients.

| ID | Action | Expected |
| --- | --- | --- |
| `SHR-01` | Start host A, then B. Inspect TUI, `status`, lock, and host folders. | Exactly one leader; one follower; host-local artifacts remain separate. |
| `SHR-02` | Add an approved magnet from the follower's CLI/TUI/watch folder. | It routes through the shared inbox, is applied once by the leader, and appears on both nodes. |
| `SHR-03` | Pause/resume and change priority from the follower. | Command is queued/applied once; both nodes converge. |
| `SHR-04` | Attempt follower Config changes that are cluster-owned or locked. | UI explains the lock and shared state is unchanged. |
| `SHR-05` | Stop the leader while the follower remains active. | Within two role-retry intervals the follower owns the lock, shared Config unlocks, its host status exists, and `status/leader.json` plus CLI `status` identify the promoted host. The old snapshot must not remain authoritative. |
| `SHR-06` | Add and control a torrent after failover. | New leader processes commands normally and writes current status. |
| `SHR-07` | Restart the old leader. | It rejoins safely as follower or leader according to the lock, with no split brain. |
| `SHR-08` | Temporarily make the shared root unavailable only if the test environment can do so safely. | Clients fail or degrade explicitly; they do not overwrite divergent state when access returns. |

## NET: Live Transfer And Integrity

Use the approved Linux ISO inputs. Do not classify an empty or unreachable swarm
as a Superseedr failure without separating external availability from client behavior.

1. Add `ISO_TORRENT_A` from its `.torrent`; add `ISO_MAGNET_B` by magnet.
2. Confirm tracker and/or DHT discovery, metadata acquisition, peer appearance,
   TCP/uTP transport visibility, choking/interested flags, and rate graphs.
3. Pause/resume during transfer and confirm byte counts do not advance materially
   while paused.
4. Restart mid-download; confirm resume state and already verified pieces remain.
5. Change global rate limits; confirm observed rates converge without deadlock.
6. Complete at least one approved ISO. Compare its checksum with
   `ISO_CHECKSUM_A` using the platform checksum tool.
7. Seed the completed ISO long enough to observe an upload when an authorized
   peer is available. `BLOCKED` is acceptable if no peer requests data.
8. Run representative passes with `SUPERSEEDR_PEER_TRANSPORT=all`, `tcp`, and
   `utp`. Record listener addresses and transport-family observations.
9. If testing a no-default-features/private build, confirm DHT and PEX are absent
   in Help/status and that approved tracker-based transfer still behaves as intended.
10. Add `V2_OR_HYBRID_TORRENT`; confirm the TUI/CLI report the expected protocol,
    metadata hydrates, and verified pieces survive restart. Complete it when the
    approved fixture has a practical size; otherwise record the exact metadata,
    peer, piece, and Merkle checks that were observed.
11. If an approved input advertises a web seed, confirm the web-seed path transfers
    and verifies data. Otherwise record this subtest as `N/A` with the missing fixture.

## POL: Automatic Peer Restriction Policy

This section is required because peer restriction is enabled in the public build.
Use only a disposable, locally controlled peer source and isolated persistence
under `$RC_ROOT`; never attempt to provoke or classify an external public peer.

Before the live policy exercise, capture focused automated evidence for policy
thresholds, expiry, persistence, address normalization, and inbound enforcement:

```bash
cargo test --all-features peer_manager -- --nocapture
cargo test --all-features blocked_peer_policy -- --nocapture
```

These focused tests supplement but do not replace the live observations below.

| ID | Action | Expected |
| --- | --- | --- |
| `POL-01` | With a controlled local peer, induce the documented reconnect threshold from one normalized IP within its window. | One restriction is created with reconnect evidence; equivalent IPv4 and IPv4-mapped representations do not create separate identities. |
| `POL-02` | Keep a session from that controlled peer active when the restriction is created. | The active session is removed promptly, including when its command path is busy; other peers and torrents remain unaffected. |
| `POL-03` | Attempt new inbound and outbound sessions from the restricted address. | Both paths reject the peer while unrestricted controlled peers continue normally. |
| `POL-04` | Inspect Peer Management and the isolated `peer_policy.toml`. | Restricted filter, reason, origin torrent where applicable, detection age, and remaining duration agree with persisted policy. No unrelated address is restricted. |
| `POL-05` | Gracefully stop and restart the release candidate before the restriction expires. | The live restriction is restored once, remains enforced, and its deadline is not extended merely by restart. |
| `POL-06` | Exercise expiry with an isolated near-expiry policy fixture or the focused deterministic test. | The restriction disappears at expiry, reconnect evidence does not linger as an active block, and the peer may connect again. Record whether expiry was live-observed or automated-only. |

If the current controlled-peer harness cannot induce the live threshold, mark the
affected live rows `BLOCKED` and record that harness gap explicitly. Do not mark
the shipped policy `N/A`, and do not infer a live pass from unit tests alone.

## PER: Persistence, Recovery, And Packaging

### `PER-01` Restart Persistence

After a graceful shutdown and restart, verify:

- torrent catalog and pause state;
- download paths and container rename;
- per-file priorities;
- completed and partial progress;
- theme, layout, refresh rate, and applicable settings;
- RSS feed/filter configuration and History;
- event journal entries;
- no stale lock, pending magnet preview, or duplicate ingest.

### `PER-02` Abrupt Termination Recovery

Only inside the isolated run root, terminate one active client without its normal
quit path. Restart and confirm atomic config/persistence recovery, valid catalog,
terminal restoration, and understandable journal/log evidence. Do not repeat
this against a production library.

### `PER-03` Release Artifact Smoke

Test the actual release artifact in addition to the source build. For Linux
install artifacts, follow [Linux Install Artifact Testing](linux-install-artifact-testing.md).
Confirm installed `--version`, `--help`, TUI startup, CLI path resolution,
protocol/file association behavior where shipped, and clean uninstall behavior.

### `PER-04` Package Contents

Run `cargo package --allow-dirty` as a packaging check and inspect the packaged
file list. Confirm required docs/assets are present and scratch data, evidence,
downloaded payloads, secrets, and local configs are absent.

## Final Regression Checks

Before sign-off:

- inspect logs for `panic`, task crashes, repeated error loops, corruption,
  unsafe deletion, and unbounded retry behavior;
- compare TUI, CLI text, CLI JSON, status files, journal, and persisted config
  for the same final state;
- confirm all temporary rate, path, port, layout, RSS, and priority changes were
  made only under `$RC_ROOT`;
- confirm all approved payload checksums;
- stop every test client and confirm no process holds the shared lock;
- prepare an exact-path cleanup list for `$RC_ROOT` and obtain human approval
  before deleting it when evidence is no longer needed.

## Release Sign-Off

```text
Release candidate:
Commit:
Build features:
Primary full-sweep platform:
Smoke platforms:
Started / completed:

PASS:
FAIL:
BLOCKED:
N/A:

Release blockers:
Accepted external blocks:
Issues filed:
Evidence root:
Payload checksum result:
Standalone result:
Shared-cluster result:
TUI result:
Peer-management result:
Peer-policy result:
CLI result:
Packaging result:

Recommendation: RELEASE | DO NOT RELEASE
Operator:
Reviewer:
```

Any crash, parser panic, terminal corruption, unsafe path mutation, checksum
mismatch, state loss, cross-torrent priority change, split brain, or destructive
action affecting an unselected path is an automatic `DO NOT RELEASE` until fixed
and rerun.
