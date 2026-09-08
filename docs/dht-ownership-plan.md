# DHT Ownership Plan

## Goal
Replace the external `mainline` dependency with a first-party DHT implementation that fits superseedr's runtime model, networking needs, and dependency posture.

This is a strategic ownership plan, not a short-term cleanup task.

## Why This Exists
Today DHT is enabled by default through the `dht` feature in `Cargo.toml` and is provided by `mainline`.

That works, but it carries tradeoffs:
- The DHT runtime is not native to the rest of superseedr's async architecture.
- The dependency subtree is larger and riskier than the narrow DHT feature surface that superseedr actually uses.
- The upstream crate remains IPv4-oriented, while superseedr is moving toward broader IPv6 support.
- Product control is limited when DHT behavior, retry policy, or protocol support need to evolve on superseedr's schedule.

## Current State
The current integration boundary is narrower than a full generic DHT stack:
- App startup builds the DHT handle, bootstraps against configured routers, and retries bootstrap when needed in `src/app/mod.rs`.
- Each torrent manager owns a DHT lookup task that repeatedly calls `get_peers(info_hash)` and forwards discovered peers into normal peer admission flow in `src/torrent_manager/manager.rs`.
- The manager-side DHT channel is still typed as `Vec<SocketAddrV4>`, which reflects the current upstream IPv4 constraint in `src/torrent_manager/manager.rs`.

Superseedr does not currently depend on a broad DHT feature set such as arbitrary key/value storage or generic application-facing DHT APIs. The immediate product use case is BitTorrent peer discovery.

## Recommendation
If superseedr chooses to own DHT, it should not start by building a general-purpose replacement for `mainline`.

The first target should be a narrow BitTorrent-only DHT client that supports:
- bootstrap
- routing table maintenance
- `get_peers`
- `announce_peer`
- token validation sufficient for BitTorrent interoperability
- clean integration with the existing Tokio runtime, manager channels, and `SocketAddr`-based peer pipeline

This keeps the project aligned with the actual product need: reliable peer discovery.

## Non-Goals For Phase 1
- Recreating every `mainline` feature
- BEP 44 or mutable/immutable item storage
- generic library-quality public APIs
- ambitious operator tooling on top of DHT before the runtime is stable
- immediate IPv6 DHT parity on day one if it materially delays a solid IPv4-first replacement

## Design Principles
- Keep DHT integration native to the existing Tokio runtime.
- Normalize discovered peers to `SocketAddr` at the boundary.
- Preserve feature gating so DHT can remain optional at build time.
- Prefer explicit, testable state machines over hidden background behavior.
- Treat bootstrap, routing health, and query concurrency as first-class operational behavior.
- Design for future IPv6 support, even if the first cut remains IPv4-first.

## Target End State
Superseedr owns a DHT subsystem that:
- runs on the same async runtime as the rest of the client
- exposes a small internal API tailored to torrent peer discovery
- emits peers directly into torrent-manager flow without adapter glue
- supports the existing bootstrap warning/retry UX
- can be extended toward IPv6/BEP 32 without waiting on upstream changes
- removes the `mainline` dependency from the default DHT path

## Architecture Direction

### Proposed Internal Boundary
Introduce an internal DHT module with a small surface, for example:
- `DhtService`
- `DhtHandle`
- `DhtCommand`
- `DhtEvent`
- `LookupStream` or manager callback/channel integration for peer discovery results

The important point is not the exact naming. The important point is that the boundary should match how superseedr already works:
- app owns lifecycle and bootstrap policy
- torrent managers request lookups and receive peer discoveries
- the DHT runtime owns socket I/O, routing, tokens, retries, and query fanout

### Query Flow
1. App starts DHT service and bootstrap workers.
2. Torrent manager requests `get_peers(info_hash)`.
3. DHT service performs iterative lookup against its routing table.
4. Peer results are streamed back as `SocketAddr`.
5. Torrent manager merges DHT peers with tracker and PEX peers through existing admission logic.

### Bootstrap / Health Model
Bootstrap behavior should preserve the current product expectation:
- startup attempts bootstrap from configured routers
- failure should degrade gracefully rather than disable the whole client
- retries should happen automatically
- warnings should remain visible in the app UI/system warning path

## Implementation Plan

### Phase 0: Extraction and Adapter Layer
Goal: isolate current DHT usage before replacing the engine.

