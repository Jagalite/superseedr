# CLI Guide

## What The CLI Is For

The Superseedr CLI is the main user-facing control surface for scripting,
automation, and headless operation.

It works in:

- standalone mode
- shared cluster mode
- online mode with a running client
- offline mode from persisted state when supported

The CLI is file-oriented rather than network-oriented. Commands either talk to a
running client through local/shared control files or operate directly on
persisted state when offline behavior is supported.

## Global Options

### `--json`

Return structured JSON output instead of text output.

Example:

```bash
superseedr --json status
```

## Quick Start

Add a torrent:

```bash
superseedr add "/path/to/example.torrent"
```

Add a magnet:

```bash
superseedr add "magnet:?xt=urn:btih:..."
```

Inspect current state:

```bash
superseedr status
superseedr torrents
superseedr journal
superseedr show-configs
```

Pause and resume:

```bash
superseedr pause <INFO_HASH_HEX_OR_PATH>
superseedr resume <INFO_HASH_HEX_OR_PATH>
```

Engineering benchmark builds can also run the local synthetic benchmark harness:

```bash
cargo run --release --features synthetic-load -- benchmark --max-torrents 1000 --max-peers 100000
```

See [`docs/synthetic-benchmark.md`](synthetic-benchmark.md) for deeper
synthetic load testing, disk-budget behavior, and per-scenario examples.

## Runtime Listen Port

To select a new available peer-listening port each time Superseedr starts, use
either environment form:

```bash
PORT=RANDOM superseedr
SUPERSEEDR_CLIENT_PORT=RANDOM superseedr
```

The namespaced variable takes precedence. It also accepts a numeric fixed port:

```bash
SUPERSEEDR_CLIENT_PORT=6681 superseedr
```

The equivalent persisted setting, including in a shared host config, is:

```toml
client_port = "RANDOM"
```

Runtime status reports the numeric port that was actually selected.

The same mode is available in the TUI configuration screen: select **Listen
Port**, press `Space`, enter `RANDOM`, and press `Enter`. Entering a numeric port
switches the setting back to a fixed port.

## Startup Network Interface

To apply strict interface binding before the client creates its initial listeners or
outbound networking generation, provide the exact operating-system interface identity:

```bash
superseedr --network-interface vpn0
```

This option starts the interactive client and cannot be combined with a CLI command
or positional torrent input. It applies only to the current run and does not modify
persisted settings or shared host configuration. A discovered single-family
interface automatically selects its available family. A missing or unusable interface
leaves networking blocked rather than falling back to unrestricted routing. Existing
exact source-address constraints are cleared because they may belong to a different
interface; the DNS policy is preserved.

To persist strict binding for the current host instead, stop its running client and use:

```bash
superseedr set-network-interface vpn0
```

The command requires the exact identity of a currently active, non-loopback interface.
It preserves the enabled address families when the interface supports them, selects the
available family for a single-family interface, clears stale exact source-address
constraints, and preserves the existing DNS policy. In shared mode the setting is
written to this host's configuration layer, not the shared defaults.
Because shared configuration uses a single-writer lock, all clients using that shared
configuration must be stopped before running the persistent command.

## Runtime Peer Transport

Production peer transport enables TCP and uTP by default. When both transports
are enabled, Superseedr races uTP and TCP connection attempts so a slow or
unreachable uTP path does not block TCP fallback.

Use `SUPERSEEDR_PEER_TRANSPORT` to override the default:

- `all`: enable TCP and uTP; this is the default
- `tcp`: use TCP only
- `utp`: use uTP only

Example:

```bash
SUPERSEEDR_PEER_TRANSPORT=tcp superseedr
```

## Targeting Torrents

Many commands accept either:

- `INFO_HASH_HEX`
- a unique file path belonging to the torrent

Supported commands:

- `info`
- `files`
- `pause`
- `resume`
- `remove`
- `purge`
- `priority`

Behavior:

- if the target is already an info hash, Superseedr uses it directly
- if the target is a file path, Superseedr reverse-resolves it to the owning torrent
- if the file path matches more than one torrent, the command fails and asks for the info hash
- if no torrent matches, the command returns an error

## Command Reference

### `add`

```bash
superseedr add <INPUT>...
superseedr add --path <PATH> <INPUT>...
superseedr add --validated <INPUT>...
superseedr add 'magnet:?xt=...,magnet:?xt=...'
```

