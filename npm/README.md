# Superseedr

A native Rust BitTorrent client with a terminal UI.

After npm publication, install with `npm install -g superseedr`, then run
`superseedr`, or launch directly with `npx superseedr`.

Requires Node.js 22 or later. Supports macOS arm64/x64, Linux glibc arm64/x64,
and Windows x64. Linux musl/Alpine and Windows ARM64 are not currently supported.
Keep optional dependencies enabled: npm selects the matching native binary.
No installation scripts run, and no Rust toolchain is required.

This package launches the native terminal client. Use the native installers for
browser magnet-link registration. Documentation, native installers, and matching
versioned source: https://github.com/Jagalite/superseedr

Licensed under GPL-3.0-or-later; see LICENSE.
