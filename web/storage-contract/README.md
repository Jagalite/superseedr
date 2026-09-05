# Production OPFS browser contract

This small Wasm wrapper imports `superseedr::web_integration::storage` from the actual production library. It uses dedicated workers and actual origin-private storage; there are no replacement storage modules or application stubs.

Run from the repository root with Rust's `wasm32-unknown-unknown` target, Python 3, Chrome/Chromium, and `wasm-bindgen-cli` 0.2.104:

```sh
cargo build --manifest-path web/storage-contract/Cargo.toml --target wasm32-unknown-unknown --locked
wasm-bindgen web/storage-contract/target/wasm32-unknown-unknown/debug/superseedr_storage_contract.wasm --target web --out-dir web/storage-contract/pkg
python3 web/storage-contract/run.py
```

If `CARGO_TARGET_DIR` is set, use that target directory in the `wasm-bindgen` input path. Set `SUPERSEEDR_BROWSER_BIN` or pass `--browser /path/to/browser` to select Chromium. The runner uses a fresh temporary browser profile, a loopback HTTP server, and a bounded wait; it exits nonzero on failure and removes the temporary profile after browser termination. It does not touch a user's browser profile or existing origin storage.

The contract checks:

- Byte equality across ordinary, padding, skipped, and sparse spans; zero-length and invalid ranges.
- Shared four-file cache limits, LRU eviction, and reuse of one handle for reads and writes.
- Namespace isolation across torrents, explicit flush, worker termination, and retained-byte recovery.
- Exclusive ownership rejection in a second worker and successful takeover after owner termination.
- Dirty-handle retention after an injected flush failure, retry, quota exhaustion, and zero-progress writes.
- Close/delete ordering and rejection of stale clones after namespace recreation.
- The same successful storage/restart path when `createSyncAccessHandle` is deliberately non-callable, forcing writable streams.

Fault injection changes only the disposable test worker's API prototypes. All production paths use the actual browser APIs. The JSON output records per-phase results and cache counters. This qualifies the storage implementation in the selected browser; full Wasm manager/session execution and browser catalog persistence are separate integration work.
