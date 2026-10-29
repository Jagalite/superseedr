# Configuration and Backup Files

This page describes where Superseedr keeps configuration files, which backup
files it writes, and how often those backups are refreshed.

For the exact paths on a specific machine, prefer:

```bash
superseedr show-configs
superseedr show-configs --all
superseedr --json show-configs --all
```

`show-configs` is the source of truth because config roots differ by operating
system, install type, environment variables, and shared-mode setup.

## Opt-in XDG Directories

Use `superseedr --xdg-dirs` to select the XDG layout at launch, or set
`SUPERSEEDR_XDG_DIRS=true` in the environment of every client and CLI invocation.
The default remains the existing platform layout. `--xdg-dirs=false` overrides
the environment for one invocation.

```bash
superseedr --xdg-dirs --json show-configs --all
superseedr --xdg-dirs
superseedr --xdg-dirs status
```

| Content | XDG location | Default base when unset, empty, or relative |
| --- | --- | --- |
| Settings, metadata, settings backups, launcher sidecars | `$XDG_CONFIG_HOME/superseedr/` | `$HOME/.config` |
| Saved torrent files, default watch and processed folders | `$XDG_DATA_HOME/superseedr/` | `$HOME/.local/share` |
| Local logs, status, journal, lock, recovery mirrors (`backups/`) | `$XDG_STATE_HOME/superseedr/` | `$HOME/.local/state` |

