# BEP 5 Node Re-Architecture Plan

## Summary
- Keep the existing app-facing DHT boundary. `DhtService`, `DhtHandle`, streamed lookup results, runtime reconfigure, bootstrap warning UX, and compatibility fallback are already the right shape for superseedr.
- Replace the current first-party internal DHT engine behind that boundary. The current implementation is a useful lookup-oriented prototype, but it is not architected like a healthy public BitTorrent DHT node.
- Target a nice public node in the end: one that can answer inbound queries, maintain a healthy routing table under churn, validate announce tokens correctly, serve peer values responsibly, persist enough state to avoid cold-start thrash, and integrate with peer-protocol DHT discovery correctly.

## Desired End State
- Outbound and inbound support for `ping`, `find_node`, `get_peers`, and `announce_peer`.
- Correct token minting and validation for locally served `announce_peer`.
- A real per-info-hash peer store with bounded retention and expiry, not just a cache of tokens from remote nodes.
- A routing table shaped around BEP 5 semantics rather than grouped heuristics.
- Bucket refresh based on routing-table state, not just global maintenance sweeps.
- Process restart recovery through persisted routing state and peer-store snapshots where appropriate.
- Accurate health reporting that reflects real node behavior rather than optimistic assumptions.
- Correct BitTorrent peer-protocol DHT integration: advertise DHT support in the handshake, parse and send `PORT` correctly, and feed discovered DHT endpoints into the DHT runtime.
- A rollout path that keeps `mainline` available as a non-default verification backend until the first-party node proves itself.

## Security Reality
- Mainline DHT has real, long-documented exposure to Sybil and eclipse-style routing attacks. This is not hypothetical and should be treated as a design constraint, not as an optional hardening pass.
- BEP 5's token rule protects against one specific abuse: a node must verify that the `announce_peer` token was previously sent to the same IP address as the querying node, which prevents a host from signing up arbitrary third-party addresses for torrents.
- BEP 42 exists specifically because selecting arbitrary node IDs close to a target info-hash makes it easy to observe or block traffic for that key space. It ties node-ID choice to external IP address and says non-compliant nodes should not count as valid storage targets or toward lookup termination.
- None of this makes Mainline DHT fully secure against a determined and well-funded adversary. The right goal is not "perfect protection." The right goal is to make attacks costlier, reduce their success rate, limit blast radius, and surface strong signals when they happen.

## Threat Model
- Sybil and eclipse attacks against routing tables and lookup paths.
- Response poisoning by returning attacker-controlled, unreachable, or fake-close nodes.
- Peer-store poisoning through invalid or abusive announces.
- Node-ID churn from the same endpoint or small endpoint set.
- Lookup manipulation via referral stuffing, especially when many referrals are dead or non-routable.
- Bootstrap poisoning through over-trusting a small router set.
- Metadata and activity surveillance through ordinary `get_peers` participation.
- UDP reflection, amplification, and state-exhaustion abuse against the node itself.
- Malformed KRPC packets and parser abuse.
- Peer-protocol bridge abuse through bad `PORT` messages or suspicious endpoint advertisement.

## Security Requirements
- Implement BEP 5 token verification correctly for inbound `announce_peer`.
- Implement BEP 42-style node-ID verification and `ip` handling, and treat non-compliant nodes as lower-trust participants. At minimum:
- do not count non-compliant nodes toward announce target selection
- do not count non-compliant nodes toward lookup termination
- do not let non-compliant nodes dominate the closest-known set without independent corroboration
- Add per-IP, per-prefix, and per-bucket diversity controls so one operator cannot cheaply monopolize our routing state.
- Add scoring for node-ID churn, repeated contradictory IDs from the same endpoint, dead referral rates, and malformed reply behavior.
- Use multi-path or diversified lookup progression so a single poisoned frontier does not fully own the search.
- Keep suspicious nodes serviceable when required for compatibility, but degrade their trust in routing, storage, and termination logic.
- Bound memory and CPU for peer-store, inflight transactions, inbound query handling, and malformed packet handling.
- Rate-limit or shape replies to avoid becoming an amplifier.
- Minimize durable logging of user-sensitive DHT activity by default.
- Expose anomaly telemetry so operators can tell the difference between "quiet swarm" and "likely path poisoning."

