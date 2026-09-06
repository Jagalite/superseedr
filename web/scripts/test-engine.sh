#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
: "${CARGO_TARGET_DIR:=$(pwd)/../target/wasm}"
export CARGO_TARGET_DIR
node --test tests/rtc-bridge-contract.mjs tests/save-file-contract.mjs
cargo build --manifest-path client-wasm/Cargo.toml --target wasm32-unknown-unknown --features contract --locked
wasm-bindgen --target web --out-dir client-pkg "$CARGO_TARGET_DIR/wasm32-unknown-unknown/debug/superseedr_browser_client.wasm"
node tests/engine-contract.mjs
