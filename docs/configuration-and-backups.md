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

The shared mount root can come from `SUPERSEEDR_SHARED_CONFIG_DIR`, persisted
launcher config, or an explicit conversion command. Use `superseedr
show-shared-config` to see which source is active.

## Critical Recovery Mirrors

Superseedr also writes a best-effort critical recovery mirror under the normal
user home directory:

```text
~/.superseedr/backups/
```

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
