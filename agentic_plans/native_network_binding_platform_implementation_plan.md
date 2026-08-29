# Native Network Binding Platform Implementation and Qualification Plan

## Status

- Shared family-aware binding model: complete.
- Shared HTTP request seam: complete; current Unix and `Any` behavior retained.
- Linux strict backend: implemented and qualified with stated limitations.
- macOS strict backend: implemented and qualified with stated limitations.
- Windows strict backend: implemented on this branch, including adapter discovery,
  raw sockets, strict HTTP, native change notifications, and a host-validation harness.
- Windows ordinary CI: configured; a fresh successful run is still required for the
  release candidate.
- Windows native multi-adapter and elevated packet qualification: evidence not yet
  recorded in this repository.
- Windows interface-binding capability: currently enabled in code. This is a release
  blocker until Gate C passes; otherwise capability exposure must be disabled again.

## Objective

Qualify the centralized, generation-owned network-binding architecture now
implemented across Linux, macOS, and Windows while preserving default `Any`
behavior.

The remaining Windows work is evidence-driven hardening and release qualification,
not a second backend implementation. Linux and macOS work is limited to regression
prevention and release-candidate evidence refreshes.

## Security contract

All platforms must preserve these rules:

1. `Any` remains unrestricted and compatible with the main-branch networking path.
2. Strict mode never falls back to `Any`, another interface, an unrestricted HTTP
   client, an automatic proxy, or system DNS when bound DNS is selected.
3. Interface state is resolved into an immutable network generation.
4. Old leases are invalidated before replacement traffic is admitted.
5. A generation is active only after listeners are bound and the actual peer port is
   known.
6. Failed resolution, preflight, listener creation, or recovery leaves networking
   blocked.
7. TCP, UDP, HTTP, DNS, trackers, peers, web seeds, RSS, DHT, and uTP obtain sockets
   through the centralized networking runtime.
8. Stale work from an invalidated generation cannot update current state.

The first Windows release guarantees outbound confinement for Superseedr-owned
traffic. It does not claim that a listener's local address proves the physical
ingress interface.

## Out of scope for the first Windows release

- Windows Filtering Platform enforcement.
- A physical-ingress-interface guarantee.
- Listening on every eligible address assigned to an adapter.
- Non-GET strict HTTP requests.
- Automatic proxy routing in strict mode.
- Any unrestricted recovery or fallback path.
- Enabling Windows interface binding before elevated qualification passes.

## Current shared foundation

The prerequisite shared refactor is complete in `src/networking/runtime.rs`:

- persisted interface identity and display name are separate;
- IPv4 and IPv6 have separate live indices;
- configured and effective source addresses are separate;
- eligible address sets and per-family host policy are represented explicitly;
- runtime status reports both families independently;
- generation equivalence distinguishes explicit sources from automatic selection;
- `NetworkHttpClient::get` returns the internal `NetworkHttpRequest` wrapper.

Unix behavior must remain unchanged:

- identity and display name are the interface name;
- both family records use the same `if_nametoindex` result;
- automatic interface mode leaves effective source selection to the OS;
- explicit sources populate configured and effective source fields;
- the HTTP wrapper delegates to the existing reqwest clients and redirect behavior.

## Ownership boundaries

Platform-specific logic belongs below `SocketFactory` and the networking runtime.
Consumers must not contain Linux, macOS, or Windows binding branches.

Primary files:

- `src/networking/runtime.rs`: discovery, resolution, generations, leases, raw
  sockets, HTTP clients, and platform backends.
- `src/networking/dns.rs`: generation-owned bound DNS.
- `src/app.rs`: listener replacement and activation ordering.
- `src/tui/screens/config.rs`: configuration and runtime-status presentation.
- torrent managers, trackers, RSS, web seeds, and DHT: consumers of leases and
  activation state only.

## Phase 1: Windows build boundary — implemented

### Tasks

1. Add a direct Windows-only `windows-sys` dependency under
   `[target.'cfg(windows)'.dependencies]`.
2. Enable only the required API feature groups:
   - Foundation;
   - IP Helper;
   - NDIS if required by the final implementation;
   - WinSock.
3. Add a `cfg(windows)` platform backend without changing Unix compilation.
4. Keep Windows capability exposure conditional on the qualification decision:
   disable it before release if Gate C has not passed.
5. Add Windows compilation to ordinary CI before enabling runtime support.

### Exit criteria

- All-target Windows compilation passes.
- Linux and macOS dependency graphs do not include Windows runtime code.
- No Cargo feature combination can accidentally weaken the Windows policy.

## Phase 2: Windows adapter discovery and resolution — implemented, native evidence pending

### Persisted identity

