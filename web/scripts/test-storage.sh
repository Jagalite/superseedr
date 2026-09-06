#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
: "${CARGO_TARGET_DIR:=$(pwd)/../target/wasm}"
export CARGO_TARGET_DIR
cargo build --manifest-path storage-contract/Cargo.toml --target wasm32-unknown-unknown --locked
wasm-bindgen --target web --out-dir storage-contract/pkg "$CARGO_TARGET_DIR/wasm32-unknown-unknown/debug/superseedr_storage_contract.wasm"
node tests/storage-contract.mjs
