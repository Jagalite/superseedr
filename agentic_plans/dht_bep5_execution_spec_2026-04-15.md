# DHT BEP 5 Execution Spec

## Purpose
- This document is the implementation companion to `dht_bep5_node_rearchitecture_plan_2026-04-15.md`.
- The plan defines architecture direction and acceptance criteria.
- This spec defines concrete module boundaries, state ownership, message flow, lookup behavior, persistence shape, and parity benchmarks so work can begin without design drift.

## Scope
- This spec covers the first-party public Mainline-compatible DHT node for superseedr.
- It assumes the BEP 5 invariants and parity guardrails from the companion plan are binding.
- It assumes the new engine is built greenfield under `src/dht/` rather than extracted from the current internal prototype.
- It does not define a public library API.
- It does not cover BEP 44 or non-BitTorrent DHT storage features.

## Current Implementation Status
- As of `2026-04-17`, the branch has completed the greenfield `src/dht/` module tree, service boundary replacement, transport/KRPC core, routing-table core, inbound KRPC handling, token service, peer store, persistence, bootstrap/maintenance execution, and the working outbound lookup engine.
- The branch has also landed the runtime fixes that mattered for public-network behavior: 4-byte transaction IDs for `mainline` interoperability, lookup lifecycle cleanup, inflight query cleanup, deferred replacement probing, soft-timeout traversal expansion, cached responder reuse, persisted local node identity reuse, and IPv4-first dual-stack scheduling.
- Deterministic correctness coverage now includes lookup replay tests, runtime UDP replay tests, and BEP 42 classification tests under `src/dht/`.
- Public-network validation is no longer in the "not yet working" state. The current branch reached and exceeded the working parity target on the active live corpus and completed long soak runs without functional collapse.
- BEP 42 classification and trust-aware lookup/routing ranking are now active, but the broader anomaly-scoring layer described in this spec is still not implemented.
- The branch still has three material design-doc gaps:
  - full anomaly scoring and telemetry in `src/dht/anomaly.rs`
  - peer-protocol DHT bridge correctness (`PORT`, handshake DHT bit, and feeding peer-discovered endpoints into the runtime)
  - normal service-path `mainline` backend verification inside `src/dht/service.rs`
- Benchmark-specific tracing and differential tooling were useful during implementation, but they are not treated as settled production-path design.

## Module Layout
- `src/dht/mod.rs`
  - owns the new DHT runtime surface
  - may be reached through a temporary compatibility adapter during migration
- `src/dht/types.rs`
  - shared core types: node IDs, transaction IDs, trust levels, lookup IDs, compact node and peer records
- `src/dht/krpc.rs`
  - typed KRPC query, response, and error models
  - bencode encode/decode helpers
  - compact node and peer parsing helpers
- `src/dht/transport.rs`
  - per-family UDP transport actor
  - transaction registration
  - inbound packet parse and outbound send
  - timeout and source validation
- `src/dht/routing.rs`
  - bucket structure
  - replacement cache
  - node insertion, eviction, split, refresh planning
  - routing snapshot generation
- `src/dht/token.rs`
  - inbound `announce_peer` token mint and validation
  - rolling secret rotation
- `src/dht/peer_store.rs`
  - bounded per-info-hash peer store
  - expiry and retrieval for inbound `get_peers`
- `src/dht/inbound.rs`
  - inbound query service
  - handles `ping`, `find_node`, `get_peers`, `announce_peer`
  - updates routing and peer store from inbound traffic
- `src/dht/lookup.rs`
  - iterative lookup state machine for `find_node` and `get_peers`
  - frontier management
  - lookup termination
  - peer streaming batches
- `src/dht/bootstrap.rs`
  - startup self-lookup
  - bootstrap router seeding
  - bucket refresh execution
- `src/dht/anomaly.rs`
  - suspicious-node scoring
  - referral-quality tracking
  - prefix concentration helpers
  - BEP 42 compliance verification
- `src/dht/persist.rs`
  - on-disk snapshot schema
  - save and restore logic
- `src/dht/health.rs`
  - health snapshot helpers and anomaly summaries
  - no probe-event compatibility layer from the legacy prototype
- `src/dht/test_support.rs`
  - local test fixtures and deterministic helpers