Only absolute XDG base paths are accepted, following the
[XDG Base Directory Specification](https://specifications.freedesktop.org/basedir/latest/).
There is currently no separate disposable cache directory, so `XDG_CACHE_HOME`
is not used. Shared config and host runtime paths remain under the selected
shared mount; local CLI logs and recovery mirrors use the XDG state directory.
If an XDG directory cannot be created, startup fails instead of silently
selecting the platform layout.

On first use, if the XDG configuration is absent, Superseedr imports the existing
platform configuration automatically. Settings, torrent metadata, launcher host
identity, saved torrent files, and default watch/processed queues are copied.
The legacy `persistence/` directory is copied into the XDG state directory,
preserving peer restrictions, RSS state, activity/network history, the event
journal, and other persisted runtime state. Runtime locks and logs are not copied.
Conflicting destination files stop the import before settings are published;
existing XDG configurations are not supplemented with stale legacy state.
Symlinked files are copied as regular files. Broken links and nested directory
symlinks stop the import with an error. Host-ID import is recorded independently,
so clearing the imported host ID remains effective in shared mode too.
Managed torrent-source paths in the imported settings are rewritten to their new
XDG data locations. Explicit watch-folder overrides, external torrent sources,
magnet URLs, and payload download paths remain unchanged. If the old launcher
selects shared configuration, that selection and host identity are copied; the
shared catalog remains on its existing mount. An environment or working-directory
shared selection takes precedence, so unrelated standalone settings are not imported.
Missing torrent sources keep their original references; they do not block importing
other settings and remain unavailable until repaired.

Original files are retained. Existing XDG settings always take precedence, even
if empty or invalid; invalid configuration is reported rather than replaced.
A failed import stops startup and can be retried. Once imported, subsequent XDG
launches use and save the XDG configuration without re-importing the old settings.
Launching without the flag or environment setting still uses the old layout.
Stop the old client before switching layouts. Standalone import holds its runtime
lock and refuses to copy while that client is active. Destination directories are
checked after resolving symlinks to prevent recursively copying into the source.

Use the same layout for the running client and its CLI commands. Installed app
and protocol-handler launches also need the environment option in their launch
environment; a shell export does not configure a desktop launcher automatically.

## Per-torrent download diagnostics

Native downloads write bounded JSON Lines diagnostics under the host runtime log
directory in `torrents/<info-hash>.jsonl` (with two rotated segments). The default
detail is `debug` during metadata acquisition and downloading. Collection stops
on pause or when the selected download completes. Use `superseedr show-configs --all`
to find the runtime log directory for the current host.
The browser build has no persistent torrent diagnostic files. A native process
must own the runtime log directory before it starts the diagnostic writer.

The settings file accepts a global policy and optional overrides keyed by a
canonical lowercase 40-character info hash:

```toml
[download_diagnostics.global]
detail = "debug" # off, summary, debug, trace
scope = "downloads_only" # or always, to continue while seeding

[download_diagnostics.torrents."0707070707070707070707070707070707070707"]
detail = "trace"
```

Omitted override fields inherit the global value. Settings reload applies level
changes to active sessions without restarting them. `trace` is explicit and
bounded to at most 100 detailed request events per torrent per second. Each
torrent retains up to three 2 MiB segments; the host diagnostic directory is
pruned toward a 128 MiB total and seven-day retention. Ordinary application logs
keep their separate policy.

To capture five minutes of Trace for a running torrent without changing its
saved policy, run `superseedr trace-torrent <info-hash>`. Use `--seconds N` for a
duration from 1 to 3600 seconds. The override expires automatically; the same
session then resumes its saved detail level. The command requires a running
native client.
The `download_diagnostics` map in a runtime status snapshot shows each registered
torrent's effective policy, collection epoch, remaining temporary Trace time,
log path, and loss counters.
Log records also group suppressed details by session, peer, capacity, request,
discovery, storage, and other reasons.

## Standalone Mode

Standalone mode uses the normal per-user application config and data
directories.

| File or directory | Purpose |
| --- | --- |
| `<config-dir>/settings.toml` | Primary standalone settings file. |
| `<config-dir>/torrent_metadata.toml` | Runtime metadata cache for torrent names, file lists, sizes, and file priorities. |
| `<config-dir>/backups_settings_files/` | Timestamped standalone settings backups. |
| `<data-dir>/torrents/` | Persisted copies of added `.torrent` files, named by info hash. |

The exact `<config-dir>` and `<data-dir>` are platform-specific. Use
`superseedr show-configs --all` to print them.

## Launcher Sidecars

These per-user files live in the normal `<config-dir>` even when shared mode is
active. They let installed app launches and protocol-handler launches enter
shared mode without relying on shell environment variables.

| File | Purpose |
| --- | --- |
| `<config-dir>/launcher_shared_config.toml` | Persisted shared mount root from `superseedr set-shared-config`. |
| `<config-dir>/launcher_host_id.toml` | Persisted shared host id from `superseedr set-host-id`. |

## macOS Handler Diagnostics

The signed macOS package keeps protocol-handler diagnostics under the always-local
application log directory, including when Superseedr uses shared mode:

| File | Purpose |
| --- | --- |
| `<data-dir>/logs/handler/installer.log` | Soft-failure-safe default-handler registration and verification results from package installation. |
| `<data-dir>/logs/handler/handler.log` | Native magnet and `.torrent` handler launches, registration outcomes, and forwarding errors. |

These logs record handler types, bundle identifiers, process IDs, and error codes.
They do not record magnet URLs, torrent filenames, or source paths.

## Shared Mode

Shared mode stores cluster-wide configuration under the shared mount root:

```text
<mount-root>/superseedr-config/
```

| File or directory | Purpose |
| --- | --- |
| `<mount-root>/superseedr-config/settings.toml` | Cluster-wide shared settings. |
| `<mount-root>/superseedr-config/catalog.toml` | Cluster-wide torrent catalog. |
| `<mount-root>/superseedr-config/torrent_metadata.toml` | Shared runtime metadata cache for torrent names, file lists, sizes, and file priorities. |
| `<mount-root>/superseedr-config/hosts/<host-id>/config.toml` | Host-specific settings such as client port and watch folder. |
| `<mount-root>/superseedr-config/torrents/` | Canonical shared copies of added `.torrent` files, named by info hash. |
| `<mount-root>/superseedr-config/cluster.revision` | Revision marker used by running nodes to notice shared config changes. |
| `<mount-root>/superseedr-config/backups/catalog/` | Time-bucketed shared catalog safety snapshots. |

The shared mount root can come from `SUPERSEEDR_SHARED_CONFIG_DIR`, current
working directory discovery, persisted launcher config, or an explicit
conversion command. Use `superseedr show-shared-config` to see which source is
active.

## Critical Recovery Mirrors

Superseedr also writes a best-effort critical recovery mirror under the normal
user home directory:

```text
~/.superseedr/backups/
```

With `--xdg-dirs`, these mirrors instead live under
`$XDG_STATE_HOME/superseedr/backups/` (default
`$HOME/.local/state/superseedr/backups/`).

These mirrors are fully refreshed. Superseedr builds a complete temporary
`latest` tree and swaps it into place, so stale files from older mirror contents
are removed on a successful refresh.

### Standalone Critical Mirror

```text
~/.superseedr/backups/local-config/latest/
├─ settings.toml
└─ torrents/
   └─ <info-hash>.torrent
```

### Shared Critical Mirror

```text
~/.superseedr/backups/shared-config/latest/
├─ settings.toml
├─ catalog.toml
├─ hosts/
│  └─ <host-id>/
│     └─ config.toml
├─ backups/
│  └─ catalog/
│     └─ catalog_YYYYMMDD_HH.toml
└─ torrents/
   └─ <info-hash>.torrent
```

The critical mirror intentionally excludes non-critical runtime files:

- `torrent_metadata.toml`
- `cluster.revision`
- logs, status snapshots, journals, and other runtime telemetry
- downloaded payload data

The mirror is meant to help recover the configuration and the persisted
`.torrent` sources needed to reload torrents. It is not a full runtime-state or
download-data backup.

## Backup Cadence

| Backup | Cadence | Retention |
| --- | --- | --- |
| Standalone timestamped settings backups | Every standalone settings save. | Latest 64 `settings_*.toml` files. |
| Standalone critical mirror | Best-effort refresh after each successful standalone settings save. Metadata-only updates do not refresh it. | One fully refreshed `latest` tree. |
| Shared critical mirror | Every running shared-mode node schedules its first best-effort local refresh after 15 minutes and repeats it every 15 minutes. The filesystem work runs in a background worker and shared-config saves do not trigger additional refreshes. | One fully refreshed `latest` tree per node. |
| Shared catalog safety snapshots | Before overwriting a changed shared `catalog.toml`, at most once per active time bucket. | Depends on catalog size; see below. |

Shared catalog safety snapshots are stored as:

```text
<mount-root>/superseedr-config/backups/catalog/catalog_YYYYMMDD_HH.toml
```

The bucket size and retention scale with catalog size:

| Torrent count | Snapshot bucket | Retained snapshots |
| --- | --- | --- |
| 0-999 | 1 hour | 16,384 |
| 1,000-9,999 | 3 hours | 4,096 |
| 10,000-99,999 | 6 hours | 1,024 |
| 100,000-999,999 | 12 hours | 256 |
| 1,000,000+ | 24 hours | 64 |

## Recovery Notes

Start with `superseedr show-configs --all` on the target machine so you know
which local and shared paths are active.

For standalone recovery, the critical files are:

- `settings.toml`
- any referenced `.torrent` files from `torrents/`

For shared recovery, the critical files are:

- `settings.toml`
- `catalog.toml`
- `hosts/<host-id>/config.toml`
- time-bucketed catalog snapshots from `backups/catalog/`
- any referenced `.torrent` files from `torrents/`

If a shared catalog was accidentally overwritten or truncated, first inspect the
time-bucketed catalog snapshots under `superseedr-config/backups/catalog/`.
Each running shared-mode node also mirrors those snapshots to its own
`~/.superseedr/backups/shared-config/latest/backups/catalog/`, so the normal
config directory keeps a recent local copy of both the latest critical state and
the catalog snapshot history.

Configuration and `.torrent` files can include tracker URLs, folder names, and
other operational details. Treat backup copies as private operational data.

## Graph history

Graph history is saved across sessions by default. To disable loading and saving
both network and activity graph history, set this top-level option in
`settings.toml` (or the shared settings file in shared mode), then restart:

```toml
persist_graph_history = false
```

Live graphs continue collecting data for the current session. Existing history
files are retained but are neither loaded nor updated while this option is false.
Set it to `true` to restore the default behavior. Torrent state, RSS history, and
lifetime transfer totals continue to be saved.

Graph range selectors adapt to the current panel's available history. `AUTO` and
`1m` are always available; larger ranges appear when samples extend beyond the
preceding range. Idle (zero-valued) samples count as history. Keyboard navigation
skips hidden ranges, and unavailable selections display the `1m` view.
