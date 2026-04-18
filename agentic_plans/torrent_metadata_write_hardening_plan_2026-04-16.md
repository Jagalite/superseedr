# Torrent Metadata Write Hardening Plan

## Summary
`torrent_metadata.toml` is not primary configuration, but today startup reads it as part of resolved settings load. That means a malformed metadata snapshot can block startup even though the app could otherwise continue with empty metadata and regenerate it later.

For the current release, we will ship the bypass that ignores invalid `torrent_metadata.toml` and treats it as empty metadata. For the next release, we should harden the write path so corrupted metadata is much less likely in the first place.

## Current Release Decision
- Keep the startup bypass for `torrent_metadata.toml`.
- If the metadata file is present but invalid, log a warning and continue with `TorrentMetadataConfig::default()`.
- Do not broaden this behavior to primary config files such as shared `settings.toml`, `catalog.toml`, or host config files.

This keeps the current release unblocked while limiting the recovery behavior to a non-primary file that can be rebuilt over time.

## Observed Failure Mode
- Shared mode was enabled through the launcher-side shared-config pointer.
- Startup selected the shared config backend and attempted to read `settings.toml`, `catalog.toml`, host config, and `torrent_metadata.toml`.
- `torrent_metadata.toml` contained malformed TOML near EOF and startup aborted.
- The malformed file shape looked like a partially corrupted or overlapped write rather than an intentional serialized form.

## Why The Current Atomic Write Is Not Sufficient
The current helper atomically replaces the destination by writing to a temp path and renaming it, which is useful but incomplete.

Gaps in the current design:
- The temp file name is deterministic per target path rather than unique per write.
- Concurrent writers targeting the same file can collide on the temp path.
- A successful rename can still publish already-corrupted temp-file contents.
- There is no explicit durability step such as fsync on the temp file and parent directory.
- Shared-mode safety assumes a single writer via leader ownership, but the write layer itself does not enforce that invariant.
- Within one leader process, multiple write paths can still overlap on `torrent_metadata.toml`.

## Scope For Next Release
This follow-up should focus on write hardening, not on broad config redesign.

In scope:
- unique temp files for atomic writes
- temp-file cleanup behavior
- stronger serialization of settings and metadata writes inside the process
- explicit shared-mode ownership checks before shared-state mutation
- tests that simulate malformed metadata recovery and write-path contention assumptions

Out of scope:
- changing the meaning of shared-mode leader election
- changing the layered shared-config file layout
- broad recovery behavior for primary config files

## Implementation Plan

### 1. Make Atomic Writes Use Unique Temp Files
- Update the atomic write helper so each write attempt uses a unique temp file in the destination directory.
- Keep the rename-based replace behavior.
- Remove temp files when a pre-rename step fails.
- Add opportunistic cleanup for stale temp files left behind by crashes or interrupted runs.

### 2. Improve Durability Guarantees
- Flush written bytes before rename.
- Fsync the temp file after content is written.
- Fsync the parent directory after rename where supported and practical.

This does not make the system fully transactional, but it improves crash tolerance and makes network-share behavior less fragile.

### 3. Serialize Writes Inside The Process
- Introduce one in-process write coordinator for config and metadata persistence.
- Ensure `save_settings()` and `upsert_torrent_metadata()` cannot overlap on the same target files.
- Prefer one critical section around the whole read-modify-write operation rather than taking independent file writes one by one.

This applies to both normal mode and shared mode.

### 4. Enforce Shared Ownership At Mutation Boundaries
- In shared mode, require shared-state mutations to happen only when the process owns the shared lock and is operating as leader.
- Do not rely only on local role labels or capability flags in higher layers.
- Validate ownership at the mutation boundary for shared backend writes.

This is shared-mode-specific and complements, rather than replaces, in-process serialization.

### 5. Keep Metadata Recovery Narrow
- Continue treating invalid `torrent_metadata.toml` as recoverable.
- Do not silently default invalid primary config files.
- Regenerate metadata naturally through later runtime persistence once valid torrent metadata becomes available.

## Suggested Code Areas
- `src/fs_atomic.rs`
- `src/config.rs`
- `src/app.rs`
- any persistence coordination path that currently writes settings and metadata independently

## Test Plan
- Add unit tests proving invalid `torrent_metadata.toml` does not block startup in normal mode.
- Add unit tests proving invalid `torrent_metadata.toml` does not block startup in shared mode.
- Add tests for unique temp-file naming behavior.
- Add tests that temp files are removed on pre-rename failure where practical.
- Add tests covering serialized access to settings and metadata writes.
- Add shared-mode tests that reject shared writes when leader ownership is not held.
- Add regression coverage for metadata regeneration after a recovery load.

## Release Notes Guidance
- Current release note:
  - invalid `torrent_metadata.toml` no longer blocks startup; the file is treated as empty metadata and rebuilt later
- Next release note:
  - harden config and metadata persistence with safer temp-file handling and stronger write serialization

## Acceptance Criteria For The Follow-Up
- Shared and normal startup succeed when `torrent_metadata.toml` is malformed.
- Atomic writes no longer reuse a fixed temp path for the same target file.
- Shared backend writes verify ownership before mutating shared files.
- In-process overlapping writes to settings and torrent metadata are serialized.
- Temp-file leftovers are bounded and recoverable.
