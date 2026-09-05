// SPDX-License-Identifier: GPL-3.0-or-later
//! WebTorrent signaling and transport execution; torrent decisions stay in TorrentState.
#[cfg(not(target_arch = "wasm32"))]
pub mod native;
#[cfg(not(target_arch = "wasm32"))]
pub mod tracker;
pub mod wire;
