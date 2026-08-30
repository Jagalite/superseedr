# Superseedr Web

Superseedr Web is a completely client-side, simulated demonstration of the production Superseedr
terminal UI. The browser uses the exact production Ratatui renderer, event dispatcher, reducers,
shared state, and command boundary. It does not perform real network or disk activity.

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
