# Native Network Binding: Platform Implementation

## Purpose

This document guides future platform work on Superseedr's native network binding.
It records the shared architectural contract and the proposed Windows implementation
at a level suitable for implementation planning and review.

Windows is the active implementation proposal. Linux and macOS document the
current shared Unix architecture, their platform-specific enforcement, and the
scope and limitations of retained validation evidence.

The platform-neutral resolved-binding model and HTTP request-wrapper seam described
below are now implemented. The Windows adapter backend and its manual redirect
state machine remain gated future work.

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

Windows interface mode should offer a fail-closed **outbound** application-level
promise: Superseedr-owned outbound traffic uses the selected adapter, and losing
that adapter cannot redirect traffic through the default route. The first release
does not promise that an inbound connection arrived on a particular physical
interface; binding a listener to an adapter address proves the local destination
address, not the ingress interface.

Windows support should be built behind `cfg(windows)` while the public capability
remains disabled. Enable `INTERFACE_BINDING_SUPPORTED` for Windows only after the
elevated packet-level qualification gate passes, in addition to ordinary CI and
native multi-adapter integration tests.

A VPN kill switch or Windows Firewall remains recommended as system-wide defense
in depth. Superseedr can constrain only sockets it owns.

## Open-source comparison

The proposed design is consistent with mature open-source networking clients:

- **libtorrent** uses `GetAdaptersAddresses`, distinguishes internal adapter
  identity from friendly display name, expands an interface into concrete
  addresses, source-binds outgoing and HTTP sockets, filters destinations by
  address family, and reopens sockets after Windows address-change notification.
