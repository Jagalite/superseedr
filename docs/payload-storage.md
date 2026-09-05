# Portable payload storage

The native app creates one `PayloadStorage::native()` per torrent and injects it through `TorrentManager::from_torrent_with_storage` or `from_magnet_with_storage`. Every manager payload task receives that capability. The existing constructors select native storage for compatibility with existing callers. `TorrentParameters` and `state.rs` remain unchanged.

The browser API is exported through `superseedr::web_integration::storage`. A dedicated worker opens `PayloadStorage::opfs(namespace).await`; the host supplies a stable torrent identity and stable logical paths, without native download-directory prefixes. Reuse the same namespace and manifest after restarting. The browser physical layout is versioned independently from the application catalog; it is not the old POC path-hash layout and does not silently migrate POC files.

## Operations and authority

| Operation | Guarantee |
| --- | --- |
| `allocate` | Preserve existing fresh-placeholder, partial resize, skip, and padding behavior. |
| `read` / `write` | Map bounded torrent spans across logical files. Successful writes are visible to later reads. Missing skipped spans and sparse tails read as zeros; these bytes still require piece verification. |
| `has_complete_layout` / `probe` | Report physical layout facts, not cryptographic integrity or verified-piece availability. |
| `flush` | Drain admitted I/O and flush dirty sync handles; fallback streams have already closed. Native storage uses `sync_all`. The browser guarantee is that of the platform API, not immunity to eviction or hardware failure. |
| `close` | Drain and flush, close namespace handles, and invalidate every clone. Idempotent after success; a failed flush leaves the scope open for retry. |
| `delete` | Drain started I/O, close handles, and remove the supplied namespace files. Old clones stay closed even if removal partially fails. Native directories are removed individually so unrelated files are preserved. |

State retains piece availability, peer admission/removal, scheduling, priorities, and deletion intent. The manager retains retries, resource permits, result handling, and shutdown. The backend owns physical handles, buffer admission, and serialization; it has no torrent scheduler. Uploads, playback, and exports must consult manager verification state before reading. A storage read alone is not permission to serve an unverified piece.

## Browser ownership and limits

One exclusive Web Lock named `superseedr-payload-v1` is held for the dedicated worker lifetime. All torrents share its OPFS directory and cache. A second worker/tab fails with `WouldBlock`; it must wait for the owner to terminate and retry. Missing Web Locks or a non-dedicated-worker host fails explicitly. OPFS requires the browser's secure-context storage APIs.

Physical names combine SHA-256(namespace) and SHA-256(logical path). Paths remain presentation/manifest data and never become browser directory traversal. Changing a namespace or logical path changes physical identity. One physical file represents each non-padding torrent file.

The worker admits up to 32 payload operations and 64 MiB of requested read/write spans. Requests wait before backend-owned buffers are copied; a single span above 64 MiB is rejected and must be split. Temporary read buffers and JS copies add overhead, so this is not a total heap limit. The cache holds at most four files across all torrents. Its mutex pins each physical operation against eviction, and dirty sync handles flush before eviction. Failed eviction retains the handle and dirty status for retry.

Synchronous access handles are preferred and reused for both reads and writes. A non-callable or absent sync-handle factory selects the writable-stream fallback. The fallback closes each write transaction before returning and is expected to be slower for random writes to large files. A callable factory that rejects returns its actual error; lock contention or permission failures are not hidden by switching modes.

Quota exhaustion maps to `StorageFull`; origin contention to `WouldBlock`; access denial to `PermissionDenied`; unsupported APIs to `Unsupported`; missing files to `NotFound`; and stale scopes to `BrokenPipe`. Zero-progress physical writes return `WriteZero`. Partial multi-file failures are reported and must never mark a piece successfully written.

The browser requests persistent storage on a best-effort basis. The host must still handle quota errors and eviction, coordinate catalog checkpoints after payload flush, and rehash uncertain bytes on recovery. Website saving/export and durable browser application persistence are separate capabilities.

## Validation

Native tests cover layout semantics, read-after-write, cancelled-operation draining, close/delete barriers, and manager use of an injected in-memory backend. The [real-browser contract](../web/storage-contract/README.md) uses production sources without module stubs, testing synchronous access, writable streams, restart, contention, eviction, injected faults, and stale scopes. Chrome execution is automated in the browser CI job. Other browser engines still need their own qualification; a successful contract is not a complete browser torrent-client test.

The implementation passed 2,268 native tests (two opt-in tests excluded), 109 Wasm application tests, and all six Chrome storage-contract phases. Strict Clippy covers native full/minimal feature sets and the Wasm targets. These are local results; the added CI job has not been run remotely in this change.
