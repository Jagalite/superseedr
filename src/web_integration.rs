// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Narrow WASM-only bridge from browser-owned behavior to production reducers and rendering.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use ratatui::Frame;

use crate::app::{
    build_torrent_preview_tree, App, AppCommand, AppMode, ConfigItem, FileBrowserMode,
    FileMetadata, FilePriority, RssPreviewItem, TorrentControlState,
};
use crate::config::{RssFeed, RssFilter, RssFilterMode};
use crate::dht_service::{DhtSizeEstimate, DhtStatus, DhtWaveTelemetry};
use crate::integrations::control::ControlRequest;
use crate::peer_manager::{PeerManagerEndpointView, PeerManagerTrackedPeer, PeerManagerView};
use crate::persistence::event_journal::{EventCategory, EventJournalEntry, EventScope, EventType};
use crate::presentation::{PresentationFixture, PresentationState};
use crate::terminal_event::Event;
use crate::torrent_manager::{DiskIoOperation, FileActivityDirection, FileActivityUpdate};
use crate::tui::screens::{peers, rss};
use crate::tui::tree::RawNode;
use strum::IntoEnumIterator;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrowserCommand {
    AddMagnet {
        magnet_link: String,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        validation_status: bool,
    },
    Pause {
        info_hash_hex: String,
    },
    Resume {
        info_hash_hex: String,
    },
    Delete {
        info_hash_hex: String,
        delete_files: bool,
    },
    FetchFileTree {
        browser_generation: u64,
        path: PathBuf,
        highlight_path: Option<PathBuf>,
    },
    AddTorrentFromFile {
        path: PathBuf,
    },
    SetTorrentConfig {
        info_hash_hex: String,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        file_priorities: Vec<BrowserFilePriorityOverride>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserFilePriority {
    High,
    Skip,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserFilePriorityOverride {
    pub file_index: usize,
    pub priority: BrowserFilePriority,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BrowserTorrentControlState {
    #[default]
    Running,
    Paused,
    Deleting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserScreen {
    Welcome,
    Normal,
    Help,
    Journal,
    PeerManagement,
    TorrentManagement,
    PowerSaving,
    DeleteConfirm,
    Config,
    FileBrowser,
    Rss,
}

#[derive(Clone, Debug, Default)]
pub struct BrowserTorrentUpdate {
    pub info_hash: Vec<u8>,
    pub torrent_name: String,
    pub torrent_or_magnet: String,
    pub pieces_total: u32,
    pub pieces_completed: u32,
    pub download_speed_bps: u64,
    pub upload_speed_bps: u64,
    pub activity_message: String,
    pub download_path: Option<PathBuf>,
    pub container_name: Option<String>,
    pub control_state: BrowserTorrentControlState,
    pub data_available: bool,
    pub is_complete: bool,
    pub total_size: u64,
    pub bytes_written: u64,
    pub session_downloaded: u64,
    pub session_uploaded: u64,
    pub peers: Vec<BrowserPeerUpdate>,
    pub files: Vec<BrowserFileUpdate>,
    pub download_history: Vec<u64>,
    pub upload_history: Vec<u64>,
    pub blocks_in_history: Vec<u64>,
    pub blocks_out_history: Vec<u64>,
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,
    pub peer_discovery_history: Vec<u64>,
    pub peer_connection_history: Vec<u64>,
    pub peer_disconnect_history: Vec<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct BrowserPeerUpdate {
    pub address: String,
    pub client: String,
    pub download_speed_bps: u64,
    pub upload_speed_bps: u64,
    pub total_downloaded: u64,
    pub total_uploaded: u64,
    pub bitfield: Vec<bool>,
    pub active: bool,
}

#[derive(Clone, Debug, Default)]
pub struct BrowserFileUpdate {
    pub relative_path: String,
    pub size: u64,
}

#[derive(Clone, Debug, Default)]
pub struct BrowserFileTreeEntry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Clone, Debug, Default)]
pub struct BrowserJournalUpdate {
    pub timestamp: String,
    pub torrent_name: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Default)]
pub struct BrowserRssUpdate {
    pub feed_url: String,
    pub filter_query: String,
    pub item_title: String,
    pub item_link: String,
    pub timestamp: String,
}

#[derive(Clone, Debug, Default)]
pub struct BrowserTelemetryUpdate {
    pub cpu_usage: f32,
    pub ram_usage_percent: f32,
    pub app_ram_usage: u64,
    pub run_time: u64,
    pub total_download_history: Vec<u64>,
    pub total_upload_history: Vec<u64>,
    pub disk_read_history: Vec<u64>,
    pub disk_write_history: Vec<u64>,
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,
    pub disk_backoff_history_ms: Vec<u64>,
    pub dht_nodes: usize,
    pub dht_active_lookups: usize,
    pub dht_peers_found: usize,
    pub filesystem: Vec<BrowserFileUpdate>,
    pub journal: Vec<BrowserJournalUpdate>,
    pub rss: Vec<BrowserRssUpdate>,
}

pub struct BrowserSession {
    app: App,
    dht_status: DhtStatus,
    dht_wave_telemetry: DhtWaveTelemetry,
}

impl BrowserSession {
    pub fn from_fixture(width: u16, height: u16, fixture: PresentationFixture) -> Self {
        let presentation = PresentationState::from_fixture(width, height, fixture);
        let (app_state, dht_status, dht_wave_telemetry, settings) = presentation.into_parts();
        Self {
            app: App::new(app_state, settings),
            dht_status,
            dht_wave_telemetry,
        }
    }

    pub async fn dispatch_event(&mut self, event: Event) {
        crate::tui::events::handle_event(event, &mut self.app).await;
    }

    pub async fn flush_pending_paste_burst(&mut self) {
        crate::tui::events::flush_pending_paste_burst(&mut self.app).await;
    }

    pub fn draw(&self, frame: &mut Frame) {
        crate::tui::view::draw(
            frame,
            &self.app.app_state,
            &self.dht_status,
            &self.dht_wave_telemetry,
            &self.app.client_configs,
        );
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.app.app_state.screen_area =
            ratatui::layout::Rect::new(0, 0, width.max(1), height.max(1));
        self.app.app_state.ui.needs_redraw = true;
    }

    pub fn screen_size(&self) -> (u16, u16) {
        (
            self.app.app_state.screen_area.width,
            self.app.app_state.screen_area.height,
        )
    }

    pub fn set_screen(&mut self, screen: BrowserScreen) {
        match screen {
            BrowserScreen::Config => {
                *self.app.app_state.ui.config.settings_edit = self.app.client_configs.clone();
                self.app.app_state.ui.config.selected_index = 0;
                self.app.app_state.ui.config.items = ConfigItem::iter().collect();
            }
            BrowserScreen::DeleteConfirm => {
                if let Some(info_hash) = self.app.app_state.torrent_list_order.first() {
                    self.app.app_state.ui.delete_confirm.info_hash = info_hash.clone();
                    self.app.app_state.ui.delete_confirm.with_files = false;
                }
            }
            BrowserScreen::TorrentManagement => {
                crate::tui::screens::torrents::initialize_torrent_management_cursor(
                    &mut self.app.app_state,
                );
            }
            BrowserScreen::FileBrowser => {
                self.app.app_state.ui.file_browser.browser_mode = FileBrowserMode::Directory;
            }
            _ => {}
        }
        self.app.app_state.mode = match screen {
            BrowserScreen::Welcome => AppMode::Welcome,
            BrowserScreen::Normal => AppMode::Normal,
            BrowserScreen::Help => AppMode::Help,
            BrowserScreen::Journal => AppMode::Journal,
            BrowserScreen::PeerManagement => AppMode::PeerManagement,
            BrowserScreen::TorrentManagement => AppMode::TorrentManagement,
            BrowserScreen::PowerSaving => AppMode::PowerSaving,
            BrowserScreen::DeleteConfirm => AppMode::DeleteConfirm,
            BrowserScreen::Config => AppMode::Config,
            BrowserScreen::FileBrowser => AppMode::FileBrowser,
            BrowserScreen::Rss => AppMode::Rss,
        };
        self.app.app_state.ui.needs_redraw = true;
    }

    pub fn screen(&self) -> BrowserScreen {
        match self.app.app_state.mode {
            AppMode::Welcome => BrowserScreen::Welcome,
            AppMode::Normal => BrowserScreen::Normal,
            AppMode::Help => BrowserScreen::Help,
            AppMode::Journal => BrowserScreen::Journal,
            AppMode::PeerManagement => BrowserScreen::PeerManagement,
            AppMode::TorrentManagement => BrowserScreen::TorrentManagement,
            AppMode::PowerSaving => BrowserScreen::PowerSaving,
            AppMode::DeleteConfirm => BrowserScreen::DeleteConfirm,
            AppMode::Config => BrowserScreen::Config,
            AppMode::FileBrowser => BrowserScreen::FileBrowser,
            AppMode::Rss => BrowserScreen::Rss,
        }
    }

    pub fn drain_commands(&mut self) -> Vec<BrowserCommand> {
        let mut commands = Vec::new();
        while let Ok(command) = self.app.app_command_rx.try_recv() {
            let command = match command {
                AppCommand::AddTorrentFromFile(path) => BrowserCommand::AddTorrentFromFile { path },
                AppCommand::FetchFileTree {
                    browser_generation,
                    path,
                    browser_mode,
                    preserve_browser_mode,
                    highlight_path,
                } => {
                    if !self.app.begin_file_browser_fetch(
                        browser_generation,
                        path.clone(),
                        browser_mode,
                        preserve_browser_mode,
                    ) {
                        continue;
                    }
                    BrowserCommand::FetchFileTree {
                        browser_generation,
                        path,
                        highlight_path,
                    }
                }
                AppCommand::SubmitControlRequest(ControlRequest::AddMagnet {
                    magnet_link,
                    download_path,
                    container_name,
                    validation_status,
                    ..
                }) => BrowserCommand::AddMagnet {
                    magnet_link,
                    download_path,
                    container_name,
                    validation_status,
                },
                AppCommand::SubmitControlRequest(ControlRequest::Pause { info_hash_hex }) => {
                    BrowserCommand::Pause { info_hash_hex }
                }
                AppCommand::SubmitControlRequest(ControlRequest::Resume { info_hash_hex }) => {
                    BrowserCommand::Resume { info_hash_hex }
                }
                AppCommand::SubmitControlRequest(ControlRequest::Delete {
                    info_hash_hex,
                    delete_files,
                }) => BrowserCommand::Delete {
                    info_hash_hex,
                    delete_files,
                },
                AppCommand::SubmitControlRequest(ControlRequest::SetTorrentConfig {
                    info_hash_hex,
                    download_path,
                    container_name,
                    file_priorities,
                }) => BrowserCommand::SetTorrentConfig {
                    info_hash_hex,
                    download_path,
                    container_name,
                    file_priorities: file_priorities
                        .into_iter()
                        .filter_map(|override_value| {
                            let priority = match override_value.priority {
                                FilePriority::High => BrowserFilePriority::High,
                                FilePriority::Skip => BrowserFilePriority::Skip,
                                FilePriority::Normal | FilePriority::Mixed => return None,
                            };
                            Some(BrowserFilePriorityOverride {
                                file_index: override_value.file_index,
                                priority,
                            })
                        })
                        .collect(),
                },
                _ => continue,
            };
            commands.push(command);
        }
        commands
    }

    pub fn apply_mock_file_tree(
        &mut self,
        browser_generation: u64,
        path: PathBuf,
        entries: Vec<BrowserFileTreeEntry>,
        highlight_path: Option<PathBuf>,
    ) -> bool {
        let browser = &mut self.app.app_state.ui.file_browser;
        if browser_generation != browser.browser_generation
            || !matches!(self.app.app_state.mode, AppMode::FileBrowser)
        {
            return false;
        }
        browser.fetch_pending = false;
        browser.fetch_error = None;
        browser.state.current_path = path.clone();
        browser.state.top_most_offset = 0;
        browser.data = entries
            .into_iter()
            .map(|entry| RawNode {
                name: entry.name.clone(),
                full_path: path.join(entry.name),
                children: Vec::new(),
                payload: FileMetadata {
                    size: entry.size,
                    modified: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                },
                is_dir: entry.is_dir,
            })
            .collect();
        browser.state.cursor_path = highlight_path
            .filter(|highlight| browser.data.iter().any(|node| &node.full_path == highlight))
            .or_else(|| browser.data.first().map(|node| node.full_path.clone()));
        self.app.app_state.ui.needs_redraw = true;
        true
    }

    pub fn apply_mock_torrent_config(
        &mut self,
        info_hash_hex: &str,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        file_priorities: &[BrowserFilePriorityOverride],
    ) -> bool {
        let Ok(info_hash) = hex::decode(info_hash_hex) else {
            return false;
        };
        let Some(torrent) = self.app.app_state.torrents.get_mut(&info_hash) else {
            return false;
        };
        torrent.latest_state.download_path = download_path;
        torrent.latest_state.container_name = container_name;
        for override_value in file_priorities {
            let priority = match override_value.priority {
                BrowserFilePriority::High => FilePriority::High,
                BrowserFilePriority::Skip => FilePriority::Skip,
            };
            torrent
                .latest_state
                .file_priorities
                .insert(override_value.file_index, priority);
        }
        self.app.app_state.ui.needs_redraw = true;
        true
    }

    pub fn upsert_mock_torrent(&mut self, update: BrowserTorrentUpdate) {
        let mut display = self
            .app
            .app_state
            .torrents
            .remove(&update.info_hash)
            .unwrap_or_default();
        display.latest_state.info_hash = update.info_hash.clone();
        display.latest_state.torrent_name = update.torrent_name;
        display.latest_state.torrent_or_magnet = update.torrent_or_magnet;
        display.latest_state.number_of_pieces_total = update.pieces_total;
        display.latest_state.number_of_pieces_completed = update.pieces_completed;
        display.latest_state.download_speed_bps = update.download_speed_bps;
        display.latest_state.upload_speed_bps = update.upload_speed_bps;
        display.latest_state.activity_message = update.activity_message;
        display.latest_state.download_path = update.download_path;
        display.latest_state.container_name = update.container_name;
        display.latest_state.data_available = update.data_available;
        display.latest_state.is_complete = update.is_complete;
        display.latest_state.total_size = update.total_size;
        display.latest_state.bytes_written = update.bytes_written;
        display.latest_state.session_total_downloaded = update.session_downloaded;
        display.latest_state.session_total_uploaded = update.session_uploaded;
        display.latest_state.file_count = Some(update.files.len());
        display.latest_state.is_multi_file = update.files.len() > 1;
        display.latest_state.peers = update
            .peers
            .iter()
            .map(|peer| crate::app::PeerInfo {
                address: peer.address.clone(),
                peer_id: peer.client.as_bytes().to_vec(),
                bitfield: peer.bitfield.clone(),
                download_speed_bps: peer.download_speed_bps,
                upload_speed_bps: peer.upload_speed_bps,
                total_downloaded: peer.total_downloaded,
                total_uploaded: peer.total_uploaded,
                last_action: if peer.active {
                    "Transferring simulated pieces".to_string()
                } else {
                    "Waiting in simulated swarm".to_string()
                },
                ..Default::default()
            })
            .collect();
        display.latest_state.number_of_successfully_connected_peers =
            display.latest_state.peers.len();
        display.latest_state.tcp_peer_count = display.latest_state.peers.len();
        display.latest_state.blocks_in_history = update.blocks_in_history.clone();
        display.latest_state.blocks_out_history = update.blocks_out_history.clone();
        display.latest_state.blocks_in_this_tick =
            update.blocks_in_history.last().copied().unwrap_or_default();
        display.latest_state.blocks_out_this_tick = update
            .blocks_out_history
            .last()
            .copied()
            .unwrap_or_default();
        display.latest_state.file_activity_updates = if update.files.is_empty() {
            Vec::new()
        } else {
            vec![FileActivityUpdate {
                touched_relative_paths: update
                    .files
                    .iter()
                    .take(2)
                    .map(|file| file.relative_path.clone())
                    .collect(),
                direction: FileActivityDirection::Download,
            }]
        };
        display.file_preview_tree = build_torrent_preview_tree(
            update
                .files
                .iter()
                .map(|file| {
                    (
                        file.relative_path
                            .split('/')
                            .filter(|segment| !segment.is_empty())
                            .map(str::to_string)
                            .collect(),
                        file.size,
                    )
                })
                .collect(),
            &display.latest_state.file_priorities,
        );
        display.download_history = update.download_history;
        display.upload_history = update.upload_history;
        display.disk_read_speed_bps = update.disk_read_bps;
        display.disk_write_speed_bps = update.disk_write_bps;
        display.disk_read_history_log = VecDeque::from([
            DiskIoOperation {
                piece_index: 1,
                offset: 0,
                length: 16 * 1024,
            },
            DiskIoOperation {
                piece_index: 2,
                offset: 16 * 1024,
                length: 16 * 1024,
            },
        ]);
        display.disk_write_history_log = VecDeque::from([DiskIoOperation {
            piece_index: 3,
            offset: 32 * 1024,
            length: 16 * 1024,
        }]);
        display.peer_discovery_history = update.peer_discovery_history;
        display.peer_connection_history = update.peer_connection_history;
        display.peer_disconnect_history = update.peer_disconnect_history;
        display.swarm_availability_history = if update.pieces_total == 0 {
            Vec::new()
        } else {
            vec![crate::app::swarm_availability_counts(
                &display.latest_state.peers,
                update.pieces_total,
            )]
        };
        display.latest_state.torrent_control_state = match update.control_state {
            BrowserTorrentControlState::Running => TorrentControlState::Running,
            BrowserTorrentControlState::Paused => TorrentControlState::Paused,
            BrowserTorrentControlState::Deleting => TorrentControlState::Deleting,
        };
        display.smoothed_download_speed_bps = display.latest_state.download_speed_bps;
        display.smoothed_upload_speed_bps = display.latest_state.upload_speed_bps;
        self.app
            .app_state
            .torrents
            .insert(update.info_hash.clone(), display);
        if !self
            .app
            .app_state
            .torrent_list_order
            .contains(&update.info_hash)
        {
            self.app.app_state.torrent_list_order.push(update.info_hash);
        }
        self.app.app_state.ui.needs_redraw = true;
    }

    pub fn apply_mock_telemetry(&mut self, update: BrowserTelemetryUpdate) {
        let state = &mut self.app.app_state;
        state.cpu_usage = update.cpu_usage;
        state.ram_usage_percent = update.ram_usage_percent;
        state.app_ram_usage = update.app_ram_usage;
        state.run_time = update.run_time;
        state.total_download_history = update.total_download_history.clone();
        state.total_upload_history = update.total_upload_history.clone();
        state.avg_download_history = update.total_download_history;
        state.avg_upload_history = update.total_upload_history;
        state.disk_read_history = update.disk_read_history;
        state.disk_write_history = update.disk_write_history;
        state.avg_disk_read_bps = update.disk_read_bps;
        state.avg_disk_write_bps = update.disk_write_bps;
        state.avg_disk_write_completed_bps = update.disk_write_bps;
        state.session_total_downloaded = state
            .torrents
            .values()
            .map(|torrent| torrent.latest_state.bytes_written)
            .sum();
        state.session_total_uploaded = state
            .torrents
            .values()
            .map(|torrent| torrent.latest_state.session_total_uploaded)
            .sum();
        state.lifetime_downloaded_from_config = state.session_total_downloaded * 3;
        state.lifetime_uploaded_from_config = state.session_total_uploaded * 3;
        state.disk_backoff_history_ms = update.disk_backoff_history_ms.into();
        state.global_disk_read_history_log = VecDeque::from([DiskIoOperation {
            piece_index: 7,
            offset: 0,
            length: 32 * 1024,
        }]);
        state.global_disk_write_history_log = VecDeque::from([DiskIoOperation {
            piece_index: 9,
            offset: 64 * 1024,
            length: 32 * 1024,
        }]);

        self.dht_status.generation = 1;
        self.dht_status.health.enabled = true;
        self.dht_status.health.cached_ipv4_routes = update.dht_nodes;
        self.dht_status.health.active_ipv4_routes = update.dht_nodes / 2;
        self.dht_status.health.dht_size_estimate = Some(DhtSizeEstimate {
            node_count: update.dht_nodes,
            std_dev: Some(update.dht_nodes as f64 * 0.08),
        });
        self.dht_wave_telemetry.active_lookups = update.dht_active_lookups;
        self.dht_wave_telemetry.inflight_ipv4_queries = update.dht_active_lookups;
        self.dht_wave_telemetry.unique_peers_found_last_10s = update.dht_peers_found;

        let base_path = PathBuf::from("/simulated");
        state.ui.file_browser.state.current_path = base_path.clone();
        state.ui.file_browser.data = update
            .filesystem
            .iter()
            .map(|file| RawNode {
                name: file.relative_path.clone(),
                full_path: base_path.join(&file.relative_path),
                children: Vec::new(),
                payload: FileMetadata {
                    size: file.size,
                    modified: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                },
                is_dir: false,
            })
            .collect();
        state.ui.file_browser.state.cursor_path = state
            .ui
            .file_browser
            .data
            .first()
            .map(|node| node.full_path.clone());

        state.event_journal_state.entries = update
            .journal
            .into_iter()
            .enumerate()
            .map(|(index, entry)| EventJournalEntry {
                id: index as u64 + 1,
                scope: EventScope::Host,
                ts_iso: entry.timestamp,
                category: EventCategory::TorrentLifecycle,
                event_type: EventType::TorrentCompleted,
                torrent_name: entry.torrent_name,
                message: Some(entry.message),
                ..Default::default()
            })
            .collect();
        state.event_journal_state.next_id = state.event_journal_state.entries.len() as u64 + 1;

        self.app.client_configs.rss.feeds = update
            .rss
            .iter()
            .map(|item| RssFeed {
                url: item.feed_url.clone(),
                enabled: true,
            })
            .collect();
        self.app.client_configs.rss.filters = update
            .rss
            .iter()
            .map(|item| RssFilter {
                query: item.filter_query.clone(),
                mode: RssFilterMode::Fuzzy,
                enabled: true,
            })
            .collect();
        state.rss_runtime.preview_items = update
            .rss
            .into_iter()
            .enumerate()
            .map(|(index, item)| RssPreviewItem {
                dedupe_key: format!("simulated-{index}"),
                title: item.item_title,
                link: Some(item.item_link),
                source: Some(item.feed_url),
                date_iso: Some(item.timestamp),
                is_match: true,
                ..Default::default()
            })
            .collect();
        rss::recompute_rss_derived(state, &self.app.client_configs);

        let mut tracked_peers = Vec::new();
        for torrent in state.torrents.values() {
            for peer in &torrent.latest_state.peers {
                let Ok(ip) = peer.address.split(':').next().unwrap_or_default().parse() else {
                    continue;
                };
                tracked_peers.push(PeerManagerTrackedPeer {
                    torrent_info_hash: torrent.latest_state.info_hash.clone(),
                    torrent_name: torrent.latest_state.torrent_name.clone(),
                    ip,
                    is_active: peer.download_speed_bps > 0 || peer.upload_speed_bps > 0,
                    endpoints: vec![PeerManagerEndpointView {
                        address: peer.address.clone(),
                        total_downloaded: peer.total_downloaded,
                        total_uploaded: peer.total_uploaded,
                    }],
                    downloaded_evidence_bytes: peer.total_downloaded,
                    uploaded_evidence_bytes: peer.total_uploaded,
                    total_downloaded_bytes: peer.total_downloaded,
                    total_uploaded_bytes: peer.total_uploaded,
                    connection_count: 2,
                    disconnect_count: 1,
                    transfer_threshold_bytes: 64 * 1024,
                    reconnect_count: 1,
                    reconnect_limit: 4,
                    reconnect_window_secs: 300,
                    last_seen: Some(
                        web_time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                    ),
                    clients: vec![String::from_utf8_lossy(&peer.peer_id).into_owned()],
                });
            }
        }
        state.peer_manager_view = Arc::new(PeerManagerView {
            registered_torrents: state.torrents.len(),
            metrics_updates: 12,
            tracked_peers,
        });
        peers::recompute_peer_management_derived(state, web_time::SystemTime::now());
        state.ui.needs_redraw = true;
    }

    pub fn set_torrent_paused_hex(&mut self, info_hash_hex: &str, paused: bool) -> bool {
        let Ok(info_hash) = hex::decode(info_hash_hex) else {
            return false;
        };
        let Some(torrent) = self.app.app_state.torrents.get_mut(&info_hash) else {
            return false;
        };
        torrent.latest_state.torrent_control_state = if paused {
            TorrentControlState::Paused
        } else {
            TorrentControlState::Running
        };
        self.app.app_state.ui.needs_redraw = true;
        true
    }

    pub fn remove_torrent_hex(&mut self, info_hash_hex: &str) -> bool {
        let Ok(info_hash) = hex::decode(info_hash_hex) else {
            return false;
        };
        let removed = self.app.app_state.torrents.remove(&info_hash).is_some();
        self.app
            .app_state
            .torrent_list_order
            .retain(|candidate| candidate != &info_hash);
        if self.app.app_state.torrent_list_order.is_empty() {
            self.app.app_state.ui.selected_torrent_index = 0;
        } else {
            self.app.app_state.ui.selected_torrent_index = self
                .app
                .app_state
                .ui
                .selected_torrent_index
                .min(self.app.app_state.torrent_list_order.len() - 1);
        }
        self.app.app_state.ui.needs_redraw = true;
        removed
    }

    pub fn torrent_control_state_hex(
        &self,
        info_hash_hex: &str,
    ) -> Option<BrowserTorrentControlState> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        self.app.app_state.torrents.get(&info_hash).map(|torrent| {
            match torrent.latest_state.torrent_control_state {
                TorrentControlState::Running => BrowserTorrentControlState::Running,
                TorrentControlState::Paused => BrowserTorrentControlState::Paused,
                TorrentControlState::Deleting => BrowserTorrentControlState::Deleting,
            }
        })
    }

    pub fn torrent_delete_files_hex(&self, info_hash_hex: &str) -> Option<bool> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        self.app
            .app_state
            .torrents
            .get(&info_hash)
            .map(|torrent| torrent.latest_state.delete_files)
    }

    pub fn torrent_file_priority_hex(
        &self,
        info_hash_hex: &str,
        file_index: usize,
    ) -> Option<BrowserFilePriority> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        self.app
            .app_state
            .torrents
            .get(&info_hash)?
            .latest_state
            .file_priorities
            .get(&file_index)
            .and_then(|priority| match priority {
                FilePriority::High => Some(BrowserFilePriority::High),
                FilePriority::Skip => Some(BrowserFilePriority::Skip),
                FilePriority::Normal | FilePriority::Mixed => None,
            })
    }

    pub fn default_download_folder(&self) -> Option<&PathBuf> {
        self.app.client_configs.default_download_folder.as_ref()
    }

    pub fn file_browser_current_path(&self) -> &PathBuf {
        &self.app.app_state.ui.file_browser.state.current_path
    }

    pub fn delete_confirmation(&self) -> Option<(&[u8], bool)> {
        matches!(self.app.app_state.mode, AppMode::DeleteConfirm).then_some((
            self.app.app_state.ui.delete_confirm.info_hash.as_slice(),
            self.app.app_state.ui.delete_confirm.with_files,
        ))
    }

    pub fn torrent_count(&self) -> usize {
        self.app.app_state.torrents.len()
    }

    pub fn anonymize_names(&self) -> bool {
        self.app.app_state.anonymize_torrent_names
    }

    pub fn selected_torrent_hash_hex(&self) -> Option<String> {
        self.app
            .app_state
            .torrent_list_order
            .get(self.app.app_state.ui.selected_torrent_index)
            .map(hex::encode)
    }

    pub fn torrent_management_cursor_hash_hex(&self) -> Option<String> {
        self.app
            .app_state
            .ui
            .torrent_management
            .cursor_hash
            .as_deref()
            .map(hex::encode)
    }
}