Add one or more torrent file paths or magnet links.

Inputs can be:

- `.torrent` paths
- magnet links
- comma-joined magnet links with no whitespace between entries
  (`magnet:?xt=...,magnet:?xt=...`)

Use `--validated` (alias: `--validate`) when rebuilding a catalog from data
that has already been verified and should be persisted with
`validation_status = true`.

Use `--path` to persist an existing download path for the added inputs. The
path must be non-empty and must already exist as a directory.

In shared mode, cross-host `.path` adds are portable when the `.torrent` file
is on the shared root.

### `stop-client`

```bash
superseedr stop-client
```

Request graceful shutdown of the running client.

Behavior:

- standalone mode: targets the local running client
- shared mode: targets the current leader through the shared inbox

### `journal`

```bash
superseedr journal
```

Show the event journal.

Behavior:

- text mode: human-readable entries
- `--json`: structured JSON envelope
- shared mode: merged view of shared command events and host-local runtime events
- network activation changes: host-local rebinding, blocked, and restored entries,
  including the full blocked diagnostic

### `set-shared-config`

```bash
superseedr set-shared-config <PATH>
```

Persist the shared mount root for launcher and protocol-handler starts.

Accepted forms:

- the shared mount root
- an explicit `.../superseedr-config` path

Superseedr normalizes both to the shared mount root.

### `clear-shared-config`

```bash
superseedr clear-shared-config
```

Remove the persisted shared-config launcher setting.

### `show-shared-config`

```bash
superseedr show-shared-config
```

Show whether shared mode is enabled, the effective shared selection, and the
source of that selection.

Shared-config precedence is:

1. `SUPERSEEDR_SHARED_CONFIG_DIR`
2. persisted launcher shared-config sidecar
3. normal standalone mode

### `show-configs`

```bash
superseedr show-configs
superseedr --json show-configs
superseedr show-configs --all
```

Show resolved absolute paths and short descriptions for the effective config,
log, status, journal, lock, and watch paths.

See [`configuration-and-backups.md`](configuration-and-backups.md) for the
configuration and backup file map, including backup cadence and retention.

Behavior:

- default mode: reports only the effective paths Superseedr is actually using
- `--all`: also reports launcher sidecars, local fallback paths, and shared
  cluster internals
- relative paths from settings are resolved to absolute paths without requiring
  the destination to already exist
- JSON mode keeps the resolved path tree and adds a `descriptions` array keyed
  by section and field name

### `set-host-id`

```bash
superseedr set-host-id <HOST_ID>
```

Persist an explicit host identity for shared mode.

This is optional. If you do not set one, Superseedr derives a host identity
automatically.

### `clear-host-id`

```bash
superseedr clear-host-id
```

Remove the persisted shared host identity.

### `show-host-id`

```bash
superseedr show-host-id
```

Show the effective host identity and its source.

Host-id precedence is:

1. `SUPERSEEDR_SHARED_HOST_ID`
2. persisted launcher host-id sidecar
3. hostname fallback

### `to-shared`

```bash
superseedr to-shared <PATH>
```

Convert the current standalone config into layered shared config at the given
shared root.

### `to-standalone`

```bash
superseedr to-standalone
```

Convert the active shared config back into local standalone config.

### `torrents`

```bash
superseedr torrents
```

List configured torrents.

### `info`

```bash
superseedr info <INFO_HASH_HEX_OR_PATH>
```

Show a single torrent by info hash or unique file path.

### `status`

```bash
superseedr status [--follow | --stop | --interval <SECONDS>]
```

Read status, stream status updates, or adjust standalone runtime status dumping.

Behavior:

- plain `status`: prints one current snapshot
- `--follow`: continuously prints new status snapshots
- `--interval <SECONDS>`: changes standalone runtime dump interval
- `--stop`: stops standalone runtime status dumping

Shared-mode rules:

- shared CLI status follows the current leader snapshot
- `status --follow` works in shared mode
- non-stream start/stop controls are not supported in shared mode because shared leaders always keep cluster status snapshots enabled
- if no shared leader is running, `status` falls back to offline shared state

### `pause`

```bash
superseedr pause <INFO_HASH_HEX_OR_PATH>...
```

Pause one or more torrents.

### `resume`

```bash
superseedr resume <INFO_HASH_HEX_OR_PATH>...
```

Resume one or more torrents.

