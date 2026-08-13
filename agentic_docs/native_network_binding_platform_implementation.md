# Native Network Binding: Platform Implementation

## Purpose

This document guides future platform work on Superseedr's native network binding.
It records the shared architectural contract and the proposed Windows implementation
at a level suitable for implementation planning and review.

Windows is the active implementation proposal. Linux and macOS document the
current shared Unix architecture, their platform-specific enforcement, and the
scope and limitations of retained validation evidence.

User-facing configuration and operating instructions remain in
`docs/native-network-binding.md`.

## Shared architecture

The platform implementations may use different operating-system APIs, but they
must preserve the same behavior:

- `Any` remains unrestricted and compatible with the main-branch networking path.
- Strict mode never falls back to `Any`, another interface, an unrestricted HTTP
  client, an automatic proxy, or system DNS when bound DNS is selected.
- Interface state is resolved into an immutable, generation-owned snapshot.
- A network change invalidates old leases before replacement traffic is allowed.
- A replacement remains pending until its listeners are bound and their actual
  peer port is known.
- A failed replacement leaves networking blocked while the application stays
  available for recovery.
- TCP, UDP, HTTP, DNS, trackers, peers, web seeds, RSS, DHT, and uTP receive their
  networking authority through the centralized runtime.
- Stale work from an invalidated generation cannot update current state.

Platform-specific discovery and socket policy should live behind a small internal
backend beneath `SocketFactory`. Consumers must not contain Windows, Linux, or
macOS binding logic.

The main implementation seams are:

- `src/networking/runtime.rs`: policy resolution, generations, leases, sockets,
  HTTP clients, and platform integration;
- `src/networking/dns.rs`: generation-owned bound DNS;
- `src/app.rs`: listener activation and replacement ordering;
- torrent managers, trackers, RSS, and DHT: consumers of leases and activation,
  not owners of platform policy.

---

# Windows

## Goal and release posture

Windows interface mode should offer the same application-level fail-closed promise
as the existing strict implementations: Superseedr-owned traffic uses the selected
adapter, and losing that adapter cannot redirect traffic through the default route.

Windows support should be built behind `cfg(windows)` while the public capability
remains disabled. Enable `INTERFACE_BINDING_SUPPORTED` for Windows only after native
socket, lifecycle, DNS, HTTP, and packet-level tests pass.

A VPN kill switch or Windows Firewall remains recommended as system-wide defense
in depth. Superseedr can constrain only sockets it owns.

## Open-source comparison

The proposed design is consistent with mature open-source networking clients:

- **libtorrent** uses `GetAdaptersAddresses`, distinguishes internal adapter
  identity from friendly display name, expands an interface into concrete
  addresses, source-binds outgoing and HTTP sockets, filters destinations by
  address family, and reopens sockets after Windows address-change notification.
- **qBittorrent** carries the Windows adapter GUID into both listener and outgoing
  libtorrent settings. If the configured adapter is unavailable, it retains the
  restriction rather than substituting an unrestricted value.
- **curl** does not support Windows interface names for `CURLOPT_INTERFACE`; its
  portable Windows path is binding a concrete source address.
- **Transmission** source-binds peer sockets and passes a selected source address
  into its HTTP transport.

These implementations support concrete source-address binding as the foundation
of Windows confinement. Windows interface-index socket options are useful
additional enforcement and verification, but should not replace source binding.

Superseedr remains intentionally stricter than these comparisons in generation
ownership, activation gating, bound DNS, redirect validation, and stale-result
rejection.

Reference sources:

