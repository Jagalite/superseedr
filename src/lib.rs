// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

// The packaged WASM library intentionally compiles shared domain models that are consumed through
// the narrow `web_integration` facade rather than referenced directly inside this crate.
#![cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]

mod app;
mod config;
#[cfg(all(feature = "dht", not(target_arch = "wasm32")))]
mod dht;
#[cfg(all(not(feature = "dht"), not(target_arch = "wasm32")))]
#[path = "dht/stub.rs"]
mod dht;
#[path = "dht/model.rs"]
mod dht_model;
mod execution;
#[cfg(not(target_arch = "wasm32"))]
pub mod fuzzing;
mod integrations;
#[cfg(not(target_arch = "wasm32"))]
mod native;
mod networking;
mod peer_manager;
mod persistence;
#[path = "tui/presentation.rs"]
pub mod presentation;
mod resource;
mod telemetry;
/// Compatibility facade for the shared TUI input model.
pub mod terminal_event {
    pub use crate::tui::input::*;
}
mod theme;
mod token_bucket;
mod torrent_file;
mod torrent_identity;
mod torrent_manager;
mod tracker;
mod tui;
#[cfg(not(target_arch = "wasm32"))]
mod tuning;
#[cfg(target_arch = "wasm32")]
pub mod web_integration;

use config::Settings;

#[cfg(not(target_arch = "wasm32"))]
pub async fn run_native() -> Result<(), Box<dyn std::error::Error>> {
    native::entrypoint::run().await
}