## BEP 5 Invariants
- These are baseline public-network compatibility requirements. They are not optional tuning knobs for the first parity target.
- `K = 8` for routing-table bucket capacity and for the default closest-node response shape on the public Mainline path.
- Node IDs are 160-bit values and XOR distance is the only closeness metric used for baseline routing behavior.
- Good/questionable/bad node semantics follow BEP 5 exactly as the baseline:
- a node is good if it responded to one of our queries within the last 15 minutes
- a node is also good if it has ever responded to one of our queries and has sent us a query within the last 15 minutes
- after 15 minutes of inactivity a node becomes questionable
- nodes become bad when they fail to respond to multiple queries in a row
- good nodes are preferred over nodes with unknown status
- Bucket splitting follows BEP 5 baseline:
- a full bucket is only split when our own node ID falls in that bucket's range
- otherwise the new node is discarded unless a bad node can be replaced
- questionable nodes should be pinged in least-recently-seen order before replacement
- Bucket freshness follows BEP 5 baseline:
- each bucket tracks `last changed`
- buckets unchanged for 15 minutes are refreshed
- refresh means selecting a random ID in the bucket range and performing `find_node` on it
- Startup behavior follows BEP 5 baseline:
- on initial insertion and startup, find the closest nodes to our own ID
- save routing-table state between client invocations
- BitTorrent peer-protocol DHT integration follows BEP 5 exactly:
- set the last bit of the 8-byte reserved handshake flags for DHT support
- `PORT` uses message ID `0x09`
- `PORT` has a two-byte payload in network byte order
- receiving a `PORT` message should trigger a ping to the remote peer IP plus advertised UDP port
- KRPC baseline follows BEP 5:
- support query, response, and error messages
- support `ping`, `find_node`, `get_peers`, and `announce_peer`
- outbound messages should include `v`
- inbound messages must tolerate peers that omit `v`
- `announce_peer` token validation follows BEP 5 baseline:
- tokens are tied to the querying IP address
- tokens remain valid for a reasonable time window
- a practical default is the common rolling-secret model described in BEP 5

## Parity Guardrails
- Security overlays must not silently mutate BEP 5 wire behavior on the public path.
- Hardening may influence ranking, routing preference, lookup progression, storage target selection, and lookup termination.
- Hardening must not, by default:
- refuse to answer otherwise valid inbound queries solely because the remote node is non-compliant with BEP 42
- break BEP 5 packet shapes or required fields
- replace XOR distance with a different closeness metric
- change `K` away from `8` on the public Mainline path
- hard-ban large classes of mixed-network peers before comparative measurement shows a net gain
- BEP 42-style enforcement should follow its transition-friendly posture on mixed networks:
- continue servicing requests from non-compliant peers
- lower their trust for storage and termination decisions
- prefer compliant nodes for announce targets where possible
- Performance parity should be judged against a BEP 5-correct baseline first, then re-measured with overlays enabled.

## What To Keep
- Keep `DhtService` as the app-owned lifecycle and policy boundary.
- Keep `DhtHandle` as the torrent-facing surface for `get_peers` and `announce_peer`.
- Keep the `SocketAddr`-based peer delivery boundary.
- Keep runtime reconfigure and health snapshot reporting as concepts.
- Keep lookup streaming and shared lookup dedupe as product behavior.
- Keep bootstrap configuration, warnings, retry behavior, and status integration.
- Keep the `mainline` adapter path during development so the new node can be validated side by side.
- Keep minimal observability as an end-state requirement, but do not treat the current probe and metrics path as migration input. Reintroduce only the health and anomaly signals that still earn their keep after the new engine is stable.

