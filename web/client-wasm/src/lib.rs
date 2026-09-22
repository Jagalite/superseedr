// SPDX-License-Identifier: GPL-3.0-or-later
//! Separate browser WebTorrent entrypoint; the live demo keeps its own bundle.
pub use superseedr::web_integration::LiveClient;

#[cfg(feature = "contract")]
pub use superseedr::web_integration::browser_runtime_contract;
