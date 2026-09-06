# Superseedr Web

The default Superseedr Web entry is a completely client-side, simulated demonstration of the production Superseedr
terminal UI. The browser uses the exact production Ratatui renderer, event dispatcher, reducers,
shared state, and command boundary. It does not perform real network or disk activity.

## Separate WebTorrent mode

The live terminal demo remains at `index.html`, using `web/wasm`, `pkg`, and `dist`.
The real WebTorrent client is a separate entry at `webtorrent.html`, using
`client-wasm`, `client-pkg`, and `client-dist`. It runs the production TorrentManager
and peer session in a dedicated worker, with Window-owned RTC and OPFS payload I/O.
The demo never opens these real client services.

```sh
npm ci
npm run build:webtorrent
npm run preview:webtorrent
# Or build and serve the separate mode for development:
npm run dev:webtorrent
```

Open `/webtorrent.html`. Paste a public v1 magnet, pass it as the URL-encoded
`?magnet=` parameter, or open a `.torrent` file. Discovery requires a reachable
WebTorrent `wss:` tracker on HTTPS deployments. Ordinary TCP/UDP-only swarms do
not supply browser-compatible peers. Downloaded, verified files remain available
for seeding while the page is open, and can be saved through the file picker
(or a bounded 64 MiB Blob fallback). Pause, resume, remove, and orderly Stop client
use the real manager command boundary.

Deploy the static contents of `client-dist` alongside the demo `dist` contents to
serve both modes on one origin. Their asset directories are distinct. Building
WebTorrent does not overwrite the demo, and the existing demo build/budget checks
remain independent. The link from the WebTorrent page to `index.html` expects both
modes to be present. A standalone WebTorrent deployment only has `webtorrent.html`.

Use HTTPS or localhost. This mode currently requires a browser with dedicated
workers, WebAssembly, WebRTC in the Window, WebSocket, Web Locks, IndexedDB, and
worker OPFS sync handles. Chromium has direct contract evidence; other browsers
are not yet qualified. Only one live client owns an origin's catalog at a time;
the demo may run alongside it. Reload revalidates retained payload bytes before
seeding. A lost RTC bridge displays “Reconnecting WebRTC…” and the worker requests
one replacement at a time. Fresh network activation follows a heartbeat acknowledgment;
TM/state retain peer lifetime authority. Tab closure may interrupt work; use Stop client
to await durable shutdown. Accepted removals remain removals if Stop overlaps cleanup.

This is an engine integration mode, with further website work tracked in
[the portability report](../docs/browser-tm-portability-audit.md). Sequential
playback/seeking, file-priority controls, local-file import for seeding, durable
history/RSS services, quota recovery, and cross-browser qualification remain open.

### Real browser acceptance

```sh
npm run build:webtorrent
SUPERSEEDR_TEST_BUILT_UI=1 npm run test:webtorrent
npm run test:storage
```

`test:webtorrent` builds a separate opt-in contract Wasm artifact and starts a
local signaling tracker. It uses generated `orbital-data.bin` bytes and an
independent browser client. Supply that client's pinned `webtorrent@3.0.21`
distribution using `SUPERSEEDR_TEST_CLIENT=/absolute/path/to/webtorrent.min.js`.
The local default is the prior image acceptance artifact at
`../target/iso-acceptance/package/dist/webtorrent.min.js`; it is **not** a tracked
repository dependency. Install the pinned Playwright Chromium browser before
running the contracts. The test records its temporary persistent profile path.

The contracts include heartbeat loss, delayed/stale replacement replies, successful
reseeding after recovery, and removal racing shutdown with/without payload deletion.
On machines where local mDNS resolution prevents even a bare Chromium RTC pair
from connecting, `SUPERSEEDR_TEST_DISABLE_MDNS=1` opts this local harness into IP
ICE candidates. It does not change production browser settings; record the override
with test results because it does not qualify the normal mDNS path.

The optional built-page check uses `client-dist` from the preceding release
build; it does not substitute the debug contract bundle for the shipped page.

## Browser support

- Chromium is the release-qualified browser and is exercised by the pinned Playwright suite in CI.
- Current Firefox and Safari releases are expected to work when WebAssembly, ES2022 modules, Canvas,
  and `requestAnimationFrame` are available, but they are not yet automated release gates.
- A physical keyboard is recommended. Touch-only and mobile software-keyboard behavior is not part
  of the first-release contract.

## Controls

Click the terminal before using keyboard or paste input.

- Arrow keys navigate the selected production screen.
- Paste a magnet link to exercise the simulated add path.
- `p` pauses or resumes the selected fictional torrent; `d` opens deletion confirmation.
- `a`, `c`, `r`, `m`, and `z` open the file browser, configuration, RSS, help, and power-saving
  screens.
- `Shift+J`, `Shift+P`, and `Shift+M` open journal, peer-management, and torrent-management screens.
- `q` or `Escape` returns from the current screen according to the production reducer.

Browser and operating-system shortcuts using Command/Meta are left to the browser. Ctrl+C is also
left to the terminal's selection/copy behavior when text is selected.

## Simulated limitations

- All torrents, peers, files, feeds, journal entries, lifecycle stages, and telemetry are
  deterministic fictional browser-owned data.
- No tracker, DHT, PEX, peer, WebTorrent, WebRTC, filesystem, browser-storage, or persistence service
  runs. Refreshing the page resets the session.
- Displayed speeds, progress, discovery, checking, stalls, seeding, and deletion are demonstrations;
  they do not represent a real transfer.
- The static site requires no application server. Any static file host that serves `dist/index.html`,
  JavaScript, CSS, and WASM with normal MIME types is sufficient.

## Reproducible local build

Prerequisites:

- Rust 1.95.0 with the `wasm32-unknown-unknown` target;
- `wasm-bindgen-cli` exactly 0.2.104;
- Node.js 24 and npm.

```text
rustup target add wasm32-unknown-unknown --toolchain 1.95.0
cargo install wasm-bindgen-cli --version 0.2.104 --locked
cd web
npm ci
npm run build
```

`npm run build` compiles a size-optimized release WASM module, checks TypeScript, creates a
relative-URL static bundle in `web/dist`, rejects server-side files, and enforces raw and gzip size
budgets. The current qualified bundle contains a roughly 2.29 MB Superseedr WASM asset (about 826 KB
gzip) and roughly 650 KB of JavaScript (about 192 KB gzip).

To execute the bundled browser contract suite:

```text
cd web
npx playwright install chromium
npm run test:browser
```

The CI job also runs the host and real-WASM Rust contracts, target check, strict WASM Clippy,
release bundle, static-dist verification, and browser suite before uploading `web/dist` as a static
build artifact. It does not deploy the artifact.