## What To Throw Out
- Throw out the idea that the current `InternalPrototypeClient` should evolve into the final node. It is too fused and its internal boundaries are wrong for spec-correct behavior.
- Throw out the response-only UDP receive model. A node that only matches outstanding transactions cannot become a real BEP 5 participant.
- Throw out the current grouped routing-table structure as the authoritative long-term design. The `32` grouped buckets, `K=20`, and fixed active-route cap are implementation heuristics, not the routing model to optimize around.
- Throw out query-success-only liveness as the core notion of node quality. A healthy BEP 5 node must account for inbound query behavior as well.
- Throw out the current warmup and maintenance strategy as the permanent routing design. `warm_lookup_targets` and the current global sweep heuristics are acceptable bootstrap scaffolding, not the routing-table law of the land.
- Throw out passive remote token caching as a substitute for a real local token service.
- Throw out fixed health claims such as unconditional server-mode reporting when the runtime is not actually serving inbound KRPC.
- Throw out the current peer-protocol DHT path as implemented. The handshake bit is incomplete, `PORT` is encoded with the wrong width, and inbound `PORT` is ignored.
- Throw out the current internal probe and instrumentation path as migration baggage. The new engine should start without inheriting debug events, probe counters, or measurement-specific control flow from the prototype.
- Throw out tests that lock in current wrong wire behavior. Preserve tests that express product outcomes, but rewrite tests that encode invalid DHT or peer-protocol semantics.

## What To Rearchitect

### 1. Thin Orchestration Layer
- Start the new engine directly under `src/dht/`.
- Treat `src/dht_service.rs` as a temporary compatibility shell only if it helps migration speed.
- Remove `src/dht_service.rs` entirely after cutover if keeping the file adds no value.

### 2. KRPC Transport and Dispatch
- Split transport into a dedicated per-family UDP actor that handles:
- packet receive and send
- transaction registration and timeout handling
- inbound query parsing
- inbound response parsing
- error packet parsing and emission
- source-address validation and anti-spoof checks
- This layer should not own routing policy or token policy. It should only move typed KRPC events in and out.

### 3. KRPC Message Model
- Create a dedicated message model for typed queries, responses, and errors.
- Encode both outbound and inbound KRPC cleanly, including error packets and optional version tags if desired.
- Keep compact peer and compact node parsing here, not spread across lookup code.

### 4. Routing Table
- Replace the current route-table core with a BEP 5-oriented design:
- per-family k-buckets shaped around local node ID
- `K = 8` on the public Mainline path
- split logic driven by bucket range and local-ID inclusion
- replacement caches per bucket
- bucket refresh timestamps and refresh scheduling
- good/questionable/bad node retention logic that can account for both outbound success and inbound query recency
- bucket-level diversity so a single IP block, prefix, or operator cannot crowd out the table cheaply
- Keep diversification and family separation, but make them secondary selection policies rather than the primary routing-table shape.

### 5. Token Service
- Add a real token service with rotating secrets and address-aware validation.
- Use it when serving inbound `get_peers`.
- Validate it when accepting inbound `announce_peer`.
- Keep the existing outbound announce-token cache concept only as a client-side helper for remote announces, not as the local token authority.

### 6. Peer Store
- Add a bounded peer store keyed by info-hash and address family.
- Store peers learned from valid inbound `announce_peer`.
- Expire peers and cap per-torrent memory.
- Serve `values` from this store when answering inbound `get_peers`.
- Do not fake this with the torrent-manager peer list or with the outbound lookup cache.

### 7. Inbound Node Service
- Add a dedicated inbound query handler responsible for:
- `ping` replies
- `find_node` replies with the closest known nodes
- `get_peers` replies with `values` when available or closer nodes otherwise, plus a locally minted token
- `announce_peer` acceptance or rejection based on token validation and port rules
- This service must update routing state and liveness from inbound traffic.
- This service must also enforce abuse controls such as rate limiting, bounded response formation, and suspicious-node scoring.

### 8. Iterative Lookup Engine
- Rebuild lookup execution as a dedicated state machine that consumes routing snapshots and transport events.
- Keep streaming peer batches as a product behavior.
- Keep shared-lookup dedupe if it remains useful, but move the core state machine into its own module rather than burying it in one monolithic client type.
- Make cancellation, timeout, fanout, and hedge behavior explicit and testable.
- Implement the BEP 5 baseline termination and progression rules first, then layer anti-eclipse logic on top without changing packet compatibility.
- Add explicit anti-eclipse logic:
- diversified frontier selection across prefixes and sources
- referral-quality scoring
- no early success condition that trusts one suspiciously homogeneous closest set
- compliance-aware announce target selection

