// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Durable state and torrent-payload persistence boundaries.
//!
//! [`app`] owns the injected application-state capability used by native and
//! browser runtime hosts. [`payload`] owns torrent content I/O through an injected
//! capability, backed by the native filesystem or worker-local browser OPFS.

pub mod activity_history;
mod app;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod atomic;
mod error;
pub mod event_journal;
pub mod network_history;
mod payload;
pub mod rss;
#[cfg(not(target_arch = "wasm32"))]
mod serialization;

pub(crate) use app::AppPersistence;
pub use error::StorageError;

pub use payload::*;