1. Introduce an internal DHT abstraction around the current `mainline` handle.
2. Move app bootstrap and retry logic behind that abstraction.
3. Change torrent-manager plumbing to depend on internal DHT types rather than `mainline` types directly.
4. Eliminate `SocketAddrV4` from manager-facing DHT channels in favor of `SocketAddr` at the abstraction boundary.

Acceptance:
- superseedr behavior is unchanged
- `mainline` remains the implementation behind an internal adapter
- DHT-specific types stop leaking through app and manager code paths

### Phase 1: First-Party Runtime Skeleton
Goal: stand up a minimal internal DHT runtime without switching product behavior yet.

1. Add internal modules for:
   - node ID / transaction IDs
   - KRPC message encoding and decoding
   - UDP socket task(s)
   - bootstrap worker
   - routing table
2. Support ping/find-node exchanges well enough to populate and refresh a routing table.
3. Add tracing and metrics hooks comparable to the current operational visibility.

Acceptance:
- internal DHT runtime can bootstrap in controlled tests
- routing table populates and stays alive under basic churn
- no torrent-manager integration change is required yet

### Phase 2: BitTorrent Peer Discovery
Goal: replace the `get_peers` path with first-party code.

1. Implement iterative `get_peers` lookups.
2. Implement token handling and `announce_peer`.
3. Stream discovered peers back as `SocketAddr`.
4. Preserve current manager behavior for periodic re-lookups and cancellation/restart.

Acceptance:
- torrent managers can discover peers through the new DHT engine
- peer discovery is stable enough for interop testing against the public network or controlled fixtures
- no major regression in peer acquisition versus the adapter-backed path

### Phase 3: Cutover and Dependency Removal
Goal: make the first-party DHT path the default implementation.

1. Add side-by-side verification mode if needed during rollout.
2. Switch the default `dht` feature implementation from `mainline` to the internal service.
3. Remove `mainline` from the runtime path.
4. Decide whether to keep an adapter fallback temporarily or fully remove the dependency.

Acceptance:
- default builds no longer depend on `mainline` for DHT
- startup, shutdown, and bootstrap-retry behavior remain stable
- torrent peer discovery remains acceptable under real swarm conditions

### Phase 4: IPv6 / BEP 32 Expansion
Goal: add DHT IPv6 support once the IPv4-first runtime is solid.

1. Extend routing and query logic for IPv6 nodes and peers.
2. Add dual-stack socket handling and address-family-aware routing behavior.
3. Validate interoperability expectations for BEP 32-style operation.
4. Revisit UI and telemetry to surface mixed-family DHT health clearly.

Acceptance:
- DHT no longer remains the IPv4-only holdout in the networking stack
- IPv6 discovery can participate without special-case manager plumbing

## Risks
- A DHT implementation that "works in tests" can still behave poorly on the public network.
- Routing-table quality, timeout policy, and token behavior matter more than packet parsing difficulty.
- This work can sprawl if it tries to become a general-purpose DHT library.
- Swapping the engine too early risks peer discovery regressions that look like general swarm instability.

## Risk Controls
- Keep the scope BitTorrent-specific.
- Preserve feature gating and adapter fallback until the new path proves itself.
- Add controlled integration coverage before default cutover.
- Roll out in layers: abstraction first, replacement second, dependency removal last.

## Testing Strategy
- Unit tests for KRPC message encoding/decoding and token validation.
- Deterministic routing-table tests with synthetic node topologies.
- Integration tests for bootstrap, `find_node`, `get_peers`, and `announce_peer`.
- Soak-style validation against real or containerized peers before default cutover.
- Regression checks ensuring torrent-manager peer admission behaves the same regardless of DHT backend.

## Decision Gates
Proceed only if the following remain true:
- DHT stays a product priority rather than an optional edge feature.
- The team wants tighter runtime ownership, not just a smaller dependency graph.
- IPv6-capable discovery remains a meaningful roadmap goal.

Defer or cancel if:
- UDP tracker + PEX + tracker IPv6 work satisfies discovery needs well enough
- DHT maintenance cost outweighs the product value of owning the subsystem
- a better-maintained upstream alternative appears with the needed runtime and IPv6 properties

## Immediate Next Steps
1. Add a Phase 0 adapter so `mainline` types stop leaking into app and manager code.
2. Convert manager-facing DHT peer delivery to `SocketAddr`.
3. Define the internal DHT service boundary and module layout.
4. Decide whether `announce_peer` is required in the first product cut or can follow `get_peers`.
5. Reassess scope after UDP tracker support lands, since that changes the urgency of DHT ownership.