### `remove`

```bash
superseedr remove <INFO_HASH_HEX_OR_PATH>...
```

Remove one or more torrents from desired state without deleting payload data.

### `purge`

```bash
superseedr purge <INFO_HASH_HEX_OR_PATH>...
```

Remove one or more torrents and delete payload data when the file layout can be
resolved safely.

### `files`

```bash
superseedr files <INFO_HASH_HEX_OR_PATH>
```

List files for a torrent, including relative and resolved full paths when
available.

### `priority`

```bash
superseedr priority <INFO_HASH_HEX_OR_PATH> (--file-index <N> | --file-path <PATH>) <normal|high|skip>
```

Set priority for one file within a torrent.

Target the file by:

- `--file-index`
- `--file-path`

### `benchmark`

```bash
cargo run --release --features synthetic-load -- benchmark [OPTIONS]
```

Run adaptive local synthetic benchmarks for download-only, upload-only, and
mixed swarm scenarios.

This command is only available in builds compiled with the `synthetic-load`
feature. It generates local synthetic torrents and peers, keeps each benchmark
step inside `--disk-budget`, writes JSON summaries and per-sample metrics, and
removes generated data after each step unless `--keep-output` is set.

Text output reports each scenario's planned final size, each step's
current/final estimated disk use, ETA for the current scenario and full
benchmark, and a final capacity report. The final report estimates the clean
torrent and peer count per scenario, shows configured resource limits, reports
observed disk payload rates, and calls out likely bottleneck signals. If a step
shows overload symptoms, benchmark mode retries the step before stopping that
scenario and continuing with the next one.

Common options:

- `--start-torrents` and `--max-torrents`
- `--start-peers` and `--max-peers`
- `--max-steps`
- `--duration-secs`
- `--disk-budget`
- `--size-per-torrent`
- `--piece-size`
- `--target-gbps`
- `--transport tcp|utp|all`
- `--issue-retries`
- `--retry-delay-ms`
- `--out`

For the full benchmark and lower-level `synthetic-load` harness guide, see
[`docs/synthetic-benchmark.md`](synthetic-benchmark.md).

## Online And Offline Behavior

### Standalone Online

With a running standalone client, control commands queue to the local runtime.

Examples:

- `pause`
- `resume`
- `remove`
- `priority`
- `stop-client`

### Standalone Offline

When no standalone runtime is running, supported commands operate from persisted
local state.

Offline-capable read commands:

- `status`
- `journal`
- `torrents`
- `info`
- `files`

Offline-capable mutation commands:

- `pause`
- `resume`
- `remove`
- `priority`
- `purge` when the file layout can be resolved safely

### Shared Online

With a running shared leader:

- shared read commands follow cluster state
- mutating commands queue through the shared inbox for the leader

Examples:

- follower-issued `pause` is queued and applied by the leader
- shared `status` reads the leader snapshot

### Shared Offline

When shared mode is enabled but no leader is running:

- shared `status` falls back to offline shared state
- offline-capable shared mutations write shared config directly instead of queueing

Offline-capable shared mutations:

- `pause`
- `resume`
- `remove`
- `priority`
- `purge` when the file layout can be resolved safely

## Shared Mode Notes

### Cross-Host `.torrent` Adds

In shared mode, a `.torrent` path is only portable across hosts if the `.torrent`
file lives on the shared root.

Good:

```bash
superseedr add "/shared/root/shared-fixtures/example.torrent"
```

Not portable across hosts:

```bash
superseedr add "/home/me/local-only/example.torrent"
```

Magnet links are naturally portable across hosts.

### Shared Status Behavior

Shared leaders always keep cluster status snapshots enabled.

That means:

- `status --follow` is supported in shared mode
- `status --interval ...` is not supported in shared mode
- `status --stop` is not supported in shared mode

### Shared Root Requirements

Shared runtime startup requires:

- an existing shared root
- a writable shared root
- write access to the host-specific shared runtime area

If the shared root is missing or not writable, startup fails with an explicit
shared-root accessibility error.

See [`docs/shared-config.md`](shared-config.md) for the full shared-mode and
cluster guide.

## JSON Output

With `--json`, successful commands return a common envelope:

```json
{
  "ok": true,
  "command": "status",
  "data": {}
}
```

Errors return:

```json
{
  "ok": false,
  "command": "status",
  "error": "..."
}
```