## Ownership Model
- `DhtService`
  - if retained during migration, owns subsystem lifecycle, config, reconfigure, health aggregation, backend selection, and app-facing status
  - does not own routing-table mutation logic or packet parsing logic
- `TransportActor`
  - owns UDP socket for one address family
  - owns inflight transaction map for that family
  - does not own routing state, token state, or lookup strategy
- `RoutingActor`
  - owns routing table for one family
  - owns node records, buckets, replacements, and refresh timestamps
  - exposes immutable snapshots for lookup planning
- `InboundActor`
  - owns inbound KRPC query handling
  - calls into routing, token, and peer-store owned state through explicit messages
- `LookupManager`
  - owns active lookup tasks
  - each lookup task owns its own frontier and local state
- `TokenService`
  - owns current and previous rolling secrets
  - does not store per-token allocations
- `PeerStore`
  - owns announced peers
  - bounded by global and per-info-hash caps
- `PersistenceManager`
  - owns read/write of persisted snapshots

## State Ownership Table
- local node ID:
  - owned by `DhtService` runtime state
  - read by routing, transport, inbound, and lookup components
- per-family socket:
  - owned by `TransportActor`
- inflight transaction map:
  - owned by `TransportActor`
- routing buckets and replacements:
  - owned by `RoutingActor`
- node quality and anomaly counters:
  - owned by `RoutingActor`
- rolling token secrets:
  - owned by `TokenService`
- announced peers:
  - owned by `PeerStore`
- active lookup tasks:
  - owned by `LookupManager`
- bootstrap router set:
  - owned by `DhtService` config and passed as immutable inputs to bootstrap operations
- health snapshots:
  - assembled by `DhtService` from subsystem snapshots

## Core Types
- `NodeId([u8; 20])`
- `InfoHash([u8; 20])`
- `TransactionId([u8; 4])`
  - BEP 5 does not require a fixed transaction-id width
  - the current public-network interoperability target should emit 4-byte outbound transaction IDs because `mainline` compatibility testing proved that 2-byte outbound IDs were not sufficient on our active path
  - inbound transport should still tolerate peers that use other short widths
- `CompactNode { id: NodeId, addr: SocketAddr }`
- `CompactPeer { addr: SocketAddr }`
- `NodeTrust`
  - `Trusted`
  - `Neutral`
  - `Suspicious`
- `Bep42State`
  - `Unknown`
  - `Compliant`
  - `NonCompliant`
  - `ExemptLocal`
- `NodeRecord`
  - `addr: SocketAddr`
  - `node_id: Option<NodeId>`
  - `last_query_sent_at: Option<Instant>`
  - `last_query_response_at: Option<Instant>`
  - `last_inbound_query_at: Option<Instant>`
  - `consecutive_failures: u8`
  - `last_changed_at: Instant`
  - `trust: NodeTrust`
  - `bep42: Bep42State`
  - `dead_referral_count: u16`
  - `live_referral_count: u16`
  - `id_churn_count: u16`
  - `prefix_key: PrefixKey`
- `Bucket`
  - `range_min: NodeId`
  - `range_max: NodeId`
  - `nodes: Vec<NodeRecord>`
  - `replacements: Vec<NodeRecord>`
  - `last_changed_at: Instant`
- `LookupState`
  - `target: InfoHash`
  - `visited: HashSet<SocketAddr>`
  - `frontier: BinaryHeap<LookupCandidate>`
  - `inflight: HashMap<TransactionId, LookupQuery>`
  - `received_peers: HashSet<SocketAddr>`
  - `closest_valid_responders: Vec<NodeRecord>`
  - `started_at: Instant`

## Wire Compatibility Rules
- Public path must emit BEP 5-compatible KRPC packets.
- Outbound messages include:
  - `t`
  - `y`
  - `v`
  - `q` plus `a` for queries
  - `r` for responses
  - `e` for errors
- Inbound messages must tolerate missing `v`.
- Error handling must support:
  - `201`
  - `202`
  - `203`
  - `204`
- Public-path `find_node` and `get_peers` closest-node replies return up to `K=8` closest good nodes.
- Public-path `PORT` handling must use exactly a 2-byte payload.

