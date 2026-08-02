# Native network binding

Superseedr can constrain its owned network traffic to a selected operating-system
interface or local address. On Linux, strict interface mode is an application-level
VPN kill switch: if the configured interface cannot be validated, the networking
generation enters `blocked`, existing generation work is cancelled, and new leases
are rejected. The process and TUI remain running so the interface can recover.

An operating-system or VPN firewall is still the strongest system-wide kill switch.
Native binding protects Superseedr-owned traffic; it cannot constrain other processes
or failures outside the application.

## Guarantee levels

| Mode | Traffic binding | DNS guarantee |
| --- | --- | --- |
| Any | The operating system selects routes and interfaces. | System DNS; no application-level leak guarantee. |
| Strict interface + system DNS | Superseedr-owned TCP, UDP, HTTP, and HTTPS connections use the selected interface. | DNS routing remains controlled by the operating system or VPN. The TUI and status output show a warning. |
| Strict interface + bound DNS | Covered traffic and DNS use generation-owned sockets on the selected interface. | No system-resolver or unbound fallback is used. |
| Local address | Source selection is constrained to one exact local address. | Weaker than device binding and not presented as a strict interface guarantee. |

The torrent privacy boundary covers incoming and outgoing TCP peers, uTP, DHT,
UDP/HTTP/HTTPS trackers, web seeds, IPv4, and IPv6. RSS and update HTTP clients also
use the current generation. A firewall remains recommended as defense in depth.

## Configure it

Open the TUI configuration screen and use the **Network** section. The settings are
host-scoped, so interface names and addresses are not copied to shared followers.
The default Normal routing view keeps binding-specific controls hidden. Selecting an
interface or local-address routing mode reveals the applicable advanced controls, and
the bound-DNS server field appears only when bound DNS is selected. Those controls expose:

- binding mode and exact interface name;
- independent IPv4 and IPv6 enablement;
- optional exact IPv4 and IPv6 source addresses;
- system or bound DNS policy;
- literal bound-DNS server socket addresses.

The equivalent host configuration is:

```toml
[network_binding]
mode = "interface"
interface = "vpn0"
enable_ipv4 = true
enable_ipv6 = false
dns_policy = "bound"
dns_servers = ["10.8.0.1:53"]
```

Bound DNS servers must be IP literals with explicit ports. Hostnames are rejected to
avoid resolver bootstrap leakage. Strict mode never falls back to `any`, an address-
only policy, an unbound HTTP client, or the system resolver when bound DNS is active.

## Runtime status

The config details pane shows the live state, generation and configuration epoch,
resolved interface/index, selected address set, warning, and Blocked reason. The
periodic JSON status adds two surfaces:

- `status_config.network_binding`: the requested host policy;
- `network`: resolved live policy, `ready` or `blocked` phase, generation/epoch,
  operating-system interface index, addresses, DNS policy/servers, warning, and
  failure reason.

Older status files remain readable: missing network fields default to the unrestricted
configuration and no live snapshot.

## Linux capability setup

Linux device binding can require `CAP_NET_RAW`. A permission failure is reported as a
Blocked reason; do not run the entire application as root to work around it.

For a systemd service, copy the reviewed drop-in from
`packaging/linux/superseedr-network-binding.service.conf` into the service's drop-in
directory, then reload and restart it:

```sh
sudo install -D -m 0644 \
  packaging/linux/superseedr-network-binding.service.conf \
  /etc/systemd/system/superseedr.service.d/network-binding.conf
sudo systemctl daemon-reload
sudo systemctl restart superseedr.service
```

For a directly launched binary, a narrower alternative to full root is a file
capability:

```sh
sudo setcap cap_net_raw=ep /usr/bin/superseedr
getcap /usr/bin/superseedr
```

`CAP_NET_RAW` is security-sensitive: it permits raw and packet sockets in addition to
interface binding. Grant it only to a trusted binary and service account. File
capabilities may be removed when a package replaces the binary, so verify them after
upgrades. The release packages do not grant this capability automatically.

## Privileged leak test

On Linux with `iproute2`, `tcpdump`, Python 3, and root privileges:

```sh
sudo ./integration_tests/network_binding/run_netns_leak_test.sh
```

The harness creates isolated client, selected-interface peer, and clear/default peer
namespaces. It runs the real Rust generation probe with bound TCP, UDP, and DNS,
attempts traffic toward the clear route, removes the selected interface, and verifies
that the generation becomes Blocked. Packet captures must contain selected-interface
traffic and zero packets on the clear/default interface. Captures are retained under
`integration_tests/artifacts/` for review.

This privileged Linux test is intentionally not claimed as executed on macOS. The
normal cross-platform suites still cover factory routing, generation invalidation,
listener/DHT recovery, HTTP client ownership, DNS cancellation, and torrent-manager
recovery semantics.
