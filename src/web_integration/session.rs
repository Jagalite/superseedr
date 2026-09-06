// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Narrow WASM-only bridge from browser-owned behavior to production reducers and rendering.

use super::types::*;
use crate::app::{ManagerLifetime, ManagerObservation, ManagerSource};

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use ratatui::Frame;
use tokio::sync::{mpsc, watch};

use crate::app::torrent_manager_protocol::{DiskIoOperation, ManagerCommand, ManagerEvent};
use crate::app::{
    advance_ui_effects_for_elapsed, align_unpinned_peer_sort_with_visible_activity,
    build_torrent_preview_tree, refresh_autosort_after_stats, remove_torrent_from_state, AppMode,
    AppState, BrowserPane, BrowserSearchState, ConfigItem, DataRate, DownloadSelectionTarget,
    FileBrowserMode, FileMetadata, FilePriority, RssPreviewItem, TorrentControlState,
    TorrentFilePreview, TorrentFilePreviewState, TorrentMetrics,
};
use crate::config::{
    RssAddedVia, RssFeed, RssFilter, RssFilterMode, RssHistoryEntry, Settings, SortDirection,
    TorrentSortColumn,
};
use crate::dht_model::{DhtStatus, DhtWaveTelemetry};
use crate::networking::NetworkInterfaceInfo;
use crate::peer_manager::{
    parse_peer_client, PeerManagerEndpointView, PeerManagerTrackedPeer, PeerManagerView,
};
use crate::persistence::activity_history::{
    ActivityHistoryPersistedState, ActivityHistoryRollupState,
};
use crate::persistence::event_journal::{
    append_event_journal_entry, EventCategory, EventJournalEntry, EventScope, EventType,
};
use crate::persistence::network_history::{
    NetworkHistoryPersistedState, NetworkHistoryRollupState,
};
use crate::persistence::AppPersistence;
use crate::presentation::{PresentationFixture, PresentationState};
use crate::telemetry::activity_history_telemetry::ActivityHistoryTelemetry;
use crate::telemetry::network_history_telemetry::NetworkHistoryTelemetry;
use crate::telemetry::ui_telemetry::{SystemTelemetrySnapshot, UiTelemetry};
use crate::terminal_event::Event;
use crate::theme::{Theme, ThemeName};
use crate::torrent_file::{Info, InfoFile, Torrent};
use crate::tui::screens::{peers, rss};
use crate::tui::tree::RawNode;
use strum::IntoEnumIterator;

const BROWSER_DISK_WARNING: &str = "System Warning: Potential FD limit hit (detected via Disk I/O backoff). Increase 'ulimit -n' if issues persist.";

fn browser_journal_event(kind: BrowserJournalKind) -> (EventCategory, EventType) {
    match kind {
        BrowserJournalKind::IngestAdded => (EventCategory::Ingest, EventType::IngestAdded),
        BrowserJournalKind::TorrentCompleted => {
            (EventCategory::TorrentLifecycle, EventType::TorrentCompleted)
        }
        BrowserJournalKind::DataUnavailable => {
            (EventCategory::DataHealth, EventType::DataUnavailable)
        }
        BrowserJournalKind::DataRecovered => (EventCategory::DataHealth, EventType::DataRecovered),
    }
}

pub struct BrowserSession {
    pub(crate) app_state: AppState,
    pub(crate) client_configs: Settings,
    app_persistence: AppPersistence,
    dht_status: DhtStatus,
    dht_wave_telemetry: DhtWaveTelemetry,
    pending_browser_commands: VecDeque<BrowserCommand>,
    pending_app_effects: VecDeque<crate::app::AppEffect>,
    checkpoint_requested: bool,
    pending_catalog_restores: HashSet<Vec<u8>>,
    unsent_shutdowns: HashSet<Vec<u8>>,
    pending_removals: HashSet<Vec<u8>>,
    #[cfg(feature = "webtorrent")]
    failed_managers: HashMap<Vec<u8>, String>,
    manager_data_rate_ms: u64,
    torrent_manager_command_txs: HashMap<Vec<u8>, mpsc::Sender<ManagerCommand>>,
    torrent_metric_watch_rxs: HashMap<Vec<u8>, watch::Receiver<TorrentMetrics>>,
    manager_event_tx: mpsc::Sender<ManagerObservation>,
    manager_event_rx: mpsc::Receiver<ManagerObservation>,
    manager_lifetimes: HashMap<Vec<u8>, ManagerLifetime>,
    telemetry_batch_tx: mpsc::Sender<(ManagerSource, BrowserTelemetryBatch)>,
    telemetry_batch_rx: mpsc::Receiver<(ManagerSource, BrowserTelemetryBatch)>,
    browser_tracked_peers: HashMap<(Vec<u8>, String), PeerManagerTrackedPeer>,
    browser_peer_metrics_updates: u64,
    browser_selected_peer_rate_frame_updates: u64,
    browser_selected_peer_rate_frame_changes: u64,
    browser_network_interface_refreshes: u64,
    fps_sample_elapsed: f64,
    fps_sample_frames: u32,
    environment: BrowserRuntimeEnvironment,
}

