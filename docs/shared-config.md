# Shared Config Cluster Mode

## What It Is

Shared config mode lets multiple Superseedr nodes participate in one cluster by
pointing them at the same mounted shared root.

- Every node is a full Superseedr client.
- Every node can run torrents and seed.
- One node is the leader at any given time.
- The leader is the only node that consumes the shared inbox and writes
  cluster-wide desired state.
- Non-leader nodes follow shared desired state and apply it locally.

Shared mode is enabled from the first available source in this order:

1. `SUPERSEEDR_SHARED_CONFIG_DIR`
2. persisted launcher shared-config sidecar
3. normal standalone mode

Host id is resolved from:

1. `SUPERSEEDR_SHARED_HOST_ID`
2. persisted launcher host-id sidecar
3. sanitized hostname fallback

This is mainly useful when:

- multiple machines share the same NAS or mounted volume
- installed browser or OS protocol launches do not inherit shell environment
- you want one shared torrent catalog with automatic leader failover

## Before You Start

Shared mode depends on a real writable shared root.

Before starting any node:

1. Create a dedicated shared folder on the mounted volume.
2. Mount that shared folder on every host.
3. Confirm every host can read and write inside it.
4. Start each client once so it can create its host-local shared files.

Use a dedicated folder inside the mounted volume, not the entire volume root.

Examples:

- Windows: `C:\Users\ExampleUser\Downloads\library\test`
- macOS: `/Volumes/seedbox/test`
- Linux: `/mnt/shared-drive/test`

If the shared root is missing, unmounted, or not writable, runtime startup will
fail with a shared-root accessibility error.

## Quick Start

### 1. Pick a Shared Root

Choose one dedicated shared folder per cluster.

Example:

```text
/mnt/shared-drive/test
```

Superseedr stores its cluster files under:

```text
/mnt/shared-drive/test/superseedr-config/
```

Payload data still lives under the shared root itself, not under
`superseedr-config/`.

For a complete map of shared config files, local launcher sidecars, recovery
mirrors, and backup cadence, see
[`configuration-and-backups.md`](configuration-and-backups.md).

### 2. Configure Each Host

You can use environment variables:

```bash
export SUPERSEEDR_SHARED_CONFIG_DIR=/mnt/shared-drive/test
superseedr
```

Or persist launcher-side setup once per user account:

```bash
superseedr set-shared-config /mnt/shared-drive/test
superseedr show-shared-config
```

Then start the client normally:

```bash
superseedr
```

Repeat on every host with the same shared root.

### 3. Confirm the Cluster

After startup:

- one node should become leader
- other nodes should run as followers
- each host should have a `hosts/<host-id>/` folder under the shared config root

Useful checks:

```bash
superseedr show-shared-config
superseedr status
superseedr journal
superseedr torrents
```

### 4. Use the Cluster

Once running, use the CLI normally:

```bash
superseedr add /path/to/file.torrent
superseedr pause <INFO_HASH_HEX_OR_PATH>
superseedr resume <INFO_HASH_HEX_OR_PATH>
superseedr remove <INFO_HASH_HEX_OR_PATH>
superseedr purge <INFO_HASH_HEX_OR_PATH>
```

## Environment Variables

### `SUPERSEEDR_SHARED_CONFIG_DIR`

Absolute path to the shared mount root.

Superseedr automatically uses:

```text
<mount-root>/superseedr-config/
```

Example:

```bash
SUPERSEEDR_SHARED_CONFIG_DIR=/mnt/shared-drive/test
```

This has the highest precedence and overrides any persisted launcher config.

### `SUPERSEEDR_SHARED_HOST_ID`

Optional explicit host id for selecting:

```text
hosts/<host-id>/config.toml
```

If unset, Superseedr falls back to a sanitized hostname.

`SUPERSEEDR_HOST_ID` is still accepted as a legacy fallback, but
`SUPERSEEDR_SHARED_HOST_ID` is the canonical name.

Example:

```bash
SUPERSEEDR_SHARED_HOST_ID=seedbox-a
```

## Launcher Commands

These commands persist launcher-side shared mode without editing runtime
`settings.toml`:

```bash
superseedr set-shared-config /mnt/shared-drive/test
superseedr show-shared-config
superseedr clear-shared-config

superseedr set-host-id seedbox-a
superseedr show-host-id
superseedr clear-host-id
```

Rules:

- `set-shared-config` requires an absolute path
- the path may be either the mount root or an explicit `.../superseedr-config`
- Superseedr normalizes and stores the mount root in a launcher sidecar file
- `set-host-id` stores a sanitized host id in a separate launcher sidecar file
- `show-shared-config` reports the effective source and resolved paths
- `show-host-id` reports the effective host id and its source
- `clear-shared-config` disables persisted shared mode unless the env var is set
- `clear-host-id` removes the persisted host id unless the env var is set

