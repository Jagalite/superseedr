# Persistence Module

This folder owns non-settings persisted state.

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

Torrent tag definitions and assignments live with the torrent catalog rather
than host-local runtime persistence. Standalone mode stores them in
`settings.toml`; shared mode stores them atomically in `catalog.toml` so the
leader, followers, TUI, and CLI observe one shared tag model.
