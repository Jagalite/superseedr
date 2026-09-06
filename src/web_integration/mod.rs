// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Narrow WASM-only bridge from browser-owned behavior to production reducers and rendering.

mod session;
mod types;

pub use crate::config::DownloadMode;
pub use session::{
    canonical_browser_magnet_info_hash, has_browser_magnet_scheme, BrowserSession,
    BrowserTorrentManagerEndpoint,
};
pub use types::*;

// The browser simulation implements the production torrent-manager port. These
// are the exact command and output types carried by that port.
pub use crate::app::torrent_manager_protocol::{
    DiskIoOperation, FileActivityDirection, FileActivityUpdate, ManagerCommand, ManagerEvent,
};
pub use crate::app::{FilePriority, PeerInfo, TorrentControlState, TorrentMetrics};

pub use crate::app::{
    AppCapabilities, AppEffect as ApplicationEffect, PersistPayload as ApplicationCheckpoint,
};

/// Production payload capability for browser runtime composition.
pub mod payload {
    pub use crate::persistence::{
        Backend, FileInfo, FileStat, IoFuture, IoLease, MultiFileInfo, Operation, OpfsPayload,
        Payload, Reply, StorageError,
    };
}

#[cfg(feature = "webtorrent")]
pub use session::LiveClient;

#[cfg(feature = "browser-contract")]
pub use crate::execution::browser_contract::browser_runtime_contract;