/// Manager-side endpoint used by the browser-owned torrent simulation.
///
/// Its contract deliberately matches the production torrent manager: commands
/// arrive over an mpsc receiver, metrics are published through a watch sender,
/// and discrete lifecycle/telemetry events use the shared manager event queue.
pub struct BrowserTorrentManagerEndpoint {
    source: ManagerSource,
    command_rx: mpsc::Receiver<ManagerCommand>,
    metrics_tx: watch::Sender<TorrentMetrics>,
    manager_event_tx: mpsc::Sender<ManagerObservation>,
    telemetry_batch_tx: mpsc::Sender<(ManagerSource, BrowserTelemetryBatch)>,
}

impl BrowserTorrentManagerEndpoint {
    pub fn drain_commands(&mut self) -> Vec<ManagerCommand> {
        let mut commands = Vec::new();
        while let Ok(command) = self.command_rx.try_recv() {
            commands.push(command);
        }
        commands
    }

    pub fn publish_metrics(&self, metrics: TorrentMetrics) {
        let _ = self.metrics_tx.send(metrics);
    }

    pub fn publish_update(&self, update: BrowserTorrentUpdate) {
        self.publish_metrics(update.into_torrent_metrics());
    }

    pub fn publish_frame(&self, update: BrowserTorrentFrameUpdate) {
        let mut metrics = self.metrics_tx.borrow().clone();
        update.apply_to_torrent_metrics(&mut metrics);
        self.publish_metrics(metrics);
    }

    /// Returns the original event on backpressure or closure so lifecycle results can be retried.
    pub fn publish_event(
        &self,
        event: ManagerEvent,
    ) -> Result<(), Box<mpsc::error::TrySendError<ManagerEvent>>> {
        self.manager_event_tx
            .try_send(ManagerObservation {
                source: self.source.clone(),
                event,
            })
            .map_err(|error| {
                Box::new(match error {
                    mpsc::error::TrySendError::Full(observation) => {
                        mpsc::error::TrySendError::Full(observation.event)
                    }
                    mpsc::error::TrySendError::Closed(observation) => {
                        mpsc::error::TrySendError::Closed(observation.event)
                    }
                })
            })
    }

    pub fn publish_telemetry(&self, batch: BrowserTelemetryBatch) {
        let _ = self
            .telemetry_batch_tx
            .try_send((self.source.clone(), batch));
    }

    pub fn publish_metadata(
        &self,
        info_hash: Vec<u8>,
        torrent_name: String,
        files: &[BrowserFileUpdate],
    ) -> Result<(), Box<mpsc::error::TrySendError<ManagerEvent>>> {
        let torrent = Torrent {
            info: Info {
                piece_length: 16_384,
                name: torrent_name,
                files: files
                    .iter()
                    .map(|file| InfoFile {
                        length: i64::try_from(file.size).unwrap_or(i64::MAX),
                        path: file
                            .relative_path
                            .split('/')
                            .filter(|segment| !segment.is_empty())
                            .map(str::to_string)
                            .collect(),
                        ..InfoFile::default()
                    })
                    .collect(),
                ..Info::default()
            },
            ..Torrent::default()
        };
        self.publish_event(ManagerEvent::MetadataLoaded {
            info_hash,
            torrent: Box::new(torrent),
        })
    }
}

fn preview_file_count(node: &RawNode<crate::app::TorrentPreviewPayload>) -> usize {
    usize::from(node.payload.file_index.is_some())
        + node.children.iter().map(preview_file_count).sum::<usize>()
}

mod bootstrap;
mod checkpoint;
mod control;
#[cfg(feature = "webtorrent")]
mod manager_lifecycle;
mod managers;
mod preview;
mod rss_results;
mod runtime;
mod settings;
mod telemetry;
mod view;

pub fn canonical_browser_magnet_info_hash(magnet_link: &str) -> Option<Vec<u8>> {
    crate::torrent_identity::canonical_info_hash_from_magnet_link(magnet_link)
}

pub fn has_browser_magnet_scheme(value: &str) -> bool {
    value
        .get(.."magnet:".len())
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("magnet:"))
}

#[cfg(feature = "webtorrent")]
mod engine;
#[cfg(feature = "webtorrent")]
pub use engine::LiveClient;
