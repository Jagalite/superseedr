// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

mod app;
#[cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]
mod config;
#[cfg(all(feature = "dht", not(target_arch = "wasm32")))]
mod dht;
#[cfg(all(not(feature = "dht"), not(target_arch = "wasm32")))]
#[path = "dht/stub.rs"]
mod dht;
#[path = "dht/model.rs"]
mod dht_model;
#[cfg(not(target_arch = "wasm32"))]
pub mod fuzzing;
#[cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]
mod integrations;
#[cfg(not(target_arch = "wasm32"))]
mod native;
mod networking;
mod peer_manager;
#[cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]
mod persistence;
#[path = "tui/presentation.rs"]
pub mod presentation;
mod resource;
#[cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]
mod telemetry;
/// Compatibility facade for the shared TUI input model.
pub mod terminal_event {
    pub use crate::tui::input::*;
}
#[cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]
mod theme;
#[cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]
mod token_bucket;
#[cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]
mod torrent_file;
#[cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]
mod torrent_identity;
#[cfg(not(target_arch = "wasm32"))]
mod torrent_manager;
#[cfg(not(target_arch = "wasm32"))]
mod tracker;
mod tui;
#[cfg(not(target_arch = "wasm32"))]
mod tuning;
#[cfg(target_arch = "wasm32")]
pub mod web_integration;

#[cfg(not(target_arch = "wasm32"))]
use config::Settings;

#[cfg(not(target_arch = "wasm32"))]
pub async fn run_native() -> Result<(), Box<dyn std::error::Error>> {
    native::entrypoint::run().await
}
