// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared application commands definitions and transitions.

use super::*;

pub enum AppCommand {
    CheckpointPersisted {
        revision: u64,
        result: Result<(), String>,
    },
    AddTorrentFromFile(PathBuf),
    AddTorrentFromPathFile(PathBuf),
    AddMagnetFromFile(PathBuf),
    MarkPortOpen {
        peer_addr: SocketAddr,
        transport: PeerTransportKind,
        scope_id: NetworkScopeId,
    },
    ReloadClusterState(PathBuf),
    SubmitControlRequest(ControlRequest),
    SubmitManualAddRequest {
        request: ControlRequest,
        pending_ingest: Option<PendingManualIngest>,
    },
    ControlRequest {
        path: PathBuf,
        request: ControlRequest,
    },
    ClientShutdown(PathBuf),
    PortFileChanged(PathBuf),
    FetchFileTree {
        browser_generation: u64,
        path: PathBuf,
        browser_mode: FileBrowserMode,
        preserve_browser_mode: bool,
        highlight_path: Option<PathBuf>,
    },
    UpdateFileBrowserData {
        request_id: u64,
        path: PathBuf,
        data: Vec<tree::RawNode<FileMetadata>>,
        highlight_path: Option<PathBuf>,
    },
    FileBrowserFetchFailed {
        request_id: u64,
        path: PathBuf,
        message: String,
    },
    UpdateTorrentFilePreview {
        browser_generation: u64,
        request_id: u64,
        path: PathBuf,
        result: Result<TorrentFilePreview, String>,
    },
    RssSyncNow,
    RssPreviewUpdated(Vec<RssPreviewItem>),
    RssSyncStatusUpdated {
        last_sync_at: Option<String>,
        next_sync_at: Option<String>,
    },
    RssFeedErrorUpdated {
        feed_url: String,
        error: Option<FeedSyncError>,
    },
    RssDownloadSelected {
        entry: RssHistoryEntry,
        command_path: Option<PathBuf>,
    },
    RssDownloadPreview(RssPreviewItem),
    NetworkHistoryLoaded(NetworkHistoryPersistedState),
    ActivityHistoryLoaded(Box<ActivityHistoryPersistedState>),
    NetworkHistoryPersisted {
        request_id: u64,
        success: bool,
    },
    ActivityHistoryPersisted {
        request_id: u64,
        success: bool,
    },
    ConfigNetworkInterfacesDiscovered {
        request_id: u64,
        result: Result<Vec<NetworkInterfaceInfo>, String>,
    },
    RefreshConfigNetworkInterfaces,
    UpdateConfig(Settings),
    UpdateVersionAvailable(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppRuntimeMode {
    Normal,
    SharedLeader,
    SharedFollower,
}

impl AppRuntimeMode {
    pub fn is_shared(self) -> bool {
        matches!(self, Self::SharedLeader | Self::SharedFollower)
    }

    pub fn is_shared_follower(self) -> bool {
        matches!(self, Self::SharedFollower)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppClusterRole {
    Leader,
    Follower,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ClusterCapabilities {
    pub(super) can_write_shared_state: bool,
    pub(super) can_queue_shared_commands: bool,
    pub(super) can_edit_host_local_config: bool,
    pub(super) can_persist_local_runtime_state: bool,
    pub(super) can_consume_shared_inbox: bool,
}

#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IngestSource {
    TorrentFile,
    TorrentPathFile,
    MagnetFile,
}

impl IngestSource {
    pub(super) fn relay_archive_extension(self) -> &'static str {
        match self {
            Self::TorrentFile => "torrent.forwarded",
            Self::TorrentPathFile => "path.forwarded",
            Self::MagnetFile => "magnet.forwarded",
        }
    }

    pub(super) fn processed_archive_extension(self) -> &'static str {
        match self {
            Self::TorrentFile => "torrent.added",
            Self::TorrentPathFile => "path.added",
            Self::MagnetFile => "magnet.added",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ResolvedAddPayload {
    TorrentFile { source_path: PathBuf },
    MagnetLink { magnet_link: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum AddIngressAction {
    RelayRawWatchFile,
    QueueControlRequest(ControlRequest),
    ApplyDirectly {
        payload: ResolvedAddPayload,
        download_path: PathBuf,
    },
    OpenManualBrowser {
        payload: ResolvedAddPayload,
    },
    IgnoreMissingSharedInboxItem {
        message: String,
    },
    Fail {
        message: String,
    },
}

pub(super) type AvailabilityTransitionLog =
    (String, bool, usize, Option<std::path::PathBuf>, Vec<String>);

#[derive(Debug, Clone)]
pub(crate) struct PendingIngestRecord {
    pub(super) correlation_id: String,
    pub(super) origin: IngestOrigin,
    pub(super) ingest_kind: IngestKind,
    pub(super) source_watch_folder: Option<PathBuf>,
    pub(super) source_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct PendingManualIngest {
    pub(super) source: IngestSource,
    pub(super) path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingControlRecord {
    pub(super) correlation_id: String,
    pub(super) request: ControlRequest,
    pub(super) origin: ControlOrigin,
    pub(super) source_watch_folder: Option<PathBuf>,
    pub(super) source_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommandIngestResult {
    Added {
        info_hash: Option<Vec<u8>>,
        torrent_name: Option<String>,
    },
    Duplicate {
        info_hash: Option<Vec<u8>>,
        torrent_name: Option<String>,
    },
    Invalid {
        info_hash: Option<Vec<u8>>,
        torrent_name: Option<String>,
        message: String,
    },
    Failed {
        info_hash: Option<Vec<u8>>,
        torrent_name: Option<String>,
        message: String,
    },
}

#[cfg(test)]
pub(super) fn move_file_with_fallback_impl<F>(
    source: &std::path::Path,
    destination: &std::path::Path,
    rename_op: F,
) -> std::io::Result<()>
where
    F: FnOnce(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
{
    crate::integrations::watch_inbox::move_file_with_fallback_impl(source, destination, rename_op)
}

pub(super) fn ingest_kind_from_path(path: &std::path::Path) -> Option<IngestKind> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("torrent") => Some(IngestKind::TorrentFile),
        Some("magnet") => Some(IngestKind::MagnetFile),
        Some("path") => Some(IngestKind::PathFile),
        _ => None,
    }
}

pub(super) fn event_correlation_id_for_path(path: &std::path::Path) -> String {
    hex::encode(sha1::Sha1::digest(path.to_string_lossy().as_bytes()))
}