## Routing Rules
- Bucket capacity is `8`.
- Bucket ranges cover the full 160-bit key space.
- A full bucket is split only if the local node ID falls within the bucket range.
- If a bucket is full and does not contain the local ID range:
  - replace a bad node if present
  - otherwise probe questionable nodes in least-recently-seen order
  - if all remain good, discard the new candidate from the authoritative bucket
  - the candidate may still be tracked as a low-priority observation outside the authoritative bucket set only if needed for anomaly accounting
- Every bucket tracks `last_changed_at`.
- A bucket is refresh-due after 15 minutes unchanged.
- Refresh means:
  - choose a random target in the bucket range
  - run `find_node` for that target
- Startup must:
  - load persisted routing state
  - seed bootstrap routers
  - perform self-lookup toward the local node ID

## Good / Questionable / Bad Semantics
- Baseline status follows BEP 5 exactly:
  - good if responded to one of our queries within 15 minutes
  - also good if it has ever responded to one of our queries and has sent us a query within 15 minutes
  - questionable after 15 minutes inactivity
  - bad after multiple consecutive failures
- Security overlays do not change these labels.
- Security overlays only change:
  - trust ranking
  - lookup termination eligibility
  - announce target eligibility
  - referral influence

## BEP 42 Handling
- Verify node IDs against observed external IPs when enough data is available.
- `Bep42State::NonCompliant` does not block servicing requests by default.
- `NonCompliant` nodes:
  - are not counted toward announce target selection
  - are not counted toward lookup termination
  - are down-ranked in frontier ranking
- Local/private address peers are exempt.
- If multiple independent replies report our external IP and our current node ID is invalid for it, mark self-restart candidate state but do not restart immediately inside the packet hot path.

## Lookup Algorithm

### Inputs
- `target`
- per-family routing snapshot
- bootstrap routers
- security policy parameters

### Seed Set
- Seed from:
  - closest trusted routing nodes
  - closest neutral routing nodes
  - bootstrap routers if trusted frontier depth is insufficient
- Do not seed primarily from suspicious nodes unless the frontier is otherwise too thin.

### Candidate Ranking
- Sort by:
  - XOR distance to target
  - trust tier
  - BEP 42 compliance
  - referral quality
  - prefix diversity
  - recency of good response
  - insertion order

### Query Fanout
- Public baseline:
  - start with up to `8` concurrent requests for the initial wave only if the frontier is deep enough
  - steady-state concurrency target defaults to `4`
- Security overlays may reduce effective concurrency for suspicious-heavy frontiers but must not change packet compatibility.

### Response Handling
- On `values`:
  - decode and emit peer batch immediately
  - dedupe peers globally for the lookup
- On `nodes`:
  - decode compact nodes
  - drop malformed and family-mismatched entries
  - update referral-quality counters for the responder
  - insert bounded number of best candidates into the frontier, preserving prefix diversity
- On error or timeout:
  - update node failure state

### Lookup Termination
- Baseline termination condition:
  - no closer valid nodes remain to be queried, or
  - the closest `8` eligible responders have replied and no newly discovered closer eligible nodes remain
- Eligibility for termination counting:
  - must satisfy BEP 5 good-response expectations
  - must not be marked `Bep42State::NonCompliant`
  - must not violate current suspicious-frontier safety rules
- Hardening must not let one concentrated prefix or suspicious cluster satisfy termination alone if diverse alternatives still exist.

### Peer Batch Streaming
- Stream peer batches immediately on receipt.
- Do not wait for lookup completion to publish the first batch.
- Preserve stable ordering only within a batch if cheap; do not add expensive global sorts in the hot path.

## Inbound Query Rules
- `ping`
  - always respond if valid and within rate limits
- `find_node`
  - respond with up to `8` closest good nodes
- `get_peers`
  - if peer store has peers, return `values` plus token
  - otherwise return up to `8` closest good nodes plus token
- `announce_peer`
  - require valid token for the querying IP
  - honor `implied_port`
  - if no `implied_port`, use explicit port
  - insert peer into peer store on success
  - return protocol error on bad token or malformed arguments

## Abuse Controls
- Per-IP inbound query rate limit
  - default: soft budget with burst, exact values to be benchmark-tuned
- Per-packet decoded node cap
  - ignore excess compact nodes beyond a configured maximum per response
- Referral acceptance cap
  - only admit bounded number of candidates per response into the frontier
- Peer-store cap
  - bounded peers per info-hash
  - bounded total peers globally
- Inflight query cap
  - hard cap per family