## Conversion Commands

You can convert between standalone local config and layered shared config:

```bash
superseedr to-shared /mnt/shared-drive/test
superseedr to-standalone
```

Behavior:

- `to-shared` reads current standalone local config and writes layered shared files
- `to-standalone` reads the currently selected shared config and writes local standalone files
- neither command modifies launcher sidecars by itself

## Shared Root Layout

```text
/mnt/shared-drive/test/
  superseedr-config/
    settings.toml
    catalog.toml
    torrent_metadata.toml
    cluster.revision
    hosts/
      seedbox-a/
        config.toml
        logs/
        persistence/
        status.json
      desktop-a/
        config.toml
        logs/
        persistence/
        status.json
    torrents/
    inbox/
    processed/
    staged-adds/
    status/
      leader.json
    superseedr.lock
  downloads/
  library/
```

Different hosts may mount the same shared root at different local paths.

Examples:

- Windows: `C:\Users\ExampleUser\Downloads\library\test`
- macOS: `/Volumes/seedbox/test`
- Linux: `/mnt/shared-drive/test`

Each host should point `SUPERSEEDR_SHARED_CONFIG_DIR` or `set-shared-config` at
its own local mount path for the same shared root.

## Layered Files

### `settings.toml`

Cluster-wide shared settings.

Examples:

- shared `client_id` default
- RSS settings
- shared UI and performance settings
- shared default download folder

### `catalog.toml`

Cluster-wide desired torrent state.

Examples:

- torrent list
- pause and resume state
- remove and purge intent
- per-torrent download path
- per-torrent file priorities

All nodes read this file and converge local runtime to it.

### `torrent_metadata.toml`

Leader-written derived torrent metadata, including persisted file lists used by
commands like `files`, `info`, reverse path lookup, and offline purge.

### `hosts/<host-id>/config.toml`

Host-local runtime settings on the shared root.

This file is bootstrapped by runtime startup if it does not exist yet.
CLI shared loads also bootstrap this host file when shared cluster settings
already exist.

Common host-local fields:

- optional `client_id`
- `client_port`
- `watch_folder`

### `hosts/<host-id>/`

Host-local runtime artifacts on the shared root.

Examples:

- `hosts/<host-id>/logs/`
- `hosts/<host-id>/persistence/network_history.bin`
- `hosts/<host-id>/persistence/activity_history.bin`
- `hosts/<host-id>/persistence/rss.toml`
- `hosts/<host-id>/persistence/event_journal.toml`
- `hosts/<host-id>/status.json`

## Leadership

Shared mode still uses the file-lock mechanism.

- Leader: holds `superseedr.lock`
- Follower: does not hold the lock
- If a follower later acquires the lock, it promotes itself to leader without a restart

Leader responsibilities:

- consume `inbox/`
- write shared desired state
- write `settings.toml`, `catalog.toml`, and `torrent_metadata.toml`
- write `cluster.revision`
- update the current leader snapshot

Followers remain active torrent clients. They do not stop running torrents just
because they are not the leader.

## Cluster Convergence

The leader writes `cluster.revision` after shared desired-state changes.

Host-only changes do not bump `cluster.revision`.

Followers:

- watch shared state
- reload layered config when `cluster.revision` changes
- converge local runtime to the shared catalog

Convergence includes:

- starting newly added torrents
- pausing or resuming torrents
- applying file-priority changes
- applying download-path changes
- removing torrents deleted from shared desired state

### Remove vs Purge

`remove` and `purge` are cluster-wide.

- `remove` removes the torrent from desired state but keeps payload data
- `purge` removes the torrent and deletes payload data

## Watch Folder Model

Each host can still define its own local ingress folder:

```toml
client_port = "RANDOM"
watch_folder = "/srv/local-watch"
```

With randomization enabled, each host asks the OS for a new available port when
it starts. The host config retains the randomization choice while runtime status
reports the port that was actually bound. `client_port = "RANDOM"` is also
accepted in standalone and shared host configs.

Behavior:

- leader watches its own local `watch_folder`
- leader also watches `inbox/`
- followers watch their own local `watch_folder`
- followers relay supported files into `inbox/`
- followers do not write shared desired state directly from local watch ingress

Supported dropped file types:

- `.torrent`
- `.magnet`
- `.path`
- `.control`

### Cross-Host Add Rules

This is the most important ingest rule in a multi-host cluster:

- magnet adds are naturally portable
- `.torrent` adds work across hosts when the torrent file is staged onto the shared root
- shared-mode `.path` adds are encoded relative to the shared root and resolved by the leader against its own local mount path