- [libtorrent interface enumeration](https://github.com/arvidn/libtorrent/blob/RC_2_0/src/enum_net.cpp)
- [libtorrent socket binding](https://github.com/arvidn/libtorrent/blob/RC_2_0/include/libtorrent/enum_net.hpp)
- [libtorrent HTTP binding](https://github.com/arvidn/libtorrent/blob/RC_2_0/src/http_connection.cpp)
- [libtorrent network-change handling](https://github.com/arvidn/libtorrent/blob/RC_2_0/src/ip_notifier.cpp)
- [qBittorrent interface configuration](https://github.com/qbittorrent/qBittorrent/blob/master/src/base/bittorrent/sessionimpl.cpp)
- [curl interface contract](https://github.com/curl/curl/blob/master/docs/libcurl/opts/CURLOPT_INTERFACE.md)
- [Transmission peer binding](https://github.com/transmission/transmission/blob/main/libtransmission/peer-socket-tcp.cc)
- [Transmission HTTP binding](https://github.com/transmission/transmission/blob/main/libtransmission/web.cc)

## Dependency boundary

Use a direct, Windows-only `windows-sys` dependency:

```toml
[target.'cfg(windows)'.dependencies]
windows-sys = { version = "0.61.2", features = [
    "Win32_Foundation",
    "Win32_NetworkManagement_IpHelper",
    "Win32_NetworkManagement_Ndis",
    "Win32_Networking_WinSock",
] }
```

Reduce the feature list if the final implementation needs less. Do not make final
Windows support an optional Cargo feature: the target gate prevents Windows-only
code from affecting Linux and macOS builds without making security behavior depend
on a feature combination.

## Adapter model

Discover adapters with `GetAdaptersAddresses(AF_UNSPEC)` and keep two concepts
separate:

- a stable adapter identity for persisted configuration;
- a friendly name for display and diagnostics.

Do not persist interface indices. Windows can assign different IPv4 and IPv6
indices, and indices can change when an adapter is disabled and re-enabled. The
live snapshot should therefore contain:

- stable identity and friendly name;
- IPv4 and IPv6 indices;
- operational state;
- preferred IPv4 and IPv6 address sets;
- one selected source address per enabled family;
- any Windows host policy that affects the confinement guarantee.

Only operational, non-loopback adapters and usable preferred unicast addresses are
eligible. Explicitly configured addresses must belong to the selected adapter.
Automatic source selection must be deterministic and observable in status.

Changes to identity, indices, selected addresses, eligible address sets, enabled
families, or relevant host policy must replace the generation.

## Socket enforcement

Every strict Windows socket should be configured centrally in this order:

1. Create the socket for the required family.
2. Set `IP_UNICAST_IF` or `IPV6_UNICAST_IF` to the corresponding live index.
3. Read the option back and reject a mismatch.
4. Bind the selected source address.
5. Connect, listen, or begin UDP use.
6. Where applicable, verify the resulting local endpoint.

Any failure is local and fail-closed. It must not trigger an unrestricted retry.

Strict TCP listeners and shared UDP sockets must bind concrete selected addresses,
not wildcard addresses. The interface-index options primarily direct outgoing
traffic and are insufficient to constrain inbound listeners by themselves.

The initial Windows implementation should select one preferred source per enabled
family. This fits the existing one-listener and one-shared-UDP-socket-per-family
model. It is narrower than listening on every address assigned to an adapter and
should be reflected in status and documentation. Supporting every adapter address
would require a deliberate listener-topology expansion.

Evaluate `SO_EXCLUSIVEADDRUSE` for Windows listeners. Datagram interface-list
options may be considered as defense in depth, but exact source binding remains the
baseline.

## HTTP

HTTP is the main platform seam because reqwest exposes interface-name binding only
on Unix-family platforms and represents only one local source address per client.

Avoid forking reqwest, hyper-util, or socket2 for the first implementation. Instead:

- Keep `Any` as one unchanged reqwest client.
- Build one generation-owned strict client per enabled Windows address family.
- Give each client its selected source address and a family-filtering resolver.
- Disable automatic proxies for strict clients.
- Preserve RSS public-address and redirect-hop validation.
- Preserve bound-DNS ownership and generation cancellation.

`NetworkHttpClient` can return a small internal GET request wrapper supporting the
operations consumers currently need: headers and `send()`. For a dual-family
request, it may try the other enabled family only after a pre-response connection
failure. It must not retry an HTTP response or escape the generation's policy.

This keeps Windows complexity centralized and avoids platform branches in tracker,
RSS, update, and web-seed consumers.

## DNS

System DNS remains managed by Windows and outside the strict application-level DNS
guarantee. The existing warning must remain.

Bound DNS requires no separate consumer design. Once `SocketFactory` enforces the
Windows policy, generation-owned UDP queries and TCP fallback inherit it. There is
no system-resolver or unrestricted fallback.

## Network changes and recovery

The first implementation may retain snapshot polling while the socket backend is
developed. Polling detects changes; it is not the confinement mechanism.

The completed Windows backend should use `NotifyUnicastIpAddressChange`, and add
`NotifyIpInterfaceChange` if native testing shows it is needed for timely adapter
state changes. Callbacks should do minimal work, signal the Tokio runtime, and
cause a complete snapshot refresh. Registrations must be cancelled safely with
`CancelMibChangeNotify2` before callback state is destroyed.

A low-frequency reconciliation poll can remain as protection against missed or
coalesced notifications.

Replacement retains the existing lifecycle:

```text
pending replacement
  -> invalidate old leases
  -> stop old uTP and drain admitted shared-UDP sends
  -> close old listeners and shared UDP
  -> resolve and preflight the current adapter snapshot
  -> bind replacement listeners and shared UDP/uTP
  -> discover the actual peer port
  -> publish active generation
  -> release generation-owned consumers
```

Failure at any point after pending publication leaves networking blocked until a
valid later snapshot can activate.

## Windows host policy

Windows normally uses strong-host routing, but weak-host behavior can be enabled by
an administrator. The backend should inspect and report the applicable
`MIB_IPINTERFACE_ROW` state.

Native negative testing must determine whether source binding plus the family
interface option remains confined when weak-host sending is enabled. If it does
not, or the state cannot be checked reliably, strict Windows activation should fail
closed for that configuration. Do not claim the Windows guarantee until this case
has packet-level evidence.

## Implementation stages

1. **Platform model:** add the target dependency, platform backend, stable adapter
   identity, family-specific indices, address filtering, and deterministic source
   selection.
2. **Raw sockets:** add option enforcement/readback, source binding, exact listener
   binding, endpoint verification, and bound-DNS coverage.
3. **HTTP:** introduce per-family strict clients and the small internal request
   wrapper while preserving `Any` unchanged.
4. **Lifecycle:** add native notifications, reconciliation, and safe callback
   ownership.
5. **Qualification:** run native and packet-level tests, establish weak-host
   behavior, verify `Any` parity, then enable Windows interface support.

## Acceptance criteria

Before Windows support is exposed, prove on a real Windows host that:

- stable adapter selection survives friendly-name changes;
- IPv4 and IPv6 indices and sources are handled independently;
- raw TCP, UDP, listeners, bound DNS, HTTP, trackers, peers, web seeds, RSS, DHT,
  and uTP use the selected adapter/source;
- strict mode ignores automatic proxy routing;
- disabling or removing the selected adapter produces no packets on a forbidden
  or default adapter;
- pending and blocked intervals produce no torrent traffic;
- recovery publishes exactly one valid replacement generation and correct port;
- stale DNS, tracker, and peer results are rejected;
- IPv4-only, IPv6-only, dual-stack, and real VPN adapters work as designed;
- the weak-host negative case has an explicit pass or fail-closed result;
- the Windows `Any` path matches refreshed main in socket policy and observed
  behavior;
- the production constructor inventory contains no unclassified socket, DNS,
  HTTP, or proxy paths.

Packet evidence should come from `pktmon`/pcapng or another suitable Windows
capture facility. Endpoint listings and logs supplement packet evidence but do not
replace it.

## CI gate

The current Windows workflow is manual-only. Before enabling the feature, Windows
must become an automatic pull-request gate for networking, dependency, activation,
consumer-transport, and Windows packaging changes.

The gate should include formatting, Clippy, the complete all-feature test suite,
and focused native Windows socket/lifecycle tests. Cross-target compilation from
another operating system is useful compile coverage but is not qualification
evidence.

## Main Windows risks

- Adapter behavior differs across Wintun, TAP, PPP, Hyper-V, and vendor-specific
  VPN implementations.
- Indices and IPv6 privacy addresses can change without a new friendly name.
- Weak-host configuration can alter source-routing assumptions.
- Windows change callbacks run on OS-managed threads and require careful teardown.
- The initial one-address-per-family contract is intentionally narrower than
  binding every address on an adapter.
- Only native packet capture can establish the final no-leak claim.

---

# Linux and macOS

## Shared Unix implementation

Linux and macOS share the same application-level architecture. Their interface
enforcement differs at the socket layer, so those mechanisms are documented
separately below.

Both platforms currently:

- discover interface names, flags, and addresses through `getifaddrs`;
- resolve the live interface index with `if_nametoindex`;
- sort and deduplicate the observed IPv4 and IPv6 address sets;
- require every enabled family to exist on the selected interface;
- validate any explicitly selected source against that interface;
- compare the resolved snapshot once per second and replace the generation when
  the index or relevant address set changes;
- use strict `socket2` construction for TCP, listeners, and UDP while leaving
  `Any` on the direct Tokio path;
- pass the interface name to reqwest for generation-owned tracker, RSS, update,
  and web-seed clients;
- disable automatic proxies in strict modes;
- route bound DNS through generation-owned `SocketFactory` UDP sockets with TCP
  fallback;
- use the same activation barrier, manager ownership checks, shared-UDP draining,
  and stale-result rejection.

The snapshot poll is the current change detector on both platforms. It is not the
confinement mechanism: the operating-system socket policy prevents route fallback
during the detection interval. Native event notification could reduce recovery
latency later, but should replace neither socket enforcement nor reconciliation
until it has equivalent lifecycle and test coverage.

Strict interface mode binds sockets to the selected interface but does not choose
one concrete source address unless the user configures one. This lets normal OS
source selection continue within the selected interface. Exact source selection
is currently limited to one enabled family when IPv4 and IPv6 would otherwise be
active together.

Local-address mode is different from interface mode. It source-binds the chosen
address, but is deliberately documented as weaker because it does not add the
platform's interface-level socket restriction if routing policy changes.

## Shared runtime and lifecycle

The generation is preflighted before becoming supervisor-ready. For each enabled
family, preflight creates representative TCP and UDP sockets, applies the strict
policy, and verifies that any requested source can bind.

Application activation is a separate barrier:

1. The App prepares a scope for the ready generation.
2. It binds the TCP peer listeners and shared UDP/uTP listener.
3. It discovers the actual port when random-port mode is used.
4. It publishes `active(generation, port)`.
5. Managers may then run trackers, peers, web seeds, DHT, and related work only
   while they own that current active scope.

During replacement, the old scope is invalidated before traffic resumes. uTP is
stopped before shared UDP, admitted sends are drained, and listener failure leaves
activation blocked. Latest-only watch state prevents manager command-queue pressure
from delaying the App's authoritative replacement.

The listener layer coordinates IPv4 and IPv6 onto one advertised port and forces
IPv6-only behavior on the IPv6 socket. A failure in one family may retain the
other usable family where the existing listener policy permits it; complete
listener failure blocks activation.

## HTTP and DNS

Reqwest's Unix interface binding is applied to all generation-owned HTTP clients.
Single-family configurations additionally set a compatible local address so DNS
results and connects cannot cross into the disabled family. The normal dual-stack
interface configuration relies on device binding and reqwest's native resolver
behavior unless bound DNS is selected.

RSS remains stricter than general traffic:

- automatic proxies are disabled;
- literal destinations and resolved answers are validated;
- private/non-public pivots are rejected;
- every redirect hop remains subject to the current family and destination policy.

With system DNS, hostname resolution remains owned by the operating system and is
outside the application-level DNS leak guarantee. With bound DNS, literal server
addresses are queried through strict generation sockets, truncated UDP responses
fall back to strict TCP, and invalidation cancels or rejects late results. There is
no system-resolver fallback from bound DNS.

## Linux-specific enforcement

Linux strict interface mode uses `SO_BINDTODEVICE` through
`socket2::Socket::bind_device`. This is applied before connect, bind, or UDP use
and covers the central TCP and UDP constructors inherited by peers, trackers,
DHT, uTP, listeners, and bound DNS. Reqwest applies its corresponding interface
policy to HTTP sockets.

Linux device binding may require `CAP_NET_RAW`. Failure to apply the option causes
preflight or socket creation to fail and leaves networking blocked; the application
must not retry without the device policy. Packaging does not grant the capability
automatically. The reviewed systemd drop-in and direct-binary capability guidance
remain in `docs/native-network-binding.md`.

Linux also performs additional IPv6 readiness filtering. `getifaddrs` does not
expose the address-state flags needed for Duplicate Address Detection, so the
runtime consults `/proc/net/if_inet6` and withholds addresses marked DAD-failed or
tentative unless they are optimistic. If this state cannot be established, IPv4
discovery can continue, but IPv6 discovery fails closed rather than treating an
unknown address as ready.

### Linux evidence

The automated Linux release workflow runs formatting, all-feature Clippy, the
complete all-feature test suite, private-build checks, and the privileged namespace
leak test.

The retained privileged audit under
`integration_tests/artifacts/network-binding-current-20260812T000856Z/` exercised
the exact production socket factory and resolver in isolated Linux namespaces. It
reported:

- zero forbidden/default-interface packets;
- zero packets from old generations or during pending/blocked intervals;
- 46 successful `SO_BINDTODEVICE` calls and no failures;
- correct listener-port publication across replacement and recovery;
- rejection of stale DNS, tracker, peer, web-seed, and activation results;
- RSS redirect/rebinding and automatic-proxy rejection;
- default-`Any` parity with the compared main revision.

This is strong packet-level evidence on Debian 12 with a LinuxKit kernel, not proof
for every distribution, kernel, VPN driver, suspend/resume path, or prolonged
interface-flapping scenario.

### Docker and Gluetun boundary

The supported Compose deployment is related but distinct from native Linux
interface mode. Superseedr normally runs in `Any` inside Gluetun's shared network
namespace; Gluetun's TUN routing and firewall provide the confinement boundary.

The retained Gluetun audit under
`integration_tests/artifacts/docker-gluetun-audit-20260812T122148Z/` confirmed the
shared namespace, VPN egress, local resolver, forwarded-port propagation, normal
torrent traffic, and public DNS/TCP/HTTPS blocking during a controlled outage. It
also found that Docker-local traffic could still use the namespace's `eth0` while
Gluetun userspace was paused. That is not evidence of a public-IP escape, but it
means the Compose firewall did not meet the audit's stricter zero-clear-interface
criterion for reachable local-container destinations.

The same audit predated later shared-UDP port-rebinding fixes and must not be used
as current proof of zero stale descriptors after forwarded-port rotation. A final
Gluetun qualification should rerun the complete outage and port-rotation audit on
the release candidate.

## macOS-specific enforcement

macOS strict interface mode resolves one interface index and applies the native
family-specific options through socket2:

- IPv4 uses the platform's IPv4 bound-interface option;
- IPv6 uses the platform's IPv6 bound-interface option.

The option is applied before TCP connect, TCP listener bind, or UDP bind. Reqwest
receives the selected interface name for HTTP. Unlike Linux, macOS does not require
`CAP_NET_RAW` for this path.

macOS currently relies on `getifaddrs` state and the periodic snapshot monitor; it
does not yet use a SystemConfiguration notification backend. Interface loss or an
address-set change therefore becomes authoritative when the next snapshot is
resolved, while already-created strict sockets remain constrained by their kernel
interface option.

The shared Unix discovery path does not currently add a macOS-specific DAD-state
source comparable to Linux's `/proc/net/if_inet6` filter. This is a platform
coverage difference worth retaining in review and future native stress testing.

### macOS evidence

The retained parity audit under
`integration_tests/artifacts/macos-any-parity-20260812T033403Z/` compared the
branch's default/unbound path with the tested main revision. It reported matching
effective IPv4/IPv6 TCP/UDP kernel profiles and socket-option sets, no unexpected
source/interface confinement, coverage of the requested consumers, isolated RSS
strictness, and successful fresh shutdown probes.

This establishes strong `Any` compatibility evidence, but not packet-level
identity. SIP denied DTrace/dtruss and BPF capture, so the audit used libproc,
nettop, lsof, application logs, and fixture telemetry. Exact syscall timing,
individual system-resolver queries, and packet capture remained unresolved.

macOS strict binding and VPN disconnect/reconnect behavior should continue to be
qualified on a real host. A release claim should distinguish live endpoint/log
evidence from packet evidence and should not imply that the Linux namespace audit
proves the Apple socket-option path.

## Linux and macOS acceptance criteria

For each platform, preserve or re-establish the following before release:

- production constructor inventory has no unclassified socket, DNS, HTTP, or
  proxy paths;
- all enabled families pass strict preflight;
- listeners and shared UDP publish the actual active port only after success;
- interface disappearance creates no forbidden-interface traffic;
- pending and blocked intervals contain no torrent traffic;
- recovery creates one current replacement generation;
- stale consumer and resolver results are rejected;
- bound DNS UDP and TCP fallback remain confined;
- strict HTTP and RSS cannot route through automatic proxies;
- `Any` remains compatible with refreshed main;
- evidence limitations are stated per platform rather than generalized across
  Linux and macOS.

---

# Cross-platform completion

Maintain all three platform sections around the same observable contract:
configured identity, live indices and addresses, socket enforcement, listener
topology, DNS and HTTP behavior, change detection, activation ordering, privileges,
evidence, and `Any` compatibility.

The operating-system mechanisms do not need to be identical. Their fail-closed
behavior and consumer ownership do.