- Parser hardening
  - reject oversize packets
  - reject malformed bencode safely
  - never allocate unbounded memory from declared list sizes

## Persistence Spec
- File location:
  - runtime persistence directory under a new DHT-specific file
- Versioned envelope:
  - `version`
  - `created_at`
  - `node_id`
  - `ipv4_routes`
  - `ipv6_routes`
  - optional `replacements`
  - optional minimal peer-store snapshot if later justified
- Restore rules:
  - ignore corrupt file and continue
  - ignore stale records older than configured max age
  - validate family and shape before load
  - loaded routes enter as historical candidates, not automatically trusted forever
- Save cadence:
  - on graceful shutdown
  - periodically on dirty state with coarse interval
  - after meaningful routing change bursts, but not on every packet

## Health Snapshot Fields
- bound sockets by family
- inflight transactions by family
- routing nodes by family
- replacement nodes by family
- refresh-due bucket count
- bootstrap responsive count
- inbound query rate
- peer-store size
- suspicious-node count
- non-compliant-node count
- dead-referral rate
- recent lookup success rate

## Benchmark Matrix

### Parity Benchmarks
- bootstrap to first healthy routing snapshot
- p50 first peer batch latency
- p95 first peer batch latency
- total unique peer yield per lookup corpus
- announce success rate
- CPU during steady lookup load
- memory footprint of routing plus lookup state

### Adversarial Benchmarks
- lookup completion under close-ID Sybil cluster
- dead-referral flood resilience
- node-ID churn resilience
- prefix concentration resilience
- mixed honest/malicious bootstrap set behavior

## Cutover Thresholds
- honest-network parity:
  - p50 first-batch latency within 10% of `mainline`
  - unique peer yield at least 95% of `mainline`
  - bootstrap success not materially worse than `mainline`
- adversarial improvement:
  - better lookup completion than the old internal prototype
  - lower dead-referral rate than the old internal prototype
  - suspicious concentration visible in telemetry

## Phase Execution Breakdown
- Phase 0: `complete`
  - `src/dht/` module tree exists
  - shared types and KRPC models exist
  - legacy probe helpers were not ported into the new engine
- Phase 1: `mostly complete`
  - transport actors and typed KRPC send/receive exist
  - wire compatibility is functional enough for live and local-testnet use
  - exact KRPC compatibility tests are still thinner than intended
- Phase 2: `mostly complete`
  - routing-table core exists
  - startup self-lookup and periodic refresh/ping/bootstrap maintenance now exist
  - replacement-probe outcomes are now acted on in the runtime
  - current public-network parity and long-soak stability targets have been met on the active corpus
  - anomaly-driven routing overlays are still thinner than intended
- Phase 3: `mostly complete`
  - inbound node service, token service, and peer store exist
  - BEP 42 trust marking is active in lookup and routing
  - the broader anomaly-scoring layer is still not wired in as intended
- Phase 4: `mostly complete`
  - lookup engine and peer batch streaming exist
  - lookup cancellation and inflight cleanup improved materially
  - live parity is met on the current corpus and soak matrix
  - peer-protocol DHT bridge follow-through and anomaly-aware hardening are still outside this phase's current implementation
- Phase 5: `partial`
  - persistence and health reporting exist
  - peer-protocol DHT bridge behavior is still not fixed
- Phase 6: `mostly complete`
  - deterministic replay coverage, differential validation, and long soak validation exist
  - current parity thresholds have been met on the active live corpus and soak runs
  - normal service-path `mainline` verification is still incomplete
- Phase 7: `pending`
  - default cutover cleanup and remaining bridge/anomaly follow-through are not finished yet

## Immediate Implementation Checklist
- Update this spec and the companion plan whenever branch status changes materially so they remain usable as handoff documents.
- Keep `mainline` wired as the differential oracle until either:
  - the normal service path can run it cleanly, or
  - the team explicitly retires that requirement
- Implement the remaining peer-protocol DHT bridge work:
  - correct handshake DHT support bit
  - correct 2-byte `PORT` wire behavior
  - ping/feed peer-discovered DHT endpoints into the runtime
- Implement the anomaly engine described in `src/dht/anomaly.rs` and wire its signals into routing, lookup ranking, and health reporting.
- Add the security-oriented tests that correspond to the anomaly layer once the bridge and anomaly work land.