### 9. Persistence
- Persist routing snapshots across process restarts, not just runtime reconfigure events.
- Persist only what materially improves cold-start behavior:
- local node ID
- routing-table snapshot
- replacement candidates if still valuable after redesign
- optionally a bounded token or peer-store snapshot if evidence shows restart stability benefits
- Treat persisted state as advisory and easy to discard if corrupt or stale.

### 10. Peer-Protocol DHT Bridge
- Fix the BitTorrent handshake reserved bit for DHT support.
- Encode and decode `PORT` correctly.
- Actually use inbound `PORT` messages to seed DHT node discovery where safe.
- Make this bridge a thin adapter into the DHT runtime, not a place where DHT routing policy leaks into peer-session code.

### 11. Health and Telemetry
- Add health and anomaly reporting only after the core node behavior is correct.
- Rebuild health reporting on top of real internal state:
- bound sockets
- inflight query count
- routing-table size by family
- bucket freshness
- token-service activity
- inbound query rate
- peer-store size
- bootstrap responsiveness
- suspicious-node counts
- dead-referral rate
- node-ID churn observations
- per-prefix concentration
- Remove optimistic fields that cannot be justified by actual runtime behavior.

## What Not To Rewrite
- Do not rewrite app-level lifecycle ownership around DHT.
- Do not rewrite torrent-manager behavior just to satisfy the DHT engine. The current manager-facing API is already narrow enough.
- Do not couple this work to the global peer-manager rewrite unless a specific boundary forces it.
- Do not expand scope into a public general-purpose DHT library.
- Do not take on BEP 44 or other storage-oriented DHT features in the same program.
- Do not let IPv6/BEP 32 delay getting a solid IPv4 public node shape first, but keep the internal boundaries family-aware from day one.

## Phased Implementation Plan

### Phase 0 - Boundary Freeze and Internal Extraction
- Freeze the current `DhtService` and `DhtHandle` product-facing contract.
- Introduce a dedicated `src/dht/` module tree and build the new engine there as a greenfield implementation.
- Keep `mainline` as the reference backend.
- Stop growing the current `InternalPrototypeClient` except for temporary bug fixes needed to keep the branch healthy.
- Do not port current probe, measurement, or instrumentation code into the new module tree.

### Phase 1 - Transport and Typed KRPC Core
- Implement per-family UDP transport actors.
- Implement typed KRPC query, response, and error models.
- Support inbound and outbound packet handling in the same transport layer.
- Add malformed-packet handling, timeout handling, and source-address verification.
- Ensure outbound `v` handling and exact BEP 5 KRPC packet compatibility before adding overlay-specific extensions.

Exit criteria:
- deterministic unit and integration tests cover inbound and outbound KRPC framing
- runtime can receive inbound queries without panicking or dropping unrelated state

### Phase 2 - Routing Table Replacement
- Implement the new routing table and replacement-cache model.
- Add node-state transitions driven by outbound success, outbound failure, and inbound query recency, with exact BEP 5 good/questionable/bad semantics as the baseline.
- Implement bucket refresh planning and per-bucket refresh bookkeeping with exact BEP 5 `last changed` behavior as the baseline.
- Add prefix and endpoint diversity rules and suspicious-node accounting from day one of the new table.
- Wire bootstrap and route discovery into this new routing table while still using the `mainline` backend for production if needed.

Exit criteria:
- routing-table invariants hold under synthetic churn
- bucket refresh and replacement behavior are deterministic in tests
- no family cross-contamination occurs

### Phase 3 - Inbound Node Semantics
- Implement inbound `ping`, `find_node`, `get_peers`, and `announce_peer`.
- Add token service and bounded peer store.
- Update routing and peer-store state from inbound traffic.
- Add rate-limit and abuse-resistance hooks if needed to protect memory and amplification profile.
- Enforce BEP 42-style trust policy and mark non-compliant nodes so later lookup stages can treat them appropriately.

Exit criteria:
- local fixture nodes can query this node and get correct responses
- invalid announce tokens are rejected
- valid announces populate peer-store state and become visible through inbound `get_peers`

### Phase 4 - Outbound Lookup and Announce Replacement
- Replace the current internal prototype lookup engine with the new iterative lookup state machine.
- Preserve streamed peer batches and lookup dedupe.
- Keep announce behavior through the new client-side token cache plus transport path.
- Match or exceed the current internal prototype on peer-yield and time-to-first-batch in controlled tests.
- Add diversified lookup and anti-poisoning checks before allowing this engine to become default.

