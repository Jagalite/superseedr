# WebTorrent transport

The `webtorrent` Cargo feature is enabled by default on native builds. It adds WebSocket tracker signaling and reliable ordered RTC DataChannels to the existing TorrentManager and PeerSession. `--no-default-features` excludes the transport.

## Ownership

- TorrentState retains peer admission, removal, pause/resume, piece scheduling, verification, and tracker announce deadlines. Its source is unchanged.
- TorrentManager executes state effects, owns tracker and session tasks, obtains peer permits, and routes tracker responses and transport failures back through state actions.
- `networking/webtorrent` owns signaling, SDP/ICE negotiation and the bounded DataChannel stream. A tracker connection can negotiate several peers; every admitted peer has its own RTC connection and ordinary peer-wire session.
- Peer keys include both the remote wire ID and a connection incarnation. Late messages from a departed connection cannot address its replacement. The wire handshake must match the signaling peer ID and torrent info hash.

## Discovery and data transfer

Both `.torrent` files and magnets retain `ws:`/`wss:` trackers. A magnet can negotiate RTC before metadata exists, fetch BEP 9 metadata, and enter the normal storage, validation, and piece pipeline. Uploads use the same state authorization, storage reads, choking, and rate limits as native socket peers.

Tracker workers execute explicit requests from the manager. They have no independent announce or reconnect schedule. An unsuccessful tracker connection reports an error and exits; state backoff determines the next attempt. SDP gathering, offer expiry, and connection handshakes have their own bounded protocol timeouts. Stopping a worker sends a best-effort `stopped` announce before closing its socket.

A manager has at most four active tracker workers and eight pending peer-admission tasks. Each tracker has two outgoing and two incoming negotiation slots. Signaling messages are capped at 128 KiB, SDP at 64 KiB, and the byte-stream bridge at 256 KiB. Outgoing DataChannel chunks are 16 KiB, including fragmentation of larger peer-wire frames. RTC has separate peer and beneficial-peer metrics.

## ICE configuration and supported policies

ICE configuration belongs to the host settings, including when using a shared catalog. The default empty list uses host candidates. Configure STUN/TURN for network deployments that need NAT traversal or relaying:

```toml
[[webtorrent_ice_servers]]
urls = ["stun:discovery.example.test:3478"]

[[webtorrent_ice_servers]]
urls = ["turn:relay.example.test:3478?transport=udp"]
username = "configured-user"
credential = "configured-token"
```

The native implementation pins `webrtc` 0.20.4 and installs its AWS-LC Rustls provider before starting the application. ICE server count, URL count and lengths, and credential lengths are bounded; the RTC library validates ICE URL syntax and TURN credentials.

The current native library integration requires unrestricted dual-stack networking with system DNS. It rejects interface/source binding, bound DNS, and active IP restrictions instead of silently bypassing those policies. Losing that capability closes RTC peers through state actions. V1 and hybrid torrents are supported; v2-only torrents are rejected by this transport. Socket transports retain their existing capabilities.

## Browser boundary

The existing browser app model remains portable, and browser interoperability tests exercise actual browser RTCPeerConnection/DataChannel APIs. Native RTC dependencies are target-gated. Running the full TorrentManager inside a browser worker is a separate composition milestone in [the production design](web-torrent-production-design.md); passing the browser app tests alone does not establish a complete browser torrent client.

## Validation

Run native tests with real local sockets available:

```sh
cargo test --lib --all-features --locked
SUPERSEEDR_WEBTORRENT_BROWSER_BIN="/path/to/browser" \
  cargo test --lib --all-features --locked \
  browser_peer_exchanges_bidirectional_blocks_and_metadata_over_webtorrent -- --ignored
```

The manager integration test starts a seed and an empty magnet destination with a local WebSocket signaling relay and no socket-peer or HTTP-web-seed sources. It requires RTC metadata bootstrap, verified piece completion, upload/download accounting, and exact retained payload bytes. The browser test exercises bidirectional block and metadata exchange with a separate browser implementation. Public-tracker/NAT/TURN deployment tests are distinct from these local interoperability checks.

The implementation was validated with 2,257 passing native tests (two explicit opt-in tests excluded from that run), the browser interoperability test run separately, 109 passing Wasm app tests, and strict Clippy for native all-features, native no-default-features, and Wasm targets.