In practice:

- if you add a `.torrent` from a follower watch folder, Superseedr may stage it into `staged-adds/`
- if you add a path-based torrent source in shared mode, the follower resolves it and stages the actual torrent file for the leader

## Data Path Rules

Shared mode requires all payload data to live under the shared root.

Rules:

- shared-mode download paths must resolve inside the shared root
- shared-mode download paths are stored root-relative in layered config
- host-specific path translation is not supported

Examples:

- `default_download_folder = ""` resolves to the shared mount root itself
- `default_download_folder = "downloads"` resolves to `<mount-root>/downloads`

Example shared catalog entry:

```toml
[[torrents]]
name = "Shared Collection"
download_path = "library/shared-collection"
```

## CLI Behavior in Shared Mode

### Read Commands

Commands like these read shared state:

- `status`
- `journal`
- `torrents`
- `info`
- `files`
- `tags list`
- `tags show`

`status` in shared mode follows the current leader snapshot rather than a purely
local standalone-style node status view.

### Mutating Commands

Mutating commands include:

- `add`
- `pause`
- `resume`
- `remove`
- `purge`
- `priority`
- `tags create`, `tags rename`, and `tags delete`
- `tags assign` and `tags remove`

Behavior:

- if a leader is running, shared mutating commands are queued through `inbox/`
- if no leader is running, offline-capable CLI commands mutate shared config directly using the offline path

That means shared CLI is not queue-only in all cases.

### `stop-client`

`stop-client` in shared mode targets the leader through the shared inbox.

Use it as a cluster-leader stop request, not a guaranteed “stop only this local
follower process” command.

## First-Run and Bootstrap Behavior

Runtime startup bootstraps shared host state.

CLI bootstrap behavior is intentionally narrower:

- in standalone mode, CLI can create first-run local settings when no local client is running
- in shared mode, CLI does not create an entirely new cluster from nothing
- in shared mode, if shared cluster settings already exist, CLI can bootstrap the current host's missing `hosts/<host-id>/config.toml`

Practical implications:

- start the client once on one host to establish the shared cluster files
- start the client once on each additional host if you want all host runtime folders to exist immediately
- if the shared root exists but shared `settings.toml` does not, CLI reports that the client has never started yet instead of silently creating a new cluster

## Status and Journal Semantics

### Status

Shared `status` is cluster-oriented and follows the leader snapshot. During
manual failover or failback, brief lag is expected while watches and snapshots
catch up.

### Journal

Shared journal output is merged:

- host-specific health/runtime events remain host-scoped
- shared command events are shared-scoped

This gives one combined operational view while preserving the difference between
host-local health and cluster-wide actions.

## Troubleshooting

### The shared root is missing or unmounted

Make sure:

- the mount exists on this host
- `SUPERSEEDR_SHARED_CONFIG_DIR` points to the mounted shared root
- the process can read and write that location

### A host cannot start in shared mode

Check:

- unique host id
- write access to `hosts/<host-id>/`
- write access to `hosts/<host-id>/config.toml`
- write access to `hosts/<host-id>/logs/`

### A cross-host add fails

Prefer:

- magnets
- `.torrent` files staged onto the shared root

Avoid relying on host-local absolute paths being valid on another OS.

## Example

Shared root:

```text
/srv/shared-drive/test/
  superseedr-config/
    settings.toml
    catalog.toml
    torrent_metadata.toml
    cluster.revision
    hosts/
      seedbox-a/
        config.toml
        logs/
        persistence/
        status.json
      desktop-a/
        config.toml
        logs/
        persistence/
        status.json
    inbox/
    processed/
    staged-adds/
    status/
      leader.json
    superseedr.lock
  downloads/
```

Shared settings:

```toml
# settings.toml
client_id = "shared-node"
default_download_folder = "downloads"
global_upload_limit_bps = 8000000
```

Shared catalog:

```toml
# catalog.toml
[[torrents]]
name = "Shared Collection"
torrent_or_magnet = "shared:torrents/0123456789abcdef0123456789abcdef01234567.torrent"
download_path = "downloads/shared-collection"
```

Host config:

```toml
# hosts/seedbox-a/config.toml
client_port = "RANDOM"
watch_folder = "/srv/local-watch"
```

## Summary

- Shared mode is opt-in.
- Environment configuration overrides persisted launcher configuration.
- The leader is the single writer for cluster-wide desired state.
- Followers are still active torrent clients.
- Shared mode supports failover and failback.
- Shared payload data must live under the shared root.
- Shared CLI reads cluster state and queues leader mutations when a leader is available.
