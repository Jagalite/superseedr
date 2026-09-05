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
    #[cfg(feature = "webtorrent")]
    install_webtorrent_crypto_provider()?;
    native::entrypoint::run().await
}

#[cfg(all(feature = "webtorrent", not(target_arch = "wasm32")))]
fn install_webtorrent_crypto_provider() -> std::io::Result<()> {
    let aws_lc_provider = rustls::crypto::aws_lc_rs::default_provider();
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = aws_lc_provider.clone().install_default();
    }

    match rustls::crypto::CryptoProvider::get_default() {
        Some(installed)
            if std::ptr::eq(installed.secure_random, aws_lc_provider.secure_random)
                && std::ptr::eq(installed.key_provider, aws_lc_provider.key_provider) =>
        {
            Ok(())
        }
        Some(_) => Err(std::io::Error::other(
            "Rustls was initialized with a crypto provider other than AWS-LC",
        )),
        None => Err(std::io::Error::other(
            "failed to install the Rustls AWS-LC crypto provider for WebTorrent",
        )),
    }
}
