// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

pub mod block_manager;
pub mod manager;
pub mod merkle;
pub mod piece_manager;
pub mod state;

pub use crate::dht_service::DhtHandle;
use crate::errors::StorageError;
use crate::Settings;

use std::collections::HashMap;

use crate::token_bucket::TokenBucket;

use crate::torrent_file::Torrent;

use crate::app::FilePriority;
use crate::app::TorrentMetrics;

use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::time::Duration;

#[cfg(feature = "synthetic-load")]
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(feature = "synthetic-load")]
use crate::networking::transport::PeerTransportKind;
use crate::networking::PeerConnection;
use crate::resource_manager::{PermitGuard, ResourceManagerClient};

pub type IncomingPeerSession = (PeerConnection, Vec<u8>, PermitGuard);

pub struct TorrentParameters {
    pub dht_handle: DhtHandle,
    pub incoming_peer_rx: Receiver<IncomingPeerSession>,
    pub metrics_tx: watch::Sender<TorrentMetrics>,
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

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct DiskIoOperation {
    pub piece_index: u32,
    pub offset: u64,
    pub length: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileProbeEntry {
    pub relative_path: PathBuf,
    pub absolute_path: PathBuf,
    pub error: StorageError,
    pub expected_size: u64,
    pub observed_size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileProbeBatchResult {
    pub epoch: u64,
    pub scanned_files: usize,
    pub next_file_index: usize,
    pub reached_end_of_manifest: bool,
    pub pending_metadata: bool,
    pub problem_files: Vec<FileProbeEntry>,
}

pub fn data_availability_from_file_probe_result(result: &FileProbeBatchResult) -> Option<bool> {
    if result.pending_metadata {
        None
    } else if !result.problem_files.is_empty() {
        Some(false)
    } else if result.reached_end_of_manifest {
        Some(true)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TorrentFileProbeStatus {
    PendingMetadata,
    Files(Vec<FileProbeEntry>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileActivityDirection {
    Download,
    Upload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileActivityUpdate {
    pub touched_relative_paths: Vec<String>,
    pub direction: FileActivityDirection,
}

#[derive(Debug)]
pub enum ManagerEvent {
    DeletionComplete(Vec<u8>, Result<(), String>),
    DataAvailabilityFault {
        info_hash: Vec<u8>,
        piece_index: u32,
        error: StorageError,
    },
    DiskReadStarted {
        info_hash: Vec<u8>,
        op: DiskIoOperation,
    },
    DiskReadFinished,
    DiskWriteStarted {
        info_hash: Vec<u8>,
        op: DiskIoOperation,
    },
    DiskWriteCompleted {
        info_hash: Vec<u8>,
        op: DiskIoOperation,
    },
    DiskWriteFinished {
        info_hash: Vec<u8>,
        piece_index: u32,
    },
    DiskIoBackoff {
        duration: Duration,
    },
    PeerDiscovered {
        info_hash: Vec<u8>,
    },
    PeerConnected {
        info_hash: Vec<u8>,
    },
    PeerDisconnected {
        info_hash: Vec<u8>,
    },
    #[cfg(feature = "synthetic-load")]
    PeerConnectAttempted {
        transport: PeerTransportKind,
    },
    #[cfg(feature = "synthetic-load")]
    PeerConnectEstablished {
        transport: PeerTransportKind,
    },
    #[cfg(feature = "synthetic-load")]
    PeerConnectFailed {
        transport: PeerTransportKind,
        reason: SyntheticPeerConnectFailure,
    },
    #[cfg(feature = "synthetic-load")]
    PeerSessionFailed,

    BlockReceived {
        info_hash: Vec<u8>,
    },
    BlockSent {
        info_hash: Vec<u8>,
    },
    FileProbeBatchResult {
        info_hash: Vec<u8>,
        result: FileProbeBatchResult,
    },
    MetadataLoaded {
        info_hash: Vec<u8>,
        torrent: Box<Torrent>,
    },
}

#[derive(Debug)]
pub struct PauseDrainResult {
    pub was_already_paused: bool,
    pub storage_relocation: Result<(), String>,
}

#[cfg(feature = "synthetic-load")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntheticPeerConnectFailure {
    PermitTimeout,
    PermitManagerShutdown,
    PermitQueueFull,
    ConnectTimeout,
    ConnectionRefused,
    ConnectionReset,
    ConnectionAborted,
    AddrInUse,
    AddrNotAvailable,
    TimedOut,
    OtherIo,
}

#[derive(Debug)]
pub enum ManagerCommand {
    #[cfg(feature = "synthetic-load")]
    ConnectToPeer(SocketAddr),
    #[cfg(feature = "synthetic-load")]
    ConnectToSyntheticPeer {
        addr: SocketAddr,
        peer_key: String,
    },
    ProbeFileBatch {
        epoch: u64,
        start_file_index: usize,
        max_files: usize,
    },
    SetDataAvailability(bool),
    Pause {
        response: Option<oneshot::Sender<Result<PauseDrainResult, String>>>,
    },
    Resume {
        response: Option<oneshot::Sender<()>>,
    },
    Shutdown,
    DeleteFile,
    SetDataRate(u64),
    UpdateListenPort(u16),
    SetUserTorrentConfig {
        torrent_data_path: PathBuf,
        file_priorities: HashMap<usize, FilePriority>,
        container_name: Option<String>,
    },
    ApplyStoragePath {
        torrent_data_path: PathBuf,
        response: oneshot::Sender<Result<(), String>>,
    },
}

pub use manager::TorrentManager;

#[cfg(test)]
mod tests {
    use super::{data_availability_from_file_probe_result, FileProbeBatchResult, FileProbeEntry};
    use crate::errors::StorageError;

    #[test]
    fn data_availability_from_completed_probe_uses_problem_file_count() {
        assert_eq!(
            data_availability_from_file_probe_result(&FileProbeBatchResult {
                epoch: 0,
                scanned_files: 1,
                next_file_index: 0,
                reached_end_of_manifest: true,
                pending_metadata: false,
                problem_files: Vec::new(),
            }),
            Some(true)
        );
        assert_eq!(
            data_availability_from_file_probe_result(&FileProbeBatchResult {
                epoch: 0,
                scanned_files: 1,
                next_file_index: 0,
                reached_end_of_manifest: true,
                pending_metadata: false,
                problem_files: vec![FileProbeEntry {
                    relative_path: "missing.bin".into(),
                    absolute_path: "/tmp/missing.bin".into(),
                    error: StorageError::from(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "No such file or directory",
                    )),
                    expected_size: 1,
                    observed_size: None,
                }],
            }),
            Some(false)
        );
    }

    #[test]
    fn data_availability_from_incomplete_probe_result_is_unknown() {
        assert_eq!(
            data_availability_from_file_probe_result(&FileProbeBatchResult {
                epoch: 0,
                scanned_files: 128,
                next_file_index: 128,
                reached_end_of_manifest: false,
                pending_metadata: false,
                problem_files: Vec::new(),
            }),
            None
        );
        assert_eq!(
            data_availability_from_file_probe_result(&FileProbeBatchResult {
                epoch: 0,
                scanned_files: 128,
                next_file_index: 128,
                reached_end_of_manifest: false,
                pending_metadata: false,
                problem_files: vec![FileProbeEntry {
                    relative_path: "missing.bin".into(),
                    absolute_path: "/tmp/missing.bin".into(),
                    error: StorageError::from(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "No such file or directory",
                    )),
                    expected_size: 1,
                    observed_size: None,
                }],
            }),
            Some(false)
        );
        assert_eq!(
            data_availability_from_file_probe_result(&FileProbeBatchResult {
                epoch: 0,
                scanned_files: 0,
                next_file_index: 0,
                reached_end_of_manifest: false,
                pending_metadata: true,
                problem_files: Vec::new(),
            }),
            None
        );
    }
}
