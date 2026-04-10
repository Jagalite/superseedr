# DHT + Global Peer Manager Program Plan

## Summary
- Build two app-owned subsystems on top of the released `develop` baseline: an authoritative `GlobalPeerManager` first, then a `DhtService` with a `mainline` adapter, then a dual-stack first-party DHT backend.
- Preserve the existing effect/action pattern for per-torrent behavior, but move global networking policy into async subsystems with pure policy/state-machine cores.
- Cut over quickly once the planned matrix passes; keep `mainline` only as a non-default compatibility backend during the final transition.

## Future Architecture and Required Refactors
- `App` owns `GlobalPeerManager`, `DhtService`, and `ResourceManager`; `TorrentManager` no longer owns a DHT handle or the authoritative peer reputation/backoff state.
- Introduce shared typed peer interfaces: `PeerCandidate { info_hash, addr: SocketAddr, source, discovered_at }`, `PeerSource`, `PeerOutcome`, `PeerReputation`, and `PeerAdmissionDecision`.
- Replace direct peer dialing from torrent effects with a submission/admission flow. `TorrentState` emits `Effect::SubmitPeerCandidates` or `Effect::RequestDhtLookup`; only `GlobalPeerManager` issues `ManagerCommand::ConnectPeer { addr, source }`.
- Move `timed_out_peers`, `last_known_peers`, duplicate suppression, cooldowns, subnet fairness, and blacklist/greylist policy out of `TorrentState` and into `GlobalPeerManager`.
- Add a separate lightweight handshake/preflight budget in `ResourceManager` such as `ResourceType::PeerHandshake`; inbound TCP handshakes use that budget, and only admitted sessions consume `PeerConnection`.
- Introduce `DhtHandle`, `DhtCommand`, `DhtEvent`, and an internal `DhtBackend` trait; `DhtService` owns bootstrap, rebind, health, query scheduling, and backend lifecycle.
- Use `SocketAddr` everywhere at subsystem boundaries. Remove remaining manager-facing `SocketAddrV4` DHT channels and stop using `String` keys for peer identity inside admission/reputation logic.
- Implement the first-party DHT as dual-stack from day one: separate IPv4 and IPv6 UDP sockets and family-aware routing/query state, unified behind `SocketAddr`.
- Extend telemetry with candidate source attribution, dedupe drops, cooldown hits, blacklist hits, dial scheduling latency, time-to-first-peer, DHT bootstrap/routing health, and per-family DHT stats.

## Phased Implementation Plan

### Phase 0 - Foundations on top of the released `develop` baseline
- Create the master program plan in `agentic_plans` and treat `docs/dht-ownership-plan.md` as the earlier DHT-only reference.
- Finish the typed peer refactor that `develop` started: remove remaining manager-facing `SocketAddrV4` usage, convert peer reputation/backoff storage from string keys to typed addresses, and normalize tracker, DHT, PEX, resume, and inbound sources to `PeerCandidate`.
- Add the new events and metrics needed for source attribution and peer admission outcomes before changing behavior.

### Phase 1 - Authoritative Global Peer Manager using current networking
- Add `GlobalPeerManager` as an app-owned actor with a pure policy core.
- Route tracker, current DHT results, PEX, resume peers, and inbound handshakes into it.
- Make it authoritative for dedupe, concurrent-dial suppression, cooldowns, fairness across torrents, subnet throttling, and blacklist/greylist decisions.
- Keep `TorrentManager` responsible only for per-session protocol and per-torrent state; it receives `ConnectPeer` commands and reports dial/session outcomes back.
- Split preflight and full-session budgeting so inbound noise cannot starve outbound dial attempts.
- Remove `timed_out_peers` and `last_known_peers` from `TorrentState` once equivalent global behavior is live.

### Phase 2 - DHT service boundary with a `mainline` adapter
- Add `DhtService` with Tokio-native lifecycle management, bootstrap state, rebind handling, lookup dedupe, and health reporting.
- Implement `MainlineBackend` behind `DhtBackend` so current DHT behavior is preserved while removing `mainline` types from `app` and `torrent_manager`.
- Move startup bootstrap, automatic retry, and port-rebind logic out of `app` into `DhtService`.
- Change the default runtime path to `DhtService` plus the adapter as soon as this phase lands; do not keep direct `mainline` handles in managers.

### Phase 3 - First-party dual-stack DHT backend
- Implement the first-party backend with dual-stack UDP sockets, family-aware routing tables, query state, token validation, bootstrap workers, `find_node`, `get_peers`, and `announce_peer`.
- Emit peer candidates and health events through the existing `DhtService` boundary so `GlobalPeerManager` and `TorrentManager` do not change again.
- Match or exceed the adapter path on peer acquisition, bootstrap stability, shutdown, and mixed-family behavior before cutover.
- Keep the `mainline` adapter only as a non-default compatibility backend for one development cycle after the first-party backend becomes default.

### Phase 4 - Hardening and optimization
- Add adaptive DHT query concurrency, cancellation, routing persistence, and family-aware health degradation behavior.
- Tighten peer-manager policy with stronger blacklist escalation, low-value peer eviction, and optional subnet-level pressure relief.
- Revisit durable reputation persistence only after soak data shows the false-positive rate is acceptable.

## Test Plan
- Unit-test the `GlobalPeerManager` policy core and the DHT query/routing state machines as pure reducers.
- Add property tests for invariants: no duplicate concurrent dial for the same `(info_hash, addr)`, no admission without budget, cooldown/blacklist transitions are monotonic and family-safe, DHT query cancel/restart never leaks in-flight state, and dual-stack routing tables never cross-contaminate address families.
- Add fuzz targets for KRPC decoding/encoding, compact peer parsing, UDP tracker parsing, inbound BitTorrent handshake parsing, and address-family normalization logic.
- Add deterministic integration tests with synthetic IPv4 and IPv6 DHT fixtures, tracker fixtures, peer storms, invalid tokens, malformed responses, and cross-torrent fairness scenarios.
- Add differential tests that compare the `mainline` adapter path and the first-party DHT backend on identical lookup scenarios before removing `mainline` from the default path.
- Add soak tests for released builds: time-to-first-peer, accepted-peer yield, duplicate-drop rate, cooldown hit rate, blacklist false-positive rate, DHT bootstrap success, routing-table population, and mixed IPv4/IPv6 swarm behavior.

## Assumptions and Defaults
- This program starts after the current `develop` branch ships to users and uses that code as the baseline.
- The first global peer manager is authoritative, not observe-only or soft-gating.
- The first first-party DHT backend is dual-stack, even though that increases initial scope.
- Torrent-level effect/action stays in place for per-torrent logic; the new subsystems use async actors with pure policy/state-machine cores rather than extending `TorrentState` into global networking state.
- The rollout is a fast cutover, not a long internal-only phase, but `mainline` remains available behind a non-default compatibility path during the final transition.
- Blacklists and reputation stay in-memory first; persistence is deferred to Phase 4.