Exit criteria:
- `get_peers` and `announce_peer` run entirely through the new engine
- lookup cancel/restart and timeout behavior are stable
- the new engine can run under `DhtService` without product-facing API changes

### Phase 5 - Persistence and Peer-Protocol Bridge
- Add durable routing snapshot persistence across process restarts.
- Fix handshake DHT support advertisement and `PORT` handling in peer protocol code.
- Feed peer-protocol DHT discoveries into the DHT runtime safely.

Exit criteria:
- restart cold-start behavior improves measurably
- peer-protocol DHT bridge produces valid node candidates
- no incorrect `PORT` wire behavior remains

### Phase 6 - Differential Validation and Cutover
- Run the new backend and the `mainline` adapter against the same controlled lookup corpus.
- Compare bootstrap stability, routing growth, peer yield, time to first batch, and announce success.
- Keep the new backend non-default until it meets the acceptance gates.
- Flip the default backend only after public-network soak data is acceptable.

Exit criteria:
- the new backend is default
- `mainline` remains only as a non-default compatibility backend for one stabilization cycle

### Phase 7 - Cleanup
- Delete the old internal prototype engine once the new backend is default and stable.
- Remove dead heuristics and constants that only existed to prop up the prototype design.
- Remove compatibility shims that preserved now-obsolete route formats or token-cache semantics.

Delete list after cutover:
- current `InternalPrototypeClient`
- current `InternalPrototypeFamilySocket` receive semantics
- grouped route-table constants and grouped-bucket helpers
- current warm-target maintenance helpers
- incorrect peer-protocol DHT message handling
- tests that validate obsolete prototype internals rather than desired node behavior

## Acceptance Gates For A Nice Node
- It is wire-compatible with BEP 5 on the public Mainline path.
- It preserves `K = 8`, XOR routing, correct `PORT` behavior, and correct token semantics.
- It answers inbound KRPC correctly and consistently.
- It does not advertise server capability it does not actually provide.
- It can build and retain a useful routing table under churn.
- It can restart without behaving like a totally cold client every time.
- It can receive valid announces, reject invalid announces, and serve peer values safely.
- It can participate in peer-protocol DHT discovery correctly.
- It does not regress peer acquisition materially against the `mainline` reference path.
- It produces believable health signals that operators can use to debug real problems.
- It does not let a small, obviously homogeneous group of endpoints dominate routing, announce targets, or lookup termination without independent confirmation.
- It exposes enough telemetry to notice likely eclipse or referral-poisoning conditions in the field.

## Test Strategy
- Unit tests for KRPC encoding, decoding, and error-path handling.
- Unit tests for token mint and validation behavior.
- Pure routing-table invariant tests with synthetic node IDs and churn scenarios.
- Exact BEP 5 baseline tests for:
- `K = 8`
- bucket split rules
- questionable-node probing order
- bucket refresh after 15 minutes unchanged
- startup self-lookup behavior
- routing-table persistence restore
- handshake reserved-bit behavior
- `PORT` two-byte wire format and ping-on-receive behavior
- Lookup-engine reducer tests for timeout, hedge, restart, and cancellation behavior.
- Integration tests with local synthetic nodes that exercise inbound and outbound `ping`, `find_node`, `get_peers`, and `announce_peer`.
- Differential tests versus the `mainline` adapter on identical lookup and announce scenarios.
- Adversarial tests for:
- close-ID Sybil clusters
- endpoint ID churn
- dead-node referral flooding
- prefix concentration
- mixed honest and malicious frontier responses
- bad-token announce attempts
- Soak tests on mixed public or controlled networks focused on:
- routing-table growth
- bucket freshness
- inbound query handling
- peer yield
- restart behavior
- malformed packet tolerance
- anomaly detection signal quality

## Immediate Next Steps
- Start by freezing the current service boundary and creating the new `src/dht/` module layout.
- Land transport and typed KRPC support before touching the routing-table replacement.
- Fix peer-protocol handshake and `PORT` handling early so new node discovery inputs are not silently wrong.
- Keep `mainline` available until the new backend has passed differential and soak validation.
