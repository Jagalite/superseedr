// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

mod app;
#[cfg(not(target_arch = "wasm32"))]
mod command;
#[cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]
mod config;
#[cfg(not(target_arch = "wasm32"))]
mod control_service;
#[cfg(all(feature = "dht", not(target_arch = "wasm32")))]
mod dht;
#[cfg(any(not(feature = "dht"), target_arch = "wasm32"))]
#[path = "dht_stub.rs"]
mod dht;
#[cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]
mod dht_service;
#[cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]
mod errors;
#[cfg(not(target_arch = "wasm32"))]
mod fs_atomic;
#[cfg(target_arch = "wasm32")]
#[path = "wasm_compat/fs_atomic.rs"]
mod fs_atomic;
#[cfg(not(target_arch = "wasm32"))]
pub mod fuzzing;
#[cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]
mod integrations;
#[cfg(not(target_arch = "wasm32"))]
mod integrity_scheduler;
#[cfg(not(target_arch = "wasm32"))]
mod logging;
#[cfg(not(target_arch = "wasm32"))]
mod native_entrypoint;
#[cfg(not(target_arch = "wasm32"))]
mod networking;
#[cfg(target_arch = "wasm32")]
#[path = "wasm_compat/networking.rs"]
mod networking;
mod peer_manager;
#[cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]
mod persistence;
#[path = "tui/presentation.rs"]
pub mod presentation;
#[cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]
mod resource_manager;
#[path = "persistence/serialization.rs"]
mod serialization;
#[cfg(not(target_arch = "wasm32"))]
mod storage;
#[cfg(feature = "synthetic-load")]
mod synthetic_load;
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
#[cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]
mod tracker;
mod tui;
#[cfg(not(target_arch = "wasm32"))]
mod tuning;
#[cfg(not(target_arch = "wasm32"))]
mod watch_inbox;
#[cfg(target_arch = "wasm32")]
pub mod web_integration;

#[cfg(not(target_arch = "wasm32"))]
use config::Settings;

#[cfg(not(target_arch = "wasm32"))]
pub async fn run_native() -> Result<(), Box<dyn std::error::Error>> {
    native_entrypoint::run().await
}
