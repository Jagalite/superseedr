// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Platform-neutral command and telemetry contract between the application and a torrent manager.

use std::collections::HashMap;
#[cfg(feature = "synthetic-load")]
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use super::FilePriority;
use crate::errors::StorageError;
#[cfg(feature = "synthetic-load")]
use crate::networking::transport::PeerTransportKind;
use crate::torrent_file::Torrent;

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct DiskIoOperation {
    pub piece_index: u32,
    pub offset: u64,
    pub length: usize,
}

/// A compact manager-to-application telemetry update for runtimes that
/// accumulate multiple manager events before yielding to the UI.
#[derive(Debug, Clone, Default)]
pub struct ManagerTelemetryBatch {
    pub info_hash: Vec<u8>,
    pub peers_discovered: usize,
    pub peers_connected: usize,
    pub peers_disconnected: usize,
    pub blocks_received: usize,
    pub blocks_sent: usize,
    pub disk_read_bytes: u64,
    pub disk_write_bytes: u64,
    pub disk_read_operations: usize,
    pub disk_write_operations: usize,
    pub disk_read_samples: Vec<DiskIoOperation>,
    pub disk_write_samples: Vec<DiskIoOperation>,
    pub disk_read_latency: Option<Duration>,
    pub disk_write_latency: Option<Duration>,
    pub receive_to_write_latency: Option<Duration>,
    pub disk_backoff: Option<Duration>,
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
    #[allow(dead_code)] // Used by manager implementations that coalesce high-rate telemetry.
    TelemetryBatch(ManagerTelemetryBatch),
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
    Pause,
    Resume,
    Shutdown,
    DeleteFile,
    SetDataRate(u64),
    SetUserTorrentConfig {
        torrent_data_path: PathBuf,
        file_priorities: HashMap<usize, FilePriority>,
        container_name: Option<String>,
    },
}

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
