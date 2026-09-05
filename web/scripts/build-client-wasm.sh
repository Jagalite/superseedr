#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
: "${CARGO_TARGET_DIR:=$(pwd)/../target/wasm}"
export CARGO_TARGET_DIR
cargo build --manifest-path client-wasm/Cargo.toml --target wasm32-unknown-unknown --release --locked
wasm-bindgen --target web --out-dir client-pkg "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/superseedr_browser_client.wasm"