- Enumerate adapters with `GetAdaptersAddresses(AF_UNSPEC)`.
- Persist `IP_ADAPTER_ADDRESSES::AdapterName` exactly.
- Use `FriendlyName` only for display and diagnostics.
- Do not persist interface indices, LUID, `NetworkGuid`, or friendly name as adapter
  identity.

### Per-family state

- Use `IfIndex` for IPv4 and `Ipv6IfIndex` for IPv6.
- Treat zero or missing indices as unavailable for that family.
- Resolve IPv4 and IPv6 independently.
- Store indices only in the current immutable snapshot.

### Source eligibility

Cross-reference adapter candidates with `GetUnicastIpAddressEntry` or
`GetUnicastIpAddressTable`.

For each family:

1. Require an operational, non-loopback, non-receive-only adapter.
2. Require a nonzero family index.
3. Normalize and deduplicate unicast addresses.
4. Accept only preferred addresses with nonzero valid/preferred lifetimes and
   `SkipAsSource == false`.
5. Reject tentative, duplicate, deprecated, invalid, unspecified, multicast, and
   loopback addresses.
6. Reject IPv6 link-local sources for the first release.
7. If an explicit source is configured, require it in the eligible set.
8. Otherwise sort by normalized address bytes and choose the first deterministic
   source.
9. Fail an enabled family closed when no source remains.

### Host policy

- Read `WeakHostSend` and `WeakHostReceive` from the applicable
  `MIB_IPINTERFACE_ROW` for each enabled family.
- Fail strict activation if either flag is enabled.
- Fail strict activation if the policy cannot be read reliably.
- Include host-policy state in status and generation equivalence.

### Exit criteria

- Identity survives friendly-name changes.
- IPv4 and IPv6 indices and sources are independent.
- Deterministic selection does not depend on adapter enumeration order.
- Every eligibility rejection has a focused test and actionable blocked reason.
- Effective source and selection origin are visible in status.

## Phase 3: Windows raw-socket enforcement — implemented, native evidence pending

### Socket creation order

Every strict raw socket must follow this order:

1. Create a socket for the requested family.
2. Apply `IP_UNICAST_IF` or `IPV6_UNICAST_IF` with the resolved live index.
3. Read the option back and reject a mismatch.
4. Bind the resolved effective source address.
5. Connect, listen, or begin UDP use.
6. Verify the resulting local endpoint where applicable.

No failure may trigger an unrestricted retry.

### Byte-order normalization

- Setting `IP_UNICAST_IF` requires the IPv4 index in network byte order.
- Reading `IP_UNICAST_IF` returns host byte order.
- `IPV6_UNICAST_IF` uses host byte order for both set and read.
- Normalize both families to host-order `u32` before readback comparison.

### Listener policy

- Bind strict TCP listeners and shared UDP sockets to concrete effective sources.
- Do not use wildcard addresses in strict Windows mode.
- Evaluate `SO_EXCLUSIVEADDRUSE` for listeners.
- Retain one selected source and one listener/shared-UDP socket per enabled family.
- Document that exact local-address binding is not proof of the physical ingress
  interface.

### Consumers to exercise

- peer TCP;
- listener TCP;
- shared UDP and uTP;
- UDP trackers;
- DHT;
- bound DNS UDP and TCP fallback.

### Exit criteria

- Unit tests cover option encoding, decoding, and readback mismatch.
- Native tests verify applied options and local endpoints.
- Bound DNS inherits the same socket enforcement without a separate fallback path.
- Constructor inventory contains no unclassified direct raw-socket path.

## Phase 4: Strict Windows HTTP — implemented, packet evidence pending

### Client topology

- Preserve one unchanged reqwest client for `Any`.
- Build one generation-owned strict client per enabled Windows family.
- Bind each strict client to that family's effective source.
- Use a family-filtering generation-owned resolver.
- Disable automatic proxies.
- Disable reqwest automatic redirects for strict Windows clients.

### Request contract

- Keep the first implementation GET-only.
- Continue routing all consumers through `NetworkHttpRequest`.
- Do not expose platform-specific clients or request builders to consumers.

### Per-hop algorithm

1. Validate the current URL and consumer-specific destination policy.
2. Resolve with the current generation's resolver.
3. Validate every resolved address before connecting.
4. Attempt one family.
5. Allow cross-family fallback only for a transport failure before any response is
   received for that hop.
6. Never retry a hop through another family after receiving a response.
7. If the response redirects, resolve `Location`, enforce loop/hop limits, validate
   the next URL, and begin a new hop.
8. Check generation ownership and cancellation before resolution, connection,
   redirect transition, and returning the response.

### Header policy

- Generate `Host` for each target URL.
- Remove hop-by-hop headers at every redirect.
- Strip credentials and sensitive headers on host or effective-port changes.
- Preserve safe end-to-end headers, including web-seed `Range`.
- Never generate an HTTPS-to-HTTP `Referer`.

