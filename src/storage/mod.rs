// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Platform storage composition.
//!
//! Application configuration persistence is injected into runtime hosts through
//! [`AppStorage`]. Torrent payload storage remains the existing native API for
//! now and is re-exported unchanged from this module.

mod app;

pub(crate) use app::AppStorage;

#[cfg(not(target_arch = "wasm32"))]
mod payload;

#[cfg(not(target_arch = "wasm32"))]
pub use payload::*;
