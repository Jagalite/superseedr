// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

mod app;
mod command;
mod config;
mod control_service;
#[cfg(feature = "dht")]
mod dht;
#[cfg(not(feature = "dht"))]
#[path = "dht_stub.rs"]
mod dht;
mod dht_service;
mod errors;
mod fs_atomic;
pub mod fuzzing;
mod integrations;
mod integrity_scheduler;
mod logging;
mod native_entrypoint;
mod networking;
mod peer_manager;
mod persistence;
mod resource_manager;
mod storage;
#[cfg(feature = "synthetic-load")]
mod synthetic_load;
mod telemetry;
mod theme;
mod token_bucket;
mod torrent_file;
mod torrent_identity;
mod torrent_manager;
mod tracker;
mod tui;
mod tuning;
mod watch_inbox;

use config::Settings;

pub async fn run_native() -> Result<(), Box<dyn std::error::Error>> {
    native_entrypoint::run().await
}
