# Persistence Module

This folder is the single ownership boundary for durable application state and
torrent payload I/O.

- `app.rs` exposes the injected application-persistence capability. Native
  composition delegates to the existing filesystem-backed configuration,
  histories, RSS state, and event journal implementations. Browser composition
  uses the in-memory backend.
- `payload.rs` contains native torrent payload allocation and random-access I/O.
  It remains a native-only concrete implementation until the torrent manager is
  migrated to an injected payload-persistence capability.
- `atomic.rs` and `serialization.rs` provide the shared native durability and
  versioned-codec primitives used by configuration, integrations, histories,
  RSS state, journals, and peer state.
- The history, RSS, and journal modules define their shared persisted data
  models and keep native filesystem execution behind their native modules.

Future filesystem, browser, or remote-object implementations belong behind the
appropriate application-state or payload capability; they should not be joined
into one universal storage interface.

For network history implementation:
- `persistence/network_history.bin` stores network-history runtime state.
- The file format is a custom binary format with an explicit magic header and `schema_version`.
- Persistence is sparse on disk: zero-only history buckets are omitted before writing.
- In-progress rollup accumulators are persisted alongside sparse tier points so restart does not need to reconstruct bucket phase from point counts.
- Restore is dense in memory: missing buckets are filled back in as zero-valued samples up to current wall time.
- Missing/corrupt `persistence/network_history.bin` is treated as recoverable and falls back to empty state.
- Legacy `persistence/network_history.toml` is ignored.

For RSS implementation:
- `settings.toml` keeps durable user config (`Settings.rss`).
- `persistence/rss.toml` keeps mutable RSS runtime state (history, sync metadata, per-feed errors).
- RSS history is retention-capped at 1000 entries; oldest entries are pruned first on persist.

The runtime should treat missing/corrupt `persistence/rss.toml` as recoverable and fall back to empty RSS state.
