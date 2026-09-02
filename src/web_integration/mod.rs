// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Narrow WASM-only bridge from browser-owned behavior to production reducers and rendering.

mod session;
mod types;

pub use session::{canonical_browser_magnet_info_hash, BrowserSession};
pub use types::*;
