#!/bin/sh
set -eu

EXPECTED_BINDGEN="wasm-bindgen 0.2.104"
ACTUAL_BINDGEN="$(wasm-bindgen --version)"

if [ "$ACTUAL_BINDGEN" != "$EXPECTED_BINDGEN" ]; then
  echo "expected $EXPECTED_BINDGEN, found $ACTUAL_BINDGEN" >&2
  exit 1
fi

mkdir -p pkg
cargo build --manifest-path wasm/Cargo.toml --target wasm32-unknown-unknown --release --locked
wasm-bindgen \
  --target web \
  --out-dir pkg \
  --out-name superseedr_web \
  wasm/target/wasm32-unknown-unknown/release/superseedr_web.wasm
