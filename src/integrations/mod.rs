// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

#[cfg(not(target_arch = "wasm32"))]
pub mod cli;
pub mod control;
#[cfg(not(target_arch = "wasm32"))]
pub mod rss_ingest;
#[cfg(not(target_arch = "wasm32"))]
pub mod rss_service;
#[cfg(not(target_arch = "wasm32"))]
pub mod status;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod watch_inbox;
#[cfg(not(target_arch = "wasm32"))]
pub mod watcher;
