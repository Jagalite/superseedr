# Named Libraries

Named libraries let one Superseedr installation switch between independent
configurations. A library is a name associated with an absolute root directory.
Only the selected library is loaded, and only its torrent manager runs.

## Local and Shared Roots

A library root does not need to be on a network share. It can be any existing,
writable directory:

| Root location | Supported | Behavior |
| --- | --- | --- |
| Local disk directory | Yes | Uses the shared/layered layout locally. |
| Mounted network or removable directory | Yes | Can participate in shared leader/follower mode. |
| Platform-default standalone config directory | No, not directly | Remains the single standalone configuration. Convert it into a library first. |

Every named library uses this layout, even when its root is on a local disk:

```text
<library-root>/
  superseedr-config/
    settings.toml
    catalog.toml
    torrent_metadata.toml
    hosts/
```

The word "shared" describes the configuration backend and directory layout; it
does not require multiple computers or shared storage.

The always-local library registry is stored in the normal Superseedr config
directory as `libraries.toml`. It contains library names and paths, not the
libraries' torrent catalogs.

## Default Standalone Configuration

When no named library, shared-config environment variable, or persisted shared
root is selected, Superseedr continues to use its normal platform-default config
and data directories. Named libraries do not change or delete that standalone
configuration.

Superseedr cannot currently register several standalone config directories and
switch among them directly. To make the current standalone configuration a
named library, convert it to a library root:

```bash
# Stop the running client before converting its persisted configuration.
mkdir -p /absolute/path/to/primary
superseedr to-shared /absolute/path/to/primary
superseedr library add primary /absolute/path/to/primary
superseedr library use primary
```

`to-shared` copies the current standalone configuration into the layered layout.
It does not remove the standalone files or select the new root by itself.

To create a new empty local library instead:

```bash
mkdir -p /absolute/path/to/archive
superseedr library add archive /absolute/path/to/archive \
  --description "Local archive configuration"
superseedr library use archive
```

The registered path must be absolute and must exist before it can be selected.
Superseedr initializes and validates the `superseedr-config` backend when the
library is first loaded; configuration files are persisted as settings and
catalog state are saved.

## Managing Libraries

```bash
superseedr library list
superseedr library add <NAME> <PATH> [--description <TEXT>]
superseedr library rename <NAME> <NEW_NAME>
superseedr library remove <NAME>
superseedr library use <NAME>
superseedr library show [NAME]
superseedr library open [NAME]
```

Press `c` to open Config, select **Libraries**, and press `Space`. From there you
can add, rename, remove, reveal, or switch libraries. Adding a library prompts
for its name and then opens the directory picker to select the library root; the
path does not need to be typed manually.

Library management stays inside Config. On wider layouts the settings list
remains visible and Libraries replaces the existing detail panel; on compact
layouts it uses that same panel area and `Esc` returns to the settings list.

When the Libraries panel opens, Superseedr reads registered catalogs in a
background task. Each library row shows its torrent count, and the highlighted
library shows an alphabetized list of torrent names. Use `Page Up`, `Page Down`,
`Home`, and `End` to scroll that list. Loading an inactive preview is read-only:
it does not acquire the library as active or start its torrent manager. Missing
and unreadable catalogs are reported in the preview without blocking the TUI.

Removing a library only removes its registry entry. It never deletes the root,
configuration, downloaded data, or torrent files. A missing or unmounted path
also remains registered but is shown as unavailable.

## Switching and Runtime Behavior

Only one named library runs in a Superseedr process. A live TUI switch:

1. validates and preloads the destination configuration
2. flushes and gracefully shuts down the current engine
3. releases the current config lock and runtime resources
4. records the new active library
5. launches a fresh Superseedr process for that library

The old library is not left seeding or downloading in the background. Running
several libraries simultaneously requires separate Superseedr processes and is
outside the named-library switching workflow.

`superseedr library use <NAME>` is intended for switching while the client is
stopped. If a client is running, switch from the TUI so its current state can be
persisted safely.

For a one-command override without changing the saved active library:

```bash
superseedr --library archive torrents
superseedr --library archive show-configs
```

Selection precedence is:

1. `--library <NAME>`
2. `SUPERSEEDR_SHARED_CONFIG_DIR`
3. active named library
4. persisted `set-shared-config` root
5. normal standalone paths

Because the environment variable has higher precedence, a process started with
`SUPERSEEDR_SHARED_CONFIG_DIR` cannot switch named libraries until that override
is removed.

## Libraries Versus Download Paths

Use separate libraries when you want separate catalogs, settings, runtime state,
and torrent-manager lifecycles. Use one configuration with different download
paths when you only want to organize payload data into folders.

For example, these torrents remain in one library and one running engine:

```bash
superseedr add --path /data/active /path/to/first.torrent
superseedr add --path /data/archive /path/to/second.torrent
```

Changing a torrent's download path does not create another library or another
torrent manager.

For the underlying file layout and multi-host behavior, see
[`configuration-and-backups.md`](configuration-and-backups.md) and
[`shared-config.md`](shared-config.md).
