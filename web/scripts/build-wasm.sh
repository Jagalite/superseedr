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

# Optimize the final bindgen module with the lockfile-pinned build tool.
# Retain compressible code/data layout: unrestricted inlining, reordering and
# memory packing reduce raw bytes but inflate gzip for this module. Both size
# limits and Binaryen validation stay enabled.
BINARYEN_CORES=1 ./node_modules/.bin/wasm-opt \
  pkg/superseedr_web_bg.wasm -Os \
  --enable-bulk-memory --enable-nontrapping-float-to-int \
  --enable-sign-ext --enable-mutable-globals \
  --skip-pass=reorder-functions --skip-pass=reorder-locals \
  --skip-pass=inlining-optimizing --skip-pass=memory-packing \
  -o pkg/superseedr_web_bg.optimized.wasm
mv pkg/superseedr_web_bg.optimized.wasm pkg/superseedr_web_bg.wasm
