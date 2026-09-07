// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native torrent-manager implementation.

pub mod block_manager;
pub(crate) mod command;
mod file_progress;
pub(crate) mod integrity_scheduler;
pub mod manager;
pub mod merkle;
pub mod piece_manager;
pub mod state;

#[cfg(feature = "synthetic-load")]
pub(crate) use crate::app::torrent_manager_protocol::SyntheticPeerConnectFailure;
pub(crate) use crate::app::torrent_manager_protocol::{
    DiskIoOperation, FileActivityDirection, FileActivityUpdate, FileProbeBatchResult,
    FileProbeEntry, ManagerCommand, ManagerEvent,
};
#[cfg(not(target_arch = "wasm32"))]
pub use crate::dht::service::DhtHandle;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;

use crate::app::{FilePriority, TorrentMetrics};
use crate::networking::NetworkActivationHandle;
#[cfg(not(target_arch = "wasm32"))]
use crate::networking::PeerConnection;
use crate::peer_manager::PeerPolicy;
use crate::resource::{PermitGuard, ResourceManagerClient};
use crate::token_bucket::TokenBucket;
use crate::Settings;

#[cfg(not(target_arch = "wasm32"))]
pub type IncomingPeerSession = (PeerConnection, Vec<u8>, PermitGuard);

pub struct TorrentParameters {
    pub network_activation: NetworkActivationHandle,
    #[cfg(not(target_arch = "wasm32"))]
    pub dht_handle: DhtHandle,
    #[cfg(not(target_arch = "wasm32"))]
    pub incoming_peer_rx: Receiver<IncomingPeerSession>,
    pub metrics_tx: watch::Sender<TorrentMetrics>,
    pub peer_policy_rx: watch::Receiver<Arc<PeerPolicy>>,
    pub torrent_validation_status: bool,
    pub torrent_data_path: Option<PathBuf>,
    pub container_name: Option<String>,
    pub manager_command_rx: Receiver<ManagerCommand>,
    pub manager_event_tx: Sender<ManagerEvent>,
    pub settings: Arc<Settings>,
    pub resource_manager: ResourceManagerClient,
    pub global_dl_bucket: Arc<TokenBucket>,
    pub global_ul_bucket: Arc<TokenBucket>,
    pub file_priorities: HashMap<usize, FilePriority>,
}

pub use manager::TorrentManager;

/// Attach the physical payload capability at the runtime composition boundary.
pub struct TorrentExecutionParameters {
    pub parameters: TorrentParameters,
    pub payload: crate::persistence::Payload,
}
impl TorrentParameters {
    pub fn with_payload(self, payload: crate::persistence::Payload) -> TorrentExecutionParameters {
        TorrentExecutionParameters {
            parameters: self,
            payload,
        }
    }
}
// Keep frozen state fixtures source-compatible; production callers must inject explicitly.
#[cfg(test)]
impl From<TorrentParameters> for TorrentExecutionParameters {
    fn from(parameters: TorrentParameters) -> Self {
        parameters.with_payload(crate::persistence::Payload::native())
    }
}