### Required tests

- same-origin and cross-origin redirects;
- cross-port and cross-scheme redirects;
- relative `Location`;
- redirect loop and hop limit;
- range preservation;
- credential and cookie stripping;
- DNS rebinding and private-address rejection for RSS;
- cross-family fallback before a response;
- no fallback after a response;
- generation invalidation during every request phase.

### Exit criteria

- Trackers, RSS, web seeds, redirects, and version checks use the wrapper.
- Strict HTTP never uses an automatic proxy or unrestricted client.
- Packet evidence exists for each HTTP consumer and redirect hop.

## Phase 5: Windows change detection and recovery — implemented, disruptive evidence pending

### Tasks

1. Keep snapshot polling during initial backend development.
2. Add `NotifyUnicastIpAddressChange`.
3. Add `NotifyIpInterfaceChange` if native tests show it is required.
4. Keep callbacks minimal: signal Tokio and perform a full snapshot refresh there.
5. Cancel registrations with `CancelMibChangeNotify2` before callback state is
   destroyed.
6. Retain a low-frequency reconciliation poll.

### Lifecycle order to preserve

Supervisor:

1. Invalidate the current generation.
2. Publish supervisor Blocked.
3. Resolve and preflight the replacement.
4. Publish supervisor Ready(candidate).

App:

1. Publish activation Pending(candidate generation).
2. Close old listeners and clear old reachability state.
3. Stop old DHT and await acknowledgement.
4. Stop old uTP/shared UDP and drain admitted sends.
5. Prepare the candidate scope.
6. Bind replacement TCP and shared UDP/uTP listeners.
7. Discover the actual peer port.
8. Publish Active(generation, port).
9. Reconfigure DHT for the active generation.

### Exit criteria

- Adapter/address/policy changes replace the generation exactly once.
- Adapter loss produces no strict traffic on another interface.
- Candidate and listener failures remain blocked.
- Stale DNS, tracker, peer, HTTP, DHT, and activation results are rejected.
- Callback teardown is race-safe under repeated enable/disable cycles.

## Phase 6: Status, configuration, and documentation — implementation present, release review pending

### Tasks

- Show persisted identity separately from friendly display name.
- Show IPv4 and IPv6 indices independently.
- Show configured and effective sources independently.
- Show eligible sources and weak-host policy per family.
- Report whether the effective source was explicit or automatic.
- Provide actionable blocked reasons for discovery, eligibility, host-policy, option,
  source-bind, listener, and HTTP failures.
- Preserve `Any` defaults and configuration migration behavior.
- Update `docs/native-network-binding.md` only after qualification defines the final
  supported Windows contract.

### Exit criteria

- Status is sufficient to diagnose selection and activation without packet capture.
- Friendly-name changes do not rewrite persisted identity.
- Windows interface mode is exposed only when the release gate has passed; otherwise
  `INTERFACE_BINDING_SUPPORTED` is returned to false before release.

## Phase 7: Qualification

### Gate A: ordinary Windows CI

Run on every relevant pull request:

- formatting and diff checks;
- all-target/all-feature Clippy;
- complete all-feature tests;
- Windows compilation and packaging checks;
- parsed adapter/unicast fixtures;
- identity/display and family-equivalence tests;
- source eligibility and deterministic selection;
- option byte-order encoding and readback tests;
- local socket binding where hosted runners permit it;
- lifecycle, cancellation, HTTP redirect, and `Any` parity tests.

This gate is regression coverage, not leak qualification.

### Gate B: native multi-adapter integration

Use a disposable Windows VM or dedicated host with at least two independently
routed adapters. Exercise:

- live adapter and unicast tables;
- separate family indices and source choices;
- raw TCP/UDP and bound DNS;
- strict per-family HTTP;
- listener activation and replacement;
- friendly-name and source-address changes;
- independent IPv4/IPv6 failure;
- real VPN adapter behavior.

Retain the exact revision, topology, commands, logs, and results.

### Gate C: elevated packet-level release qualification

Use an elevated disposable VM or self-hosted runner. Capture and attribute:

- HTTP trackers;
- RSS and every redirect hop;
- web seeds;
- bound DNS UDP and TCP;
- peer/listener TCP;
- UDP trackers;
- DHT;
- uTP/shared UDP.

Exercise:

- allowed and forbidden adapters;
- weak-host send and receive negative cases;
- adapter disable/re-enable;
- VPN loss and recovery;
- pending and blocked intervals;
- stale-generation completion;
- IPv4-only, IPv6-only, and dual-stack configurations;
- strict mode and default `Any` parity.

Required results:

