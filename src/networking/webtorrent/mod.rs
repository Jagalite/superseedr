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

// Diagnostic events compile out of ordinary builds. Fields are evaluated only
// when an explicit synthetic trace destination has been configured.
macro_rules! rtc_trace {
    ($event:literal, $fields:tt) => {
        #[cfg(all(feature = "synthetic-load", not(target_arch = "wasm32")))]
        $crate::networking::webtorrent::diagnostics::record($event, || serde_json::json!($fields));
    };
}
pub(crate) use rtc_trace;
#[cfg(all(feature = "synthetic-load", not(target_arch = "wasm32")))]
pub(crate) mod diagnostics;
