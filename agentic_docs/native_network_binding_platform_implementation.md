# Native Network Binding: Platform Implementation

## Purpose

This document guides future platform work on Superseedr's native network binding.
It records the shared architectural contract and the proposed Windows implementation
at a level suitable for implementation planning and review.

Windows is the active design section. Linux and macOS have reserved sections so
their implementation details and evidence can be documented without mixing
platform mechanisms.

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

# Linux

## Reserved platform section

Document the verified Linux implementation here, including:

- device-binding mechanism and capability requirements;
- interface/address discovery and change detection;
- listener, shared UDP, uTP, and bound-DNS behavior;
- generation replacement and blocked-state invariants;
- namespace and packet-capture evidence;
- container and VPN-specific findings;
- `Any` parity and known limitations.

Do not derive Linux behavior from the Windows plan; document the production code
and Linux audit evidence directly.

---

# macOS

## Reserved platform section

Document the verified macOS implementation here, including:

- IPv4 and IPv6 interface-index socket options;
- interface/address discovery and SystemConfiguration notifications;
- listener, shared UDP, uTP, and bound-DNS behavior;
- generation replacement and VPN recovery;
- `Any` parity;
- packet/syscall tracing limitations under SIP and the alternative evidence used.

Do not derive macOS behavior from the Windows plan; document the production code
and macOS audit evidence directly.

---

# Cross-platform completion

Once all three platform sections are populated, reconcile them around the common
observable contract: configured identity, live indices and addresses, socket
enforcement, listener topology, DNS and HTTP behavior, change detection, activation
ordering, privileges, evidence, and `Any` compatibility.

The operating-system mechanisms do not need to be identical. Their fail-closed
behavior and consumer ownership do.