- zero packets on a forbidden/default adapter in strict mode;
- zero torrent traffic while pending or blocked;
- exactly one valid replacement activation per recovery;
- advertised ports match actual listeners;
- stale results are rejected;
- automatic proxies receive no strict traffic;
- default `Any` matches refreshed main.

The branch currently enables `INTERFACE_BINDING_SUPPORTED` on Windows. Gate C must
therefore pass on the release candidate before merge/release, or the capability must
be disabled again. A validation harness is not itself Gate C evidence.

## Linux preservation plan

Do not redesign the Linux backend while implementing Windows.

Preserve:

- `getifaddrs` discovery and `if_nametoindex` identity resolution;
- `SO_BINDTODEVICE` before socket use;
- fail-closed handling when the option cannot be applied;
- Linux IPv6 readiness filtering through `/proc/net/if_inet6`;
- strict reqwest interface policy;
- generation-owned bound DNS;
- activation, invalidation, and stale-result rules;
- direct Tokio/system-DNS behavior for `Any`.

Regression gates:

- production constructor inventory;
- privileged namespace leak audit;
- pending/blocked and old-generation packet checks;
- listener-port replacement and recovery;
- strict RSS/redirect/proxy checks;
- default-`Any` parity.

Packaging must continue to document the `CAP_NET_RAW` requirement where Linux
device binding requires it.

## macOS preservation plan

Do not redesign the macOS backend while implementing Windows.

Preserve:

- `getifaddrs` discovery and `if_nametoindex` resolution;
- native family-specific bound-interface socket options;
- option application before connect/bind/use;
- strict reqwest interface policy;
- snapshot polling and generation replacement;
- generation-owned bound DNS;
- direct Tokio/system-DNS behavior for `Any`.

Regression gates:

- default-`Any` main parity;
- strict interface selection and source/interface observation;
- VPN disconnect/reconnect and interface disappearance;
- pending/blocked traffic checks;
- listener-port activation and recovery;
- bound DNS and strict HTTP/RSS behavior;
- explicit separation between endpoint/log evidence and unavailable packet evidence.

macOS does not currently have Linux-equivalent IPv6 DAD-state filtering or native
SystemConfiguration notification ownership. Treat both as documented coverage gaps,
not implicit guarantees.

## Docker and Gluetun qualification

Treat Gluetun as a separate deployment boundary:

- Superseedr normally runs in `Any` inside Gluetun's network namespace.
- Gluetun's routing and firewall provide confinement.
- Native Linux interface-binding evidence does not substitute for Gluetun evidence.

Before release, rerun the complete Gluetun audit on the release candidate:

- shared namespace and VPN egress;
- DNS behavior;
- outage blocking;
- forwarded-port propagation and rotation;
- stale UDP/uTP descriptor checks;
- local-container versus public-route observations.

## Cross-platform release checklist

- [ ] No unclassified production socket, DNS, HTTP, or proxy constructor.
- [ ] All enabled families pass strict preflight.
- [ ] Actual listener port is published only after successful activation.
- [ ] Interface disappearance produces no forbidden-route traffic.
- [ ] Pending and blocked intervals produce no torrent traffic.
- [ ] Recovery creates one current replacement generation.
- [ ] Stale consumer and resolver results are rejected.
- [ ] Bound DNS UDP and TCP fallback remain confined.
- [ ] Strict HTTP and RSS cannot use automatic proxies.
- [ ] `Any` remains compatible with refreshed main.
- [ ] Platform evidence limitations are stated independently.
- [ ] Windows Gates A, B, and C pass before capability enablement.
- [ ] If Gate C is unavailable or fails, Windows interface capability is disabled
      before merge/release.
- [ ] Linux namespace evidence is refreshed for the release candidate.
- [ ] macOS native VPN/interface evidence is refreshed for the release candidate.
- [ ] Gluetun outage and port-rotation evidence is refreshed for the release candidate.

## Execution order

1. Audit the exact Windows implementation against Phases 1–6 and close any code or
   focused-test gaps found by that audit.
2. Pass Gate A on the rebased release-candidate commit and retain the CI links.
3. Run Gate B on a native multi-adapter Windows host and retain the revision,
   topology, commands, logs, and results.
4. Run Gate C with elevated packet capture and disruptive adapter/VPN cases.
5. Fix any failed consumer, lifecycle, or confinement case and repeat the affected
   gate on the new revision.
6. If Gate C cannot pass or cannot be run, disable Windows interface capability
   before merge/release; do not substitute the harness or hosted CI for packet
   evidence.
7. Update user documentation to match the qualified Windows contract and record the
   remaining limitations.
8. Refresh the Linux namespace, macOS native, and Gluetun evidence on the same
   release candidate.
9. Complete the cross-platform checklist and record the final capability decision.