- **qBittorrent** carries its configured Windows adapter selection into both
  listener and outgoing libtorrent settings. If the configured adapter is
  unavailable, it retains the restriction rather than substituting an unrestricted
  value. This comparison does not define Superseedr's persisted adapter identity.
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
- [Windows adapter identity and family indices](https://learn.microsoft.com/en-us/windows/win32/api/iptypes/ns-iptypes-ip_adapter_addresses_lh)
- [Windows unicast address state](https://learn.microsoft.com/en-us/windows/win32/api/netioapi/nf-netioapi-getunicastipaddressentry)
- [Windows per-family host policy](https://learn.microsoft.com/en-us/windows/win32/api/netioapi/ns-netioapi-mib_ipinterface_row)
- [Windows IPv4 socket options](https://learn.microsoft.com/en-us/windows/win32/winsock/ipproto-ip-socket-options)
- [Windows IPv6 socket options](https://learn.microsoft.com/en-us/windows/win32/winsock/ipproto-ipv6-socket-options)

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

## Implemented prerequisite: platform-neutral resolved model

The former `ResolvedNetworkBinding` and `InterfaceSnapshot` were shaped around the
Unix implementation: one interface name, one interface index, and
`ipv4_address`/`ipv6_address` fields that represented an explicitly configured
source. The shared runtime has now been refactored before adding the Windows
backend.

Do not automatically place a Windows-selected source into the existing address
fields. `generation_equivalent` currently stops comparing an interface's address
set when the corresponding address field is populated, because a populated field
means an explicit source on Unix. Reusing it for an automatic choice could leave a
generation active after its eligible-address set or selection policy changed.

The implementation retains the serialized `NetworkBindingConfig.interface` field
as the platform's persisted identity and replaces the internal
one-index/one-address representation with an adapter identity plus two resolved
family records. In simplified form:

```text
ResolvedNetworkBinding
  interface_identity       persisted configuration value
  interface_display_name   diagnostics only
  ipv4: ResolvedAddressFamily<Ipv4Addr>
  ipv6: ResolvedAddressFamily<Ipv6Addr>

ResolvedAddressFamily<T>
  enabled
  interface_index
  configured_source        copied only from explicit configuration
  effective_source         explicit or automatically selected source
  eligible_sources         normalized, sorted set used by selection
  host_policy              per-family policy relevant to confinement
```

On Unix, identity and display name remain the same, both family records receive the
same `if_nametoindex` value, and effective sources remain unset in automatic
interface-only mode. Explicit sources populate both configured and effective
fields. This keeps the Unix socket behavior unchanged while removing Windows
assumptions from the shared model.

`InterfaceSnapshot` and `NetworkInterfaceInfo` now expose identity, display, and
family-specific indices. Runtime status reports configured and effective sources,
eligible source sets, both live indices, and both host-policy states. The existing
`selected_ipv4_address`/`selected_ipv6_address` fields report effective sources;
`configured_ipv4_address`/`configured_ipv6_address` are distinct.

Generation equivalence compares identity, display metadata, live indices, enabled
families, configured sources, effective sources, and host policy for enabled
families. Eligible sets are compared whenever source selection is automatic. For
an explicit source, resolution still fails if that address ceases to be eligible,
while an unrelated eligible-address change does not churn the generation. Display
metadata is not persisted identity, but a display-only change replaces the current
immutable snapshot so status is refreshed.

## Windows adapter discovery and source eligibility

Discover adapters with `GetAdaptersAddresses(AF_UNSPEC)`. Persist the exact opaque
value from `IP_ADAPTER_ADDRESSES::AdapterName` in
`NetworkBindingConfig.interface`. Use `FriendlyName` only for display and
diagnostics. Do not persist `IfIndex`, `Ipv6IfIndex`, `Luid`, or `NetworkGuid` as the
adapter identity.

`IfIndex` is the live IPv4 index and `Ipv6IfIndex` is the live IPv6 index. They are
family-specific, can change after disable/re-enable, and belong only in the current
snapshot. A zero or missing index makes that enabled family unavailable.

`GetAdaptersAddresses` supplies adapter membership and candidate unicast addresses,
but is not sufficient to decide source eligibility. Cross-reference each candidate
with `GetUnicastIpAddressEntry`, or enumerate `GetUnicastIpAddressTable`, to obtain
the corresponding `MIB_UNICASTIPADDRESS_ROW`.

Apply this deterministic policy independently to IPv4 and IPv6:

1. Require the adapter to be operational and not loopback or receive-only.
2. Require the family's live interface index to be nonzero.
3. Normalize and deduplicate the candidate unicast addresses.
4. Keep only rows for the selected adapter/family whose `DadState` is exactly
   `IpDadStatePreferred`, whose valid and preferred lifetimes are nonzero, and whose
   `SkipAsSource` value is false. Tentative, duplicate, deprecated, invalid,
   unspecified, multicast, and loopback sources are ineligible.
5. Reject IPv6 link-local sources for the first release because reqwest's local
   address API cannot carry the required scope identifier through this design.
6. If an explicit source is configured, require it to be in the eligible set and
   use it. Otherwise sort eligible addresses by their normalized address bytes and
   choose the first. The ordering from `GetAdaptersAddresses` is not stable and
   must never decide the source.
7. Fail that family closed if no source remains. If the family is enabled, the
   generation cannot activate.

The effective choice and whether it was explicit or automatic must be visible in
runtime status. Any change to the automatic eligible set, effective choice, family
index, operational state, or applicable host policy replaces the generation.

## Socket enforcement

Every strict Windows socket should be configured centrally in this order:

1. Create the socket for the required family.
2. Set `IP_UNICAST_IF` or `IPV6_UNICAST_IF` to the corresponding live index.
3. Read the option back and reject a mismatch.
4. Bind the selected source address.
5. Connect, listen, or begin UDP use.
6. Where applicable, verify the resulting local endpoint.

Any failure is local and fail-closed. It must not trigger an unrestricted retry.

The interface-index option has a family-specific byte-order asymmetry that the
backend must hide:

- setting `IP_UNICAST_IF` takes the IPv4 interface index encoded in network byte
  order, while reading it with `getsockopt` returns host byte order;
- setting and reading `IPV6_UNICAST_IF` both use host byte order.

Normalize the set value and the readback value to a host-order `u32` before
comparison. Unit tests should cover encoding, decoding, and readback mismatch; a
native socket test should verify both options against the discovered live indices.

Strict TCP listeners and shared UDP sockets must bind concrete selected addresses,
not wildcard addresses. The interface-index options direct outgoing traffic and do
not constrain TCP ingress. Binding a listener to an address must not be described
as proof that a connection arrived through the corresponding physical adapter.

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
on Unix-family platforms, represents only one local source address per client, and
cannot apply `IP_UNICAST_IF` or `IPV6_UNICAST_IF` to its Windows sockets.

Avoid forking reqwest, hyper-util, or socket2 for the first implementation. Instead:

- Keep `Any` as one unchanged reqwest client.
- Build one generation-owned strict client per enabled Windows address family.
- Give each client its effective source address and a family-filtering resolver.
- Disable automatic proxies for strict clients.
- Disable reqwest's automatic redirect handling for strict Windows clients.
- Preserve bound-DNS ownership and generation cancellation.

`NetworkHttpClient::get` now returns the small `NetworkHttpRequest` GET wrapper
supporting the operations consumers currently use: headers and `send()`. Consumers
no longer receive a raw `reqwest::RequestBuilder`. The current wrapper deliberately
delegates to the existing reqwest clients, preserving Unix strict and `Any`
redirect behavior. The future strict-Windows strategy will use this seam to own
manual redirect processing and per-family attempts without adding platform logic to
consumers.

The first implementation is GET-only and follows this per-hop algorithm:

1. Validate the current URL, including enabled-family rules and RSS public-address
   and credential restrictions where applicable.
2. Resolve through the generation's selected resolver. For RSS, validate every
   actual answer before connect so DNS rebinding remains blocked.
3. Send the request with one family client. Cross-family fallback is allowed only
   for a transport failure before any response is received for this exact hop.
   Validation, policy, and cancellation failures are terminal and never trigger
   family fallback.
4. Once a response is received, never retry that hop through another family. If it
   is not a redirect, return it.
5. For a redirect, resolve `Location` against the current URL, enforce a finite hop
   limit and loop detection, validate the new URL, and begin a new hop. A failure on
   the redirected hop must never restart the original URL through another family.
6. Before every resolve, connect, redirect transition, and returned response, check
   generation ownership/cancellation and reject stale work.

Header forwarding must be implemented deliberately rather than copying the first
request wholesale. Generate `Host` from each target URL and remove hop-by-hop
headers on every hop. On a host or effective-port change, strip `Authorization`,
`Cookie`, `Cookie2`, `Proxy-Authorization`, and `WWW-Authenticate`, matching the
current reqwest sensitive-header rule. Preserve safe end-to-end headers, including
the web-seed `Range` header, across redirects. If `Referer` is generated, follow the
current reqwest behavior and never send it from HTTPS to HTTP. Tests must cover
same-origin, cross-origin, cross-port, cross-scheme, relative-location, loop,
hop-limit, range preservation, and credential/header stripping cases.

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

The current lifecycle is split between the supervisor and App. The following is the
actual call order to preserve unless a separate lifecycle refactor intentionally
changes it:

```text
NetworkSupervisor::rebuild_desired
  -> invalidate the current generation
  -> publish supervisor Blocked
  -> resolve and preflight the replacement candidate
  -> publish supervisor Ready(candidate)

App::handle_network_state_changed(Ready)
  -> publish activation Pending(candidate generation)
  -> close the old listener set
  -> clear old reachability state
  -> stop old DHT and wait for acknowledgement
  -> shut down old uTP/shared-UDP generation and drain admitted sends
  -> acquire and prepare the candidate scope
  -> bind replacement TCP listeners and shared UDP/uTP
  -> discover the actual peer port
  -> publish activation Active(generation, port)
  -> reconfigure DHT for the active generation
```

The supervisor resolves and preflights the candidate before App finishes old
listener/uTP teardown. App closes listeners before shutting down the old UDP
generation. The supervisor watch may coalesce its intermediate `Blocked` state, so
App must rely on generation invalidation and its own pending activation barrier,
not on observing every supervisor state transition.

Candidate preflight failure leaves the supervisor blocked. App teardown or listener
failure leaves activation blocked and reports the failure back to the supervisor.
Consumers remain gated on current activation plus current-generation ownership.

## Windows host policy

Windows normally uses strong-host routing, but weak-host behavior can be enabled by
an administrator. For each enabled family, read the applicable
`MIB_IPINTERFACE_ROW` and retain both `WeakHostSend` and `WeakHostReceive` in the
resolved family state.

The first-release rule is conservative: if either flag is enabled, or either value
cannot be read reliably for an applicable family, strict Windows activation fails
closed. This rule applies even though the application promise is scoped to outbound
confinement. It prevents HTTP source-only binding from being justified by raw-socket
tests that also applied an interface option, and it avoids implying stronger
listener ingress semantics than the backend provides.

WFP or another mechanism for enforcing the physical inbound interface is deferred.
It should be treated as a separate requirement and threat model, not silently added
to the first Windows release.

## Scope impact

This is no longer a Windows-backend-only change. Two prerequisite refactors affect
shared code; their structural portions are now complete:

- the resolved binding, interface inventory, runtime status, and generation
  comparison are family-aware on every platform;
- `NetworkHttpClient` no longer exposes a raw request builder; tracker, RSS,
  version-check, and web-seed consumers use the internal GET wrapper.

The behavior of Unix strict mode and `Any` should remain unchanged, but preserving
that behavior now requires explicit compatibility tests around both refactors.
Manual strict-Windows redirect handling, per-family fallback, and header rules add
meaningful HTTP state-machine and fixture coverage.

The first release does **not** add WFP, a physical-ingress guarantee, multi-address
listeners, non-GET request support, or an unrestricted recovery path.

## Implementation stages

1. **Shared prerequisite refactor — complete:** split adapter identity/display and
   per-family indices, configured/effective sources, eligible sets, host policy,
   status, and generation equivalence without changing Unix behavior.
2. **HTTP prerequisite seam — partially complete:** the GET request wrapper is in
   place and preserves current behavior. Explicit strict-Windows redirect and
   family-fallback behavior remains part of the Windows implementation.
3. **Windows platform model:** add the target dependency, `AdapterName` identity,
   family-specific discovery, unicast-row filtering, deterministic source
   selection, and weak-host fail-closed policy.
4. **Raw sockets:** add option encoding/enforcement/readback, source binding, exact
   listener binding, endpoint verification, and bound-DNS coverage.
5. **Lifecycle:** add native notifications, reconciliation, and safe callback
   ownership without misordering the current App activation sequence.
6. **Qualification:** pass ordinary CI, native multi-adapter integration, and the
   elevated packet gate; verify `Any` parity; only then enable Windows interface
   support.

## Acceptance criteria

Before Windows support is exposed, prove on a real Windows host that:

- persisted `AdapterName` selection survives friendly-name changes;
- IPv4 and IPv6 indices and sources are handled independently;
- source eligibility rejects tentative, duplicate, deprecated, invalid, and
  skip-as-source addresses and reports the effective choice;
- `WeakHostSend` or `WeakHostReceive` blocks strict activation independently for
  each applicable family;
- outbound TCP, UDP, bound DNS, HTTP, HTTP trackers, RSS, web seeds, redirects,
  peer TCP, UDP trackers, DHT, and uTP are each captured and shown to use the
  selected adapter/source;
- listener tests establish exact local-address binding but do not claim the
  physical ingress interface;
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
capture facility. HTTP, trackers, RSS, web seeds, every redirect hop, bound DNS
UDP/TCP, raw/peer TCP, UDP trackers, DHT, and uTP need individually attributable
evidence; aggregate process traffic is insufficient. Endpoint listings and logs
supplement packet evidence but do not replace it.

## Qualification gates

Qualification is intentionally split into three evidence classes.

### Ordinary Windows CI

Make Windows an automatic pull-request gate for networking, dependency, activation,
consumer-transport, and packaging changes. Hosted CI should cover formatting,
Clippy, the complete all-feature test suite, Windows compilation, parsed discovery
fixtures, identity/display handling, per-family generation equivalence, source
eligibility/selection, option byte-order encoding, live option readback where the
runner permits it, local socket binding, lifecycle tests, manual redirect behavior,
cancellation, and default-`Any` parity. This is necessary regression coverage, not
multi-adapter leak qualification.

### Native multi-adapter integration

A disposable Windows VM or dedicated host with at least two independently routed
adapters should exercise real `GetAdaptersAddresses` and unicast/interface rows,
separate IPv4/IPv6 indices, actual raw sockets, bound DNS, per-family HTTP clients,
listeners, generation replacement, source changes, friendly-name changes, and
independent family failure. These tests may be automated separately from every PR,
but their exact revision and machine topology must be retained as evidence.

### Elevated packet-level release gate

An elevated disposable VM or self-hosted runner must perform authoritative packet
capture and the disruptive cases: weak-host send/receive changes, adapter
disable/re-enable, VPN loss/recovery, pending and blocked intervals, forbidden-route
observation, and stale-generation rejection. This gate must attribute every listed
consumer/transport individually and include strict and `Any` runs.

Cross-target compilation from another operating system is useful compile coverage
only. Keep `INTERFACE_BINDING_SUPPORTED` false on Windows until the elevated gate
passes on the release candidate.

## Main Windows risks

- Adapter behavior differs across Wintun, TAP, PPP, Hyper-V, and vendor-specific
  VPN implementations.
- Indices and IPv6 privacy addresses can change without a new friendly name.
- Source-bound reqwest sockets cannot apply the raw socket interface-index option;
  strong-host state and independent HTTP packet evidence are therefore mandatory.
- Windows change callbacks run on OS-managed threads and require careful teardown.
- The initial one-address-per-family contract is intentionally narrower than
  binding every address on an adapter.
- Manual redirect ownership expands the shared HTTP abstraction and its consumer
  tests even though the strict per-family behavior is Windows-specific.
- Only native packet capture can establish the final no-leak claim.

## Questions requiring native Windows evidence

- Do Wintun, TAP, PPP, Hyper-V, and supported vendor VPN adapters expose stable
  `AdapterName`, usable family indices, unicast rows, and weak-host state in the
  expected way?
- Does option readback and source binding behave consistently for TCP, UDP, and the
  socket shapes used by DHT/uTP on supported Windows versions?
- Under strong-host policy, does source-only reqwest binding remain confined during
  route changes and adapter/VPN loss for both families?
- Are `NotifyUnicastIpAddressChange` plus `NotifyIpInterfaceChange` sufficient for
  adapter state, index, address eligibility, weak-host, and VPN transitions, or are
  additional notifications needed?
- How much generation churn does deterministic one-source IPv6 selection produce
  with privacy-address rotation, and is a more stable documented ranking needed?
- Can the chosen elevated capture stack attribute all consumer traffic and prove
  both zero forbidden packets and default-`Any` parity without observer gaps?

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

The shared family-aware refactor does not change those mechanisms. Unix discovery
uses the interface name as both persisted identity and display name, maps its one
`if_nametoindex` result into the IPv4 and IPv6 family records, records explicit
sources separately from effective sources, and leaves effective sources empty when
interface mode delegates source choice to the OS. Unix host-policy status remains
not applicable. The HTTP wrapper delegates to the same reqwest clients and redirect
policies used before the refactor.

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
