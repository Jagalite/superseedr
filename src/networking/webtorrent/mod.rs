// SPDX-License-Identifier: GPL-3.0-or-later
//! WebTorrent signaling and transport execution; torrent decisions stay in TorrentState.
#[cfg(target_arch = "wasm32")]
pub mod browser;
#[cfg(not(target_arch = "wasm32"))]
pub mod native;
pub mod tracker;
#[cfg(target_arch = "wasm32")]
pub(crate) use browser as transport;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native as transport;
pub mod wire;
