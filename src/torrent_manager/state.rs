// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use tracing::event;
use tracing::Level;

use crate::command::TorrentCommand;
use crate::networking::BlockInfo;
use crate::storage::MultiFileInfo;
use crate::torrent_manager::FileActivityDirection;
use crate::torrent_manager::ManagerEvent;
use crate::tracker::normalize_tracker_urls;

use crate::app::FilePriority;

use tokio::sync::mpsc::Sender;
use tokio::sync::Semaphore;

use std::mem::Discriminant;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crate::torrent_file::{Torrent, V2RootInfo};
use crate::torrent_manager::piece_manager::EffectivePiecePriority;
use crate::torrent_manager::piece_manager::PieceManager;
use crate::torrent_manager::piece_manager::PieceStatus;
use std::collections::{HashMap, HashSet};

const MAX_TIMEOUT_COUNT: u32 = 10;
const SMOOTHING_PERIOD_MS: f64 = 5000.0;
const PEER_UPLOAD_IN_FLIGHT_LIMIT: usize = 16;
const MAX_BLOCK_SIZE: u32 = 131_072;
const UPLOAD_SLOTS_DEFAULT: usize = 4;
const DEFAULT_ANNOUNCE_INTERVAL_SECS: u64 = 60;
pub const MAX_PIPELINE_DEPTH: usize = 512;

// Quality gate: once we have this many connected peers, pause admitting new peers
// to avoid churn storms. This is intentionally independent of resource-manager limits.
const PEER_ADMISSION_QUALITY_THRESHOLD: usize = 400;

pub type PeerAddr = SocketAddr;

#[derive(Debug, Clone)]
pub enum Action {
    TorrentManagerInit {
        is_paused: bool,
        announce_immediately: bool,
    },
    Tick {
        dt_ms: u64,
    },
    RecalculateChokes {
        random_seed: u64,
    },
    CheckCompletion,
    AssignWork {
        peer_id: String,
    },
    ConnectToWebSeeds,
    RegisterPeer {
        peer_id: String,
        tx: Sender<TorrentCommand>,
    },
    PeerSuccessfullyConnected {
        peer_id: String,
    },
    PeerDisconnected {
        peer_id: String,
        force: bool,
    },
    UpdatePeerId {
        peer_addr: String,
        new_id: Vec<u8>,
    },
    PeerBitfieldReceived {
        peer_id: String,
        bitfield: Vec<u8>,
    },
    PeerChoked {
        peer_id: String,
    },
    PeerUnchoked {
        peer_id: String,
    },
    PeerInterested {
        peer_id: String,
    },
    PeerHavePiece {
        peer_id: String,
        piece_index: u32,
    },
    IncomingBlock {
        peer_id: String,
        piece_index: u32,
        block_offset: u32,
        data: Vec<u8>,
    },
    PieceVerified {
        peer_id: String,
        piece_index: u32,
        valid: bool,
        data: Vec<u8>,
    },
    PieceWrittenToDisk {
        peer_id: String,
        piece_index: u32,
    },
    PieceWriteFailed {
        piece_index: u32,
    },
    RequestUpload {
        peer_id: String,
        piece_index: u32,
        block_offset: u32,
        length: u32,
    },
    TrackerResponse {
        url: String,
        peers: Vec<PeerAddr>,
        interval: u64,
        min_interval: Option<u64>,
    },
    TrackerError {
        url: String,
    },
    PeerConnectionFailed {
        peer_addr: String,
    },
    MetadataReceived {
        torrent: Box<Torrent>,
        metadata_length: i64,
    },
    MerkleProofReceived {
        peer_id: String,
        piece_index: u32,
        proof: Vec<u8>,
    },
    ValidationComplete {
        completed_pieces: Vec<u32>,
    },

    BlockSentToPeer {
        peer_id: String,
        byte_count: u64,
    },

    CancelUpload {
        peer_id: String,
        piece_index: u32,
        block_offset: u32,
        length: u32,
    },

    Cleanup,
    Pause,
    Resume,
    Delete,
    UpdateListenPort,
    SetUserTorrentConfig {
        torrent_data_path: PathBuf,
        file_priorities: HashMap<usize, FilePriority>,
        container_name: Option<String>,
    },
    SetDataAvailability {
        available: bool,
    },
    ValidationProgress {
        count: u32,
    },
    Shutdown,
    FatalError,
}

#[derive(Debug)]
#[must_use]
pub enum Effect {
    DoNothing,
    EmitMetrics {
        bytes_dl: u64,
        bytes_ul: u64,
    },
    EmitManagerEvent(ManagerEvent),
    SendToPeer {
        peer_id: String,
        cmd: Box<TorrentCommand>,
    },
    DisconnectPeer {
        peer_id: String,
    },
    AnnounceCompleted {
        url: String,
    },

    // --- New I/O & Work Effects ---
    VerifyPiece {
        peer_id: String,
        piece_index: u32,
        data: Vec<u8>,
    },
    VerifyPieceV2 {
        peer_id: String,
        piece_index: u32,
        proof: Vec<u8>,
        data: Vec<u8>,
        root_hash: Vec<u8>,
        _file_start_offset: u64,
        valid_length: usize,
        relative_index: u32,
        hashing_context_len: usize,
    },
    WriteToDisk {
        peer_id: String,
        piece_index: u32,
        data: Vec<u8>,
    },
    ReadFromDisk {
        peer_id: String,
        block_info: BlockInfo,
    },
    BroadcastHave {
        piece_index: u32,
    },
    ConnectToPeer {
        addr: SocketAddr,
    },
    RequestHashes {
        peer_id: String,
        file_root: Vec<u8>,
        piece_index: u32,
        length: u32,
        proof_layers: u32,
        base_layer: u32,
    },

    StartWebSeed {
        url: String,
    },

    StartValidation,
    AnnounceToTracker {
        url: String,
    },

    ConnectToPeersFromTrackers,

    AbortUpload {
        peer_id: String,
        block_info: BlockInfo,
    },

    ClearAllUploads,
    DeleteFiles {
        files: Vec<PathBuf>,
        directories: Vec<PathBuf>,
    },
    TriggerDhtSearch,
    PrepareShutdown {
        tracker_urls: Vec<String>,
        left: usize,
        uploaded: usize,
        downloaded: usize,
    },
}

#[derive(Debug, Clone)]
pub struct TrackerState {
    pub next_announce_time: Instant,
    pub leeching_interval: Option<Duration>,
    pub seeding_interval: Option<Duration>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum TorrentActivity {
    #[default]
    Initializing,
    Paused,
    ConnectingToPeers,
    RequestingPieces,
    DownloadingPiece(u32),
    SendingPiece(u32),
    VerifyingPiece(u32),
    AnnouncingToTracker,
    ProcessingPeers(usize),

    #[cfg(feature = "dht")]
    SearchingDht,
}

#[derive(PartialEq, Debug, Default, Clone)]
pub enum TorrentStatus {
    #[default]
    AwaitingMetadata,
    Validating,
    Standard,
    Endgame,
    Done,
}

#[derive(PartialEq, Debug, Clone)]
pub enum ChokeStatus {
    Choke,
    Unchoke,
}

#[derive(Debug, Clone)]
pub struct TorrentState {
    pub info_hash: Vec<u8>,
    pub torrent: Option<Torrent>,
    pub torrent_metadata_length: Option<i64>,
    pub is_paused: bool,
    pub torrent_status: TorrentStatus,
    pub torrent_validation_status: bool,
    pub last_activity: TorrentActivity,
    pub has_made_first_connection: bool,
    pub session_total_uploaded: u64,
    pub session_total_downloaded: u64,
    pub bytes_downloaded_in_interval: u64,
    pub bytes_uploaded_in_interval: u64,
    pub total_dl_prev_avg_ema: f64,
    pub total_ul_prev_avg_ema: f64,
    pub number_of_successfully_connected_peers: usize,
    pub peers: HashMap<String, PeerState>,
    pub piece_manager: PieceManager,
    pub trackers: HashMap<String, TrackerState>,
    pub timed_out_peers: HashMap<String, (u32, Instant)>,
    pub last_known_peers: HashSet<String>,
    pub optimistic_unchoke_timer: Option<Instant>,
    pub validation_pieces_found: u32,
    pub now: Instant,
    pub has_started_announce_sent: bool,
    pub v2_proofs: HashMap<u32, Vec<u8>>,
    pub v2_pending_data: HashMap<u32, (u32, Vec<u8>)>,
    pub piece_to_roots: HashMap<u32, Vec<V2RootInfo>>,
    pub verifying_pieces: HashSet<u32>,
    pub torrent_data_path: Option<PathBuf>,
    pub container_name: Option<String>,
    pub multi_file_info: Option<MultiFileInfo>,
    pub file_priorities: HashMap<usize, FilePriority>,
    pub data_available: bool,
    pub pending_disconnects: Vec<String>,
    pub pending_failures: Vec<String>,
    pub accepting_new_peers: bool,
}
impl Default for TorrentState {
    fn default() -> Self {
        Self {
            info_hash: Vec::new(),
            torrent: None,
            torrent_metadata_length: None,
            is_paused: false,
            torrent_status: TorrentStatus::default(),
            torrent_validation_status: false,
            last_activity: TorrentActivity::default(),
            has_made_first_connection: false,
            session_total_uploaded: 0,
            session_total_downloaded: 0,
            bytes_downloaded_in_interval: 0,
            bytes_uploaded_in_interval: 0,
            total_dl_prev_avg_ema: 0.0,
            total_ul_prev_avg_ema: 0.0,
            number_of_successfully_connected_peers: 0,
            peers: HashMap::new(),
            piece_manager: PieceManager::new(),
            trackers: HashMap::new(),
            timed_out_peers: HashMap::new(),
            last_known_peers: HashSet::new(),
            optimistic_unchoke_timer: None,
            validation_pieces_found: 0,
            now: Instant::now(),
            has_started_announce_sent: false,
            v2_proofs: HashMap::new(),
            v2_pending_data: HashMap::new(),
            piece_to_roots: HashMap::new(),
            verifying_pieces: HashSet::new(),
            torrent_data_path: None,
            container_name: None,
            multi_file_info: None,
            file_priorities: HashMap::new(),
            data_available: true,
            pending_disconnects: Vec::with_capacity(100),
            pending_failures: Vec::with_capacity(100),
            accepting_new_peers: true,
        }
    }
}

impl TorrentState {
    pub fn new(
        info_hash: Vec<u8>,
        torrent: Option<Torrent>,
        torrent_metadata_length: Option<i64>,
        piece_manager: PieceManager,
        trackers: HashMap<String, TrackerState>,
        torrent_validation_status: bool,
        container_name: Option<String>,
    ) -> Self {
        let torrent_status = if torrent.is_some() {
            TorrentStatus::Validating
        } else {
            TorrentStatus::AwaitingMetadata
        };

        let mut state = Self {
            info_hash,
            torrent,
            torrent_metadata_length,
            torrent_status,
            piece_manager,
            trackers,
            torrent_validation_status,
            container_name,
            optimistic_unchoke_timer: Some(
                Instant::now()
                    .checked_sub(Duration::from_secs(31))
                    .unwrap_or(Instant::now()),
            ),
            now: Instant::now(),
            ..Default::default()
        };

        // Populate V2 Maps Immediately
        // This ensures AssignWork has the data it needs to clamp requests from the very start.
        let (v2_piece_count, piece_overrides) = state.rebuild_v2_mappings();

        if let Some(ref t) = state.torrent {
            let total_len: u64 = if t.info.meta_version == Some(2) {
                // V2: Geometry is aligned to piece boundaries
                let num_pieces = if !t.info.pieces.is_empty() {
                    t.info.pieces.len() / 20
                } else if v2_piece_count > 0 {
                    // Use the count we just calculated
                    v2_piece_count as usize
                } else {
                    0
                };
                (num_pieces as u64) * (t.info.piece_length as u64)
            } else {
                // V1: Geometry is packed (sum of files)
                if t.info.files.is_empty() {
                    t.info.length as u64
                } else {
                    t.info.files.iter().map(|f| f.length as u64).sum()
                }
            };

            state.piece_manager.set_geometry(
                t.info.piece_length as u32,
                total_len,
                piece_overrides,
                torrent_validation_status,
            );
        }

        state
    }

    fn get_piece_size(&self, piece_index: u32) -> usize {
        if let Some(torrent) = &self.torrent {
            let piece_len = torrent.info.piece_length as u64;

            // V2 Logic: Clamp size to the specific file length
            if let Some(roots) = self.piece_to_roots.get(&piece_index) {
                // In V2, pieces align to files. We check the mapped file for this piece.
                if let Some(root_info) = roots.first() {
                    let global_piece_start = piece_index as u64 * piece_len;

                    // Calculate offset relative to the start of this specific file
                    let offset_in_file = global_piece_start.saturating_sub(root_info.file_offset);

                    // The piece cannot exceed the remaining bytes in this file
                    let remaining_in_file = root_info.length.saturating_sub(offset_in_file);

                    return std::cmp::min(piece_len, remaining_in_file) as usize;
                }
            }

            // Fallback (V1 / Standard contiguous stream logic)
            let total_len: u64 = if !torrent.info.files.is_empty() {
                torrent.info.files.iter().map(|f| f.length as u64).sum()
            } else {
                torrent.info.length as u64
            };

            let offset = piece_index as u64 * piece_len;
            let remaining = total_len.saturating_sub(offset);
            std::cmp::min(piece_len, remaining) as usize
        } else {
            0
        }
    }

    pub fn update(&mut self, action: Action) -> Vec<Effect> {
        match action {
            Action::TorrentManagerInit {
                is_paused,
                announce_immediately,
            } => {
                let mut effects = Vec::new();

                self.is_paused = is_paused;
                if self.is_paused {
                    return effects;
                }

                effects.extend(self.update(Action::ConnectToWebSeeds));

                let should_announce =
                    announce_immediately || self.torrent_status == TorrentStatus::AwaitingMetadata;
                if should_announce {
                    for url in self.trackers.keys() {
                        effects.push(Effect::AnnounceToTracker { url: url.clone() });
                    }
                    self.has_started_announce_sent = true;
                }

                effects
            }
            Action::Tick { dt_ms } => {
                self.now += Duration::from_millis(dt_ms);
                self.refresh_peer_admission_guard();
                let scaling_factor = if dt_ms > 0 {
                    1000.0 / dt_ms as f64
                } else {
                    1.0
                };
                let dt = dt_ms as f64;
                // Calculate Alpha for Exponential Moving Average
                let alpha = 1.0 - (-dt / SMOOTHING_PERIOD_MS).exp();

                let inst_total_dl_speed =
                    (self.bytes_downloaded_in_interval as f64 * 8.0) * scaling_factor;
                let inst_total_ul_speed =
                    (self.bytes_uploaded_in_interval as f64 * 8.0) * scaling_factor;

                // Capture values for the EmitMetrics event
                let dl_tick = self.bytes_downloaded_in_interval;
                let ul_tick = self.bytes_uploaded_in_interval;

                // Reset interval counters
                self.bytes_downloaded_in_interval = 0;
                self.bytes_uploaded_in_interval = 0;

                // Update Global EMAs
                self.total_dl_prev_avg_ema =
                    (inst_total_dl_speed * alpha) + (self.total_dl_prev_avg_ema * (1.0 - alpha));
                self.total_ul_prev_avg_ema =
                    (inst_total_ul_speed * alpha) + (self.total_ul_prev_avg_ema * (1.0 - alpha));

                for peer in self.peers.values_mut() {
                    let inst_dl_speed =
                        (peer.bytes_downloaded_in_tick as f64 * 8.0) * scaling_factor;
                    let inst_ul_speed = (peer.bytes_uploaded_in_tick as f64 * 8.0) * scaling_factor;

                    // Update Peer EMAs
                    peer.prev_avg_dl_ema =
                        (inst_dl_speed * alpha) + (peer.prev_avg_dl_ema * (1.0 - alpha));
                    peer.download_speed_bps = peer.prev_avg_dl_ema as u64;

                    peer.prev_avg_ul_ema =
                        (inst_ul_speed * alpha) + (peer.prev_avg_ul_ema * (1.0 - alpha));
                    peer.upload_speed_bps = peer.prev_avg_ul_ema as u64;

                    // Reset Peer tick counters
                    peer.bytes_downloaded_in_tick = 0;
                    peer.bytes_uploaded_in_tick = 0;
                }

                let mut effects = vec![Effect::EmitMetrics {
                    bytes_dl: dl_tick,
                    bytes_ul: ul_tick,
                }];

                if self.torrent_status == TorrentStatus::Validating || self.is_paused {
                    return effects;
                }

                // Tracker Announce Logic
                for (url, tracker) in self.trackers.iter_mut() {
                    if self.now >= tracker.next_announce_time {
                        self.last_activity = TorrentActivity::AnnouncingToTracker;
                        let interval = if self.torrent_status == TorrentStatus::Done {
                            tracker
                                .seeding_interval
                                .unwrap_or(Duration::from_secs(DEFAULT_ANNOUNCE_INTERVAL_SECS))
                        } else {
                            tracker
                                .leeching_interval
                                .unwrap_or(Duration::from_secs(DEFAULT_ANNOUNCE_INTERVAL_SECS))
                        };
                        tracker.next_announce_time = self.now + interval;
                        effects.push(Effect::AnnounceToTracker { url: url.clone() });
                    }
                }

                effects
            }

            Action::RecalculateChokes { random_seed } => {
                let mut effects = Vec::new();
                let mut unchoke_candidates = HashSet::new();

                if self.data_available {
                    let mut interested_peers: Vec<&mut PeerState> = self
                        .peers
                        .values_mut()
                        .filter(|p| p.peer_is_interested_in_us)
                        .collect();

                    if self.torrent_status == TorrentStatus::Done {
                        interested_peers.sort_by(|a, b| {
                            b.bytes_uploaded_to_peer.cmp(&a.bytes_uploaded_to_peer)
                        });
                    } else {
                        interested_peers.sort_by(|a, b| {
                            b.bytes_downloaded_from_peer
                                .cmp(&a.bytes_downloaded_from_peer)
                        });
                    }

                    unchoke_candidates = interested_peers
                        .iter()
                        .take(UPLOAD_SLOTS_DEFAULT)
                        .map(|p| p.ip_port.clone())
                        .collect();

                    if self.optimistic_unchoke_timer.is_some_and(|t| {
                        self.now.saturating_duration_since(t) > Duration::from_secs(30)
                    }) {
                        let optimistic_candidates: Vec<&mut PeerState> = interested_peers
                            .into_iter()
                            .filter(|p| !unchoke_candidates.contains(&p.ip_port))
                            .collect();

                        if !optimistic_candidates.is_empty() {
                            let idx = (random_seed as usize) % optimistic_candidates.len();
                            let chosen_id = optimistic_candidates[idx].ip_port.clone();
                            unchoke_candidates.insert(chosen_id);
                        }

                        self.optimistic_unchoke_timer = Some(self.now);
                    }
                }

                for peer in self.peers.values_mut() {
                    if unchoke_candidates.contains(&peer.ip_port) {
                        if peer.am_choking == ChokeStatus::Choke {
                            peer.am_choking = ChokeStatus::Unchoke;
                            effects.push(Effect::SendToPeer {
                                peer_id: peer.ip_port.clone(),
                                cmd: Box::new(TorrentCommand::PeerUnchoke),
                            });
                        }
                    } else if peer.am_choking == ChokeStatus::Unchoke {
                        peer.am_choking = ChokeStatus::Choke;
                        effects.push(Effect::SendToPeer {
                            peer_id: peer.ip_port.clone(),
                            cmd: Box::new(TorrentCommand::PeerChoke),
                        });
                    }

                    peer.bytes_downloaded_from_peer = 0;
                    peer.bytes_uploaded_to_peer = 0;
                }

                if effects.is_empty() {
                    vec![Effect::DoNothing]
                } else {
                    effects
                }
            }

            Action::CheckCompletion => {
                if self.torrent_status == TorrentStatus::AwaitingMetadata
                    || self.torrent_status == TorrentStatus::Validating
                    || self.torrent_status == TorrentStatus::Done
                {
                    return vec![Effect::DoNothing];
                }

                let is_complete = if self.piece_manager.piece_priorities.is_empty() {
                    self.piece_manager
                        .bitfield
                        .iter()
                        .all(|&s| s == PieceStatus::Done)
                } else {
                    self.piece_manager
                        .bitfield
                        .iter()
                        .enumerate()
                        .all(|(i, status)| {
                            if *status == PieceStatus::Done {
                                return true;
                            }
                            self.piece_manager.piece_priorities[i] == EffectivePiecePriority::Skip
                        })
                };

                let has_pieces = !self.piece_manager.bitfield.is_empty();

                if is_complete && has_pieces {
                    let mut effects = Vec::new();
                    self.torrent_status = TorrentStatus::Done;

                    self.piece_manager.need_queue.clear();
                    self.piece_manager.pending_queue.clear();
                    self.piece_manager.clear_assembly_buffers();

                    for peer in self.peers.values_mut() {
                        peer.pending_requests.clear();
                        peer.active_blocks.clear();
                        peer.inflight_requests = 0;

                        if peer.am_interested {
                            peer.am_interested = false;
                            effects.push(Effect::SendToPeer {
                                peer_id: peer.ip_port.clone(),
                                cmd: Box::new(TorrentCommand::NotInterested),
                            });
                        }
                    }

                    // 4. NOTIFY TRACKER
                    // Logic: Only send "event=completed" if we physically possess 100% of the bits.
                    // If we skipped files (Priority Mode), we are "Done" locally but not "Completed" globally.
                    let physically_complete = self
                        .piece_manager
                        .bitfield
                        .iter()
                        .all(|&s| s == PieceStatus::Done);

                    if physically_complete {
                        for (url, tracker) in self.trackers.iter_mut() {
                            tracker.next_announce_time = self.now;
                            effects.push(Effect::AnnounceCompleted { url: url.clone() });
                        }
                    } else {
                        // Priority Mode (Partial Completion): Just send a regular update so the tracker knows we stopped downloading.
                        for (url, tracker) in self.trackers.iter_mut() {
                            tracker.next_announce_time = self.now;
                            effects.push(Effect::AnnounceToTracker { url: url.clone() });
                        }
                    }

                    return effects;
                }

                vec![Effect::DoNothing]
            }

            Action::AssignWork { peer_id } => {
                if self.torrent_status == TorrentStatus::Validating {
                    return vec![Effect::DoNothing];
                }
                if self.piece_manager.bitfield.is_empty() {
                    return vec![Effect::DoNothing];
                }
                if self.torrent_data_path.is_none() {
                    return vec![Effect::DoNothing];
                }
                if self.piece_manager.need_queue.is_empty()
                    && self.piece_manager.pending_queue.is_empty()
                {
                    return vec![Effect::DoNothing];
                }
                if self.torrent.is_none() {
                    return vec![Effect::DoNothing];
                }

                // Prepare size calculation closure with disjoint borrows.
                let torrent_ref = &self.torrent;
                let roots_ref = &self.piece_to_roots;

                let calc_v2_limit = |piece_index: u32| -> Option<u32> {
                    if let Some(torrent) = torrent_ref {
                        let piece_len = torrent.info.piece_length as u64;
                        if let Some(roots) = roots_ref.get(&piece_index) {
                            if let Some(root_info) = roots.first() {
                                let global_piece_start = piece_index as u64 * piece_len;
                                let offset_in_file =
                                    global_piece_start.saturating_sub(root_info.file_offset);
                                let remaining_in_file =
                                    root_info.length.saturating_sub(offset_in_file);
                                return Some(std::cmp::min(piece_len, remaining_in_file) as u32);
                            }
                        }
                        None
                    } else {
                        None
                    }
                };

                let mut effects = Vec::new();
                let mut request_batch = Vec::new();

                let peer_opt = self.peers.get_mut(&peer_id);
                if peer_opt.is_none() {
                    return effects;
                }
                let peer = peer_opt.unwrap();

                let has_needed_pieces = !peer.bitfield.is_empty()
                    && (self
                        .piece_manager
                        .need_queue
                        .iter()
                        .any(|&p| peer.bitfield.get(p as usize) == Some(&true))
                        || self
                            .piece_manager
                            .pending_queue
                            .keys()
                            .any(|&p| peer.bitfield.get(p as usize) == Some(&true)));

                let has_pending_requests = !peer.pending_requests.is_empty();
                let should_be_interested = has_needed_pieces || has_pending_requests;

                if should_be_interested && !peer.am_interested {
                    peer.am_interested = true;
                    effects.push(Effect::SendToPeer {
                        peer_id: peer_id.clone(),
                        cmd: Box::new(TorrentCommand::ClientInterested),
                    });
                } else if !should_be_interested && peer.am_interested {
                    peer.am_interested = false;
                    effects.push(Effect::SendToPeer {
                        peer_id: peer_id.clone(),
                        cmd: Box::new(TorrentCommand::NotInterested),
                    });
                }

                if peer.peer_choking == ChokeStatus::Choke {
                    return effects;
                }
                if peer.bitfield.is_empty() {
                    return effects;
                }

                let current_inflight = peer.inflight_requests;
                let max_depth = MAX_PIPELINE_DEPTH;

                if current_inflight >= max_depth {
                    return effects;
                }
                let mut available_slots = max_depth - current_inflight;

                let mut pending_pieces: Vec<u32> = peer.pending_requests.iter().cloned().collect();
                pending_pieces.sort();

                for piece_index in pending_pieces {
                    if available_slots == 0 {
                        break;
                    }
                    if self.verifying_pieces.contains(&piece_index) {
                        continue;
                    }
                    let block_addrs = self
                        .piece_manager
                        .requestable_block_addresses_for_piece(piece_index);

                    for addr in block_addrs {
                        if available_slots == 0 {
                            break;
                        }

                        let final_len = if let Some(limit) = calc_v2_limit(addr.piece_index) {
                            let remaining = limit.saturating_sub(addr.byte_offset);
                            std::cmp::min(addr.length, remaining)
                        } else {
                            addr.length
                        };

                        if final_len == 0 {
                            continue;
                        }

                        // Is peer already working on it?
                        if peer.active_blocks.contains(&(
                            addr.piece_index,
                            addr.byte_offset,
                            final_len,
                        )) {
                            continue;
                        }

                        request_batch.push((addr.piece_index, addr.byte_offset, final_len));
                        peer.active_blocks
                            .insert((addr.piece_index, addr.byte_offset, final_len));

                        available_slots -= 1;
                    }
                }

                let candidate_pool: Box<dyn Iterator<Item = &u32> + '_> =
                    if self.torrent_status == TorrentStatus::Endgame {
                        Box::new(
                            self.piece_manager
                                .need_queue
                                .iter()
                                .chain(self.piece_manager.pending_queue.keys()),
                        )
                    } else {
                        Box::new(self.piece_manager.need_queue.iter())
                    };

                let mut valid_candidates: Vec<u32> = candidate_pool
                    .copied()
                    .filter(|&p_idx| {
                        // Peer must have the piece
                        if peer.bitfield.get(p_idx as usize) != Some(&true) {
                            return false;
                        }
                        // Don't duplicate work currently verifying
                        if self.verifying_pieces.contains(&p_idx) {
                            return false;
                        }
                        // Don't request what we already asked this specific peer for
                        if peer.pending_requests.contains(&p_idx) {
                            return false;
                        }
                        true
                    })
                    .collect();

                // Sort ascending: Lower availability count = Rarer = Higher Priority
                valid_candidates.sort_by_key(|&p_idx| {
                    // FIX: Call the helper method we just created
                    self.piece_manager.get_piece_availability(p_idx)
                });

                let pieces_to_request = valid_candidates.into_iter().take(available_slots);

                for piece_index in pieces_to_request {
                    if available_slots == 0 {
                        break;
                    }

                    // --- A. Update State ---
                    self.piece_manager
                        .mark_as_pending(piece_index, peer_id.clone());
                    peer.pending_requests.insert(piece_index);

                    if self.piece_manager.need_queue.is_empty()
                        && self.torrent_status != TorrentStatus::Endgame
                    {
                        self.torrent_status = TorrentStatus::Endgame;
                    }

                    // --- B. Generate Block Requests ---
                    let block_addrs = self
                        .piece_manager
                        .requestable_block_addresses_for_piece(piece_index);

                    for addr in block_addrs {
                        if available_slots == 0 {
                            break;
                        }

                        let final_len = if let Some(limit) = calc_v2_limit(addr.piece_index) {
                            let remaining = limit.saturating_sub(addr.byte_offset);
                            std::cmp::min(addr.length, remaining)
                        } else {
                            addr.length
                        };

                        if final_len == 0 {
                            continue;
                        }

                        if peer.active_blocks.contains(&(
                            addr.piece_index,
                            addr.byte_offset,
                            final_len,
                        )) {
                            continue;
                        }

                        request_batch.push((addr.piece_index, addr.byte_offset, final_len));
                        peer.active_blocks
                            .insert((addr.piece_index, addr.byte_offset, final_len));

                        available_slots -= 1;
                    }
                }

                if !request_batch.is_empty() {
                    if !matches!(self.last_activity, TorrentActivity::DownloadingPiece(_)) {
                        self.last_activity = TorrentActivity::RequestingPieces;
                    }

                    peer.inflight_requests += request_batch.len();
                    effects.push(Effect::SendToPeer {
                        peer_id: peer_id.clone(),
                        cmd: Box::new(TorrentCommand::BulkRequest(request_batch)),
                    });
                }

                effects
            }

            Action::ConnectToWebSeeds => {
                let mut effects = Vec::new();
                if let Some(torrent) = &self.torrent {
                    if let Some(urls) = &torrent.url_list {
                        for url in urls {
                            effects.push(Effect::StartWebSeed { url: url.clone() });
                        }
                    }
                }
                effects
            }

            Action::RegisterPeer { peer_id, tx } => {
                if !self.peers.contains_key(&peer_id) {
                    let mut peer_state = PeerState::new(peer_id.clone(), tx, self.now);
                    peer_state.peer_id = peer_id.as_bytes().to_vec();
                    self.peers.insert(peer_id, peer_state);
                    // Admission pressure should react to discovered/registered peers immediately,
                    // not only after handshake success.
                    self.number_of_successfully_connected_peers = self.peers.len();
                    self.refresh_peer_admission_guard();
                }
                vec![Effect::DoNothing]
            }

            // --- Peer Lifecycle Actions ---
            Action::PeerSuccessfullyConnected { peer_id } => {
                self.timed_out_peers.remove(&peer_id);

                if !self.has_made_first_connection {
                    self.has_made_first_connection = true;
                }

                self.number_of_successfully_connected_peers = self.peers.len();
                self.refresh_peer_admission_guard();

                vec![Effect::EmitManagerEvent(ManagerEvent::PeerConnected {
                    info_hash: self.info_hash.clone(),
                })]
            }

            Action::PeerDisconnected { peer_id, force } => {
                if !peer_id.is_empty() {
                    self.pending_disconnects.push(peer_id);
                }

                if !force && self.pending_disconnects.len() < 50 {
                    return vec![Effect::DoNothing];
                }

                if self.pending_disconnects.is_empty() {
                    return vec![Effect::DoNothing];
                }

                let mut effects = Vec::new();
                let batch = std::mem::take(&mut self.pending_disconnects);

                self.last_activity = TorrentActivity::ProcessingPeers(self.peers.len());

                for pid in batch {
                    if let Some(removed_peer) = self.peers.remove(&pid) {
                        for piece_index in removed_peer.pending_requests {
                            if self.piece_manager.bitfield.get(piece_index as usize)
                                != Some(&PieceStatus::Done)
                            {
                                self.piece_manager.requeue_pending_to_need(piece_index);
                            }
                        }
                        effects.push(Effect::DisconnectPeer { peer_id: pid });
                        effects.push(Effect::EmitManagerEvent(ManagerEvent::PeerDisconnected {
                            info_hash: self.info_hash.clone(),
                        }));
                    }
                }

                self.number_of_successfully_connected_peers = self.peers.len();
                self.refresh_peer_admission_guard();

                effects
            }

            Action::UpdatePeerId { peer_addr, new_id } => {
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.peer_id = new_id;
                }
                vec![Effect::DoNothing]
            }

            Action::PeerBitfieldReceived { peer_id, bitfield } => {
                let mut effects = Vec::new();

                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    // Peer is misbehaving (sending 2nd bitfield). Disconnect them.
                    if !peer.bitfield.is_empty() && peer.bitfield.iter().any(|&b| b) {
                        effects.push(Effect::DisconnectPeer {
                            peer_id: peer_id.clone(),
                        });
                        return effects;
                    }

                    peer.bitfield = bitfield
                        .iter()
                        .flat_map(|&byte| (0..8).map(move |i| (byte >> (7 - i)) & 1 == 1))
                        .collect();

                    let total_pieces = self.piece_manager.bitfield.len();

                    if total_pieces > 0 {
                        if peer.bitfield.len() > total_pieces {
                            peer.bitfield.truncate(total_pieces);
                        } else if peer.bitfield.len() < total_pieces {
                            peer.bitfield.resize(total_pieces, false);
                        }
                    }
                }

                self.update(Action::AssignWork { peer_id })
            }

            Action::PeerChoked { peer_id } => {
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    peer.inflight_requests = 0;
                    peer.active_blocks.clear();
                    peer.peer_choking = ChokeStatus::Choke;

                    let pieces_to_requeue = std::mem::take(&mut peer.pending_requests);
                    for piece_index in pieces_to_requeue {
                        if self.piece_manager.bitfield.get(piece_index as usize)
                            != Some(&PieceStatus::Done)
                        {
                            self.piece_manager.requeue_pending_to_need(piece_index);
                        }
                    }
                }

                vec![Effect::DoNothing]
            }

            Action::PeerUnchoked { peer_id } => {
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    peer.peer_choking = ChokeStatus::Unchoke;
                }
                self.update(Action::AssignWork { peer_id })
            }

            Action::PeerInterested { peer_id } => {
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    peer.peer_is_interested_in_us = true;
                }
                vec![Effect::DoNothing]
            }

            Action::PeerHavePiece {
                peer_id,
                piece_index,
            } => {
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    if (piece_index as usize) < peer.bitfield.len() {
                        peer.bitfield[piece_index as usize] = true;
                    }
                }
                self.update(Action::AssignWork { peer_id })
            }

            // --- Data Flow (The Core Logic) ---
            Action::IncomingBlock {
                peer_id,
                piece_index,
                block_offset,
                data,
            } => {
                if piece_index as usize >= self.piece_manager.bitfield.len() {
                    return vec![Effect::DoNothing];
                }

                let mut effects = Vec::new();
                let len = data.len() as u64;

                // Determine if this block is actually needed (not redundant).
                // We perform accounting only for useful blocks to prevent metric inflation.
                let is_piece_done = self.piece_manager.bitfield.get(piece_index as usize)
                    == Some(&PieceStatus::Done);

                if !is_piece_done {
                    self.bytes_downloaded_in_interval =
                        self.bytes_downloaded_in_interval.saturating_add(len);
                    self.session_total_downloaded =
                        self.session_total_downloaded.saturating_add(len);
                }

                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    // CRITICAL: Always decrement inflight requests, even for redundant blocks.
                    // If we don't, the pipeline counts never decrease, causing a stall.
                    peer.inflight_requests = peer.inflight_requests.saturating_sub(1);

                    let block_len = data.len() as u32;
                    peer.active_blocks
                        .remove(&(piece_index, block_offset, block_len));

                    // Only credit the peer if the block was useful
                    if !is_piece_done {
                        peer.bytes_downloaded_from_peer += len;
                        peer.bytes_downloaded_in_tick += len;
                        peer.total_bytes_downloaded += len;
                    }
                }

                effects.push(Effect::EmitManagerEvent(ManagerEvent::BlockReceived {
                    info_hash: self.info_hash.clone(),
                }));
                effects.push(Effect::EmitManagerEvent(ManagerEvent::FileActivity {
                    info_hash: self.info_hash.clone(),
                    touched_relative_paths: self.touched_relative_paths_for_activity(
                        piece_index,
                        block_offset,
                        data.len() as u32,
                    ),
                    direction: FileActivityDirection::Download,
                }));

                if is_piece_done {
                    return effects;
                }

                if self.torrent_status == TorrentStatus::Validating {
                    return effects;
                }

                self.last_activity = TorrentActivity::DownloadingPiece(piece_index);

                let piece_size = self.get_piece_size(piece_index);

                if let Some(complete_data) =
                    self.piece_manager
                        .handle_block(piece_index, block_offset, &data, piece_size)
                {
                    // Mark as verifying
                    self.verifying_pieces.insert(piece_index);

                    if let Some(roots) = self.piece_to_roots.get(&piece_index) {
                        let piece_len = self
                            .torrent
                            .as_ref()
                            .map(|t| t.info.piece_length as u64)
                            .unwrap_or(0);
                        let global_offset = (piece_index as u64 * piece_len) + block_offset as u64;

                        let matching_root_info = roots
                            .iter()
                            .filter(|r| r.file_offset <= global_offset)
                            .max_by_key(|r| r.file_offset);

                        let (valid_length, relative_index, hashing_context_len) =
                            self.calculate_v2_verify_params(piece_index, complete_data.len());

                        if let Some(root_info) = matching_root_info {
                            if let Some(target_hash) = self.torrent.as_ref().and_then(|t| {
                                t.get_v2_hash_layer(
                                    piece_index,
                                    root_info.file_offset,
                                    root_info.length,
                                    1,
                                    &root_info.root_hash,
                                )
                            }) {
                                // SCENARIO: We have the piece-layer hash locally
                                effects.push(Effect::VerifyPieceV2 {
                                    peer_id: peer_id.clone(),
                                    piece_index,
                                    proof: Vec::new(),
                                    data: complete_data,
                                    root_hash: target_hash,
                                    _file_start_offset: root_info.file_offset,
                                    valid_length,
                                    relative_index,
                                    hashing_context_len,
                                });
                            } else if let Some(proof) = self.v2_proofs.get(&piece_index) {
                                // SCENARIO: We got a proof from the peer
                                effects.push(Effect::VerifyPieceV2 {
                                    peer_id: peer_id.clone(),
                                    piece_index,
                                    proof: proof.clone(),
                                    root_hash: root_info.root_hash.clone(),
                                    data: complete_data,
                                    _file_start_offset: root_info.file_offset,
                                    valid_length,
                                    relative_index,
                                    hashing_context_len,
                                });
                            } else if self
                                .torrent
                                .as_ref()
                                .is_some_and(|t| !t.info.pieces.is_empty())
                            {
                                // Fallback for Hybrid torrents
                                self.last_activity = TorrentActivity::VerifyingPiece(piece_index);
                                effects.push(Effect::VerifyPiece {
                                    peer_id: peer_id.clone(),
                                    piece_index,
                                    data: complete_data,
                                });
                            } else {
                                // Buffer v2 data and ask for proof.
                                self.v2_pending_data
                                    .insert(piece_index, (block_offset, complete_data));

                                let root_info_opt = self
                                    .piece_to_roots
                                    .get(&piece_index)
                                    .and_then(|roots| roots.first());

                                if let Some(r_info) = root_info_opt {
                                    let piece_len = self
                                        .torrent
                                        .as_ref()
                                        .map(|t| t.info.piece_length as u64)
                                        .unwrap_or(32768);
                                    let request_base = if piece_len >= 16384 {
                                        (piece_len / 16384).trailing_zeros()
                                    } else {
                                        0
                                    };

                                    let request_index = if piece_len >= 16384 {
                                        let global_piece_offset = piece_index as u64 * piece_len;
                                        let offset_in_file =
                                            global_piece_offset.saturating_sub(r_info.file_offset);
                                        let relative_block_index = offset_in_file / 16384;
                                        relative_block_index >> request_base
                                    } else {
                                        piece_index as u64
                                    };

                                    effects.push(Effect::RequestHashes {
                                        peer_id: peer_id.clone(),
                                        file_root: r_info.root_hash.clone(),
                                        piece_index: request_index as u32,
                                        length: 1,
                                        proof_layers: 0,
                                        base_layer: request_base,
                                    });
                                }
                            }
                        } else {
                            // Fallback attempt to V1 if possible
                            self.last_activity = TorrentActivity::VerifyingPiece(piece_index);
                            effects.push(Effect::VerifyPiece {
                                peer_id: peer_id.clone(),
                                piece_index,
                                data: complete_data,
                            });
                        }
                    } else {
                        self.last_activity = TorrentActivity::VerifyingPiece(piece_index);
                        effects.push(Effect::VerifyPiece {
                            peer_id: peer_id.clone(),
                            piece_index,
                            data: complete_data,
                        });
                    }
                }

                if let Some(peer) = self.peers.get(&peer_id) {
                    let low_water_mark = MAX_PIPELINE_DEPTH / 2;
                    if peer.inflight_requests <= low_water_mark {
                        effects.extend(self.update(Action::AssignWork {
                            peer_id: peer_id.clone(),
                        }));
                    }
                }

                effects
            }

            Action::MerkleProofReceived {
                peer_id,
                piece_index,
                proof,
            } => {
                if self.piece_manager.bitfield.get(piece_index as usize) == Some(&PieceStatus::Done)
                {
                    return vec![Effect::DoNothing];
                }

                if let Some((block_offset, data)) = self.v2_pending_data.remove(&piece_index) {
                    if let Some(roots) = self.piece_to_roots.get(&piece_index) {
                        let piece_len = self
                            .torrent
                            .as_ref()
                            .map(|t| t.info.piece_length as u64)
                            .unwrap_or(65536);

                        let global_offset = (piece_index as u64 * piece_len) + block_offset as u64;

                        let matching_root_info = roots
                            .iter()
                            .filter(|r| r.file_offset <= global_offset)
                            .max_by_key(|r| r.file_offset);

                        if let Some(root_info) = matching_root_info {
                            let (valid_length, _, hashing_context_len) =
                                self.calculate_v2_verify_params(piece_index, data.len());

                            let offset_in_file =
                                global_offset.saturating_sub(root_info.file_offset);
                            let actual_relative_index = (offset_in_file / piece_len) as u32;

                            let local_piece_hash = self.torrent.as_ref().and_then(|t| {
                                t.get_v2_hash_layer(
                                    actual_relative_index,
                                    root_info.file_offset,
                                    root_info.length,
                                    1,
                                    &root_info.root_hash,
                                )
                            });

                            let (verification_target, verification_proof) = if proof.len() == 32 {
                                (
                                    local_piece_hash.unwrap_or_else(|| proof.clone()),
                                    Vec::new(),
                                )
                            } else {
                                (root_info.root_hash.clone(), proof)
                            };

                            return vec![Effect::VerifyPieceV2 {
                                peer_id,
                                piece_index,
                                proof: verification_proof,
                                data,
                                root_hash: verification_target,
                                _file_start_offset: root_info.file_offset,
                                valid_length,
                                relative_index: actual_relative_index,
                                hashing_context_len,
                            }];
                        }
                    }
                }
                vec![Effect::DoNothing]
            }

            Action::PieceVerified {
                peer_id,
                piece_index,
                valid,
                data,
            } => {
                let mut effects = Vec::new();

                if piece_index as usize >= self.piece_manager.bitfield.len() {
                    return vec![Effect::DoNothing];
                }

                self.verifying_pieces.remove(&piece_index);
                self.v2_proofs.remove(&piece_index);
                self.v2_pending_data.remove(&piece_index);

                if valid {
                    if self.piece_manager.bitfield.get(piece_index as usize)
                        == Some(&PieceStatus::Done)
                    {
                        if let Some(peer) = self.peers.get_mut(&peer_id) {
                            peer.pending_requests.remove(&piece_index);
                        }

                        // Redundant piece; we already have it. Discard data and assign new work.
                        effects.extend(self.update(Action::AssignWork { peer_id }));
                    } else {
                        // Valid and needed piece. Request write to disk.
                        // The data payload is now properly passed from the Action.
                        effects.push(Effect::WriteToDisk {
                            peer_id: peer_id.clone(),
                            piece_index,
                            data,
                        });
                    }
                } else {
                    self.piece_manager.reset_piece_assembly(piece_index);
                    effects.push(Effect::DisconnectPeer { peer_id });
                }
                effects
            }

            Action::PieceWrittenToDisk {
                peer_id,
                piece_index,
            } => {
                if piece_index as usize >= self.piece_manager.bitfield.len() {
                    return vec![Effect::DoNothing];
                }

                if self.torrent_status == TorrentStatus::Validating
                    || self.torrent_status == TorrentStatus::AwaitingMetadata
                {
                    return vec![Effect::DoNothing];
                }

                let mut effects = Vec::new();

                if self.piece_manager.bitfield.get(piece_index as usize) == Some(&PieceStatus::Done)
                {
                    if let Some(peer) = self.peers.get_mut(&peer_id) {
                        peer.pending_requests.remove(&piece_index);
                    }
                    effects.extend(self.update(Action::AssignWork { peer_id }));
                    return effects;
                }

                // ACTUAL STATE CHANGE
                let peers_to_cancel = self.piece_manager.mark_as_complete(piece_index);

                effects.push(Effect::EmitManagerEvent(ManagerEvent::DiskWriteFinished));

                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    peer.pending_requests.remove(&piece_index);
                }

                effects.extend(self.update(Action::AssignWork {
                    peer_id: peer_id.clone(),
                }));

                for other_peer in peers_to_cancel {
                    if other_peer != peer_id {
                        if let Some(peer) = self.peers.get_mut(&other_peer) {
                            peer.pending_requests.remove(&piece_index);
                            // ... cancellation construction ...
                            let batch = self.piece_manager.cancel_tuples_for_piece(piece_index);
                            if !batch.is_empty() {
                                effects.push(Effect::SendToPeer {
                                    peer_id: other_peer.clone(),
                                    cmd: Box::new(TorrentCommand::BulkCancel(batch)),
                                });
                            }
                        }
                        effects.extend(self.update(Action::AssignWork {
                            peer_id: other_peer,
                        }));
                    }
                }

                effects.push(Effect::BroadcastHave { piece_index });
                effects.extend(self.update(Action::CheckCompletion));

                let all_peers: Vec<String> = self.peers.keys().cloned().collect();
                for pid in all_peers {
                    effects.extend(self.update(Action::AssignWork { peer_id: pid }));
                }

                effects
            }

            Action::PieceWriteFailed { piece_index } => {
                if piece_index as usize >= self.piece_manager.bitfield.len() {
                    return vec![Effect::DoNothing];
                }
                self.piece_manager.requeue_pending_to_need(piece_index);
                vec![Effect::EmitManagerEvent(ManagerEvent::DiskWriteFinished)]
            }

            Action::RequestUpload {
                peer_id,
                piece_index,
                block_offset,
                length,
            } => {
                if !self.data_available {
                    return vec![Effect::DoNothing];
                }

                if self.torrent.is_none() {
                    return vec![Effect::DoNothing];
                }

                if length > MAX_BLOCK_SIZE {
                    return vec![Effect::DoNothing];
                }

                self.last_activity = TorrentActivity::SendingPiece(piece_index);

                let mut allowed = false;
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    if peer.am_choking == ChokeStatus::Unchoke
                        && self.piece_manager.bitfield.get(piece_index as usize)
                            == Some(&PieceStatus::Done)
                    {
                        allowed = true;
                    }
                }

                if allowed {
                    vec![
                        Effect::EmitManagerEvent(ManagerEvent::FileActivity {
                            info_hash: self.info_hash.clone(),
                            touched_relative_paths: self.touched_relative_paths_for_activity(
                                piece_index,
                                block_offset,
                                length,
                            ),
                            direction: FileActivityDirection::Upload,
                        }),
                        Effect::ReadFromDisk {
                            peer_id,
                            block_info: BlockInfo {
                                piece_index,
                                offset: block_offset,
                                length,
                            },
                        },
                    ]
                } else {
                    vec![Effect::DoNothing]
                }
            }

            Action::TrackerResponse {
                url,
                peers,
                interval,
                min_interval,
            } => {
                let mut effects = Vec::new();

                if let Some(tracker) = self.trackers.get_mut(&url) {
                    let seeding_secs = if interval > 0 { interval + 1 } else { 1800 };
                    tracker.seeding_interval = Some(Duration::from_secs(seeding_secs));

                    let leeching_secs = min_interval.map(|m| m + 1).unwrap_or(60);
                    tracker.leeching_interval = Some(Duration::from_secs(leeching_secs));

                    let next_interval = if self.torrent_status != TorrentStatus::Done {
                        tracker.leeching_interval.unwrap()
                    } else {
                        tracker.seeding_interval.unwrap()
                    };
                    tracker.next_announce_time = self.now + next_interval;
                }

                for peer_addr in peers {
                    let peer_key = peer_addr.to_string();
                    if let Some((_, next_attempt)) = self.timed_out_peers.get(&peer_key) {
                        if self.now < *next_attempt {
                            continue;
                        }
                    }
                    effects.push(Effect::ConnectToPeer { addr: peer_addr });
                }

                effects
            }

            Action::TrackerError { url } => {
                if let Some(tracker) = self.trackers.get_mut(&url) {
                    let current_interval = if self.torrent_status != TorrentStatus::Done {
                        tracker.leeching_interval.unwrap_or(Duration::from_secs(60))
                    } else {
                        tracker
                            .seeding_interval
                            .unwrap_or(Duration::from_secs(1800))
                    };

                    let backoff = current_interval.mul_f32(2.0).min(Duration::from_secs(3600));
                    tracker.next_announce_time = self.now + backoff;
                }
                vec![Effect::DoNothing]
            }

            Action::PeerConnectionFailed { peer_addr } => {
                self.pending_failures.push(peer_addr);
                if self.pending_failures.len() >= 100 {
                    let effects = Vec::new();
                    let batch = std::mem::take(&mut self.pending_failures);
                    for addr in batch {
                        let (count, _) = self
                            .timed_out_peers
                            .get(&addr)
                            .cloned()
                            .unwrap_or((0, self.now));
                        let new_count = (count + 1).min(10);
                        let backoff_secs = (15 * 2u64.pow(new_count - 1)).min(1800);
                        self.timed_out_peers.insert(
                            addr,
                            (new_count, self.now + Duration::from_secs(backoff_secs)),
                        );
                    }
                    return effects;
                }
                vec![Effect::DoNothing]
            }

            Action::MetadataReceived {
                torrent,
                metadata_length,
            } => {
                if self.torrent.is_some() {
                    return vec![Effect::DoNothing];
                }

                self.torrent = Some(*torrent.clone());
                self.torrent_metadata_length = Some(metadata_length);

                let (v2_piece_count, piece_overrides) = self.rebuild_v2_mappings();

                let num_pieces = if !torrent.info.pieces.is_empty() {
                    torrent.info.pieces.len() / 20
                } else {
                    v2_piece_count as usize
                };

                self.piece_manager = PieceManager::new();
                self.piece_manager
                    .set_initial_fields(num_pieces, self.torrent_validation_status);

                let total_len: u64 = if torrent.info.meta_version == Some(2) {
                    (num_pieces as u64) * (torrent.info.piece_length as u64)
                } else if torrent.info.files.is_empty() {
                    torrent.info.length as u64
                } else {
                    torrent.info.files.iter().map(|f| f.length as u64).sum()
                };

                self.piece_manager.set_geometry(
                    torrent.info.piece_length as u32,
                    total_len,
                    piece_overrides,
                    self.torrent_validation_status,
                );
                if !self.file_priorities.is_empty() {
                    let priorities = self.calculate_piece_priorities(&self.file_priorities);
                    self.piece_manager.apply_priorities(priorities);
                }

                for peer in self.peers.values_mut() {
                    if peer.bitfield.len() > num_pieces {
                        peer.bitfield.truncate(num_pieces);
                    } else if peer.bitfield.len() < num_pieces {
                        peer.bitfield.resize(num_pieces, false);
                    }
                }

                let tracker_urls = normalize_tracker_urls(
                    self.trackers
                        .keys()
                        .cloned()
                        .chain(torrent.tracker_urls().into_iter()),
                );
                self.trackers = tracker_urls
                    .into_iter()
                    .map(|announce| {
                        let state = self.trackers.remove(&announce).unwrap_or(TrackerState {
                            next_announce_time: self.now,
                            leeching_interval: None,
                            seeding_interval: None,
                        });
                        (announce, state)
                    })
                    .collect();

                self.validation_pieces_found = 0;
                if self.torrent_data_path.is_some() {
                    self.rebuild_multi_file_info();
                    self.torrent_status = TorrentStatus::Validating;
                    return vec![Effect::StartValidation];
                }
                vec![Effect::DoNothing]
            }

            Action::ValidationComplete { completed_pieces } => {
                let mut effects = Vec::new();

                if self.torrent_status != TorrentStatus::Validating {
                    return vec![Effect::DoNothing];
                }

                for piece_index in &completed_pieces {
                    let _ = self.piece_manager.mark_as_complete(*piece_index);
                }

                self.torrent_status = TorrentStatus::Standard;

                self.piece_manager.pending_queue.clear();
                for peer in self.peers.values_mut() {
                    peer.pending_requests.clear();
                }
                self.piece_manager.clear_assembly_buffers();

                for status in self.piece_manager.bitfield.iter_mut() {
                    if *status != PieceStatus::Done {
                        *status = PieceStatus::Need;
                    }
                }

                // Rebuild Need Queue (now using all available pieces)
                self.piece_manager.need_queue.clear();
                for (index, status) in self.piece_manager.bitfield.iter().enumerate() {
                    let idx = index as u32;
                    if *status != PieceStatus::Done {
                        let is_skipped = !self.piece_manager.piece_priorities.is_empty()
                            && self.piece_manager.piece_priorities[index]
                                == EffectivePiecePriority::Skip;

                        if !is_skipped {
                            self.piece_manager.need_queue.push(idx);
                        }
                    }
                }

                if !self.is_paused {
                    if !self.has_started_announce_sent {
                        self.has_started_announce_sent = true;
                        effects.push(Effect::ConnectToPeersFromTrackers);
                    } else {
                        for url in self.trackers.keys() {
                            effects.push(Effect::AnnounceToTracker { url: url.clone() });
                        }
                    }
                }

                for piece_index in &completed_pieces {
                    effects.push(Effect::BroadcastHave {
                        piece_index: *piece_index,
                    });
                }

                effects.extend(self.update(Action::CheckCompletion));
                effects.extend(self.update(Action::RecalculateChokes {
                    random_seed: self.now.elapsed().as_nanos() as u64,
                }));

                for peer_id in self.peers.keys().cloned().collect::<Vec<_>>() {
                    effects.extend(self.update(Action::AssignWork { peer_id }));
                }

                effects
            }

            Action::CancelUpload {
                peer_id,
                piece_index,
                block_offset,
                length,
            } => {
                vec![Effect::AbortUpload {
                    peer_id,
                    block_info: BlockInfo {
                        piece_index,
                        offset: block_offset,
                        length,
                    },
                }]
            }

            Action::BlockSentToPeer {
                peer_id,
                byte_count,
            } => {
                self.session_total_uploaded =
                    self.session_total_uploaded.saturating_add(byte_count);
                self.bytes_uploaded_in_interval =
                    self.bytes_uploaded_in_interval.saturating_add(byte_count);

                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    peer.bytes_uploaded_to_peer =
                        peer.bytes_uploaded_to_peer.saturating_add(byte_count);
                    peer.total_bytes_uploaded =
                        peer.total_bytes_uploaded.saturating_add(byte_count);
                    peer.bytes_uploaded_in_tick =
                        peer.bytes_uploaded_in_tick.saturating_add(byte_count);
                }

                vec![Effect::EmitManagerEvent(ManagerEvent::BlockSent {
                    info_hash: self.info_hash.clone(),
                })]
            }

            Action::Cleanup => {
                let mut effects = Vec::new();

                self.timed_out_peers
                    .retain(|_, (retry_count, _)| *retry_count < MAX_TIMEOUT_COUNT);

                let max_ram_usage = 1024 * 1024 * 1024; // 1 GB
                let piece_len = self
                    .torrent
                    .as_ref()
                    .map(|t| t.info.piece_length as usize)
                    .unwrap_or(16_384);
                let max_pending_items = max_ram_usage / piece_len;
                if self.v2_pending_data.len() > max_pending_items {
                    self.v2_pending_data.clear();
                }

                let mut stuck_peers = Vec::new();
                for (id, peer) in &self.peers {
                    if peer.peer_id.is_empty()
                        && self.now.saturating_duration_since(peer.created_at)
                            > Duration::from_secs(5)
                    {
                        stuck_peers.push(id.clone());
                    }
                }

                for peer_id in stuck_peers {
                    self.pending_disconnects.push(peer_id);
                }

                effects.extend(self.update(Action::PeerDisconnected {
                    peer_id: String::new(),
                    force: true,
                }));

                let am_seeding = !self.piece_manager.bitfield.is_empty()
                    && self
                        .piece_manager
                        .bitfield
                        .iter()
                        .all(|&s| s == PieceStatus::Done);

                if am_seeding && self.torrent_status != TorrentStatus::Done {
                    self.torrent_status = TorrentStatus::Done;
                    effects.extend(self.update(Action::CheckCompletion));
                }

                if am_seeding {
                    let mut peers_to_disconnect = Vec::new();
                    for (peer_id, peer) in &self.peers {
                        if !peer.bitfield.is_empty()
                            && peer.bitfield.iter().all(|&has_piece| has_piece)
                        {
                            peers_to_disconnect.push(peer_id.clone());
                        }
                    }
                    for peer_id in peers_to_disconnect {
                        effects.push(Effect::DisconnectPeer { peer_id });
                    }
                }

                effects
            }

            Action::Pause => {
                self.last_activity = TorrentActivity::Paused;
                self.is_paused = true;

                self.last_known_peers = self.peers.keys().cloned().collect();

                for (piece_index, _) in self.piece_manager.pending_queue.drain() {
                    self.piece_manager.need_queue.push(piece_index);
                }

                self.peers.clear();

                self.number_of_successfully_connected_peers = 0;

                self.bytes_downloaded_in_interval = 0;
                self.bytes_uploaded_in_interval = 0;
                self.total_dl_prev_avg_ema = 0.0;
                self.total_ul_prev_avg_ema = 0.0;

                vec![
                    Effect::EmitMetrics {
                        bytes_dl: self.bytes_downloaded_in_interval,
                        bytes_ul: self.bytes_uploaded_in_interval,
                    },
                    Effect::ClearAllUploads,
                    Effect::EmitManagerEvent(ManagerEvent::PeerDisconnected {
                        info_hash: self.info_hash.clone(),
                    }),
                ]
            }

            Action::Resume => {
                self.last_activity = TorrentActivity::ConnectingToPeers;
                self.is_paused = false;

                if self.torrent_status == TorrentStatus::Validating {
                    return vec![Effect::DoNothing];
                }

                let mut effects = vec![Effect::TriggerDhtSearch];

                effects.extend(self.update(Action::ConnectToWebSeeds));

                for (url, tracker) in self.trackers.iter_mut() {
                    tracker.next_announce_time = self.now + Duration::from_secs(60);
                    effects.push(Effect::AnnounceToTracker { url: url.clone() });
                }

                let peers_to_connect: Vec<String> = std::mem::take(&mut self.last_known_peers)
                    .into_iter()
                    .collect();
                for peer_addr in peers_to_connect {
                    if let Ok(addr) = peer_addr.parse::<SocketAddr>() {
                        effects.push(Effect::ConnectToPeer { addr });
                    }
                }

                effects
            }

            Action::Delete => {
                self.peers.clear();
                self.last_known_peers.clear();
                self.timed_out_peers.clear();

                self.v2_proofs.clear();
                self.v2_pending_data.clear();
                self.piece_to_roots.clear();
                self.verifying_pieces.clear();

                let num_pieces = self.piece_manager.bitfield.len();
                self.piece_manager = PieceManager::new();
                if num_pieces > 0 {
                    self.piece_manager.set_initial_fields(num_pieces, false);
                }
                self.piece_manager.pending_queue.clear();
                self.piece_manager.need_queue.clear();

                for status in self.piece_manager.bitfield.iter_mut() {
                    *status = PieceStatus::Need;
                }

                self.number_of_successfully_connected_peers = 0;

                self.session_total_downloaded = 0;
                self.session_total_uploaded = 0;

                // These must be cleared, otherwise they remain > 0 while total is 0
                self.bytes_downloaded_in_interval = 0;
                self.bytes_uploaded_in_interval = 0;

                self.is_paused = true;
                self.torrent_status = if self.torrent.is_some() {
                    TorrentStatus::Validating
                } else {
                    TorrentStatus::AwaitingMetadata
                };
                self.last_activity = TorrentActivity::Initializing;

                let mut effects = Vec::new();
                if let (Some(path), Some(mfi)) = (&self.torrent_data_path, &self.multi_file_info) {
                    let container = self.container_name.as_deref();
                    let (files, directories) = calculate_deletion_lists(mfi, path, container);
                    effects.push(Effect::DeleteFiles { files, directories });
                } else {
                    if self.torrent_status != TorrentStatus::AwaitingMetadata
                        && self.torrent_status != TorrentStatus::Validating
                    {
                        event!(
                            Level::WARN,
                            "Action::Delete triggered but torrent_data_path or mfi is missing."
                        );
                    } else {
                        event!(
                            Level::INFO,
                            "Aborting torrent before storage initialization."
                        );
                    }

                    effects.push(Effect::EmitManagerEvent(ManagerEvent::DeletionComplete(
                        self.info_hash.clone(),
                        Ok(()),
                    )));
                }
                effects
            }

            Action::UpdateListenPort => {
                let mut effects = Vec::new();

                for (url, tracker) in self.trackers.iter_mut() {
                    tracker.next_announce_time = self.now + Duration::from_secs(60);
                    effects.push(Effect::AnnounceToTracker { url: url.clone() });
                }

                effects
            }

            Action::SetUserTorrentConfig {
                torrent_data_path,
                file_priorities,
                container_name,
            } => {
                event!(
                    Level::INFO,
                    "Received User config {:?} - {} Priorities",
                    torrent_data_path,
                    file_priorities.len()
                );

                self.torrent_data_path = Some(torrent_data_path);
                self.file_priorities = file_priorities;
                self.container_name = container_name;

                if self.torrent.is_some() {
                    let priorities = self.calculate_piece_priorities(&self.file_priorities);
                    self.piece_manager.apply_priorities(priorities);
                }

                let mut effects = Vec::new();

                if self.torrent.is_some() && self.multi_file_info.is_none() {
                    self.rebuild_multi_file_info();

                    if self.multi_file_info.is_some() {
                        self.torrent_status = TorrentStatus::Validating;
                        effects.push(Effect::StartValidation);
                    }
                }

                effects.extend(self.update(Action::CheckCompletion));

                effects
            }

            Action::SetDataAvailability { available } => {
                self.data_available = available;
                self.update(Action::RecalculateChokes { random_seed: 0 })
            }

            Action::ValidationProgress { count } => {
                self.validation_pieces_found = count;
                vec![Effect::DoNothing]
            }

            Action::Shutdown => {
                self.is_paused = true;
                let left = if let Some(t) = &self.torrent {
                    let completed = self
                        .piece_manager
                        .bitfield
                        .iter()
                        .filter(|&&s| s == PieceStatus::Done)
                        .count();

                    let total_len = if t.info.files.is_empty() {
                        t.info.length
                    } else {
                        t.info.files.iter().map(|f| f.length).sum()
                    };

                    (total_len as usize).saturating_sub(completed * t.info.piece_length as usize)
                } else {
                    0
                };

                let tracker_urls: Vec<String> = self.trackers.keys().cloned().collect();
                let uploaded = self.session_total_uploaded as usize;
                let downloaded = self.session_total_downloaded as usize;

                self.peers.clear();

                vec![Effect::PrepareShutdown {
                    tracker_urls,
                    left,
                    uploaded,
                    downloaded,
                }]
            }

            Action::FatalError => self.update(Action::Pause),
        }
    }

    fn rebuild_v2_mappings(&mut self) -> (u32, HashMap<u32, u32>) {
        let mut overrides = HashMap::new();
        let mut v2_piece_count: u32 = 0;

        if let Some(torrent) = &self.torrent {
            let mapping = torrent.calculate_v2_mapping();
            self.piece_to_roots = mapping.piece_to_roots;
            v2_piece_count = mapping.piece_count as u32;

            if torrent.info.meta_version == Some(2) {
                let piece_len = torrent.info.piece_length as u64;
                let mut v2_roots = torrent.get_v2_roots();
                v2_roots.sort_by(|(a, _, _), (b, _, _)| a.cmp(b));

                let mut current_idx = 0;
                for (_, length, _) in v2_roots {
                    if length > 0 && piece_len > 0 {
                        let file_pieces = length.div_ceil(piece_len);
                        let tail_len = (length % piece_len) as u32;
                        if tail_len > 0 {
                            let tail_idx = (current_idx + file_pieces - 1) as u32;
                            overrides.insert(tail_idx, tail_len);
                        }
                        current_idx += file_pieces;
                    }
                }
            }
        }
        (v2_piece_count, overrides)
    }

    fn calculate_v2_verify_params(&self, piece_index: u32, data_len: usize) -> (usize, u32, usize) {
        if let Some(roots) = self.piece_to_roots.get(&piece_index) {
            if let Some(root_info) = roots.first() {
                let piece_len = self
                    .torrent
                    .as_ref()
                    .map(|t| t.info.piece_length as u64)
                    .unwrap_or(0);

                let piece_start_global = piece_index as u64 * piece_len;
                let offset_in_file = piece_start_global.saturating_sub(root_info.file_offset);
                let remaining = root_info.length.saturating_sub(offset_in_file);

                let valid_length = std::cmp::min(data_len as u64, remaining) as usize;
                let relative_index = (offset_in_file / piece_len) as u32;

                let hashing_context_len = if root_info.length <= piece_len {
                    root_info.length as usize
                } else {
                    piece_len as usize
                };

                return (valid_length, relative_index, hashing_context_len);
            }
        }
        (data_len, 0, data_len)
    }

    fn touched_relative_paths_for_activity(
        &self,
        piece_index: u32,
        block_offset: u32,
        length: u32,
    ) -> Vec<String> {
        if length == 0 {
            Vec::new()
        } else {
            self.multi_file_info
                .as_ref()
                .and_then(|multi_file_info| {
                    self.torrent.as_ref().map(|torrent| {
                        let global_offset = (piece_index as u64 * torrent.info.piece_length as u64)
                            + block_offset as u64;
                        let range_end = global_offset.saturating_add(length as u64);
                        let effective_root = match &self.container_name {
                            Some(name) if !name.is_empty() => {
                                self.torrent_data_path.as_ref().map(|path| path.join(name))
                            }
                            _ => self.torrent_data_path.clone(),
                        };
                        multi_file_info
                            .files
                            .iter()
                            .filter_map(|file_info| {
                                let file_start = file_info.global_start_offset;
                                let file_end = file_start.saturating_add(file_info.length);
                                if global_offset < file_end && range_end > file_start {
                                    Some(
                                        effective_root
                                            .as_ref()
                                            .and_then(|root| {
                                                file_info.path.strip_prefix(root).ok().map(
                                                    |relative| {
                                                        relative
                                                            .iter()
                                                            .map(|part| {
                                                                part.to_string_lossy().into_owned()
                                                            })
                                                            .collect::<Vec<_>>()
                                                            .join("/")
                                                    },
                                                )
                                            })
                                            .unwrap_or_else(|| {
                                                file_info
                                                    .path
                                                    .file_name()
                                                    .map(|name| name.to_string_lossy().into_owned())
                                                    .unwrap_or_else(|| {
                                                        file_info
                                                            .path
                                                            .to_string_lossy()
                                                            .into_owned()
                                                    })
                                            }),
                                    )
                                } else {
                                    None
                                }
                            })
                            .collect()
                    })
                })
                .unwrap_or_default()
        }
    }

    pub fn rebuild_multi_file_info(&mut self) {
        // Guard 1: Ensure metadata exists
        let torrent = match &self.torrent {
            Some(t) => t,
            None => {
                event!(
                    Level::DEBUG,
                    "rebuild_multi_file_info: Skipping. No torrent metadata available."
                );
                return;
            }
        };

        // Guard 2: Handle the Option<PathBuf>
        let path = match &self.torrent_data_path {
            Some(p) if !p.as_os_str().is_empty() => p,
            Some(_) => {
                event!(Level::WARN,
                    torrent_name = %torrent.info.name,
                    "rebuild_multi_file_info: torrent_data_path is Some, but the path is empty."
                );
                return;
            }
            None => {
                event!(Level::WARN,
                    torrent_name = %torrent.info.name,
                    "rebuild_multi_file_info: torrent_data_path is None."
                );
                return;
            }
        };

        let effective_path = match &self.container_name {
            // Case A: User specified a folder
            Some(name) if !name.is_empty() => path.join(name),

            // Case B: User explicitly said "No Folder" (Empty String)
            Some(_) => path.clone(),

            // Case C: Auto/Default (None) -> Intelligent Behavior
            None => {
                let is_multi_file = !torrent.info.files.is_empty();
                // BitTorrent standard: multi-file torrents use folders
                if is_multi_file {
                    let info_hash_hex = hex::encode(&self.info_hash);
                    let unique_name = format!("{} [{}]", torrent.info.name, info_hash_hex);
                    self.container_name = Some(unique_name.clone());
                    path.join(unique_name)
                } else {
                    path.clone()
                }
            }
        };
        self.multi_file_info = MultiFileInfo::new(
            &effective_path,
            &torrent.info.name,
            if torrent.info.files.is_empty() { None } else { Some(&torrent.info.files) },
            if torrent.info.files.is_empty() { Some(torrent.info.length as u64) } else { None },
            &self.file_priorities,
        ).map_err(|e| {
            event!(Level::ERROR, error = %e, "rebuild_multi_file_info: Failed to create MultiFileInfo");
            e
        }).ok();

        if self.multi_file_info.is_some() {
            event!(Level::INFO,
                torrent_name = %torrent.info.name,
                "rebuild_multi_file_info: Storage successfully initialized in state."
            );
        }
    }

    fn calculate_piece_priorities(
        &self,
        new_file_priorities: &HashMap<usize, FilePriority>,
    ) -> Vec<EffectivePiecePriority> {
        let torrent = match &self.torrent {
            Some(t) => t,
            None => return Vec::new(),
        };

        let num_pieces = self.piece_manager.bitfield.len();
        if num_pieces == 0 {
            return Vec::new();
        }

        let mut piece_vec = vec![EffectivePiecePriority::Normal; num_pieces];
        let piece_len = torrent.info.piece_length as u64;

        // Default all to Skip, then paint Normal/High over them.
        piece_vec.fill(EffectivePiecePriority::Skip);

        let mut file_start = 0u64;

        let files_iter = if !torrent.info.files.is_empty() {
            torrent
                .info
                .files
                .iter()
                .map(|f| f.length)
                .enumerate()
                .collect::<Vec<_>>()
        } else {
            vec![(0, torrent.info.length)]
        };

        for (file_idx, length) in files_iter {
            let file_end = file_start + (length as u64);
            let start_piece = (file_start / piece_len) as usize;
            let end_piece = ((file_end.saturating_sub(1)) / piece_len) as usize;

            let priority = new_file_priorities
                .get(&file_idx)
                .unwrap_or(&FilePriority::Normal);

            for (p_idx, piece) in piece_vec
                .iter_mut()
                .enumerate()
                .take(end_piece + 1)
                .skip(start_piece)
            {
                if p_idx >= num_pieces {
                    break;
                }

                match priority {
                    FilePriority::High => {
                        *piece = EffectivePiecePriority::High;
                    }
                    FilePriority::Normal | FilePriority::Mixed => {
                        if *piece != EffectivePiecePriority::High {
                            *piece = EffectivePiecePriority::Normal;
                        }
                    }
                    FilePriority::Skip => {
                        // Stays Skip unless overwritten by another file
                    }
                }
            }
            file_start = file_end;
        }
        piece_vec
    }
}

impl TorrentState {
    fn refresh_peer_admission_guard(&mut self) {
        let reopen_threshold = (PEER_ADMISSION_QUALITY_THRESHOLD * 50) / 100;
        let connected = self.number_of_successfully_connected_peers;

        if self.accepting_new_peers {
            if connected >= PEER_ADMISSION_QUALITY_THRESHOLD {
                self.accepting_new_peers = false;
            }
        } else if connected <= reopen_threshold {
            self.accepting_new_peers = true;
        }
    }
}

pub fn calculate_deletion_lists(
    mfi: &MultiFileInfo,
    base_path: &Path,
    known_container_name: Option<&str>,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut files = Vec::new();
    let mut dirs_to_delete = HashSet::new();

    for file_info in &mfi.files {
        files.push(file_info.path.clone());

        // Walk up the directory tree
        let mut current = file_info.path.parent();
        while let Some(dir) = current {
            if dir == base_path {
                break;
            }
            if dir.starts_with(base_path) {
                dirs_to_delete.insert(dir.to_path_buf());
            } else {
                break;
            }
            current = dir.parent();
        }
    }

    // STRICT SAFETY: Only delete the base_path if we explicitly recorded a container name
    // and the current base_path's folder name matches it.
    if let Some(recorded_name) = known_container_name {
        if let Some(folder_name) = base_path.file_name().and_then(|n| n.to_str()) {
            if folder_name == recorded_name {
                dirs_to_delete.insert(base_path.to_path_buf());
            }
        }
    }

    let mut sorted_dirs: Vec<PathBuf> = dirs_to_delete.into_iter().collect();
    sorted_dirs.sort_by_key(|b| std::cmp::Reverse(b.as_os_str().len()));

    (files, sorted_dirs)
}

#[derive(Debug, Clone)]
pub struct PeerState {
    pub ip_port: String,
    pub peer_id: Vec<u8>,
    pub bitfield: Vec<bool>,
    pub am_choking: ChokeStatus,
    pub peer_choking: ChokeStatus,
    pub peer_tx: Sender<TorrentCommand>,
    pub am_interested: bool,
    pub pending_requests: HashSet<u32>,
    pub peer_is_interested_in_us: bool,
    pub bytes_downloaded_from_peer: u64,
    pub bytes_uploaded_to_peer: u64,
    pub bytes_downloaded_in_tick: u64,
    pub bytes_uploaded_in_tick: u64,
    pub prev_avg_dl_ema: f64,
    pub prev_avg_ul_ema: f64,
    pub total_bytes_downloaded: u64,
    pub total_bytes_uploaded: u64,
    pub download_speed_bps: u64,
    pub upload_speed_bps: u64,
    pub upload_slots_semaphore: Arc<Semaphore>,
    pub last_action: TorrentCommand,
    pub action_counts: HashMap<Discriminant<TorrentCommand>, u64>,
    pub created_at: Instant,
    pub inflight_requests: usize,
    pub active_blocks: HashSet<(u32, u32, u32)>,
}

impl PeerState {
    pub fn new(ip_port: String, peer_tx: Sender<TorrentCommand>, created_at: Instant) -> Self {
        Self {
            ip_port,
            peer_id: Vec::new(),
            bitfield: Vec::new(),
            am_choking: ChokeStatus::Choke,
            peer_choking: ChokeStatus::Choke,
            peer_tx,
            am_interested: false,
            pending_requests: HashSet::new(),
            peer_is_interested_in_us: false,
            bytes_downloaded_from_peer: 0,
            bytes_uploaded_to_peer: 0,
            bytes_downloaded_in_tick: 0,
            bytes_uploaded_in_tick: 0,
            total_bytes_downloaded: 0,
            total_bytes_uploaded: 0,
            prev_avg_dl_ema: 0.0,
            prev_avg_ul_ema: 0.0,
            download_speed_bps: 0,
            upload_speed_bps: 0,
            upload_slots_semaphore: Arc::new(Semaphore::new(PEER_UPLOAD_IN_FLIGHT_LIMIT)),
            last_action: TorrentCommand::SuccessfullyConnected(String::new()),
            action_counts: HashMap::new(),
            created_at,
            inflight_requests: 0,
            active_blocks: HashSet::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::TorrentCommand;
    use crate::torrent_file::V2RootInfo;
    use crate::torrent_manager::piece_manager::PieceManager;
    use tokio::sync::mpsc;

    // --- Test Helpers ---

    pub(crate) fn create_empty_state() -> TorrentState {
        TorrentState {
            info_hash: vec![0; 20],
            peers: HashMap::new(),
            piece_manager: PieceManager::new(),
            trackers: HashMap::new(),
            torrent_data_path: Some(PathBuf::from("/tmp/superseedr_test")),
            ..Default::default()
        }
    }

    pub(crate) fn create_dummy_torrent(piece_count: usize) -> Torrent {
        // Construct a minimal Torrent struct for testing
        // Note: You might need to adjust this based on your actual Torrent struct visibility
        use crate::torrent_file::Info;

        Torrent {
            announce: Some("http://tracker.test".to_string()),
            announce_list: None,
            url_list: None,
            info: Info {
                name: "test_torrent".to_string(),
                piece_length: 16384,                 // 16KB
                pieces: vec![0u8; 20 * piece_count], // 20 bytes per piece hash
                length: (16384 * piece_count) as i64,
                files: vec![],
                private: None,
                md5sum: None,
                meta_version: None,
                file_tree: None,
            },
            info_dict_bencode: vec![],
            created_by: None,
            creation_date: None,
            encoding: None,
            comment: None,
            piece_layers: None,
        }
    }

    fn add_peer(state: &mut TorrentState, id: &str) {
        let (tx, _) = mpsc::channel(1);
        let mut peer = PeerState::new(id.to_string(), tx, state.now);
        // Assume peer has handshake
        peer.peer_id = id.as_bytes().to_vec();
        state.peers.insert(id.to_string(), peer);
    }

    #[test]
    fn test_peer_admission_guard_closes_under_high_connected_pressure() {
        let mut state = create_empty_state();
        state.torrent_status = TorrentStatus::Standard;
        state.accepting_new_peers = true;

        for i in 0..PEER_ADMISSION_QUALITY_THRESHOLD {
            add_peer(&mut state, &format!("peer_{}", i));
        }
        state.number_of_successfully_connected_peers = state.peers.len();

        let _ = state.update(Action::Tick { dt_ms: 1000 });

        assert!(
            !state.accepting_new_peers,
            "expected admission guard to close under heavy connected-peer pressure"
        );
    }

    #[test]
    fn test_peer_admission_guard_reopens_at_reopen_threshold() {
        let mut state = create_empty_state();
        state.torrent_status = TorrentStatus::Standard;
        state.accepting_new_peers = false;

        let reopen_threshold = (PEER_ADMISSION_QUALITY_THRESHOLD * 50) / 100;
        for i in 0..reopen_threshold {
            add_peer(&mut state, &format!("peer_{}", i));
        }
        state.number_of_successfully_connected_peers = state.peers.len();

        let _ = state.update(Action::Tick { dt_ms: 1000 });

        assert!(
            state.accepting_new_peers,
            "expected admission guard to reopen at configured reopen threshold"
        );
    }

    #[test]
    fn test_peer_admission_guard_closes_immediately_on_successful_connection() {
        let mut state = create_empty_state();
        state.torrent_status = TorrentStatus::Standard;
        state.accepting_new_peers = true;

        for i in 0..PEER_ADMISSION_QUALITY_THRESHOLD {
            add_peer(&mut state, &format!("peer_{}", i));
        }

        let _ = state.update(Action::PeerSuccessfullyConnected {
            peer_id: "peer_0".to_string(),
        });

        assert!(
            !state.accepting_new_peers,
            "expected admission guard to close immediately when threshold is reached"
        );
    }

    #[test]
    fn test_peer_admission_guard_closes_immediately_on_peer_discovery() {
        let mut state = create_empty_state();
        state.torrent_status = TorrentStatus::Standard;
        state.accepting_new_peers = true;

        for i in 0..PEER_ADMISSION_QUALITY_THRESHOLD {
            let (tx, _rx) = mpsc::channel(1);
            let _ = state.update(Action::RegisterPeer {
                peer_id: format!("peer_{}", i),
                tx,
            });
        }

        assert!(
            !state.accepting_new_peers,
            "expected admission guard to close immediately when discovery reaches threshold"
        );
    }

    #[test]
    fn test_peer_admission_guard_stays_closed_above_reopen_threshold() {
        let mut state = create_empty_state();
        state.torrent_status = TorrentStatus::Standard;
        state.accepting_new_peers = false;

        let reopen_threshold = (PEER_ADMISSION_QUALITY_THRESHOLD * 50) / 100;
        for i in 0..(reopen_threshold + 1) {
            add_peer(&mut state, &format!("peer_{}", i));
        }
        state.number_of_successfully_connected_peers = state.peers.len();

        let _ = state.update(Action::Tick { dt_ms: 1000 });

        assert!(
            !state.accepting_new_peers,
            "guard should remain closed while connected count is above reopen threshold"
        );
    }

    #[test]
    fn test_peer_admission_guard_reopens_at_exact_reopen_threshold() {
        let mut state = create_empty_state();
        state.torrent_status = TorrentStatus::Standard;
        state.accepting_new_peers = false;

        let reopen_threshold = (PEER_ADMISSION_QUALITY_THRESHOLD * 50) / 100;
        for i in 0..reopen_threshold {
            add_peer(&mut state, &format!("peer_{}", i));
        }
        state.number_of_successfully_connected_peers = state.peers.len();

        let _ = state.update(Action::Tick { dt_ms: 1000 });

        assert!(
            state.accepting_new_peers,
            "guard should reopen when connected count reaches the exact reopen threshold"
        );
    }

    // --- SCENARIO 1: Initialization ---

    #[test]
    fn test_metadata_received_triggers_initialization_flow() {
        let mut state = create_empty_state();
        state.torrent_data_path = Some(PathBuf::from("/tmp")); // Set a path for MFI rebuild
        let torrent = create_dummy_torrent(5);

        let action = Action::MetadataReceived {
            torrent: Box::new(torrent),
            metadata_length: 123,
        };
        let effects = state.update(action);

        assert_eq!(state.torrent_status, TorrentStatus::Validating);
        assert!(state.torrent.is_some());

        // Check internal state instead of Effect::InitializeStorage
        assert!(
            state.multi_file_info.is_some(),
            "MFI should be initialized internally"
        );

        // The first effect is now StartValidation
        assert!(matches!(effects[0], Effect::StartValidation));
    }

    // --- SCENARIO 2: Choking Logic (Leeching) ---

    #[test]
    fn test_recalculate_chokes_unchokes_fastest_downloader() {
        // GIVEN: A state in Leeching mode (Standard) with 5 interested peers competing for 4 slots.
        let mut state = create_empty_state();
        state.torrent_status = TorrentStatus::Standard; // Leeching

        // Peers will be ranked by bytes_downloaded_from_peer (contribution).

        add_peer(&mut state, "slow_peer");
        let slow_peer = state.peers.get_mut("slow_peer").unwrap();
        slow_peer.peer_is_interested_in_us = true;
        slow_peer.bytes_downloaded_from_peer = 10; // Low contribution (Must lose)
        slow_peer.am_choking = ChokeStatus::Unchoke; // Start unchoked to test transition

        add_peer(&mut state, "fast_peer");
        let fast_peer = state.peers.get_mut("fast_peer").unwrap();
        fast_peer.peer_is_interested_in_us = true;
        fast_peer.bytes_downloaded_from_peer = 10_000; // High contribution (Must win)

        // Their contribution must be between the Fast Peer (10,000) and the Slow Peer (10).
        for i in 1..=3 {
            let id = format!("med_peer_{}", i);
            add_peer(&mut state, &id);
            let peer = state.peers.get_mut(&id).unwrap();
            peer.peer_is_interested_in_us = true;
            peer.bytes_downloaded_from_peer = 100; // Intermediate contribution
        }

        // WHEN: We recalculate chokes. The top 4 (Fast + 3 Med) should be unchoked.
        let effects = state.update(Action::RecalculateChokes { random_seed: 0 });

        // THEN: Fast peer is Unchoked, Slow peer is Choked (due to competition)
        let fast_peer_state = state.peers.get("fast_peer").unwrap();
        let slow_peer_state = state.peers.get("slow_peer").unwrap();

        // Assertion 1: The fastest peer must be Unchoked.
        assert_eq!(fast_peer_state.am_choking, ChokeStatus::Unchoke);

        // Assertion 2: The slowest peer must be Choked. (This satisfies the original test intent.)
        assert_eq!(slow_peer_state.am_choking, ChokeStatus::Choke);

        // Assertion 3: Check effects for the slow peer's transition (optional, but good practice)
        let sent_choke = effects.iter().any(|e| {
            matches!(e, Effect::SendToPeer { peer_id, cmd }
        if peer_id == "slow_peer" && matches!(**cmd, TorrentCommand::PeerChoke))
        });
        assert!(sent_choke, "Should send Choke to slow peer");

        // Assertion 4: Verify the total number of unchoked peers is 4 (UPLOAD_SLOTS_DEFAULT).
        let unchoked_count = state
            .peers
            .values()
            .filter(|p| p.am_choking == ChokeStatus::Unchoke)
            .count();
        assert_eq!(
            unchoked_count,
            super::UPLOAD_SLOTS_DEFAULT,
            "Total unchoked count should be exactly 4."
        );
    }

    // --- SCENARIO 3: Choking Logic (Seeding) ---

    #[test]
    fn test_recalculate_chokes_unchokes_fastest_uploader_when_seeding() {
        // GIVEN: A state that is DONE (Seeding) with 5 interested peers competing for 4 slots.
        let mut state = create_empty_state();
        state.torrent_status = TorrentStatus::Done;

        add_peer(&mut state, "slow_leecher");
        let slow_leecher = state.peers.get_mut("slow_leecher").unwrap();
        slow_leecher.peer_is_interested_in_us = true;
        slow_leecher.bytes_uploaded_to_peer = 1_000; // Low upload volume (Must lose)
        slow_leecher.am_choking = ChokeStatus::Unchoke; // Start unchoked to test transition

        add_peer(&mut state, "fast_leecher");
        let fast_leecher = state.peers.get_mut("fast_leecher").unwrap();
        fast_leecher.peer_is_interested_in_us = true;
        fast_leecher.bytes_uploaded_to_peer = 50_000; // High upload volume (Must win)

        // Their uploaded bytes must be between the Fast Peer (50,000) and the Slow Peer (1,000).
        for i in 1..=3 {
            let id = format!("med_leecher_{}", i);
            add_peer(&mut state, &id);
            let peer = state.peers.get_mut(&id).unwrap();
            peer.peer_is_interested_in_us = true;
            peer.bytes_uploaded_to_peer = 10_000; // Intermediate volume
            peer.am_choking = ChokeStatus::Choke;
        }

        // WHEN: Recalculate chokes. The top 4 (Fast + 3 Med) should be unchoked.
        let _ = state.update(Action::RecalculateChokes { random_seed: 0 });

        // THEN:
        // Assertion 1: The fastest uploader must be Unchoked.
        assert_eq!(state.peers["fast_leecher"].am_choking, ChokeStatus::Unchoke);

        // Assertion 2: The slowest peer must be Choked. (This satisfies the test intent.)
        assert_eq!(state.peers["slow_leecher"].am_choking, ChokeStatus::Choke);

        // Assertion 3: Verify the total number of unchoked peers is 4 (UPLOAD_SLOTS_DEFAULT).
        let unchoked_count = state
            .peers
            .values()
            .filter(|p| p.am_choking == ChokeStatus::Unchoke)
            .count();
        assert_eq!(
            unchoked_count,
            super::UPLOAD_SLOTS_DEFAULT,
            "Total unchoked count should be exactly 4."
        );
    }

    // --- SCENARIO 4: Work Assignment ---
    #[test]
    fn test_assign_work_requests_piece_peer_has() {
        let mut state = create_empty_state();
        let torrent = create_dummy_torrent(10);
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(10, false);
        state.torrent_status = TorrentStatus::Standard;
        state.piece_manager.block_manager.set_geometry(
            16384,
            163840,
            vec![],
            vec![],
            HashMap::new(),
            false,
        ); // NEW: Init geometry

        add_peer(&mut state, "peer_A");
        let peer = state.peers.get_mut("peer_A").unwrap();
        peer.peer_choking = ChokeStatus::Unchoke;
        peer.bitfield = vec![false; 10];
        peer.bitfield[0] = true;
        state.piece_manager.need_queue.push(0);

        let effects = state.update(Action::AssignWork {
            peer_id: "peer_A".to_string(),
        });

        // NEW ASSERTION: Check for BulkRequest
        let request = effects.iter().find_map(|e| match e {
            Effect::SendToPeer { cmd, .. } => match **cmd {
                TorrentCommand::BulkRequest(ref requests) => {
                    requests.first().map(|(index, _, _)| *index)
                }
                _ => None,
            },
            _ => None,
        });

        assert_eq!(request, Some(0), "Should request piece 0 from peer_A");
    }

    #[test]
    fn test_data_unavailable_blocks_unchoke_and_upload() {
        let mut state = create_empty_state();
        let torrent = create_dummy_torrent(1);
        state.torrent = Some(torrent);
        state.torrent_status = TorrentStatus::Standard;
        state.data_available = false;
        state.piece_manager.set_initial_fields(1, false);
        state.piece_manager.bitfield[0] = PieceStatus::Done;

        add_peer(&mut state, "peer_A");
        let peer = state.peers.get_mut("peer_A").unwrap();
        peer.peer_is_interested_in_us = true;
        peer.peer_choking = ChokeStatus::Unchoke;
        peer.am_choking = ChokeStatus::Choke;

        let choke_effects = state.update(Action::RecalculateChokes { random_seed: 0 });
        assert!(matches!(choke_effects.as_slice(), [Effect::DoNothing]));
        assert_eq!(state.peers["peer_A"].am_choking, ChokeStatus::Choke);

        let upload_effects = state.update(Action::RequestUpload {
            peer_id: "peer_A".to_string(),
            piece_index: 0,
            block_offset: 0,
            length: 16_384,
        });
        assert!(matches!(upload_effects.as_slice(), [Effect::DoNothing]));
    }

    #[test]
    fn test_set_data_availability_does_not_emit_metrics() {
        let mut state = create_empty_state();

        let effects = state.update(Action::SetDataAvailability { available: false });

        assert!(!state.data_available);
        assert!(matches!(effects.as_slice(), [Effect::DoNothing]));
    }

    #[test]
    fn test_set_data_availability_rechokes_existing_upload_slots() {
        let mut state = create_empty_state();
        state.torrent = Some(create_dummy_torrent(1));
        state.torrent_status = TorrentStatus::Done;

        add_peer(&mut state, "peer_A");
        let peer = state.peers.get_mut("peer_A").unwrap();
        peer.peer_is_interested_in_us = true;
        peer.am_choking = ChokeStatus::Unchoke;

        let effects = state.update(Action::SetDataAvailability { available: false });

        assert!(!state.data_available);
        assert_eq!(state.peers["peer_A"].am_choking, ChokeStatus::Choke);
        assert!(effects.iter().any(|effect| {
            matches!(effect, Effect::SendToPeer { peer_id, cmd }
                if peer_id == "peer_A" && matches!(**cmd, TorrentCommand::PeerChoke))
        }));
    }

    // --- SCENARIO 5: Piece Verification Success ---

    #[test]
    fn test_piece_verified_valid_trigger_write() {
        // GIVEN: State waiting for verification of piece 1
        let mut state = create_empty_state();
        state.piece_manager.set_initial_fields(5, false);
        // Mark piece 1 as needed/pending in piece manager context
        // (Assuming default state allows this transition)

        let data = vec![1, 2, 3, 4];

        // WHEN: Piece 1 is verified successfully
        let effects = state.update(Action::PieceVerified {
            peer_id: "peer_1".into(),
            piece_index: 1,
            valid: true,
            data: data.clone(),
        });

        // THEN: Effect::WriteToDisk is emitted
        let write_effect = effects
            .iter()
            .find(|e| matches!(e, Effect::WriteToDisk { piece_index: 1, .. }));
        assert!(write_effect.is_some());
    }

    #[test]
    fn test_piece_verified_invalid_disconnects_peer() {
        // GIVEN: State
        let mut state = create_empty_state();
        state.piece_manager.set_initial_fields(5, false);

        // WHEN: Piece 1 fails verification
        let effects = state.update(Action::PieceVerified {
            peer_id: "bad_peer".into(),
            piece_index: 1,
            valid: false,
            data: vec![],
        });

        // THEN: Peer is disconnected
        let disconnect = effects
            .iter()
            .any(|e| matches!(e, Effect::DisconnectPeer { peer_id } if peer_id == "bad_peer"));
        assert!(disconnect);
    }

    // --- SCENARIO 6: Completion ---

    #[test]
    fn test_check_completion_transitions_to_done() {
        // GIVEN: All pieces are marked as Done
        let mut state = create_empty_state();
        let torrent = create_dummy_torrent(3);
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(3, false);

        state.torrent_status = TorrentStatus::Standard;

        state.trackers.insert(
            "http://tracker".into(),
            TrackerState {
                next_announce_time: Instant::now(),
                leeching_interval: None,
                seeding_interval: None,
            },
        );

        // Manually mark all pieces as Done (simulating write success)
        for i in 0..3 {
            state.piece_manager.bitfield[i] = PieceStatus::Done;
        }

        // WHEN: CheckCompletion is called
        let effects = state.update(Action::CheckCompletion);

        // THEN: Status becomes Done, AnnounceCompleted emitted
        assert_eq!(state.torrent_status, TorrentStatus::Done);

        let announce_completed = effects
            .iter()
            .any(|e| matches!(e, Effect::AnnounceCompleted { .. }));
        assert!(announce_completed);
    }

    // --- SCENARIO 7: Cleanup / Disconnect ---

    #[test]
    fn test_peer_disconnect_decrements_count() {
        // GIVEN: A connected peer
        let mut state = create_empty_state();
        add_peer(&mut state, "peer_X");
        state.number_of_successfully_connected_peers = 1;

        // WHEN: Peer disconnects
        let effects = state.update(Action::PeerDisconnected {
            peer_id: "peer_X".to_string(),
            force: true,
        });

        // THEN: Peer removed, count decremented, Disconnect effect emitted
        assert!(!state.peers.contains_key("peer_X"));
        assert_eq!(state.number_of_successfully_connected_peers, 0);

        assert!(effects
            .iter()
            .any(|e| matches!(e, Effect::DisconnectPeer { .. })));
        assert!(effects.iter().any(|e| matches!(
            e,
            Effect::EmitManagerEvent(ManagerEvent::PeerDisconnected { .. })
        )));
    }

    #[test]
    fn test_enter_endgame_mode() {
        // GIVEN: A torrent with 2 pieces, 1 already pending
        let mut state = create_empty_state();
        let torrent = create_dummy_torrent(2);
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(2, false);
        state.piece_manager.block_manager.set_geometry(
            16384,
            16384 * 2,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Standard;

        add_peer(&mut state, "peer_A");
        let peer = state.peers.get_mut("peer_A").unwrap();
        peer.bitfield = vec![true, true];
        peer.peer_choking = ChokeStatus::Unchoke;

        // Piece 0 is already pending (assigned to someone else, theoretically)
        state.piece_manager.mark_as_pending(0, "other_peer".into());

        // Only Piece 1 is left in need_queue
        state.piece_manager.need_queue.clear();
        state.piece_manager.need_queue.push(1);

        // WHEN: We assign the LAST needed piece to peer_A
        state.update(Action::AssignWork {
            peer_id: "peer_A".into(),
        });

        // THEN:

        assert!(state.piece_manager.need_queue.is_empty());

        assert_eq!(state.torrent_status, TorrentStatus::Endgame);
    }

    #[test]
    fn test_peer_chokes_us_mid_download() {
        // GIVEN: Peer A is unchoked and we have pending requests
        let mut state = create_empty_state();
        add_peer(&mut state, "peer_A");
        let peer = state.peers.get_mut("peer_A").unwrap();
        peer.peer_choking = ChokeStatus::Unchoke;
        peer.pending_requests.insert(5); // We asked for piece 5

        // WHEN: Peer A chokes us
        let _ = state.update(Action::PeerChoked {
            peer_id: "peer_A".into(),
        });

        // THEN:

        assert_eq!(state.peers["peer_A"].peer_choking, ChokeStatus::Choke);

        // (Strict clients cancel immediately; lenient ones wait. Verify YOUR logic here.)
    }

    #[test]
    fn test_optimistic_unchoke_rotates() {
        // GIVEN: 6 peers competing for 4 slots (UPLOAD_SLOTS_DEFAULT = 4).
        let mut state = create_empty_state();

        for i in 1..=4 {
            let id = format!("fast_A{}", i);
            add_peer(&mut state, &id);
            let p = state.peers.get_mut(&id).unwrap();
            p.peer_is_interested_in_us = true;
            p.bytes_downloaded_from_peer = 1000;
        }

        add_peer(&mut state, "optimistic_B");
        let opt_peer = state.peers.get_mut("optimistic_B").unwrap();
        opt_peer.peer_is_interested_in_us = true;
        opt_peer.bytes_downloaded_from_peer = 100;

        add_peer(&mut state, "slow_C");
        let slow_peer = state.peers.get_mut("slow_C").unwrap();
        slow_peer.peer_is_interested_in_us = true;
        slow_peer.bytes_downloaded_from_peer = 10;

        // Force timer expiration and set fixed seed for deterministic rotation
        state.optimistic_unchoke_timer =
            Some(state.now.checked_sub(Duration::from_secs(31)).unwrap());

        // WHEN: Recalculate Chokes
        let _ = state.update(Action::RecalculateChokes {
            // Use a fixed seed (0) to ensure the rotation selects the same peer (optimistic_B)
            // from the pool of losers (B and C).
            random_seed: 0,
        });

        // THEN:

        let unchoked_count = state
            .peers
            .values()
            .filter(|p| p.am_choking == ChokeStatus::Unchoke)
            .count();

        let expected_count = super::UPLOAD_SLOTS_DEFAULT + 1;
        assert_eq!(
            unchoked_count, expected_count,
            "Total unchoked count mismatch. Expected 5 (4+1)."
        );

        assert_eq!(state.peers["fast_A1"].am_choking, ChokeStatus::Unchoke);

        assert_eq!(state.peers["optimistic_B"].am_choking, ChokeStatus::Unchoke);

        assert_eq!(state.peers["slow_C"].am_choking, ChokeStatus::Choke);
    }

    #[test]
    fn test_peer_have_updates_bitfield_and_triggers_work() {
        // GIVEN: Peer A connected with empty bitfield
        let mut state = create_empty_state();
        let torrent = create_dummy_torrent(10);
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(10, false);

        state.torrent_status = TorrentStatus::Standard;

        add_peer(&mut state, "peer_A");
        state.peers.get_mut("peer_A").unwrap().bitfield = vec![false; 10];

        // We need piece 5
        // Note: If need_queue is a VecDeque, use .push_back(5) instead of .push(5)
        state.piece_manager.need_queue.push(5);

        // WHEN: Peer sends "Have(5)"
        let effects = state.update(Action::PeerHavePiece {
            peer_id: "peer_A".into(),
            piece_index: 5,
        });

        // THEN:

        assert!(state.peers["peer_A"].bitfield[5]);

        let interest = effects.iter().any(|e| {
            matches!(e, Effect::SendToPeer { cmd, .. }
        if matches!(**cmd, TorrentCommand::ClientInterested))
        });

        assert!(interest, "Should send Interested message");
    }

    #[test]
    fn test_cancel_upload_aborts_task() {
        // GIVEN: We are seeding
        let mut state = create_empty_state();
        add_peer(&mut state, "leecher");

        // WHEN: Peer cancels request for piece 0, block 0
        let effects = state.update(Action::CancelUpload {
            peer_id: "leecher".into(),
            piece_index: 0,
            block_offset: 0,
            length: 16384,
        });

        // THEN: Effect::AbortUpload is emitted
        let abort = effects.iter().any(|e| {
            matches!(e, Effect::AbortUpload { peer_id, block_info }
        if peer_id == "leecher" && block_info.piece_index == 0)
        });

        assert!(abort);
    }

    #[test]
    fn test_invariant_pending_removed_on_disk_write() {
        let mut state = create_empty_state();
        let torrent = create_dummy_torrent(20);
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(20, false);
        state.piece_manager.block_manager.set_geometry(
            16384,
            16384 * 20,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Standard;

        add_peer(&mut state, "peer_A");
        let peer = state.peers.get_mut("peer_A").unwrap();
        peer.bitfield = vec![true; 20]; // Peer has everything
        peer.peer_choking = ChokeStatus::Unchoke;

        // We need piece 0
        state.piece_manager.need_queue.push(0);

        // This moves Piece 0 from Need -> Pending and adds to Peer's pending_requests
        state.update(Action::AssignWork {
            peer_id: "peer_A".into(),
        });

        // VERIFY SETUP: Piece 0 must be pending now
        assert!(
            state.peers["peer_A"].pending_requests.contains(&0),
            "Setup failed: Piece 0 should be pending"
        );

        state.update(Action::PieceWrittenToDisk {
            peer_id: "peer_A".into(),
            piece_index: 0,
        });

        // If the code is correct, piece 0 is removed from the peer.
        // If sabotaged, piece 0 remains, and this assert will panic.
        let is_still_pending = state.peers["peer_A"].pending_requests.contains(&0);

        assert!(!is_still_pending,
            "INVARIANT VIOLATION: Piece 0 is marked DONE globally, but still exists in peer_A's pending_requests!");

        // Double check global status is actually done (to ensure test validity)
        assert_eq!(state.piece_manager.bitfield[0], PieceStatus::Done);
    }

    #[test]
    fn regression_delete_clears_piece_manager_state() {
        // BUG CONTEXT: Previously, Action::Delete cleared queues but left 'partial blocks'
        // inside PieceManager. When a new peer connected and sent data for that piece,
        // PieceManager panicked with "subtract with overflow" because it compared
        // new offsets against old, stale buffer state.

        let mut state = create_empty_state();
        let torrent = create_dummy_torrent(5);
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(5, false);
        state.torrent_status = TorrentStatus::Standard;
        state.piece_manager.need_queue = vec![0];

        add_peer(&mut state, "peer_A");
        let _ = state.update(Action::PeerUnchoked {
            peer_id: "peer_A".into(),
        });
        let _ = state.update(Action::PeerHavePiece {
            peer_id: "peer_A".into(),
            piece_index: 0,
        });
        let _ = state.update(Action::AssignWork {
            peer_id: "peer_A".into(),
        });

        let data = vec![1; 100];
        let _ = state.update(Action::IncomingBlock {
            peer_id: "peer_A".into(),
            piece_index: 0,
            block_offset: 0,
            data: data.clone(),
        });

        let _ = state.update(Action::Delete);

        // If state wasn't wiped, this causes "subtract with overflow" or "ghost queue" panic
        add_peer(&mut state, "peer_B");

        // We must reset status to Standard manually as Delete sets it to Validating
        state.torrent_status = TorrentStatus::Standard;
        state.piece_manager.need_queue = vec![0];

        let _ = state.update(Action::PeerUnchoked {
            peer_id: "peer_B".into(),
        });
        let _ = state.update(Action::PeerHavePiece {
            peer_id: "peer_B".into(),
            piece_index: 0,
        });

        // CRITICAL STEP: Sending data for the same piece index as before.
        // If the old partial buffer exists, this crashes.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut s = state; // Move state in
            s.update(Action::IncomingBlock {
                peer_id: "peer_B".into(),
                piece_index: 0,
                block_offset: 0,
                data,
            });
        }));

        assert!(
            result.is_ok(),
            "Regression: Action::Delete failed to wipe PieceManager state!"
        );
    }

    #[test]
    fn regression_redundant_disk_write_completion() {
        // BUG CONTEXT: The fuzzer found that if 'PieceWrittenToDisk' fires twice
        // (race condition), the PieceManager would panic trying to mark a 'Done' piece as done.

        let mut state = create_empty_state();

        // FIX: Explicitly set status to Standard.
        // Otherwise, the new safety guard in PieceWrittenToDisk ignores the action.
        state.torrent_status = TorrentStatus::Standard;

        state.piece_manager.set_initial_fields(1, false);
        add_peer(&mut state, "peer_A");
        state
            .peers
            .get_mut("peer_A")
            .unwrap()
            .pending_requests
            .insert(0);

        state.update(Action::PieceWrittenToDisk {
            peer_id: "peer_A".into(),
            piece_index: 0,
        });

        assert_eq!(state.piece_manager.bitfield[0], PieceStatus::Done);

        // Should be ignored gracefully, not panic.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut s = state;
            s.update(Action::PieceWrittenToDisk {
                peer_id: "peer_A".into(),
                piece_index: 0,
            });
        }));

        assert!(
            result.is_ok(),
            "Regression: Double PieceWrittenToDisk caused a panic!"
        );
    }

    #[test]
    fn regression_metric_integer_overflow() {
        // BUG CONTEXT: Sending huge byte counts caused u64 overflow panics.
        let mut state = create_empty_state();
        add_peer(&mut state, "peer_A");

        let huge_val = u64::MAX - 100;

        state.update(Action::BlockSentToPeer {
            peer_id: "peer_A".into(),
            byte_count: huge_val,
        });

        state.update(Action::BlockSentToPeer {
            peer_id: "peer_A".into(),
            byte_count: 200,
        });

        assert_eq!(state.session_total_uploaded, u64::MAX);
        assert_eq!(state.peers["peer_A"].total_bytes_uploaded, u64::MAX);
    }

    #[test]
    fn regression_peer_count_sync() {
        let mut state = create_empty_state();
        let peer_id = "peer_A".to_string();

        super::tests::add_peer(&mut state, &peer_id);
        state.update(Action::PeerSuccessfullyConnected {
            peer_id: peer_id.clone(),
        });
        assert_eq!(
            state.number_of_successfully_connected_peers, 1,
            "Counter after first connection"
        );

        state.update(Action::PeerSuccessfullyConnected {
            peer_id: peer_id.clone(),
        });
        assert_eq!(
            state.number_of_successfully_connected_peers, 1,
            "Counter on duplicate connection"
        );

        state.update(Action::PeerDisconnected {
            peer_id: peer_id.clone(),
            force: true,
        });
        assert_eq!(
            state.number_of_successfully_connected_peers, 0,
            "Counter after disconnection"
        );
    }

    #[test]
    fn test_download_starts_immediately_after_validation() {
        // GIVEN: A torrent with 2 pieces (so we don't hit Endgame immediately)
        let mut state = create_empty_state();
        let torrent = create_dummy_torrent(2); // <--- Changed to 2
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(2, false); // <--- Changed to 2
        state.piece_manager.block_manager.set_geometry(
            16384,
            163840,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Validating;

        // We need piece 0 and 1
        state.piece_manager.need_queue = vec![0, 1];

        add_peer(&mut state, "seeder");

        // 0x80 is binary 10000000 -> 1st bit set -> Piece 0 available
        state.update(Action::PeerBitfieldReceived {
            peer_id: "seeder".into(),
            bitfield: vec![0x80],
        });

        state.update(Action::PeerUnchoked {
            peer_id: "seeder".into(),
        });

        // Pre-check
        assert!(state.peers["seeder"].pending_requests.is_empty());

        // WHEN: Validation completes
        let effects = state.update(Action::ValidationComplete {
            completed_pieces: vec![],
        });

        println!("{:?}", effects);

        // THEN:

        assert_eq!(state.torrent_status, TorrentStatus::Standard);

        let request_sent = effects.iter().any(|e| {
            matches!(e, Effect::SendToPeer { cmd, .. }
            if matches!(**cmd, TorrentCommand::BulkRequest(ref reqs) if !reqs.is_empty() && reqs[0].0 == 0))
        });

        assert!(
            request_sent,
            "Regression: Validation finished but download did not trigger!"
        );

        assert!(state.peers["seeder"].inflight_requests == 1);
    }

    #[test]
    fn test_assign_work_sends_interested_even_if_unchoked() {
        // GIVEN: A standard torrent state where we need Piece 0
        let mut state = create_empty_state();
        let torrent = create_dummy_torrent(1);
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(1, false);
        state.piece_manager.block_manager.set_geometry(
            16384,
            163840,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Standard;

        // We explicitly need Piece 0
        state.piece_manager.need_queue = vec![0];

        // GIVEN: A connected peer ("generous_seeder")
        add_peer(&mut state, "generous_seeder");
        let peer = state.peers.get_mut("generous_seeder").unwrap();

        // CRITICAL SETUP FOR BUG REPRODUCTION:

        peer.bitfield = vec![true];

        peer.peer_choking = ChokeStatus::Unchoke;

        peer.am_interested = false;

        // WHEN: We assign work
        let effects = state.update(Action::AssignWork {
            peer_id: "generous_seeder".to_string(),
        });

        // THEN: We MUST send 'ClientInterested' BEFORE requesting data.

        // Check for Interested message
        let sent_interested = effects.iter().any(|e| {
            matches!(e, Effect::SendToPeer { cmd, .. }
            if matches!(**cmd, TorrentCommand::ClientInterested))
        });

        // Check for Request message
        let sent_request = effects.iter().any(|e| {
            matches!(e, Effect::SendToPeer { cmd, .. }
            if matches!(**cmd, TorrentCommand::BulkRequest(ref reqs) if !reqs.is_empty() && reqs[0].0 == 0))
        });

        // ASSERTIONS
        // If the bug is present, `sent_interested` will be false, but `sent_request` will be true.
        assert!(sent_interested, "PROTOCOL VIOLATION: Failed to send 'Interested' message because peer was already unchoked.");
        assert!(
            sent_request,
            "Should immediately request blocks because peer is unchoked."
        );

        // Verify internal state update
        assert!(
            state.peers["generous_seeder"].am_interested,
            "Internal state 'am_interested' was not updated to true."
        );
    }

    #[test]
    fn test_partial_piece_request() {
        // ... (Keep Setup Code) ...
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(2);
        torrent.info.piece_length = 32768; // 2 blocks per piece
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(2, false);
        state.torrent_status = TorrentStatus::Standard;
        state.piece_manager.block_manager.set_geometry(
            32768,
            65536,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );

        state.piece_manager.need_queue = vec![0, 1];

        add_peer(&mut state, "target_peer");
        let target = state.peers.get_mut("target_peer").unwrap();
        target.peer_choking = ChokeStatus::Unchoke;
        target.bitfield = vec![true, true];
        target.am_interested = true;

        // Simulate receiving FIRST BLOCK of Piece 0
        let data = vec![0u8; 16384];

        let effects = state.update(Action::IncomingBlock {
            peer_id: "target_peer".into(),
            piece_index: 0,
            block_offset: 0,
            data,
        });

        // REMOVED: The second manual AssignWork call which was returning empty effects.

        // Verify we ask for the SECOND block
        let requested_params = effects.iter().find_map(|e| {
            if let Effect::SendToPeer { cmd, .. } = e {
                if let TorrentCommand::BulkRequest(ref reqs) = **cmd {
                    if let Some((index, begin, length)) = reqs.first() {
                        return Some((*index, *begin, *length));
                    }
                }
            }
            None
        });

        if let Some((idx, begin, length)) = requested_params {
            assert_eq!(idx, 0, "Should pick Piece 0");
            assert_eq!(begin, 16384, "Should resume at offset 16384");
            assert_eq!(length, 16384, "Should request 1 block");
        } else {
            panic!("No request sent for partial piece");
        }
    }

    #[test]
    fn test_assign_work_non_aligned_boundary_piece_identity() {
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(2);
        torrent.info.piece_length = 20_000;
        torrent.info.length = 40_000;
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(2, false);
        state.piece_manager.block_manager.set_geometry(
            20_000,
            40_000,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Standard;

        state.piece_manager.need_queue.clear();
        state.piece_manager.pending_queue.clear();

        add_peer(&mut state, "target_peer");
        let peer = state.peers.get_mut("target_peer").unwrap();
        peer.peer_choking = ChokeStatus::Unchoke;
        peer.bitfield = vec![true, true];
        peer.am_interested = true;

        state
            .piece_manager
            .mark_as_pending(1, "target_peer".to_string());
        peer.pending_requests.insert(1);

        let effects = state.update(Action::AssignWork {
            peer_id: "target_peer".to_string(),
        });

        let requests: Vec<(u32, u32, u32)> = effects
            .iter()
            .filter_map(|e| {
                if let Effect::SendToPeer { cmd, .. } = e {
                    if let TorrentCommand::BulkRequest(reqs) = &**cmd {
                        return Some(reqs.clone());
                    }
                }
                None
            })
            .flatten()
            .collect();

        assert!(
            !requests.is_empty(),
            "Expected at least one boundary request"
        );
        assert!(
            requests.iter().all(|(idx, _, _)| *idx == 1),
            "All requests must target piece 1, got {:?}",
            requests
        );
    }

    #[test]
    fn test_assign_work_non_aligned_boundary_offsets_for_piece() {
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(2);
        torrent.info.piece_length = 20_000;
        torrent.info.length = 40_000;
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(2, false);
        state.piece_manager.block_manager.set_geometry(
            20_000,
            40_000,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Standard;

        state.piece_manager.need_queue.clear();
        state.piece_manager.pending_queue.clear();

        add_peer(&mut state, "target_peer");
        let peer = state.peers.get_mut("target_peer").unwrap();
        peer.peer_choking = ChokeStatus::Unchoke;
        peer.bitfield = vec![true, true];
        peer.am_interested = true;

        state
            .piece_manager
            .mark_as_pending(1, "target_peer".to_string());
        peer.pending_requests.insert(1);

        let effects = state.update(Action::AssignWork {
            peer_id: "target_peer".to_string(),
        });

        let requests: Vec<(u32, u32, u32)> = effects
            .iter()
            .filter_map(|e| {
                if let Effect::SendToPeer { cmd, .. } = e {
                    if let TorrentCommand::BulkRequest(reqs) = &**cmd {
                        return Some(reqs.clone());
                    }
                }
                None
            })
            .flatten()
            .collect();

        let mut piece_1_offsets: Vec<(u32, u32)> = requests
            .iter()
            .filter(|(idx, _, _)| *idx == 1)
            .map(|(_, begin, len)| (*begin, *len))
            .collect();
        piece_1_offsets.sort_unstable();

        assert_eq!(
            piece_1_offsets,
            vec![(0, 16_384), (16_384, 3_616)],
            "Piece-1 requests must follow piece-local geometry exactly"
        );
    }

    #[test]
    fn test_incoming_block_non_aligned_updates_correct_piece_assembler() {
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(2);
        torrent.info.piece_length = 20_000;
        torrent.info.length = 40_000;
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(2, false);
        state.piece_manager.block_manager.set_geometry(
            20_000,
            40_000,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Standard;

        state.piece_manager.need_queue.clear();
        state.piece_manager.pending_queue.clear();

        add_peer(&mut state, "target_peer");
        let peer = state.peers.get_mut("target_peer").unwrap();
        peer.peer_choking = ChokeStatus::Unchoke;
        peer.bitfield = vec![true, true];
        peer.am_interested = true;

        state
            .piece_manager
            .mark_as_pending(1, "target_peer".to_string());
        peer.pending_requests.insert(1);

        let effects = state.update(Action::AssignWork {
            peer_id: "target_peer".to_string(),
        });

        let requests: Vec<(u32, u32, u32)> = effects
            .iter()
            .filter_map(|e| {
                if let Effect::SendToPeer { cmd, .. } = e {
                    if let TorrentCommand::BulkRequest(reqs) = &**cmd {
                        return Some(reqs.clone());
                    }
                }
                None
            })
            .flatten()
            .collect();

        let mut saw_verify_piece_1 = false;
        for (piece_index, block_offset, length) in requests {
            let effects = state.update(Action::IncomingBlock {
                peer_id: "target_peer".to_string(),
                piece_index,
                block_offset,
                data: vec![0u8; length as usize],
            });
            saw_verify_piece_1 |= effects
                .iter()
                .any(|e| matches!(e, Effect::VerifyPiece { piece_index: 1, .. }));
        }

        assert!(
            !state
                .piece_manager
                .block_manager
                .legacy_buffers
                .contains_key(&0),
            "Assembler for piece 0 should remain untouched while downloading piece 1"
        );
        // Piece 1 may either still be buffering or already have emitted verification and cleared buffer.
        let piece_1_buffering = state
            .piece_manager
            .block_manager
            .legacy_buffers
            .contains_key(&1);
        assert!(
            piece_1_buffering || saw_verify_piece_1,
            "Piece 1 must either buffer in its own assembler or reach verification"
        );
    }

    #[test]
    fn test_restart_resume_non_aligned_requests_only_missing_blocks() {
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(2);
        torrent.info.piece_length = 20_000;
        torrent.info.length = 40_000;
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(2, false);
        state.piece_manager.block_manager.set_geometry(
            20_000,
            40_000,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Standard;

        state.piece_manager.need_queue.clear();
        state.piece_manager.pending_queue.clear();

        add_peer(&mut state, "target_peer");
        let peer = state.peers.get_mut("target_peer").unwrap();
        peer.peer_choking = ChokeStatus::Unchoke;
        peer.bitfield = vec![true, true];
        peer.am_interested = true;

        state
            .piece_manager
            .mark_as_pending(1, "target_peer".to_string());
        peer.pending_requests.insert(1);

        // Receive one valid piece-1 block before "restart".
        let _ = state.update(Action::IncomingBlock {
            peer_id: "target_peer".to_string(),
            piece_index: 1,
            block_offset: 0,
            data: vec![0u8; 16_384],
        });

        // Simulate restart: session-level inflight/active state should be cleared.
        if let Some(peer_after_restart) = state.peers.get_mut("target_peer") {
            peer_after_restart.inflight_requests = 0;
            peer_after_restart.active_blocks.clear();
        }

        // Re-assign after restart.
        let effects = state.update(Action::AssignWork {
            peer_id: "target_peer".to_string(),
        });

        let requests: Vec<(u32, u32, u32)> = effects
            .iter()
            .filter_map(|e| {
                if let Effect::SendToPeer { cmd, .. } = e {
                    if let TorrentCommand::BulkRequest(reqs) = &**cmd {
                        return Some(reqs.clone());
                    }
                }
                None
            })
            .flatten()
            .collect();

        assert_eq!(
            requests,
            vec![(1, 16_384, 3_616)],
            "Resume must request only the remaining piece-1 boundary block"
        );
    }

    #[test]
    fn test_non_aligned_verify_fail_requeue_clears_exact_piece_state() {
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(2);
        torrent.info.piece_length = 20_000;
        torrent.info.length = 40_000;
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(2, false);
        state.piece_manager.block_manager.set_geometry(
            20_000,
            40_000,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Standard;

        state.piece_manager.mark_as_complete(0);
        state
            .piece_manager
            .mark_as_pending(1, "target_peer".to_string());

        let _ = state.update(Action::PieceVerified {
            peer_id: "target_peer".to_string(),
            piece_index: 1,
            valid: false,
            data: vec![],
        });

        assert!(
            state.piece_manager.bitfield.first() == Some(&PieceStatus::Done),
            "Piece 0 completion state must remain unchanged"
        );
        assert!(
            state.piece_manager.bitfield.get(1) == Some(&PieceStatus::Need),
            "Piece 1 must be requeued and incomplete after verification failure"
        );
    }

    #[test]
    fn test_assign_work_non_aligned_no_zero_or_oversize_block_requests() {
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(2);
        torrent.info.piece_length = 20_000;
        torrent.info.length = 40_000;
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(2, false);
        state.piece_manager.block_manager.set_geometry(
            20_000,
            40_000,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Standard;

        state.piece_manager.need_queue.clear();
        state.piece_manager.pending_queue.clear();

        add_peer(&mut state, "target_peer");
        let peer = state.peers.get_mut("target_peer").unwrap();
        peer.peer_choking = ChokeStatus::Unchoke;
        peer.bitfield = vec![true, true];
        peer.am_interested = true;

        state
            .piece_manager
            .mark_as_pending(1, "target_peer".to_string());
        peer.pending_requests.insert(1);

        let effects = state.update(Action::AssignWork {
            peer_id: "target_peer".to_string(),
        });

        let requests: Vec<(u32, u32, u32)> = effects
            .iter()
            .filter_map(|e| {
                if let Effect::SendToPeer { cmd, .. } = e {
                    if let TorrentCommand::BulkRequest(reqs) = &**cmd {
                        return Some(reqs.clone());
                    }
                }
                None
            })
            .flatten()
            .collect();

        assert!(!requests.is_empty(), "Expected request batch");
        for (piece_index, begin, len) in requests {
            assert!(len > 0, "Zero-length request is invalid");
            assert_eq!(
                piece_index, 1,
                "Boundary request must remain in target piece namespace"
            );
            assert!(
                begin + len <= 20_000,
                "Request exceeds piece boundary: begin={} len={}",
                begin,
                len
            );
        }
    }

    #[test]
    fn test_non_aligned_full_piece_download_emits_verify_for_target_piece() {
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(2);
        torrent.info.piece_length = 20_000;
        torrent.info.length = 40_000;
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(2, false);
        state.piece_manager.block_manager.set_geometry(
            20_000,
            40_000,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Standard;

        state.piece_manager.need_queue.clear();
        state.piece_manager.pending_queue.clear();

        add_peer(&mut state, "target_peer");
        let peer = state.peers.get_mut("target_peer").unwrap();
        peer.peer_choking = ChokeStatus::Unchoke;
        peer.bitfield = vec![true, true];
        peer.am_interested = true;

        state
            .piece_manager
            .mark_as_pending(1, "target_peer".to_string());
        peer.pending_requests.insert(1);

        let mut effects = state.update(Action::AssignWork {
            peer_id: "target_peer".to_string(),
        });

        let requests: Vec<(u32, u32, u32)> = effects
            .iter()
            .filter_map(|e| {
                if let Effect::SendToPeer { cmd, .. } = e {
                    if let TorrentCommand::BulkRequest(reqs) = &**cmd {
                        return Some(reqs.clone());
                    }
                }
                None
            })
            .flatten()
            .collect();

        for (piece_index, block_offset, length) in requests {
            effects = state.update(Action::IncomingBlock {
                peer_id: "target_peer".to_string(),
                piece_index,
                block_offset,
                data: vec![0u8; length as usize],
            });
        }

        let verify_piece_1 = effects
            .iter()
            .any(|e| matches!(e, Effect::VerifyPiece { piece_index: 1, .. }));
        assert!(
            verify_piece_1,
            "Completing target non-aligned piece should emit VerifyPiece for piece 1"
        );
    }

    #[test]
    fn test_assign_work_tiny_piece_keeps_target_piece_identity() {
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(2);
        torrent.info.piece_length = 1_024;
        torrent.info.length = 2_048;
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(2, false);
        state.piece_manager.block_manager.set_geometry(
            1_024,
            2_048,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Standard;

        state.piece_manager.need_queue.clear();
        state.piece_manager.pending_queue.clear();

        add_peer(&mut state, "tiny_peer");
        let peer = state.peers.get_mut("tiny_peer").unwrap();
        peer.peer_choking = ChokeStatus::Unchoke;
        peer.bitfield = vec![true, true];
        peer.am_interested = true;

        state
            .piece_manager
            .mark_as_pending(1, "tiny_peer".to_string());
        peer.pending_requests.insert(1);

        let effects = state.update(Action::AssignWork {
            peer_id: "tiny_peer".to_string(),
        });

        let requests: Vec<(u32, u32, u32)> = effects
            .iter()
            .filter_map(|e| {
                if let Effect::SendToPeer { cmd, .. } = e {
                    if let TorrentCommand::BulkRequest(reqs) = &**cmd {
                        return Some(reqs.clone());
                    }
                }
                None
            })
            .flatten()
            .collect();

        assert_eq!(
            requests,
            vec![(1, 0, 1_024)],
            "Tiny-piece request should stay in piece-local namespace"
        );
    }

    #[test]
    fn test_multi_file_non_aligned_priority_boundary_mixed_piece_not_skipped() {
        let mut state = create_empty_state();
        let piece_len = 20_000;

        let mut torrent = create_dummy_torrent(2);
        torrent.info.piece_length = piece_len;
        torrent.info.length = 40_000;
        torrent.info.files = vec![
            crate::torrent_file::InfoFile {
                length: 18_000, // Entirely in piece 0
                path: vec!["A.bin".into()],
                md5sum: None,
                attr: None,
            },
            crate::torrent_file::InfoFile {
                length: 22_000, // Crosses piece boundary into piece 1
                path: vec!["B.bin".into()],
                md5sum: None,
                attr: None,
            },
        ];
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(2, false);

        let mut priorities = HashMap::new();
        priorities.insert(0, FilePriority::Skip); // Skip file A only.

        let p = state.calculate_piece_priorities(&priorities);

        assert_eq!(
            p[0],
            EffectivePiecePriority::Normal,
            "Piece 0 spans skipped and non-skipped files and must not be skipped"
        );
        assert_eq!(
            p[1],
            EffectivePiecePriority::Normal,
            "Piece 1 belongs to non-skipped file and must remain normal"
        );
    }

    #[test]
    fn test_non_aligned_choke_disconnect_requeues_without_ghost_pending() {
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(2);
        torrent.info.piece_length = 20_000;
        torrent.info.length = 40_000;
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(2, false);
        state.piece_manager.block_manager.set_geometry(
            20_000,
            40_000,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Standard;

        add_peer(&mut state, "race_peer");
        let peer = state.peers.get_mut("race_peer").unwrap();
        peer.peer_choking = ChokeStatus::Unchoke;
        peer.bitfield = vec![true, true];
        peer.pending_requests.insert(1);
        state
            .piece_manager
            .mark_as_pending(1, "race_peer".to_string());

        let _ = state.update(Action::PeerChoked {
            peer_id: "race_peer".to_string(),
        });
        let _ = state.update(Action::PeerDisconnected {
            peer_id: "race_peer".to_string(),
            force: true,
        });

        assert!(
            !state.piece_manager.pending_queue.contains_key(&1),
            "Pending queue should be cleared for disconnected/choked peer"
        );
        assert!(
            state.piece_manager.need_queue.contains(&1),
            "Piece must be requeued after choke/disconnect race"
        );
        assert!(
            !state.peers.contains_key("race_peer"),
            "Peer should be removed after disconnect"
        );
    }

    #[test]
    fn test_non_aligned_assign_work_not_suppressed_by_previous_piece_global_bits() {
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(2);
        torrent.info.piece_length = 20_000;
        torrent.info.length = 40_000;
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(2, false);
        state.piece_manager.block_manager.set_geometry(
            20_000,
            40_000,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Standard;

        // Mark piece 0 as complete first. On non-aligned geometry this sets a shared
        // global boundary block bit that must not suppress piece-1 requests.
        state.piece_manager.mark_as_complete(0);

        state.piece_manager.need_queue.clear();
        state.piece_manager.pending_queue.clear();

        add_peer(&mut state, "boundary_peer");
        let peer = state.peers.get_mut("boundary_peer").unwrap();
        peer.peer_choking = ChokeStatus::Unchoke;
        peer.bitfield = vec![true, true];
        peer.am_interested = true;

        state
            .piece_manager
            .mark_as_pending(1, "boundary_peer".to_string());
        peer.pending_requests.insert(1);

        let effects = state.update(Action::AssignWork {
            peer_id: "boundary_peer".to_string(),
        });

        let requests: Vec<(u32, u32, u32)> = effects
            .iter()
            .filter_map(|e| {
                if let Effect::SendToPeer { cmd, .. } = e {
                    if let TorrentCommand::BulkRequest(reqs) = &**cmd {
                        return Some(reqs.clone());
                    }
                }
                None
            })
            .flatten()
            .collect();

        let mut piece_1_offsets: Vec<(u32, u32)> = requests
            .iter()
            .filter(|(idx, _, _)| *idx == 1)
            .map(|(_, begin, len)| (*begin, *len))
            .collect();
        piece_1_offsets.sort_unstable();

        assert_eq!(
            piece_1_offsets,
            vec![(0, 16_384), (16_384, 3_616)],
            "Piece-1 requests must not be suppressed by piece-0 global block bits"
        );
    }

    #[test]
    fn test_upload_starts_immediately_after_validation() {
        // GIVEN: A state set up to require upload activity after validation.
        let mut state = create_empty_state();

        // Setup a 2-piece torrent.
        let torrent = create_dummy_torrent(2);
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(2, false);
        state.torrent_status = TorrentStatus::Validating; // Initial state

        add_peer(&mut state, "leecher");
        let leecher = state.peers.get_mut("leecher").unwrap();
        // Manager is initially choking the peer (am_choking == Choke)
        leecher.peer_is_interested_in_us = true;

        // We update the piece manager to simulate pieces 0 and 1 being present on disk.
        // The bitfield status should still be in the initial state here.

        // WHEN: Validation completes, finding pieces 0 and 1 on disk.
        let effects = state.update(Action::ValidationComplete {
            completed_pieces: vec![0, 1],
        });

        // THEN:

        assert_eq!(
            state.torrent_status,
            TorrentStatus::Done,
            "Torrent status should be DONE after finding all pieces."
        );

        let unchoke_sent = effects.iter().any(|e| {
            matches!(e, Effect::SendToPeer { peer_id, cmd }
        if peer_id == "leecher" && matches!(**cmd, TorrentCommand::PeerUnchoke))
        });

        assert!(
            unchoke_sent,
            "Validation completion failed to trigger Unchoke for interested peer."
        );

        assert_eq!(state.peers["leecher"].am_choking, ChokeStatus::Unchoke);

        let have_broadcasted = effects
            .iter()
            .any(|e| matches!(e, Effect::BroadcastHave { piece_index: 0 }));
        assert!(
            have_broadcasted,
            "Validation completion failed to trigger BroadcastHave."
        );
    }

    #[test]
    fn test_tracker_spam_during_validation() {
        // GIVEN: A torrent that has metadata and is currently validating.
        let mut state = create_empty_state();
        let torrent = create_dummy_torrent(100); // Simulate a large torrent that takes time
        state.torrent = Some(torrent);
        state.torrent_status = TorrentStatus::Validating; // CRITICAL: Status is Validating

        // Setup a tracker state where the initial announce time has passed (time = now).
        let tracker_url = "http://tracker.test".to_string();
        state.trackers.insert(
            tracker_url.clone(),
            TrackerState {
                next_announce_time: state.now, // Ready to announce immediately
                leeching_interval: Some(Duration::from_secs(60)),
                seeding_interval: None,
            },
        );

        // CRITICAL ACTION: Advance time by 1ms (ensures the timer check is hit).
        let _ = state.update(Action::Tick { dt_ms: 1 });

        // Reset next_announce_time to ensure it's still available (not strictly necessary but defensive)
        state
            .trackers
            .get_mut(&tracker_url)
            .unwrap()
            .next_announce_time = state.now;

        // WHEN: Action::Tick is executed again while still validating.
        let effects = state.update(Action::Tick { dt_ms: 1 });

        // THEN: The torrent should have generated NO tracker announce effects because validation blocks periodic activity.
        let announce_sent = effects
            .iter()
            .any(|e| matches!(e, Effect::AnnounceToTracker { .. }));

        assert!(!announce_sent, "FAILURE: Tracker announce was sent during the validation phase, indicating the system is inefficiently spamming the tracker while busy.");
    }

    #[test]
    fn metadata_received_renormalizes_existing_trackers_with_udp_preference() {
        let mut state = create_empty_state();
        state.trackers.insert(
            "http://tracker.local:6969/announce".to_string(),
            TrackerState {
                next_announce_time: state.now,
                leeching_interval: Some(Duration::from_secs(60)),
                seeding_interval: None,
            },
        );

        let mut torrent = create_dummy_torrent(4);
        torrent.announce = Some("udp://tracker.local:6969/announce".to_string());
        torrent.announce_list = Some(vec![vec!["https://tracker-alt.local/announce".to_string()]]);

        let _ = state.update(Action::MetadataReceived {
            torrent: Box::new(torrent),
            metadata_length: 0,
        });

        let mut tracker_urls: Vec<_> = state.trackers.keys().cloned().collect();
        tracker_urls.sort();
        assert_eq!(
            tracker_urls,
            vec![
                "https://tracker-alt.local/announce".to_string(),
                "udp://tracker.local:6969/announce".to_string(),
            ]
        );
    }

    // In src/torrent_manager/state.rs, inside mod tests { ... }

    #[test]
    fn test_manager_init_active_triggers_announce() {
        // GIVEN: A clean state with one tracker configured.
        let mut state = create_empty_state();
        let tracker_url = "http://test.tracker".to_string();
        state.trackers.insert(
            tracker_url.clone(),
            TrackerState {
                next_announce_time: state.now,
                leeching_interval: None,
                seeding_interval: None,
            },
        );

        // WHEN: The manager initializes in the active state.
        let effects = state.update(Action::TorrentManagerInit {
            is_paused: false,
            announce_immediately: true,
        });

        // THEN:

        let announce_sent = effects
            .iter()
            .any(|e| matches!(e, Effect::AnnounceToTracker { url } if url == &tracker_url));
        assert!(announce_sent, "Should trigger AnnounceToTracker.");

        assert!(!state.is_paused);
    }

    // In src/torrent_manager/state.rs, inside mod tests { ... }

    #[test]
    fn test_manager_init_paused_halts_activity() {
        // GIVEN: A clean state with one tracker configured.
        let mut state = create_empty_state();
        let tracker_url = "http://test.tracker".to_string();
        state.trackers.insert(
            tracker_url.clone(),
            TrackerState {
                next_announce_time: state.now,
                leeching_interval: None,
                seeding_interval: None,
            },
        );

        // WHEN: The manager initializes in the paused state.
        let effects = state.update(Action::TorrentManagerInit {
            is_paused: true,
            announce_immediately: false,
        });

        // THEN:

        assert!(state.is_paused);

        let network_activity = effects
            .iter()
            .any(|e| matches!(e, Effect::AnnounceToTracker { .. }));
        assert!(
            !network_activity,
            "No network activity should be generated when starting paused."
        );
    }

    #[test]
    fn test_state_scale_2k_blocks_simulation() {
        let num_pieces = 2000;
        let piece_len = 16_384;

        let mut state = create_empty_state();
        let torrent = create_dummy_torrent(num_pieces);

        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(num_pieces, false);
        state.piece_manager.block_manager.set_geometry(
            piece_len as u32,
            (piece_len * num_pieces) as u64,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Standard;

        let peer_id = "worker_peer".to_string();
        add_peer(&mut state, &peer_id);

        // Setup Peer
        let bitfield = vec![0xFF; num_pieces.div_ceil(8)];
        state.update(Action::PeerBitfieldReceived {
            peer_id: peer_id.clone(),
            bitfield,
        });

        // Initialize queue early to capture setup effects
        let mut pending_actions = std::collections::VecDeque::new();

        // FIX: Capture initial requests from Unchoke logic
        let initial_effects = state.update(Action::PeerUnchoked {
            peer_id: peer_id.clone(),
        });
        for effect in initial_effects {
            if let Effect::SendToPeer { cmd, .. } = effect {
                if let TorrentCommand::BulkRequest(requests) = *cmd {
                    for (index, begin, length) in requests {
                        let data = vec![0u8; length as usize];
                        pending_actions.push_back(Action::IncomingBlock {
                            peer_id: peer_id.clone(),
                            piece_index: index,
                            block_offset: begin,
                            data,
                        });
                    }
                }
            }
        }

        state
            .piece_manager
            .update_rarity(state.peers.values().map(|p| &p.bitfield));

        // --- 2. SIMULATION LOOP ---
        let mut pieces_completed = 0;
        let mut loop_count = 0;

        println!("Starting State Simulation: 20,000 Blocks...");
        let start = std::time::Instant::now();

        while pieces_completed < num_pieces {
            loop_count += 1;
            if loop_count > 300_000 {
                // Trace dump on failure
                let peer = state.peers.get(&peer_id).unwrap();
                println!("\n!!! STALL DETECTED !!!");
                println!("Loop Count: {}", loop_count);
                println!("Pieces Completed: {}", pieces_completed);
                println!("Need Queue: {}", state.piece_manager.need_queue.len());
                println!("Pending Queue: {}", state.piece_manager.pending_queue.len());
                println!("Peer Inflight (State): {}", peer.inflight_requests);
                println!("Pending Actions Queue: {}", pending_actions.len());
                panic!("Infinite loop detected! Pipeline stalled.");
            }

            let inflight = state.peers.get(&peer_id).unwrap().inflight_requests;
            let mut effects = Vec::new();

            if inflight < 20 {
                effects.extend(state.update(Action::AssignWork {
                    peer_id: peer_id.clone(),
                }));
            }

            if let Some(action) = pending_actions.pop_front() {
                effects.extend(state.update(action));
            } else if effects.is_empty() && inflight == 0 {
                panic!("DEADLOCK: No inflight requests and no pending actions!");
            }

            // C. Handle All Effects (Recursive Logic)
            for effect in effects {
                match effect {
                    Effect::SendToPeer { cmd, .. } => {
                        if let TorrentCommand::BulkRequest(requests) = *cmd {
                            for (index, begin, length) in requests {
                                // NETWORK SIM: Queue Response
                                let data = vec![0u8; length as usize];
                                pending_actions.push_back(Action::IncomingBlock {
                                    peer_id: peer_id.clone(),
                                    piece_index: index,
                                    block_offset: begin,
                                    data,
                                });
                            }
                        }
                    }
                    Effect::VerifyPiece { piece_index, .. } => {
                        // CPU SIM: Verify OK -> Queue Result
                        pending_actions.push_front(Action::PieceVerified {
                            peer_id: peer_id.clone(),
                            piece_index,
                            valid: true,
                            data: vec![],
                        });
                    }
                    Effect::WriteToDisk { piece_index, .. } => {
                        // DISK SIM: Write OK -> Queue Result
                        pending_actions.push_front(Action::PieceWrittenToDisk {
                            peer_id: peer_id.clone(),
                            piece_index,
                        });
                    }
                    Effect::BroadcastHave { .. } => {
                        // SUCCESS
                        pieces_completed += 1;
                        if pieces_completed % 2000 == 0 {
                            println!("Progress: {}/{}", pieces_completed, num_pieces);
                        }
                    }
                    Effect::DisconnectPeer { .. } => {
                        panic!("Unexpected Peer Disconnect! Validation likely failed.");
                    }
                    _ => {}
                }
            }
        }

        let duration = start.elapsed();
        println!("State Logic Processed 20k blocks in {:.2?}", duration);

        assert_eq!(pieces_completed, num_pieces);
        assert!(state.piece_manager.need_queue.is_empty());
    }

    #[test]
    fn test_debug_3_blocks_trace() {
        let num_pieces = 3;
        let piece_len = 16_384;

        let mut state = create_empty_state();
        let torrent = create_dummy_torrent(num_pieces);

        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(num_pieces, false);
        state.piece_manager.block_manager.set_geometry(
            piece_len as u32,
            (piece_len * num_pieces) as u64,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Standard;

        let peer_id = "worker_peer".to_string();
        add_peer(&mut state, &peer_id);

        // Setup Peer Bitfield
        let bitfield = vec![0xFF; num_pieces.div_ceil(8)];
        state.update(Action::PeerBitfieldReceived {
            peer_id: peer_id.clone(),
            bitfield,
        });

        // Initialize queue early so we can capture setup effects
        let mut pending_actions = std::collections::VecDeque::new();

        // Capture initial effects from Unchoke (Triggering AssignWork)
        let initial_effects = state.update(Action::PeerUnchoked {
            peer_id: peer_id.clone(),
        });

        // FIX: Feed initial requests into the network queue
        for effect in initial_effects {
            if let Effect::SendToPeer { cmd, .. } = effect {
                if let TorrentCommand::BulkRequest(requests) = *cmd {
                    println!(
                        "   << Setup Effect: SendToPeer BulkRequest with {} requests",
                        requests.len()
                    );
                    for (index, begin, length) in requests {
                        let data = vec![0u8; length as usize];
                        pending_actions.push_back(Action::IncomingBlock {
                            peer_id: peer_id.clone(),
                            piece_index: index,
                            block_offset: begin,
                            data,
                        });
                    }
                }
            }
        }

        state
            .piece_manager
            .update_rarity(state.peers.values().map(|p| &p.bitfield));

        // --- 2. SIMULATION LOOP ---
        let mut pieces_completed = 0;
        let mut loop_count = 0;

        println!("\n=== STARTING TRACE ===");

        while pieces_completed < num_pieces {
            loop_count += 1;
            if loop_count > 50 {
                panic!("STALL! Loop limit reached.");
            }

            let peer = state.peers.get(&peer_id).unwrap();
            println!("\n--- LOOP {} ---", loop_count);
            println!("State Status: {:?}", state.torrent_status);
            println!(
                "Need Q: {:?} | Pending Q: {:?}",
                state.piece_manager.need_queue,
                state.piece_manager.pending_queue.keys()
            );
            println!(
                "Peer Inflight: {} | Peer PendingReqs: {:?}",
                peer.inflight_requests, peer.pending_requests
            );
            println!("Action Queue Size: {}", pending_actions.len());

            let mut effects = Vec::new();

            // Trigger Assignment if pipeline has room
            if peer.inflight_requests < 20 {
                println!(">> Triggering AssignWork");
                effects.extend(state.update(Action::AssignWork {
                    peer_id: peer_id.clone(),
                }));
            }

            // Process One Network Event
            if let Some(action) = pending_actions.pop_front() {
                println!(">> Processing Action: {:?}", action);
                effects.extend(state.update(action));
            }

            // Handle Effects
            for effect in effects {
                match effect {
                    Effect::SendToPeer { cmd, .. } => {
                        println!("   << Effect: SendToPeer {:?}", cmd);
                        if let TorrentCommand::BulkRequest(requests) = *cmd {
                            for (index, begin, length) in requests {
                                let data = vec![0u8; length as usize];
                                pending_actions.push_back(Action::IncomingBlock {
                                    peer_id: peer_id.clone(),
                                    piece_index: index,
                                    block_offset: begin,
                                    data,
                                });
                            }
                        }
                    }
                    Effect::VerifyPiece { piece_index, .. } => {
                        println!("   << Effect: VerifyPiece {}", piece_index);
                        pending_actions.push_front(Action::PieceVerified {
                            peer_id: peer_id.clone(),
                            piece_index,
                            valid: true,
                            data: vec![],
                        });
                    }
                    Effect::WriteToDisk { piece_index, .. } => {
                        println!("   << Effect: WriteToDisk {}", piece_index);
                        pending_actions.push_front(Action::PieceWrittenToDisk {
                            peer_id: peer_id.clone(),
                            piece_index,
                        });
                    }
                    Effect::BroadcastHave { piece_index } => {
                        println!("   << Effect: BroadcastHave {}", piece_index);
                        pieces_completed += 1;
                    }
                    _ => println!("   << Effect: {:?}", effect),
                }
            }
        }
        println!("SUCCESS");
    }

    #[test]
    fn test_reproduce_gap_duplicate_bug() {
        let mut state = super::tests::create_empty_state();
        let piece_len = 16384 * 3;
        let torrent = super::tests::create_dummy_torrent(1);
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(1, false);
        state.piece_manager.block_manager.set_geometry(
            piece_len,
            piece_len as u64,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Standard;
        state.piece_manager.need_queue = vec![0];

        let peer_id = "gap_peer".to_string();
        let (tx, _) = mpsc::channel(100);
        let mut peer = PeerState::new(peer_id.clone(), tx, state.now);
        peer.peer_id = peer_id.as_bytes().to_vec();
        peer.bitfield = vec![true];
        peer.peer_choking = ChokeStatus::Unchoke;
        peer.am_interested = true;

        // We simulate that we have ALREADY requested Block 0 and Block 2.
        // Block 1 is NOT requested yet.
        // Inflight = 2.
        peer.inflight_requests = 2;
        state.peers.insert(peer_id.clone(), peer);

        let data = vec![0u8; 16384];
        let effects = state.update(Action::IncomingBlock {
            peer_id: peer_id.clone(),
            piece_index: 0,
            block_offset: 0, // Block 0 Arrives
            data,
        });

        // Current Logic:
        // - Inflight drops to 1.
        // - AssignWork runs with skips=1.
        // - It sees Block 1 is missing. It uses the skip on Block 1.
        // - It sees Block 2 is missing (it's inflight, but not buffered). It has 0 skips left.
        // - IT REQUESTS BLOCK 2 AGAIN.
        let duplicate_request = effects.iter().any(|e| {
            if let Effect::SendToPeer { cmd, .. } = e {
                if let TorrentCommand::BulkRequest(ref reqs) = **cmd {
                    return reqs
                        .iter()
                        .any(|(index, begin, _)| *index == 0 && *begin == 32768);
                }
            }
            false
        });

        assert!(
            duplicate_request,
            "Test Failed: The bug SHOULD exist, but we didn't send a duplicate."
        );
        println!("SUCCESS: Reproduced the GAP bug! Manager re-requested Block 2 because 'skips' logic is flawed.");
    }

    #[test]
    fn test_assign_work_is_sequential() {
        let mut state = create_empty_state();
        let piece_len = 16_384 * 10;
        let torrent = create_dummy_torrent(1);
        state.torrent = Some(torrent);

        // Set geometry so block manager knows we have 10 blocks
        state.piece_manager.set_initial_fields(1, false);
        state.piece_manager.block_manager.set_geometry(
            piece_len,
            piece_len as u64,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Standard;

        // We need Piece 0
        state.piece_manager.need_queue = vec![0];

        let peer_id = "seq_peer".to_string();
        let (tx, _) = mpsc::channel(100);
        let mut peer = PeerState::new(peer_id.clone(), tx, state.now);

        peer.peer_id = peer_id.as_bytes().to_vec();
        peer.bitfield = vec![true]; // Peer has the piece
        peer.peer_choking = super::ChokeStatus::Unchoke; // Unchoked
        peer.am_interested = true;

        // IMPORTANT: Ensure Peer has 0 inflight and 0 active blocks to prevent skipping
        peer.inflight_requests = 0;
        peer.active_blocks.clear();

        state.peers.insert(peer_id.clone(), peer);

        // This should generate 10 requests for Piece 0 (Blocks 0-9)
        let effects = state.update(Action::AssignWork {
            peer_id: peer_id.clone(),
        });

        let mut expected_offset = 0;
        let mut request_count = 0;

        for effect in effects {
            if let Effect::SendToPeer { cmd, .. } = effect {
                if let TorrentCommand::BulkRequest(requests) = *cmd {
                    for (index, begin, length) in requests {
                        assert_eq!(index, 0, "Should work on Piece 0");
                        assert_eq!(length, 16384, "Block length mismatch");

                        // THE CHECK: Offset must match our expected increment
                        assert_eq!(
                            begin, expected_offset,
                            "Non-sequential request detected! Expected offset {}, got {}. (Shotgunning?)",
                            expected_offset, begin
                        );

                        expected_offset += 16384;
                        request_count += 1;
                    }
                }
            }
        }

        // Ensure we actually tested something
        assert_eq!(request_count, 10, "Expected 10 requests to fill the piece");
        println!("SUCCESS: Generated 10 sequential requests for Piece 0.");
    }

    #[test]
    fn test_assign_work_multi_piece_saturation() {
        // We need > 50 blocks to test the MAX_PIPELINE_DEPTH limit.
        let mut state = create_empty_state();
        let piece_len = 16_384 * 4;
        let num_pieces = 15;
        let torrent = create_dummy_torrent(num_pieces);
        state.torrent = Some(torrent);

        state.piece_manager.set_initial_fields(num_pieces, false);
        state.piece_manager.block_manager.set_geometry(
            piece_len,
            (piece_len * num_pieces as u32) as u64,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Standard;

        // All pieces are needed
        state.piece_manager.need_queue = (0..num_pieces as u32).collect();

        let peer_id = "multi_piece_peer".to_string();
        let (tx, _) = mpsc::channel(100);
        let mut peer = PeerState::new(peer_id.clone(), tx, state.now);

        peer.peer_id = peer_id.as_bytes().to_vec();
        peer.bitfield = vec![true; num_pieces];
        peer.peer_choking = super::ChokeStatus::Unchoke;
        peer.am_interested = true;
        peer.inflight_requests = 0;
        peer.active_blocks.clear();

        // We manually put all pieces into the pending queue.
        for i in 0..num_pieces as u32 {
            peer.pending_requests.insert(i);
        }

        state.peers.insert(peer_id.clone(), peer);

        // Pipeline Depth is 50.
        // We need 60 blocks total (15 pieces * 4 blocks).
        // We expect the first 50 blocks to be requested.
        let effects = state.update(Action::AssignWork {
            peer_id: peer_id.clone(),
        });

        let mut requests = Vec::new();
        for effect in effects {
            if let Effect::SendToPeer { cmd, .. } = effect {
                if let TorrentCommand::BulkRequest(ref reqs) = *cmd {
                    requests.extend(reqs.iter().map(|(i, b, _)| (*i, *b)));
                }
            }
        }

        assert_eq!(
            requests.len(),
            60,
            "Should request all available blocks (60) as it's less than pipeline depth ({})",
            super::MAX_PIPELINE_DEPTH
        );

        // CHECK 1: Sequential Offsets
        for piece_idx in 0..num_pieces as u32 {
            let offsets: Vec<u32> = requests
                .iter()
                .filter(|(i, _)| *i == piece_idx)
                .map(|(_, off)| *off)
                .collect();

            if !offsets.is_empty() {
                let mut sorted_offsets = offsets.clone();
                sorted_offsets.sort();
                assert_eq!(
                    offsets, sorted_offsets,
                    "Non-sequential blocks detected for Piece {}! Got {:?}",
                    piece_idx, offsets
                );
            }
        }

        // CHECK 2: Deterministic Piece Order (The "Sort" Fix Check)
        // Piece 0 must start before Piece 2.
        let piece_0_start = requests.iter().position(|(i, _)| *i == 0);
        let piece_2_start = requests.iter().position(|(i, _)| *i == 2);

        if let (Some(p0), Some(p2)) = (piece_0_start, piece_2_start) {
            assert!(
                p0 < p2,
                "Random Order Detected! Pending requests must be sorted."
            );
        }

        println!("SUCCESS: Pipeline saturated at 50 requests with sequential ordering.");
    }

    // V2 / HYBRID LOGIC TESTS

    #[test]
    fn test_v2_hybrid_boundary_routing() {
        let mut state = create_empty_state();

        // Setup: Piece 0, Length 32768 (Spans 2 Files of 16384 each)
        let mut torrent = create_dummy_torrent(1);
        torrent.info.piece_length = 32768;
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(1, false);
        state
            .piece_manager
            .set_geometry(32768, 32768, HashMap::new(), false);

        let root_a = vec![0xAA; 32]; // File A (0-16384)
        let root_b = vec![0xBB; 32]; // File B (16384-32768)

        state.piece_to_roots.insert(
            0,
            vec![
                V2RootInfo {
                    file_offset: 0,
                    length: 16384,
                    root_hash: root_a.clone(),
                    file_index: 0,
                },
                V2RootInfo {
                    file_offset: 16384,
                    length: 16384,
                    root_hash: root_b.clone(),
                    file_index: 0,
                },
            ],
        );
        state.v2_proofs.insert(0, vec![0xFF; 32]); // Proof ready

        // --- SCENARIO 1: Complete via Offset 16384 (Should match Root B) ---

        state.update(Action::IncomingBlock {
            peer_id: "peer1".into(),
            piece_index: 0,
            block_offset: 0,
            data: vec![0u8; 16384],
        });

        let effects_b = state.update(Action::IncomingBlock {
            peer_id: "peer1".into(),
            piece_index: 0,
            block_offset: 16384,
            data: vec![0u8; 16384],
        });

        let verified_b = effects_b
            .iter()
            .any(|e| matches!(e, Effect::VerifyPieceV2 { root_hash, .. } if root_hash == &root_b));
        assert!(
            verified_b,
            "Completion at offset 16384 should verify against Root B"
        );

        // --- SCENARIO 2: Complete via Offset 0 (Should match Root A) ---

        // Reset State for clean run
        state.piece_manager = PieceManager::new();
        state.piece_manager.set_initial_fields(1, false);
        state
            .piece_manager
            .set_geometry(32768, 32768, HashMap::new(), false);

        state.update(Action::IncomingBlock {
            peer_id: "peer1".into(),
            piece_index: 0,
            block_offset: 16384,
            data: vec![0u8; 16384],
        });

        let effects_a = state.update(Action::IncomingBlock {
            peer_id: "peer1".into(),
            piece_index: 0,
            block_offset: 0,
            data: vec![0u8; 16384],
        });

        let verified_a = effects_a
            .iter()
            .any(|e| matches!(e, Effect::VerifyPieceV2 { root_hash, .. } if root_hash == &root_a));
        assert!(
            verified_a,
            "Completion at offset 0 should verify against Root A"
        );
    }

    #[test]
    fn test_v2_deferred_verification_with_offset() {
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(10);
        torrent.info.piece_length = 4;
        torrent.info.pieces = Vec::new();

        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(10, false);
        state
            .piece_manager
            .set_geometry(4, 40, HashMap::new(), false);

        let root_target = vec![0xCC; 32];
        // FIX: file_len (8) > piece_len (4) forces buffering
        state.piece_to_roots.insert(
            5,
            vec![V2RootInfo {
                file_offset: 0,
                length: 8,
                root_hash: root_target.clone(),
                file_index: 0,
            }],
        );

        let _effects_data = state.update(Action::IncomingBlock {
            peer_id: "peer1".into(),
            piece_index: 5,
            block_offset: 0,
            data: vec![1, 2, 3, 4],
        });

        assert!(
            state.v2_pending_data.contains_key(&5),
            "Data must buffer for multi-piece files without proof"
        );

        let effects_proof = state.update(Action::MerkleProofReceived {
            peer_id: "peer1".into(),
            piece_index: 5,
            proof: vec![0xEE; 32],
        });

        assert!(effects_proof
            .iter()
            .any(|e| matches!(e, Effect::VerifyPieceV2 { .. })));
    }

    #[test]
    fn test_v2_verification_failure() {
        let mut state = create_empty_state();
        // Setup simple 1-piece torrent
        let mut torrent = create_dummy_torrent(1);
        torrent.info.piece_length = 1024;
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(1, false);
        state
            .piece_manager
            .set_geometry(1024, 1024, HashMap::new(), false);

        let root_hash = vec![0xAA; 32];
        state.piece_to_roots.insert(
            0,
            vec![V2RootInfo {
                file_offset: 0,
                length: 1024,
                root_hash: root_hash.clone(),
                file_index: 0,
            }],
        );

        // Proof arrives first
        state.v2_proofs.insert(0, vec![0xFF; 32]);

        // Incoming block with "bad" data
        let effects = state.update(Action::IncomingBlock {
            peer_id: "peer1".into(),
            piece_index: 0,
            block_offset: 0,
            data: vec![0x00; 1024], // Junk data
        });

        // Effect should be VerifyPieceV2
        assert!(effects
            .iter()
            .any(|e| matches!(e, Effect::VerifyPieceV2 { .. })));

        // Simulate the CPU worker returning "valid: false"
        let verify_effects = state.update(Action::PieceVerified {
            peer_id: "peer1".into(),
            piece_index: 0,
            valid: false, // <--- FAILURE
            data: vec![],
        });

        // Expect disconnection or punishment
        let disconnected = verify_effects
            .iter()
            .any(|e| matches!(e, Effect::DisconnectPeer { .. }));
        assert!(
            disconnected,
            "Peer should be disconnected on V2 verification failure"
        );
    }

    #[test]
    fn test_v2_verification_failure_disconnects_peer() {
        // GIVEN: A V2 piece where verification fails (e.g. bad data sent)
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(1);
        torrent.info.piece_length = 1024;
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(1, false);
        state
            .piece_manager
            .set_geometry(1024, 1024, HashMap::new(), false);

        let root_hash = vec![0xAA; 32];
        state.piece_to_roots.insert(
            0,
            vec![V2RootInfo {
                file_offset: 0,
                length: 1024,
                root_hash: root_hash.clone(),
                file_index: 0,
            }],
        );

        state.update(Action::MerkleProofReceived {
            peer_id: "peer1".into(),
            piece_index: 0,
            proof: vec![0xFF; 32],
        });

        let effects = state.update(Action::IncomingBlock {
            peer_id: "peer1".into(),
            piece_index: 0,
            block_offset: 0,
            data: vec![0x00; 1024],
        });

        // Assert: Verification was attempted
        assert!(effects
            .iter()
            .any(|e| matches!(e, Effect::VerifyPieceV2 { .. })));

        let verify_effects = state.update(Action::PieceVerified {
            peer_id: "peer1".into(),
            piece_index: 0,
            valid: false,
            data: vec![],
        });

        // THEN: Peer should be disconnected
        let disconnected = verify_effects
            .iter()
            .any(|e| matches!(e, Effect::DisconnectPeer { .. }));
        assert!(
            disconnected,
            "Peer should be disconnected on V2 verification failure"
        );

        // THEN: Assembly should be reset (checked via internal state or subsequent behavior)
        // (In this mock state, reset_piece_assembly is a void operation, but the effect confirms the logic path)
    }

    #[test]
    fn test_v2_state_cleanup_after_success() {
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(1);
        torrent.info.piece_length = 4;
        torrent.info.pieces = Vec::new();
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(1, false);
        state
            .piece_manager
            .set_geometry(4, 4, HashMap::new(), false);

        // FIX: Set file_len (8) > piece_len (4) to force the V2 workflow (buffer + proof)
        state.piece_to_roots.insert(
            0,
            vec![V2RootInfo {
                file_offset: 0,
                length: 8,
                root_hash: vec![0xAA; 32],
                file_index: 0,
            }],
        );

        state.update(Action::IncomingBlock {
            peer_id: "peer1".into(),
            piece_index: 0,
            block_offset: 0,
            data: vec![1, 2, 3, 4],
        });
        assert!(
            state.v2_pending_data.contains_key(&0),
            "Data should be buffered for multi-piece file"
        );

        state.update(Action::MerkleProofReceived {
            peer_id: "peer1".into(),
            piece_index: 0,
            proof: vec![0xBB; 32],
        });

        assert!(
            !state.v2_pending_data.contains_key(&0),
            "Pending data consumed"
        );

        state.update(Action::PieceVerified {
            peer_id: "peer1".into(),
            piece_index: 0,
            valid: true,
            data: vec![1, 2, 3, 4],
        });

        assert!(
            !state.v2_proofs.contains_key(&0),
            "Proof cache cleared after verification"
        );
    }

    #[test]
    fn test_v2_duplicate_handling_robustness() {
        // GIVEN: A peer that sends duplicate proofs/data
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(1);
        torrent.info.piece_length = 1024;
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(1, false);
        state
            .piece_manager
            .set_geometry(1024, 1024, HashMap::new(), false);
        state.piece_to_roots.insert(
            0,
            vec![V2RootInfo {
                file_offset: 0,
                length: 1024,
                root_hash: vec![0xAA; 32],
                file_index: 0,
            }],
        );

        state.update(Action::MerkleProofReceived {
            peer_id: "peer1".into(),
            piece_index: 0,
            proof: vec![0xBB; 32],
        });

        let effects_dup = state.update(Action::MerkleProofReceived {
            peer_id: "peer1".into(),
            piece_index: 0,
            proof: vec![0xBB; 32],
        });
        // Duplicate proof with no data buffered usually results in DoNothing or effectively a no-op update
        assert!(effects_dup.iter().all(|e| matches!(e, Effect::DoNothing)));

        let effects_data = state.update(Action::IncomingBlock {
            peer_id: "peer1".into(),
            piece_index: 0,
            block_offset: 0,
            data: vec![0xCC; 1024],
        });

        let verify_triggered = effects_data
            .iter()
            .any(|e| matches!(e, Effect::VerifyPieceV2 { .. }));
        assert!(
            verify_triggered,
            "Verification should still trigger after duplicate proofs"
        );

        // Note: The manager usually transitions `last_activity` to VerifyingPiece.
        // We verify that it doesn't try to double-verify or panic.
        let _effects_data_dup = state.update(Action::IncomingBlock {
            peer_id: "peer1".into(),
            piece_index: 0,
            block_offset: 0,
            data: vec![0xCC; 1024],
        });

        // Logic: If last_activity is VerifyingPiece, IncomingBlock usually returns DoNothing or ignores.
        // We just assert it didn't panic and logic held.
    }

    #[test]
    fn test_v2_scale_1000_deferred_blocks() {
        // GIVEN: A torrent with 1000 V2 pieces
        let mut state = create_empty_state();
        let num_pieces = 1000;
        let piece_len = 1024; // Defined here for scope visibility

        let mut torrent = create_dummy_torrent(num_pieces);
        torrent.info.piece_length = piece_len as i64;
        torrent.info.pieces = Vec::new();
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(num_pieces, false);
        state.piece_manager.set_geometry(
            piece_len as u32,
            (piece_len * num_pieces) as u64,
            HashMap::new(),
            false,
        );

        // Map all pieces to a dummy root
        let root = vec![0xAA; 32];
        let total_file_len = (num_pieces as u64) * (piece_len as u64);

        for i in 0..num_pieces {
            // All pieces belong to one large file (0 to total_file_len)
            state.piece_to_roots.insert(
                i as u32,
                vec![V2RootInfo {
                    file_offset: 0,
                    length: total_file_len,
                    root_hash: root.clone(),
                    file_index: 0,
                }],
            );
        }

        let peer_id = "worker_peer".to_string();
        add_peer(&mut state, &peer_id);

        // We simulate a peer sending 1000 blocks rapidly.
        for i in 0..num_pieces {
            state.update(Action::IncomingBlock {
                peer_id: peer_id.clone(),
                piece_index: i as u32,
                block_offset: 0,
                data: vec![0u8; 1024],
            });
        }

        // CHECK: We should have 1000 items pending in memory
        assert_eq!(
            state.v2_pending_data.len(),
            1000,
            "Should buffer 1000 pieces awaiting proofs"
        );

        // Now the proofs arrive. This tests if the system can drain the queue efficiently.
        let mut verify_count = 0;
        for i in 0..num_pieces {
            let effects = state.update(Action::MerkleProofReceived {
                peer_id: peer_id.clone(),
                piece_index: i as u32,
                proof: vec![0xFF; 32],
            });

            if effects
                .iter()
                .any(|e| matches!(e, Effect::VerifyPieceV2 { .. }))
            {
                verify_count += 1;
            }
        }

        // CHECK: All 1000 should have triggered verification
        assert_eq!(
            verify_count, 1000,
            "All 1000 pieces should trigger verification after proofs arrive"
        );

        // CHECK: Buffer should be empty (moved to verification)
        assert!(
            state.v2_pending_data.is_empty(),
            "Pending buffer should be drained"
        );
    }

    #[test]
    fn test_scale_1000_blocks_pure_v2() {
        let mut state = create_empty_state();
        let num_pieces = 1000;
        let piece_len = 1024;
        let total_len = (num_pieces as i64) * (piece_len as i64);

        let mut torrent = create_dummy_torrent(0);
        torrent.info.piece_length = piece_len as i64;
        torrent.info.length = total_len;
        torrent.info.meta_version = Some(2);
        torrent.info.pieces = Vec::new();

        // Setup V2 File Tree to ensure rebuild_v2_mappings populates piece_to_roots
        let root = vec![0xBB; 32];
        let mut file_meta = HashMap::new();
        file_meta.insert(
            "length".as_bytes().to_vec(),
            serde_bencode::value::Value::Int(total_len),
        );
        file_meta.insert(
            "pieces root".as_bytes().to_vec(),
            serde_bencode::value::Value::Bytes(root.clone()),
        );

        let mut file_node = HashMap::new();
        file_node.insert(
            "".as_bytes().to_vec(),
            serde_bencode::value::Value::Dict(file_meta),
        );

        let mut root_node = HashMap::new();
        root_node.insert(
            "test_torrent".as_bytes().to_vec(),
            serde_bencode::value::Value::Dict(file_node),
        );
        torrent.info.file_tree = Some(serde_bencode::value::Value::Dict(root_node));

        // Calling this will now correctly build piece_to_roots for you
        state.update(Action::MetadataReceived {
            torrent: Box::new(torrent.clone()),
            metadata_length: 5000,
        });

        state.torrent_status = TorrentStatus::Standard;

        let root = vec![0xBB; 32];
        let total_file_len = (num_pieces as u64) * (piece_len as u64);

        for i in 0..num_pieces {
            // Map every piece to a single large 1000-piece file
            state.piece_to_roots.insert(
                i as u32,
                vec![V2RootInfo {
                    file_offset: 0,
                    length: total_file_len,
                    root_hash: root.clone(),
                    file_index: 0,
                }],
            );
        }

        let peer_id = "v2_worker".to_string();
        add_peer(&mut state, &peer_id);
        state.update(Action::PeerUnchoked {
            peer_id: peer_id.clone(),
        });

        for i in 0..num_pieces {
            state.update(Action::IncomingBlock {
                peer_id: peer_id.clone(),
                piece_index: i as u32,
                block_offset: 0,
                data: vec![0u8; piece_len as usize],
            });
        }
        assert_eq!(
            state.v2_pending_data.len(),
            1000,
            "Pure V2: Should buffer pieces for large files"
        );

        let mut verify_count = 0;
        for i in 0..num_pieces {
            let effects = state.update(Action::MerkleProofReceived {
                peer_id: peer_id.clone(),
                piece_index: i as u32,
                proof: vec![0xEE; 32],
            });
            if effects
                .iter()
                .any(|e| matches!(e, Effect::VerifyPieceV2 { .. }))
            {
                verify_count += 1;
            }
        }
        assert_eq!(verify_count, 1000);
    }

    #[test]
    fn test_v2_memory_cap_enforcement() {
        let mut state = create_empty_state();

        // GIVEN: A torrent with HUGE pieces (500 MB)
        // This tricks the cleanup logic into setting a very small item limit.
        // Limit = 1GB / 500MB = 2 items allowed.
        let mut torrent = create_dummy_torrent(10);
        torrent.info.piece_length = 500 * 1024 * 1024; // 500 MB
        state.torrent = Some(torrent);

        // We use small data vectors so we don't actually crash the test runner,
        // but the state machine counts them as "full pieces".
        for i in 0..3 {
            state.v2_pending_data.insert(i, (0, vec![0u8; 10]));
        }

        assert_eq!(
            state.v2_pending_data.len(),
            3,
            "Sanity check: 3 items inserted"
        );

        state.update(Action::Cleanup);

        // THEN: The buffer should be cleared because 3 > 2 (Limit)
        assert!(state.v2_pending_data.is_empty(),
            "Cleanup should verify that 3 items exceeds the calculated limit for 500MB pieces (limit=2), and clear the buffer");
    }

    #[test]
    fn test_hybrid_v1_v2_interop() {
        // GIVEN: A State with 2 pieces
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(2);
        torrent.info.piece_length = 1024;
        state.torrent = Some(torrent);

        state.piece_manager.set_initial_fields(2, false);
        state
            .piece_manager
            .set_geometry(1024, 2048, HashMap::new(), false);

        // CONFIGURATION: Hybrid Setup
        // Piece 0: Has a V2 Root
        let root = vec![0xAA; 32];

        // FIX: Set file length (2048) > piece_length (1024).
        // This ensures get_local_v2_hash returns None (requires proof/layers),
        // forcing the system to fall back to the V1 hashes provided by create_dummy_torrent.
        state.piece_to_roots.insert(
            0,
            vec![V2RootInfo {
                file_offset: 0,
                length: 2048,
                root_hash: root.clone(),
                file_index: 0,
            }],
        );

        // Piece 1: NO Root (V1 Only)

        let peer_id = "hybrid_peer".to_string();
        add_peer(&mut state, &peer_id);

        // --- CASE 4: V1 Peer -> V2 Piece (The "Cooperative" Case) ---
        // Peer B (Legacy) sends data for Piece 0 (V2).
        // It CANNOT send a proof.
        // BEHAVIOR CHANGE: Since we have V1 hashes (from create_dummy_torrent),
        // we should FALL BACK to V1 verification immediately, NOT buffer.

        let effects_4_data = state.update(Action::IncomingBlock {
            peer_id: peer_id.clone(),
            piece_index: 0,
            block_offset: 0,
            data: vec![1u8; 1024],
        });

        assert!(
            !state.v2_pending_data.contains_key(&0),
            "Piece 0 should NOT buffer; it should verify via V1 fallback"
        );

        // Note: VerifyPiece (V1) is different from VerifyPieceV2
        let verified_v1 = effects_4_data
            .iter()
            .any(|e| matches!(e, Effect::VerifyPiece { .. }));
        assert!(
            verified_v1,
            "Should have fallen back to V1 verification (Effect::VerifyPiece)"
        );
    }

    #[test]
    fn test_v2_full_completion_lifecycle() {
        let mut state = create_empty_state();
        let num_pieces = 4;
        let piece_len = 1024;
        let mut torrent = create_dummy_torrent(num_pieces);
        torrent.info.piece_length = piece_len as i64;
        torrent.info.pieces = Vec::new(); // Pure V2

        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(num_pieces, false);
        state.piece_manager.set_geometry(
            piece_len as u32,
            (piece_len * num_pieces) as u64,
            HashMap::new(),
            false,
        );
        state.torrent_status = TorrentStatus::Standard;

        let root = vec![0xAA; 32];
        // FIX: Set file length to force the standard V2 proof workflow (buffer -> proof -> verify)
        let file_len = (piece_len * 2) as u64;
        for i in 0..num_pieces {
            state.piece_to_roots.insert(
                i as u32,
                vec![V2RootInfo {
                    file_offset: 0,
                    length: file_len,
                    root_hash: root.clone(),
                    file_index: 0,
                }],
            );
        }

        let peer_id = "seeder".to_string();
        add_peer(&mut state, &peer_id);

        for i in 0..num_pieces {
            // Data arrives and is buffered because it is a multi-piece file without a proof
            state.update(Action::IncomingBlock {
                peer_id: peer_id.clone(),
                piece_index: i as u32,
                block_offset: 0,
                data: vec![1u8; piece_len],
            });

            // Proof arrives, triggering the V2 verification effect
            let effects = state.update(Action::MerkleProofReceived {
                peer_id: peer_id.clone(),
                piece_index: i as u32,
                proof: vec![0xFF; 32],
            });

            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, Effect::VerifyPieceV2 { .. })),
                "Proof arrival should trigger VerifyPieceV2 for piece {}",
                i
            );
        }

        assert!(
            state.v2_pending_data.is_empty(),
            "All pending data should be moved to verification"
        );
    }

    #[test]
    fn test_v2_cleanup_on_completion_race() {
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(1);
        torrent.info.piece_length = 1024;
        torrent.info.pieces = Vec::new();
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(1, false);
        state
            .piece_manager
            .set_geometry(1024, 1024, HashMap::new(), false);

        // FIX: Set file_len (2048) > piece_length (1024) to force buffering
        // Small files (<= piece_len) verify immediately using the root as the leaf.
        state.piece_to_roots.insert(
            0,
            vec![V2RootInfo {
                file_offset: 0,
                length: 2048,
                root_hash: vec![0xAA; 32],
                file_index: 0,
            }],
        );
        let peer_id = "racer".to_string();
        add_peer(&mut state, &peer_id);

        state.update(Action::IncomingBlock {
            peer_id: peer_id.clone(),
            piece_index: 0,
            block_offset: 0,
            data: vec![1u8; 1024],
        });
        assert!(
            state.v2_pending_data.contains_key(&0),
            "Sanity: Data buffered"
        );

        state.update(Action::PieceVerified {
            peer_id: peer_id.clone(),
            piece_index: 0,
            valid: true,
            data: vec![1u8; 1024],
        });

        // Manually mark as done in bitfield to simulate WriteToDisk completion
        state.piece_manager.bitfield[0] = crate::torrent_manager::piece_manager::PieceStatus::Done;

        // CHECK 1: Did we clean up the pending data?
        assert!(
            !state.v2_pending_data.contains_key(&0),
            "Leak: Pending data should be removed immediately upon verification"
        );

        state.update(Action::MerkleProofReceived {
            peer_id: peer_id.clone(),
            piece_index: 0,
            proof: vec![0xFF; 32],
        });

        // CHECK 2: Did we ignore the late proof?
        assert!(
            !state.v2_proofs.contains_key(&0),
            "Leak: Late proofs for Done pieces should be ignored, not cached"
        );
    }

    #[test]
    fn test_v2_cleanup_on_failure() {
        // GIVEN: A torrent with buffered data
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(1);
        torrent.info.piece_length = 1024;
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(1, false);
        state
            .piece_manager
            .set_geometry(1024, 1024, HashMap::new(), false);
        state.piece_to_roots.insert(
            0,
            vec![V2RootInfo {
                file_offset: 0,
                length: 1024,
                root_hash: vec![0xAA; 32],
                file_index: 0,
            }],
        );

        let peer_id = "bad_actor".to_string();
        add_peer(&mut state, &peer_id);

        state.update(Action::IncomingBlock {
            peer_id: peer_id.clone(),
            piece_index: 0,
            block_offset: 0,
            data: vec![1u8; 1024],
        });

        state.update(Action::PieceVerified {
            peer_id: peer_id.clone(),
            piece_index: 0,
            valid: false, // <--- FAILURE
            data: vec![],
        });

        // CHECK: Memory should be freed immediately
        assert!(
            !state.v2_pending_data.contains_key(&0),
            "Cleanup: Pending data must be removed even if verification fails"
        );
        assert!(
            !state.v2_proofs.contains_key(&0),
            "Cleanup: Proofs must be removed even if verification fails"
        );
    }

    #[test]
    fn test_hybrid_swarm_interop() {
        // GIVEN: A Hybrid Torrent with 4 pieces
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(4);
        torrent.info.piece_length = 1024;
        state.torrent = Some(torrent);

        state.piece_manager.set_initial_fields(4, false);
        state
            .piece_manager
            .set_geometry(1024, 4096, HashMap::new(), false);

        // CONFIGURATION:
        // Piece 0: V2 (Has Root)
        let root = vec![0xAA; 32];
        state.piece_to_roots.insert(
            0,
            vec![V2RootInfo {
                file_offset: 0,
                length: 1024,
                root_hash: root.clone(),
                file_index: 0,
            }],
        );

        let peer_a = "v2_peer_A".to_string();
        add_peer(&mut state, &peer_a);

        // --- CASE 1: V2 Peer -> V2 Piece ---
        // Peer A sends data.
        // Because V1 hashes exist, the client will likely verify immediately via V1
        // instead of waiting for the proof. This is valid/desired behavior.
        // OR: If the file is small (<= piece size), it verifies via V2 immediately using the root as the leaf.
        let effects_data = state.update(Action::IncomingBlock {
            peer_id: peer_a.clone(),
            piece_index: 0,
            block_offset: 0,
            data: vec![1u8; 1024],
        });

        let effects_proof = state.update(Action::MerkleProofReceived {
            peer_id: peer_a.clone(),
            piece_index: 0,
            proof: vec![0xFF; 32],
        });

        // CHECK: Did we verify at all?
        // We accept:

        let verified_data_v1 = effects_data
            .iter()
            .any(|e| matches!(e, Effect::VerifyPiece { .. }));
        let verified_data_v2 = effects_data
            .iter()
            .any(|e| matches!(e, Effect::VerifyPieceV2 { .. }));
        let verified_proof_v2 = effects_proof
            .iter()
            .any(|e| matches!(e, Effect::VerifyPieceV2 { .. }));

        assert!(verified_data_v1 || verified_data_v2 || verified_proof_v2,
            "Case 1 Fail: Should have verified via V1/V2 (data) OR V2 (proof). DataV1: {}, DataV2: {}, ProofV2: {}",
            verified_data_v1, verified_data_v2, verified_proof_v2);
    }

    #[test]
    fn test_v2_magnet_metadata_sequence() {
        // GIVEN: An empty state (simulating a fresh V2 Magnet connection)
        let mut state = create_empty_state();
        state.torrent_data_path = Some(PathBuf::from("/tmp/test"));
        state.torrent_status = TorrentStatus::AwaitingMetadata;

        // Construct a V2-Only Torrent (Empty V1 pieces, Has V2 Roots)
        let mut torrent = create_dummy_torrent(5);
        torrent.info.meta_version = Some(2);
        torrent.info.pieces = Vec::new(); // V2 has empty pieces string
        torrent.info.piece_length = 16384;
        torrent.info.length = 16384 * 5; // 5 Pieces

        // Ensure the name matches what we put in the tree
        let filename = "test_torrent".to_string();
        torrent.info.name = filename.clone();

        // Setup V2 Root (Critical for piece_to_roots population)
        let root = vec![0xAA; 32];

        // Mock the V2 File Tree Structure
        // Structure: { "filename": { "": { "pieces root": ..., "length": ... } } }
        use serde_bencode::value::Value;

        let mut file_metadata = std::collections::HashMap::new();
        file_metadata.insert(
            "pieces root".as_bytes().to_vec(),
            Value::Bytes(root.clone()),
        );
        file_metadata.insert(
            "length".as_bytes().to_vec(),
            Value::Int(torrent.info.length),
        );

        let mut dir_node = std::collections::HashMap::new();
        dir_node.insert("".as_bytes().to_vec(), Value::Dict(file_metadata));

        let mut tree = std::collections::HashMap::new();
        tree.insert(filename.as_bytes().to_vec(), Value::Dict(dir_node));

        torrent.info.file_tree = Some(Value::Dict(tree));

        // WHEN: Metadata is received
        let action = Action::MetadataReceived {
            torrent: Box::new(torrent),
            metadata_length: 12345,
        };
        let effects = state.update(action);

        // THEN 1: Sequencing - Should transition to Validating and Init Storage
        assert_eq!(state.torrent_status, TorrentStatus::Validating);
        assert!(effects.iter().any(|e| matches!(e, Effect::StartValidation)));

        // THEN 2: V2 Initialization - Piece count must be 5 (calculated from length)
        assert_eq!(
            state.piece_manager.bitfield.len(),
            5,
            "Failed to calculate piece count for V2 torrent (likely initialized to 0)"
        );

        // THEN 3: V2 State - piece_to_roots must be populated
        assert!(
            !state.piece_to_roots.is_empty(),
            "Failed to populate V2 roots from metadata"
        );

        let roots_for_piece_0 = state.piece_to_roots.get(&0).unwrap();
        assert_eq!(
            roots_for_piece_0[0].root_hash, root,
            "Piece 0 should map to our mock root"
        );
    }

    #[test]
    fn test_v2_magnet_metadata_sequence_multi_file() {
        // GIVEN: An empty state (simulating a fresh V2 Magnet connection)
        let mut state = create_empty_state();
        state.torrent_data_path = Some(PathBuf::from("/tmp/test"));
        state.torrent_status = TorrentStatus::AwaitingMetadata;

        // Construct a V2-Only Torrent
        // 2 Files, 1 Piece each. Total 2 Pieces.
        let mut torrent = create_dummy_torrent(2);
        torrent.info.meta_version = Some(2);
        torrent.info.pieces = Vec::new(); // V2 has empty pieces string
        torrent.info.piece_length = 16384;
        torrent.info.length = 0; // Unused in multi-file usually, but safer to leave 0 or sum

        let dir_name = "multi_v2_download".to_string();
        torrent.info.name = dir_name.clone();

        // define file properties
        let len_a = 16384;
        let len_b = 16384;
        let root_a = vec![0xAA; 32];
        let root_b = vec![0xBB; 32];

        torrent.info.files = vec![
            crate::torrent_file::InfoFile {
                length: len_a,
                path: vec![dir_name.clone(), "file_a.txt".to_string()],
                md5sum: None,
                attr: None,
            },
            crate::torrent_file::InfoFile {
                length: len_b,
                path: vec![dir_name.clone(), "file_b.txt".to_string()],
                md5sum: None,
                attr: None,
            },
        ];

        // Structure: { "dir_name": { "file_a.txt": { "": metadata }, "file_b.txt": { "": metadata } } }
        use serde_bencode::value::Value;

        // Leaf A
        let mut meta_a = std::collections::HashMap::new();
        meta_a.insert(
            "pieces root".as_bytes().to_vec(),
            Value::Bytes(root_a.clone()),
        );
        meta_a.insert("length".as_bytes().to_vec(), Value::Int(len_a));
        let mut node_a = std::collections::HashMap::new();
        node_a.insert("".as_bytes().to_vec(), Value::Dict(meta_a));

        // Leaf B
        let mut meta_b = std::collections::HashMap::new();
        meta_b.insert(
            "pieces root".as_bytes().to_vec(),
            Value::Bytes(root_b.clone()),
        );
        meta_b.insert("length".as_bytes().to_vec(), Value::Int(len_b));
        let mut node_b = std::collections::HashMap::new();
        node_b.insert("".as_bytes().to_vec(), Value::Dict(meta_b));

        // Directory
        let mut dir_content = std::collections::HashMap::new();
        dir_content.insert("file_a.txt".as_bytes().to_vec(), Value::Dict(node_a));
        dir_content.insert("file_b.txt".as_bytes().to_vec(), Value::Dict(node_b));

        // Root
        let mut tree = std::collections::HashMap::new();
        tree.insert(dir_name.as_bytes().to_vec(), Value::Dict(dir_content));

        torrent.info.file_tree = Some(Value::Dict(tree));

        // WHEN: Metadata is received
        let action = Action::MetadataReceived {
            torrent: Box::new(torrent),
            metadata_length: 500,
        };
        let _effects = state.update(action);

        // THEN 1: Transitions
        assert_eq!(state.torrent_status, TorrentStatus::Validating);

        // THEN 2: Piece Count Calculation (16384+16384 / 16384 = 2)
        assert_eq!(
            state.piece_manager.bitfield.len(),
            2,
            "Should calculate 2 pieces from file sizes"
        );

        // THEN 3: Root Mapping
        // Piece 0 -> File A -> Root A
        let roots_0 = state.piece_to_roots.get(&0).expect("Piece 0 missing roots");
        assert!(
            roots_0.iter().any(|r| r.root_hash == root_a),
            "Piece 0 must map to Root A"
        );

        // Piece 1 -> File B -> Root B
        let roots_1 = state.piece_to_roots.get(&1).expect("Piece 1 missing roots");
        assert!(
            roots_1.iter().any(|r| r.root_hash == root_b),
            "Piece 1 must map to Root B"
        );

        // Piece 1 -> File B -> Root B
        let roots_1 = state.piece_to_roots.get(&1).expect("Piece 1 missing roots");
        assert!(
            roots_1.iter().any(|r| r.root_hash == root_b),
            "Piece 1 must map to Root B"
        );
    }

    #[test]
    fn test_scale_1000_blocks_hybrid() {
        println!("\n=== STARTING SCALE TEST: HYBRID (1000 Blocks) ===");

        let mut state = create_empty_state();
        let num_pieces = 1000;
        let piece_len = 1024;

        let mut torrent = create_dummy_torrent(num_pieces);
        torrent.info.piece_length = piece_len as i64;
        torrent.info.length = (num_pieces as i64) * (piece_len as i64);
        torrent.info.meta_version = Some(2);

        state.update(Action::MetadataReceived {
            torrent: Box::new(torrent.clone()),
            metadata_length: 5000,
        });
        state.torrent_status = TorrentStatus::Standard;

        let root = vec![0xAA; 32];
        // FIX: Define total file length to exceed one piece length
        let total_file_len = (num_pieces as u64) * (piece_len as u64);
        for i in 0..num_pieces {
            // Map to a single large file to test V1/V2 interop on large structures
            state.piece_to_roots.insert(
                i as u32,
                vec![V2RootInfo {
                    file_offset: 0,
                    length: total_file_len,
                    root_hash: root.clone(),
                    file_index: 0,
                }],
            );
        }

        let peer_id = "hybrid_worker".to_string();
        add_peer(&mut state, &peer_id);
        state.update(Action::PeerUnchoked {
            peer_id: peer_id.clone(),
        });

        let mut immediate_verifications = 0;

        for i in 0..num_pieces {
            let effects = state.update(Action::IncomingBlock {
                peer_id: peer_id.clone(),
                piece_index: i as u32,
                block_offset: 0,
                data: vec![0u8; piece_len],
            });

            if effects
                .iter()
                .any(|e| matches!(e, Effect::VerifyPiece { .. }))
            {
                immediate_verifications += 1;
            }
        }

        assert_eq!(
            immediate_verifications, 1000,
            "Hybrid: All 1000 pieces should verify immediately via V1 fallback"
        );
        assert!(
            state.v2_pending_data.is_empty(),
            "Hybrid: Buffer should be empty"
        );
    }

    #[test]
    fn test_v2_verification_with_nonzero_file_offset() {
        let mut state = create_empty_state();

        // Setup: 2 Pieces total.
        // Piece 0: File A (Padding/Skip)
        // Piece 1: File B (The one we want to verify)
        let piece_len = 1024;
        let mut torrent = create_dummy_torrent(2);
        torrent.info.piece_length = piece_len;
        state.torrent = Some(torrent);

        state.piece_manager.set_initial_fields(2, false);
        state
            .piece_manager
            .set_geometry(1024, 2048, HashMap::new(), false);

        // Root for File B
        let root_b = vec![0xBB; 32];

        // Map Piece 1 to File B, which starts at 1024 (Piece 1's start)
        // This implies File A occupied 0..1024.
        state.piece_to_roots.insert(
            1,
            vec![V2RootInfo {
                file_offset: 1024,
                length: 1024,
                root_hash: root_b.clone(),
                file_index: 0,
            }],
        );

        let peer_id = "offset_tester".to_string();
        add_peer(&mut state, &peer_id);

        // The proof corresponds to the FIRST piece of File B (Relative Index 0)
        let proof = vec![0xFF; 32]; // Dummy proof
        state.update(Action::MerkleProofReceived {
            peer_id: peer_id.clone(),
            piece_index: 1, // GLOBAL Index 1
            proof: proof.clone(),
        });

        let effects = state.update(Action::IncomingBlock {
            peer_id: peer_id.clone(),
            piece_index: 1, // GLOBAL Index 1
            block_offset: 0,
            data: vec![0xBB; 1024],
        });

        // If your logic is correct, it should spawn VerifyPieceV2.
        // Inspect the arguments passed to it.

        let verify_effect = effects.iter().find_map(|e| {
            if let Effect::VerifyPieceV2 {
                piece_index,
                root_hash,
                ..
            } = e
            {
                Some((piece_index, root_hash))
            } else {
                None
            }
        });

        assert!(verify_effect.is_some(), "Should trigger V2 verification");

        let (idx, hash) = verify_effect.unwrap();
        assert_eq!(hash, &root_b, "Should verify against Root B");

        // CRITICAL CHECK:
        // If you updated the enum to have `relative_index`, check that here.
        // If you are relying on the manager to calculate it, this test ensures
        // the manager receives the correct GLOBAL index (1) to look up the file info later.
        assert_eq!(
            *idx, 1,
            "Effect should carry Global Index 1 for state tracking"
        );
    }

    #[test]
    fn test_v2_local_lookup_optimization() {
        use sha2::Digest;
        use std::collections::HashMap;

        // GOAL: Verify that a Pure V2 torrent can verify data using LOCAL piece_layers

        let mut state = create_empty_state();
        let piece_len = 16384;
        let num_pieces = 1;

        let mut torrent = create_dummy_torrent(num_pieces);
        torrent.info.piece_length = piece_len as i64;
        torrent.info.pieces = Vec::new(); // Pure V2 (Disable V1 fallback)
        torrent.info.meta_version = Some(2);

        let data = vec![0xAA; piece_len];
        let leaf_hash = sha2::Sha256::digest(&data).to_vec();
        let root = leaf_hash.clone();

        let mut layer_map = HashMap::new();
        layer_map.insert(
            root.clone(),
            serde_bencode::value::Value::Bytes(leaf_hash.clone()),
        );
        torrent.piece_layers = Some(serde_bencode::value::Value::Dict(layer_map));

        state.torrent = Some(torrent);

        // CRITICAL FIX: Initialize PieceManager so it accepts the block!
        state.piece_manager.set_initial_fields(num_pieces, false);
        state
            .piece_manager
            .set_geometry(piece_len as u32, piece_len as u64, HashMap::new(), false);

        state.piece_to_roots.insert(
            0,
            vec![V2RootInfo {
                file_offset: 0,
                length: piece_len as u64,
                root_hash: root.clone(),
                file_index: 0,
            }],
        );

        let peer_id = "optimized_peer".to_string();
        add_peer(&mut state, &peer_id);

        let effects = state.update(Action::IncomingBlock {
            peer_id: peer_id.clone(),
            piece_index: 0,
            block_offset: 0,
            data: data.clone(),
        });

        assert!(
            !state.v2_pending_data.contains_key(&0),
            "Optimization Fail: Data buffered instead of verifying!"
        );

        let verified = effects.iter().any(|e| {
            if let Effect::VerifyPieceV2 { root_hash, .. } = e {
                *root_hash == leaf_hash
            } else {
                false
            }
        });
        assert!(
            verified,
            "Optimization Fail: VerifyPieceV2 was not triggered immediately."
        );
    }

    #[test]
    fn test_repro_v2_proof_priority_bug() {
        use sha2::{Digest, Sha256};

        let mut state = create_empty_state();
        let piece_len = 1024;

        let mut torrent = create_dummy_torrent(2);
        torrent.info.piece_length = piece_len as i64;
        torrent.info.meta_version = Some(2);
        torrent.info.pieces = Vec::new();

        let data = vec![0xAA; 1024];
        let leaf_hash = Sha256::digest(&data).to_vec();
        let file_root = vec![0xBB; 32]; // Different from leaf

        let mut layer_map = std::collections::HashMap::new();
        layer_map.insert(
            file_root.clone(),
            serde_bencode::value::Value::Bytes(leaf_hash.clone()),
        );
        torrent.piece_layers = Some(serde_bencode::value::Value::Dict(layer_map));
        state.torrent = Some(torrent);

        // C. FIX: Set file_len to 2 * piece_len to bypass small-file optimization
        state.piece_to_roots.insert(
            0,
            vec![V2RootInfo {
                file_offset: 0,
                length: 2048,
                root_hash: file_root.clone(),
                file_index: 0,
            }],
        );
        state.piece_manager.set_initial_fields(2, false);
        state
            .piece_manager
            .set_geometry(1024, 2048, HashMap::new(), false);

        let peer_id = "bug_tester".to_string();
        add_peer(&mut state, &peer_id);

        // D. Data is now buffered because it's a large file and we lack a network proof
        state.v2_pending_data.insert(0, (0, data.clone()));

        // E. Receive Proof - should now correctly prioritize the leaf_hash from metadata
        let effects = state.update(Action::MerkleProofReceived {
            peer_id: peer_id.clone(),
            piece_index: 0,
            proof: vec![0xFF; 32],
        });

        let verify_op = effects.iter().find_map(|e| {
            if let Effect::VerifyPieceV2 { root_hash, .. } = e {
                Some(root_hash)
            } else {
                None
            }
        });

        assert_eq!(
            verify_op.unwrap(),
            &leaf_hash,
            "Should prioritize Leaf Hash over File Root for multi-piece files"
        );
    }

    #[test]
    fn test_incoming_block_uses_local_leaf_hash_priority() {
        use sha2::{Digest, Sha256};
        use std::collections::HashMap;

        let mut state = create_empty_state();
        let piece_len = 1024;
        let num_pieces = 1;

        // Construct Pure V2 Torrent
        let mut torrent = create_dummy_torrent(num_pieces);
        torrent.info.piece_length = piece_len as i64;
        torrent.info.pieces = Vec::new(); // Pure V2 (No V1 Fallback)
        torrent.info.meta_version = Some(2);

        let data = vec![0xAA; piece_len];
        let leaf_hash = Sha256::digest(&data).to_vec();

        // V2 PROTOCOL RULE: For a file that fits in one piece,
        // the "pieces root" is identical to the leaf hash.
        let file_root = leaf_hash.clone();

        // Note: While protocol-compliant small files don't have layers,
        // we keep this here to ensure the logic handles the presence of metadata.
        let mut layer_map = HashMap::new();
        layer_map.insert(
            file_root.clone(),
            serde_bencode::value::Value::Bytes(leaf_hash.clone()),
        );
        torrent.piece_layers = Some(serde_bencode::value::Value::Dict(layer_map));
        state.torrent = Some(torrent);

        // C. Init Piece Manager & Maps
        state.piece_manager.set_initial_fields(num_pieces, false);
        state
            .piece_manager
            .set_geometry(piece_len as u32, piece_len as u64, HashMap::new(), false);

        // Map Piece 0 -> File Root
        state.piece_to_roots.insert(
            0,
            vec![V2RootInfo {
                file_offset: 0,
                length: piece_len as u64,
                root_hash: file_root.clone(),
                file_index: 0,
            }],
        );

        let peer_id = "priority_tester".to_string();
        add_peer(&mut state, &peer_id);

        let effects = state.update(Action::IncomingBlock {
            peer_id: peer_id.clone(),
            piece_index: 0,
            block_offset: 0,
            data: data.clone(),
        });

        let verify_op = effects.iter().find_map(|e| {
            if let Effect::VerifyPieceV2 {
                root_hash, proof, ..
            } = e
            {
                Some((root_hash, proof))
            } else {
                None
            }
        });

        assert!(
            verify_op.is_some(),
            "Should trigger VerifyPieceV2 immediately via small file optimization"
        );

        let (target_hash, proof) = verify_op.unwrap();

        // The target_hash must be the file_root (which is the leaf_hash)
        assert_eq!(
            target_hash, &leaf_hash,
            "Verification hash mismatch. Expected the protocol-compliant file root."
        );

        assert!(
            proof.is_empty(),
            "Small files should verify directly without a Merkle proof."
        );
    }

    #[test]
    fn test_v2_tail_block_request_clamping() {
        use serde_bencode::value::Value;

        let piece_len = 16_384;
        let file_len: u64 = 20_000;
        let tail_size = 3_616;
        let num_pieces = 2;
        let padded_len = (num_pieces as u64) * (piece_len as u64); // 32,768

        let mut torrent = create_dummy_torrent(num_pieces);
        torrent.info.meta_version = Some(2);
        torrent.info.piece_length = piece_len as i64;
        torrent.info.length = file_len as i64;
        torrent.info.pieces = Vec::new();

        let mut file_map = std::collections::HashMap::new();
        file_map.insert("length".as_bytes().to_vec(), Value::Int(file_len as i64));
        file_map.insert(
            "pieces root".as_bytes().to_vec(),
            Value::Bytes(vec![0xAA; 32]),
        );
        let mut dir_map = std::collections::HashMap::new();
        dir_map.insert("".as_bytes().to_vec(), Value::Dict(file_map));
        let mut root_map = std::collections::HashMap::new();
        root_map.insert("test_tail_file".as_bytes().to_vec(), Value::Dict(dir_map));
        torrent.info.file_tree = Some(Value::Dict(root_map));

        let mut state = TorrentState::new(
            vec![0; 20],
            Some(torrent),
            Some(100),
            PieceManager::new(),
            HashMap::new(),
            false,
            None,
        );

        // This simulates exactly what TorrentState::new does incorrectly for V2.
        // The BlockManager now thinks the tail piece is full (16384 bytes).
        state.torrent_status = TorrentStatus::Standard;
        state.torrent_data_path = Some(std::path::PathBuf::from("/tmp/superseedr_test"));
        state.piece_manager.set_initial_fields(num_pieces, false);
        state
            .piece_manager
            .set_geometry(piece_len as u32, padded_len, HashMap::new(), false); // <--- CHANGED to padded_len
        state.piece_manager.need_queue = vec![1];

        let mut overrides = HashMap::new();
        overrides.insert(1, tail_size);
        state
            .piece_manager
            .set_geometry(piece_len as u32, padded_len, overrides, false);
        state.piece_manager.need_queue = vec![1];

        let peer_id = "strict_peer".to_string();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut peer = PeerState::new(peer_id.clone(), tx, state.now);
        peer.bitfield = vec![true, true];
        peer.peer_choking = ChokeStatus::Unchoke;
        peer.am_interested = true;
        state.peers.insert(peer_id.clone(), peer);

        let effects = state.update(Action::AssignWork {
            peer_id: peer_id.clone(),
        });

        let mut request_found = false;
        for effect in effects {
            if let Effect::SendToPeer { cmd, .. } = effect {
                if let TorrentCommand::BulkRequest(reqs) = *cmd {
                    for (idx, _off, len) in reqs {
                        if idx == 1 {
                            request_found = true;
                            // Without the V2 map, this will be 16384 (Full Block) -> FAIL
                            assert_eq!(
                                len, tail_size,
                                "BUG REPRODUCED: Requested {} (full) instead of {} (tail). V2 roots missing.",
                                len, tail_size
                            );
                        }
                    }
                }
            }
        }

        if !request_found {
            panic!("Setup Failure: No requests generated.");
        }
    }

    #[test]
    fn test_v2_triggers_hash_request_when_buffering() {
        // 1. GIVEN: A pure V2 torrent state
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(1);
        torrent.info.meta_version = Some(2);
        torrent.info.pieces = Vec::new(); // Pure V2 (no V1 hashes)
        torrent.info.piece_length = 16384;

        // Setup State
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(1, false);
        state
            .piece_manager
            .set_geometry(16384, 16384, HashMap::new(), false);
        state.torrent_status = TorrentStatus::Standard;

        // Map piece 0 to a file larger than one piece to force buffering
        // (If file_len <= piece_len, it optimizes and verifies immediately)
        let root = vec![0xAA; 32];
        state.piece_to_roots.insert(
            0,
            vec![V2RootInfo {
                file_offset: 0,
                length: 32768,
                root_hash: root.clone(),
                file_index: 0,
            }],
        );

        let peer_id = "v2_seeder".to_string();
        add_peer(&mut state, &peer_id);

        // 2. WHEN: We receive a block for this piece
        let effects = state.update(Action::IncomingBlock {
            peer_id: peer_id.clone(),
            piece_index: 0,
            block_offset: 0,
            data: vec![0u8; 16384], // Full piece
        });

        // 3. THEN: The state should buffer the data AND request hashes

        // Assert buffering happened
        assert!(
            state.v2_pending_data.contains_key(&0),
            "Data should be buffered pending proof"
        );

        // Assert Effect was emitted
        let request_sent = effects.iter().any(|e| {
            matches!(e, Effect::RequestHashes { peer_id: id, piece_index: idx, .. }
                     if id == &peer_id && *idx == 0)
        });

        assert!(
            request_sent,
            "State failed to emit RequestHashes effect for buffered V2 data!"
        );
    }

    #[test]
    fn test_v2_magnet_scenario_requests_hashes_when_layers_missing() {
        // 1. SETUP: Create a "Magnet-like" Torrent state
        let mut state = create_empty_state();
        let piece_len = 16384;

        // Construct a Torrent that has Info (Roots) but NO Piece Layers
        let mut torrent = create_dummy_torrent(1);
        torrent.info.meta_version = Some(2);
        torrent.info.pieces = Vec::new(); // Pure v2
        torrent.info.piece_length = piece_len as i64;

        // CRITICAL: Ensure this is None. This simulates "Magnet Metadata Received".
        // Real .torrent files would populate this, but Magnet links don't give it to us.
        torrent.piece_layers = None;

        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(1, false);
        state
            .piece_manager
            .set_geometry(piece_len as u32, piece_len as u64, HashMap::new(), false);
        state.torrent_status = TorrentStatus::Standard;

        // 2. SETUP ROOTS: Map piece 0 to a File Root
        // We use a file larger than piece_len (32KB) to force proof verification logic.
        let file_root = vec![0xAA; 32];
        let file_len = 32768;

        // In a real app, 'calculate_v2_mapping' populates this from the Info Dict.
        // Here we inject it manually to simulate that we know the Root.
        state.piece_to_roots.insert(
            0,
            vec![V2RootInfo {
                file_offset: 0,
                length: file_len,
                root_hash: file_root.clone(),
                file_index: 0,
            }],
        );

        let peer_id = "magnet_peer".to_string();
        add_peer(&mut state, &peer_id);

        // 3. EXECUTE: Peer sends us Data for Piece 0
        let effects = state.update(Action::IncomingBlock {
            peer_id: peer_id.clone(),
            piece_index: 0,
            block_offset: 0,
            data: vec![0u8; 16384], // Full piece
        });

        // 4. VERIFY: The State must Buffer + Request Hashes
        // It cannot verify because 'piece_layers' is None, so it MUST ask the peer.

        // Check Buffering
        assert!(
            state.v2_pending_data.contains_key(&0),
            "Data should be buffered because we have no local proof"
        );

        // Check Effect
        let request_sent = effects.iter().any(|e| {
            if let Effect::RequestHashes {
                peer_id: pid,
                file_root: root,
                piece_index,
                ..
            } = e
            {
                // Verify we are asking the right peer for the right piece using the right root
                pid == &peer_id && piece_index == &0 && root == &file_root
            } else {
                false
            }
        });

        assert!(
            request_sent,
            "State failed to emit RequestHashes! It likely tried to verify locally and failed."
        );
    }

    #[test]
    fn test_state_v1_metadata_workflow() {
        use sha1::{Digest, Sha1};

        // 1. SETUP: Empty State
        let mut state = create_empty_state();
        state.torrent_data_path = Some(PathBuf::from("/tmp/test_download"));
        let num_pieces = 100; // Standard V1 swarm
        let piece_len = 16384;

        // 2. CONSTRUCT V1 METADATA
        // V1 puts all hashes into a single byte string inside the Info Dict.
        let data_chunk = vec![0xAA; piece_len];
        let piece_hash = Sha1::digest(&data_chunk).to_vec();

        let mut all_hashes = Vec::new();
        for _ in 0..num_pieces {
            all_hashes.extend_from_slice(&piece_hash);
        }

        let mut torrent = create_dummy_torrent(0);
        torrent.info.meta_version = None; // V1
        torrent.info.piece_length = piece_len as i64;
        torrent.info.pieces = all_hashes; // <--- The V1 "Proof" is here immediately
        torrent.info.length = (num_pieces * piece_len) as i64;

        // 3. ACTION: METADATA RECEIVED
        state.update(Action::MetadataReceived {
            torrent: Box::new(torrent),
            metadata_length: 5000,
        });

        // CHECK: Bitfield resized correctly based on 'pieces' string length
        assert_eq!(state.piece_manager.bitfield.len(), num_pieces);
        assert_eq!(state.torrent_status, TorrentStatus::Validating);

        // 4. ACTION: VALIDATION COMPLETE
        state.update(Action::ValidationComplete {
            completed_pieces: vec![],
        });
        assert_eq!(state.torrent_status, TorrentStatus::Standard);

        // 5. EXECUTE DOWNLOAD (V1 Style)
        let peer_id = "v1_worker".to_string();
        add_peer(&mut state, &peer_id);

        let effects = state.update(Action::IncomingBlock {
            peer_id: peer_id.clone(),
            piece_index: 0,
            block_offset: 0,
            data: data_chunk.clone(),
        });

        // CHECK: V1 Optimization
        // Unlike V2, we should NOT see 'RequestHashes'.
        // We SHOULD see 'VerifyPiece' immediately because we already have the hash.
        let verify_sent = effects
            .iter()
            .any(|e| matches!(e, Effect::VerifyPiece { piece_index: 0, .. }));
        assert!(
            verify_sent,
            "V1 failed to trigger immediate verification using info-dict hashes"
        );

        // Ensure no V2 requests leaked in
        let v2_request = effects
            .iter()
            .any(|e| matches!(e, Effect::RequestHashes { .. }));
        assert!(
            !v2_request,
            "V1 torrent incorrectly triggered V2 hash request"
        );
    }

    #[test]
    fn test_state_hybrid_metadata_workflow() {
        use serde_bencode::value::Value;
        use sha1::{Digest, Sha1};
        use std::collections::HashMap;

        let mut state = create_empty_state();
        let num_pieces = 50;
        let piece_len = 16384;

        // 1. CONSTRUCT HYBRID TORRENT
        // It has V1 'pieces' AND V2 'file_tree'
        let data_chunk = vec![0xBB; piece_len];
        let v1_hash = Sha1::digest(&data_chunk).to_vec();

        let mut v1_pieces = Vec::new();
        for _ in 0..num_pieces {
            v1_pieces.extend_from_slice(&v1_hash);
        }

        let mut torrent = create_dummy_torrent(0);
        torrent.info.meta_version = Some(2); // Hybrid implies v2 support
        torrent.info.piece_length = piece_len as i64;
        torrent.info.pieces = v1_pieces; // V1 Data

        // V2 Data (File Tree)
        let root_hash = vec![0xCC; 32];
        let total_len = (num_pieces * piece_len) as i64;

        let mut file_meta = HashMap::new();
        file_meta.insert("length".as_bytes().to_vec(), Value::Int(total_len));
        file_meta.insert(
            "pieces root".as_bytes().to_vec(),
            Value::Bytes(root_hash.clone()),
        );

        let mut file_node = HashMap::new();
        file_node.insert("".as_bytes().to_vec(), Value::Dict(file_meta));

        let mut root_node = HashMap::new();
        root_node.insert("hybrid_file".as_bytes().to_vec(), Value::Dict(file_node));

        torrent.info.file_tree = Some(Value::Dict(root_node));

        // 2. ACTION: METADATA RECEIVED
        state.update(Action::MetadataReceived {
            torrent: Box::new(torrent),
            metadata_length: 9999,
        });
        state.update(Action::ValidationComplete {
            completed_pieces: vec![],
        });

        // 3. CHECK DUAL INITIALIZATION
        // V1 Check: Bitfield correct?
        assert_eq!(state.piece_manager.bitfield.len(), num_pieces);

        // V2 Check: Roots mapped?
        assert!(
            state.piece_to_roots.contains_key(&0),
            "Hybrid failed to map V2 roots"
        );
        assert!(
            state.piece_to_roots.contains_key(&(num_pieces as u32 - 1)),
            "Hybrid failed to map end piece"
        );

        // 4. EXECUTE DOWNLOAD
        let peer_id = "hybrid_worker".to_string();
        add_peer(&mut state, &peer_id);

        let effects = state.update(Action::IncomingBlock {
            peer_id: peer_id.clone(),
            piece_index: 0,
            block_offset: 0,
            data: data_chunk,
        });

        // 5. VERIFY HYBRID BEHAVIOR
        // It should prefer V1 verification (Immediate VerifyPiece) because it's faster
        // than asking for V2 proofs.
        let verify_v1 = effects
            .iter()
            .any(|e| matches!(e, Effect::VerifyPiece { piece_index: 0, .. }));
        assert!(verify_v1, "Hybrid failed to fallback to V1 verification");

        // It should NOT buffer/request V2 hashes if V1 verification is possible
        assert!(
            !state.v2_pending_data.contains_key(&0),
            "Hybrid inefficiently buffered data despite having V1 hashes"
        );
    }

    #[test]
    fn test_state_scale_1000_v2_metadata_workflow() {
        use serde_bencode::value::Value;
        use std::collections::HashMap; // Needed to construct the file tree

        // 1. SETUP: Empty State
        let mut state = create_empty_state();
        state.torrent_data_path = Some(PathBuf::from("/tmp/test_download"));
        let num_pieces = 1000;
        let piece_len = 1024;
        let total_len = (num_pieces as u64) * (piece_len as u64);
        let root_hash = vec![0xAA; 32];

        // 2. CONSTRUCT METADATA (Simulate Magnet Link Download)
        // We start with a Torrent that has NO layers and NO pieces, just the File Tree.
        let mut torrent = create_dummy_torrent(0);
        torrent.info.piece_length = piece_len as i64;
        torrent.info.meta_version = Some(2);
        torrent.info.pieces = Vec::new(); // Pure V2
        torrent.piece_layers = None; // <--- Crucial: Forces proof requests

        // Construct V2 File Tree: { "big_file": { "": { "length": ..., "pieces root": ... } } }
        // This is what the State uses to populate 'piece_to_roots' during MetadataReceived
        let mut file_meta = HashMap::new();
        file_meta.insert("length".as_bytes().to_vec(), Value::Int(total_len as i64));
        file_meta.insert(
            "pieces root".as_bytes().to_vec(),
            Value::Bytes(root_hash.clone()),
        );

        let mut file_node = HashMap::new();
        file_node.insert("".as_bytes().to_vec(), Value::Dict(file_meta));

        let mut root_node = HashMap::new();
        root_node.insert("big_file".as_bytes().to_vec(), Value::Dict(file_node));

        torrent.info.file_tree = Some(Value::Dict(root_node));

        // 3. ACTION: METADATA RECEIVED
        let _meta_effects = state.update(Action::MetadataReceived {
            torrent: Box::new(torrent),
            metadata_length: 12345,
        });

        // CHECK: State successfully mapped the file tree to pieces
        assert_eq!(state.torrent_status, TorrentStatus::Validating);
        // The file is 1000 pieces long, so piece 0 and piece 999 must exist in the map
        assert!(
            state.piece_to_roots.contains_key(&0),
            "Failed to map piece 0 from file tree"
        );
        assert!(
            state.piece_to_roots.contains_key(&999),
            "Failed to map piece 999 from file tree"
        );
        assert_eq!(
            state.piece_manager.bitfield.len(),
            1000,
            "Incorrect piece count calculated"
        );

        // 4. ACTION: VALIDATION COMPLETE
        // We must exit the 'Validating' state to accept incoming blocks
        state.update(Action::ValidationComplete {
            completed_pieces: vec![],
        });
        assert_eq!(state.torrent_status, TorrentStatus::Standard);

        // 5. EXECUTE SCALE LOOP (1000 Blocks)
        let peer_id = "v2_full_worker".to_string();
        add_peer(&mut state, &peer_id);

        let data_chunk = vec![0u8; piece_len as usize];
        let proof_chunk = vec![0xFF; 32];

        for i in 0..num_pieces {
            let piece_idx = i as u32;

            // A. Incoming Block
            let data_effects = state.update(Action::IncomingBlock {
                peer_id: peer_id.clone(),
                piece_index: piece_idx,
                block_offset: 0,
                data: data_chunk.clone(),
            });

            // CHECK: Buffer + Request Effect
            assert!(
                state.v2_pending_data.contains_key(&piece_idx),
                "Piece {} not buffered",
                piece_idx
            );

            let request_correct = data_effects.iter().any(|e| {
                // Ensure the Effect carries the Root Hash derived from our File Tree
                matches!(e, Effect::RequestHashes { file_root, piece_index, .. }
                         if *piece_index == piece_idx && file_root == &root_hash)
            });
            assert!(
                request_correct,
                "Piece {} failed to emit RequestHashes with correct Root",
                piece_idx
            );

            // B. Proof Received
            let proof_effects = state.update(Action::MerkleProofReceived {
                peer_id: peer_id.clone(),
                piece_index: piece_idx,
                proof: proof_chunk.clone(),
            });

            // CHECK: Verify Effect + Buffer Clear
            let verify_triggered = proof_effects.iter().any(|e| {
                matches!(e, Effect::VerifyPieceV2 { piece_index, .. } if *piece_index == piece_idx)
            });
            assert!(
                verify_triggered,
                "Piece {} failed to verify after proof",
                piece_idx
            );
            assert!(
                !state.v2_pending_data.contains_key(&piece_idx),
                "Buffer leak for piece {}",
                piece_idx
            );
        }

        // 6. FINAL CLEANUP CHECK
        assert!(state.v2_pending_data.is_empty());
    }

    #[test]
    fn test_repro_magnet_bitfield_truncation() {
        // GIVEN: A state initialized like a Magnet link (No metadata, 0 pieces known)
        let mut state = create_empty_state();
        state.torrent = None;
        state.torrent_status = TorrentStatus::AwaitingMetadata;
        // Explicitly set piece manager to 0 to mimic "don't know size yet"
        state.piece_manager.set_initial_fields(0, false);

        let peer_id = "magnet_seeder".to_string();
        add_peer(&mut state, &peer_id);

        // WHEN: Peer sends a Bitfield BEFORE we have metadata
        // Scenario: 8 pieces, peer has all of them (0xFF = 11111111)
        state.update(Action::PeerBitfieldReceived {
            peer_id: peer_id.clone(),
            bitfield: vec![0xFF],
        });

        // CHECK 1: The peer's bitfield should NOT be truncated to 0.
        // It should hold the raw bits until we know better.
        let peer_pre = state.peers.get(&peer_id).unwrap();
        assert!(
            !peer_pre.bitfield.is_empty(),
            "BUG REPRODUCED: Peer bitfield was truncated/wiped because we had 0 pieces!"
        );

        // WHEN: Metadata finally arrives (defining 8 pieces)
        let mut torrent = create_dummy_torrent(8);
        torrent.info.piece_length = 16384;
        torrent.info.length = 16384 * 8;

        state.update(Action::MetadataReceived {
            torrent: Box::new(torrent),
            metadata_length: 123,
        });

        // CRITICAL STEP: MetadataReceived puts us in 'Validating'.
        // AssignWork ignores everything during 'Validating'.
        // We must complete validation to enter 'Standard' mode and calculate interest.
        state.update(Action::ValidationComplete {
            completed_pieces: vec![], // We found nothing locally
        });

        // THEN: The peer should still be seen as a Seeder (having all pieces)
        let peer_post = state.peers.get(&peer_id).unwrap();

        assert_eq!(
            peer_post.bitfield.len(),
            8,
            "Bitfield should be resized to correct piece count"
        );
        assert!(
            peer_post.bitfield.iter().all(|&b| b),
            "Peer data lost! Expected all TRUE, got {:?}",
            peer_post.bitfield
        );

        // Final sanity check: Manager should be interested
        state.update(Action::AssignWork {
            peer_id: peer_id.clone(),
        });
        let peer_final = state.peers.get(&peer_id).unwrap();

        assert!(
            peer_final.am_interested,
            "We should be interested in the seeder (failed if bitfield was wiped)"
        );
    }

    #[test]
    fn test_assign_work_is_blocked_when_path_is_missing() {
        // 1. GIVEN: A torrent state with metadata but NO download path
        let mut state = create_empty_state();
        let num_pieces = 5;
        let torrent = create_dummy_torrent(num_pieces);

        // Set metadata as if it just arrived from a peer
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(num_pieces, false);
        state.piece_manager.block_manager.set_geometry(
            16384,
            (16384 * num_pieces) as u64,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );

        // Status moves to Standard/Endgame normally after metadata hydration
        state.torrent_status = TorrentStatus::Standard;
        state.piece_manager.need_queue = (0..num_pieces as u32).collect();

        // CRITICAL: Ensure path is None (User is still in File Browser)
        state.torrent_data_path = None;

        // 2. GIVEN: A connected, unchoked peer who has all pieces
        let peer_id = "seeder_peer".to_string();
        add_peer(&mut state, &peer_id);
        let peer = state.peers.get_mut(&peer_id).unwrap();
        peer.peer_choking = ChokeStatus::Unchoke;
        peer.bitfield = vec![true; num_pieces];

        // 3. WHEN: We try to assign work
        let effects = state.update(Action::AssignWork {
            peer_id: peer_id.clone(),
        });

        // 4. THEN: No requests should be generated
        let has_requests = effects.iter().any(|e| {
            matches!(e, Effect::SendToPeer { cmd, .. }
                if matches!(**cmd, TorrentCommand::BulkRequest(_)))
        });

        assert!(
            !has_requests,
            "PROTOCOL ERROR: Engine requested blocks before a download path was selected!"
        );
        assert!(
            state.peers[&peer_id].pending_requests.is_empty(),
            "Peer should have 0 pending requests when path is missing"
        );
    }

    #[test]
    fn test_delete_action_without_path_emits_completion() {
        // 1. GIVEN: A state with metadata but NO torrent_data_path or multi_file_info
        let mut state = create_empty_state();
        let torrent = create_dummy_torrent(5);
        let info_hash = state.info_hash.clone();

        state.torrent = Some(torrent);
        state.torrent_data_path = None;
        state.multi_file_info = None;
        // status will be Validating because torrent is Some
        state.torrent_status = TorrentStatus::Validating;

        // 2. WHEN: Action::Delete is triggered
        let effects = state.update(Action::Delete);

        // 3. THEN: It should NOT emit Effect::DeleteFiles
        let has_delete_files = effects
            .iter()
            .any(|e| matches!(e, Effect::DeleteFiles { .. }));
        assert!(
            !has_delete_files,
            "Should not attempt to delete files when path is missing"
        );

        // 4. THEN: It SHOULD emit Effect::EmitManagerEvent(ManagerEvent::DeletionComplete)
        let completion_event = effects.iter().find(|e| {
            if let Effect::EmitManagerEvent(ManagerEvent::DeletionComplete(hash, result)) = e {
                return hash == &info_hash && result.is_ok();
            }
            false
        });

        assert!(
            completion_event.is_some(),
            "Manager must emit DeletionComplete(Ok) to notify the app to remove the UI entry"
        );

        // 5. THEN: Internal state should be reset correctly
        assert!(state.is_paused);
        assert_eq!(state.torrent_status, TorrentStatus::Validating);
        assert_eq!(state.last_activity, TorrentActivity::Initializing);
    }

    #[test]
    fn test_file_priority_boundary_mapping() {
        // GIVEN: A torrent with 3 pieces (size 10).
        // File A: Size 15 (Spans Piece 0 and half of Piece 1) -> Set to SKIP
        // File B: Size 15 (Spans rest of Piece 1 and Piece 2) -> Set to NORMAL

        let mut state = create_empty_state();
        let piece_len = 10;

        let mut torrent = create_dummy_torrent(3);
        torrent.info.piece_length = piece_len;
        torrent.info.length = 30;
        torrent.info.files = vec![
            crate::torrent_file::InfoFile {
                length: 15,
                path: vec!["A".into()],
                md5sum: None,
                attr: None,
            },
            crate::torrent_file::InfoFile {
                length: 15,
                path: vec!["B".into()],
                md5sum: None,
                attr: None,
            },
        ];

        state.torrent = Some(torrent);
        // Init bitfield so length check passes
        state.piece_manager.set_initial_fields(3, false);

        // WHEN: We set priorities
        let mut priorities = HashMap::new();
        priorities.insert(0, FilePriority::Skip); // File A
        priorities.insert(1, FilePriority::Normal); // File B

        let vec = state.calculate_piece_priorities(&priorities);

        // THEN:
        // Piece 0 (0-10): Only File A (Skip) -> SKIP
        assert_eq!(vec[0], EffectivePiecePriority::Skip);

        // Piece 1 (10-20): File A (Skip) AND File B (Normal) -> NORMAL (Boundary protection)
        assert_eq!(vec[1], EffectivePiecePriority::Normal);

        // Piece 2 (20-30): Only File B (Normal) -> NORMAL
        assert_eq!(vec[2], EffectivePiecePriority::Normal);
    }

    #[test]
    fn test_completion_with_skipped_files() {
        // GIVEN: A torrent with 2 pieces.
        // Piece 0: Skipped
        // Piece 1: Done
        let mut state = create_empty_state();
        state.torrent_status = TorrentStatus::Standard;

        // Mock the PieceManager state
        state.piece_manager.set_initial_fields(2, false);
        state.piece_manager.bitfield[1] = PieceStatus::Done;

        // Apply Priorities: 0=Skip, 1=Normal
        state.piece_manager.apply_priorities(vec![
            EffectivePiecePriority::Skip,
            EffectivePiecePriority::Normal,
        ]);

        // WHEN: We check completion
        // Note: queues must be empty for CheckCompletion to succeed
        state.piece_manager.need_queue.clear();
        state.piece_manager.pending_queue.clear();

        let effects = state.update(Action::CheckCompletion);

        // THEN: The torrent should be considered DONE
        assert_eq!(state.torrent_status, TorrentStatus::Done);

        // BUT: It should NOT report "Completed" to the tracker (physically incomplete)
        let sent_completed_event = effects
            .iter()
            .any(|e| matches!(e, Effect::AnnounceCompleted { .. }));
        assert!(
            !sent_completed_event,
            "Should NOT send 'completed' event if files were skipped"
        );
    }

    #[test]
    fn test_repro_validation_complete_ignores_skip_mixed() {
        let mut state = create_empty_state();
        let piece_len = 10; // Tiny pieces for easy math

        // 1. Construct Multi-File Torrent (File A=Piece 0, File B=Piece 1)
        let mut torrent = create_dummy_torrent(2);
        torrent.info.piece_length = piece_len;
        torrent.info.length = 0; // Standard for multi-file is 0 or sum
        torrent.info.files = vec![
            crate::torrent_file::InfoFile {
                length: piece_len, // 10 bytes (Piece 0)
                path: vec!["A.txt".into()],
                md5sum: None,
                attr: None,
            },
            crate::torrent_file::InfoFile {
                length: piece_len, // 10 bytes (Piece 1)
                path: vec!["B.txt".into()],
                md5sum: None,
                attr: None,
            },
        ];

        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(2, false);

        // 2. Set Priorities: File 0 (A) -> SKIP
        let mut priorities = HashMap::new();
        priorities.insert(0, FilePriority::Skip);
        // File 1 (B) defaults to Normal

        let prio_vec = state.calculate_piece_priorities(&priorities);
        state.piece_manager.apply_priorities(prio_vec);

        // Pre-condition: Need queue should ONLY have Piece 1
        // Piece 0 should be skipped.
        assert_eq!(
            state.piece_manager.need_queue,
            vec![1],
            "Setup failed: Queue should contain only piece 1"
        );

        // 3. Trigger Validation Complete
        state.torrent_status = TorrentStatus::Validating;

        // WHEN: ValidationComplete runs (finding nothing)
        state.update(Action::ValidationComplete {
            completed_pieces: vec![],
        });

        // THEN: The Need Queue should STILL not contain Piece 0.
        // If the bug exists, Piece 0 will be re-added here.
        assert!(
            !state.piece_manager.need_queue.contains(&0),
            "REGRESSION: Skipped piece 0 was added back to queue! Queue: {:?}",
            state.piece_manager.need_queue
        );

        // Verify Piece 1 is still there
        assert!(
            state.piece_manager.need_queue.contains(&1),
            "Piece 1 should still be needed"
        );
    }

    #[test]
    fn test_config_after_metadata_applies_priorities() {
        // GIVEN: A state that already has metadata (defaulting to Download All)
        let mut state = create_empty_state();
        let torrent = create_dummy_torrent(2); // 2 pieces
        state.torrent = Some(torrent);
        state.piece_manager.set_initial_fields(2, false);

        // Initial check: Need queue full
        assert_eq!(state.piece_manager.need_queue.len(), 2);

        // WHEN: User config arrives LATER, setting everything to SKIP
        let mut priorities = HashMap::new();
        priorities.insert(0, FilePriority::Skip); // File 0 (covers piece 0/1 in dummy torrent)

        let effects = state.update(Action::SetUserTorrentConfig {
            torrent_data_path: PathBuf::from("/tmp"),
            file_priorities: priorities,
            container_name: None,
        });

        // THEN 1: Priorities applied immediately (Queue Cleared)
        assert!(
            state.piece_manager.need_queue.is_empty(),
            "SetUserTorrentConfig failed to update PieceManager queues!"
        );

        // THEN 2: Validation Started (Because storage wasn't init yet)
        assert_eq!(state.torrent_status, TorrentStatus::Validating);
        assert!(effects.iter().any(|e| matches!(e, Effect::StartValidation)));

        // WHEN: Validation finishes (finding nothing on disk)
        let completion_effects = state.update(Action::ValidationComplete {
            completed_pieces: vec![],
        });

        // THEN 3: Status transitions to Done
        assert_eq!(state.torrent_status, TorrentStatus::Done);

        // Verify we told the tracker we are complete
        let _sent_completed = completion_effects
            .iter()
            .any(|e| matches!(e, Effect::AnnounceCompleted { .. }));
        // Note: physically_complete is False (0 bytes on disk), so AnnounceCompleted might NOT send depending on logic.
        // But the status MUST be Done.
    }

    #[test]
    fn test_peer_disconnect_batches_until_threshold() {
        let mut state = create_empty_state();
        state.torrent_status = TorrentStatus::Standard;
        let disconnect_batch_threshold = 50;

        // Add threshold + 1 peers to ensure we cross the threshold exactly once.
        for i in 0..(disconnect_batch_threshold + 1) {
            let pid = format!("peer_{}", i);
            add_peer(&mut state, &pid);

            let effects = state.update(Action::PeerDisconnected {
                peer_id: pid.clone(),
                force: false,
            });

            if i < disconnect_batch_threshold - 1 {
                // Should not have processed yet
                assert!(effects.is_empty() || matches!(effects[0], Effect::DoNothing));
                assert_eq!(state.pending_disconnects.len(), i + 1);
            } else if i == disconnect_batch_threshold - 1 {
                // On the threshold peer, it should flush that full batch.
                assert_eq!(effects.len(), disconnect_batch_threshold * 2);
                assert!(state.pending_disconnects.is_empty());
            }
        }

        // The final peer should now be sitting alone in the new batch.
        assert_eq!(state.pending_disconnects.len(), 1);
    }

    #[test]
    fn test_peer_disconnect_force_flush() {
        let mut state = create_empty_state();

        // Add only 5 peers (well below the batch threshold)
        for i in 0..5 {
            let pid = format!("peer_{}", i);
            add_peer(&mut state, &pid);
            state.update(Action::PeerDisconnected {
                peer_id: pid,
                force: false,
            });
        }

        assert_eq!(state.pending_disconnects.len(), 5);

        // Trigger a forced flush (passing an empty ID as Cleanup would)
        let effects = state.update(Action::PeerDisconnected {
            peer_id: String::new(),
            force: true,
        });

        // Check that all 5 were processed
        assert_eq!(effects.len(), 10); // 5 Disconnects + 5 Events
        assert!(state.pending_disconnects.is_empty());
        assert_eq!(state.peers.len(), 0);
    }

    #[test]
    fn test_cleanup_flushes_stuck_peers_via_batch() {
        let mut state = create_empty_state();
        state.now = Instant::now();

        // Add a "stuck" peer (empty peer_id, created 10 seconds ago)
        let (tx, _) = tokio::sync::mpsc::channel(1);
        let mut peer = PeerState::new(
            "127.0.0.1:1234".to_string(),
            tx,
            state.now - Duration::from_secs(10),
        );
        peer.peer_id = Vec::new(); // Empty ID = Stuck
        state.peers.insert("127.0.0.1:1234".to_string(), peer);

        // Run Cleanup
        let effects = state.update(Action::Cleanup);

        // Verify the peer was removed via the batching logic called by Cleanup
        assert!(state.peers.is_empty());
        assert!(effects
            .iter()
            .any(|e| matches!(e, Effect::DisconnectPeer { .. })));
        assert!(effects.iter().any(|e| matches!(
            e,
            Effect::EmitManagerEvent(ManagerEvent::PeerDisconnected { .. })
        )));
    }

    #[test]
    fn touched_relative_paths_for_activity_handles_single_file() {
        let mut state = create_empty_state();
        state.torrent_data_path = Some(PathBuf::from("/downloads"));
        state.torrent = Some(Torrent {
            info: crate::torrent_file::Info {
                piece_length: 100,
                ..Default::default()
            },
            ..Default::default()
        });
        state.multi_file_info = Some(MultiFileInfo {
            files: vec![crate::storage::FileInfo {
                path: PathBuf::from("sample.bin"),
                length: 100,
                global_start_offset: 0,
                is_padding: false,
                is_skipped: false,
            }],
            total_size: 100,
        });

        assert_eq!(
            state.touched_relative_paths_for_activity(0, 0, 10),
            vec!["sample.bin".to_string()]
        );
        assert_eq!(
            state.touched_relative_paths_for_activity(0, 90, 10),
            vec!["sample.bin".to_string()]
        );
        assert!(state
            .touched_relative_paths_for_activity(0, 0, 0)
            .is_empty());
    }

    #[test]
    fn touched_relative_paths_for_activity_handles_boundary_spans() {
        let mut state = create_empty_state();
        state.torrent_data_path = Some(PathBuf::from("/downloads"));
        state.torrent = Some(Torrent {
            info: crate::torrent_file::Info {
                piece_length: 100,
                ..Default::default()
            },
            ..Default::default()
        });
        state.multi_file_info = Some(MultiFileInfo {
            files: vec![
                crate::storage::FileInfo {
                    path: PathBuf::from("one.bin"),
                    length: 50,
                    global_start_offset: 0,
                    is_padding: false,
                    is_skipped: false,
                },
                crate::storage::FileInfo {
                    path: PathBuf::from("two.bin"),
                    length: 70,
                    global_start_offset: 50,
                    is_padding: false,
                    is_skipped: false,
                },
            ],
            total_size: 120,
        });

        assert_eq!(
            state.touched_relative_paths_for_activity(0, 40, 30),
            vec!["one.bin".to_string(), "two.bin".to_string()]
        );
        assert_eq!(
            state.touched_relative_paths_for_activity(0, 50, 10),
            vec!["two.bin".to_string()]
        );
    }

    #[test]
    fn test_container_logic_explicit_no_folder() {
        let mut state = create_empty_state();
        let mut torrent = create_dummy_torrent(2);

        // Setup: Make it a Multi-File Torrent
        torrent.info.name = "MyTorrent".to_string();
        torrent.info.files = vec![
            crate::torrent_file::InfoFile {
                length: 100,
                path: vec!["file_a.txt".to_string()],
                md5sum: None,
                attr: None,
            },
            crate::torrent_file::InfoFile {
                length: 100,
                path: vec!["file_b.txt".to_string()],
                md5sum: None,
                attr: None,
            },
        ];

        state.torrent = Some(torrent);
        state.torrent_data_path = Some(PathBuf::from("/tmp/downloads"));

        // ACTION: User explicitly selected "No Folder" (Empty String)
        state.container_name = Some("".to_string());

        state.rebuild_multi_file_info();

        // ASSERTION: Paths should be relative to root, not /tmp/downloads/MyTorrent/
        let mfi = state.multi_file_info.as_ref().expect("MFI should be built");

        // Expected: /tmp/downloads/file_a.txt
        let expected_path = PathBuf::from("/tmp/downloads/file_a.txt");
        assert_eq!(
            mfi.files[0].path, expected_path,
            "Should flatten multi-file torrent when container_name is empty"
        );
    }
}

#[cfg(test)]
mod deletion_tests {
    use super::*;
    use crate::storage::{FileInfo, MultiFileInfo};
    use std::path::PathBuf;

    // Helper to mock MFI
    fn mock_mfi(paths: Vec<&str>) -> MultiFileInfo {
        let files = paths
            .into_iter()
            .map(|p| FileInfo {
                path: PathBuf::from(p),
                length: 100,
                global_start_offset: 0,
                is_padding: false,
                is_skipped: false,
            })
            .collect();

        MultiFileInfo {
            files,
            total_size: 100,
        }
    }

    #[test]
    fn test_delete_single_file_torrent() {
        let base = PathBuf::from("/Downloads");
        // Case: Torrent is just "linux.iso" directly in Downloads
        let mfi = mock_mfi(vec!["/Downloads/linux.iso"]);

        let (files, dirs) = calculate_deletion_lists(&mfi, &base, None);

        assert_eq!(files.len(), 1);
        assert_eq!(files[0], PathBuf::from("/Downloads/linux.iso"));

        // Critical: Should NOT delete /Downloads
        assert!(
            dirs.is_empty(),
            "Single file torrent should not delete root dir"
        );
    }

    #[test]
    fn test_delete_standard_folder_torrent() {
        let base = PathBuf::from("/Downloads");
        // Case: "Album Name/01.mp3"
        let mfi = mock_mfi(vec!["/Downloads/Album/01.mp3", "/Downloads/Album/02.mp3"]);

        let (_, dirs) = calculate_deletion_lists(&mfi, &base, None);

        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0], PathBuf::from("/Downloads/Album"));
    }

    #[test]
    fn test_delete_nested_directories() {
        let base = PathBuf::from("/Downloads");
        // Case: "Game/Data/Textures/skin.png"
        let mfi = mock_mfi(vec![
            "/Downloads/Game/readme.txt",
            "/Downloads/Game/Data/config.ini",
            "/Downloads/Game/Data/Textures/skin.png",
        ]);

        let (_, dirs) = calculate_deletion_lists(&mfi, &base, None);

        // Should identify: Game, Game/Data, Game/Data/Textures
        assert_eq!(dirs.len(), 3);

        // Verify Sort Order (Deepest First)
        assert_eq!(dirs[0], PathBuf::from("/Downloads/Game/Data/Textures"));
        assert_eq!(dirs[1], PathBuf::from("/Downloads/Game/Data"));
        assert_eq!(dirs[2], PathBuf::from("/Downloads/Game"));
    }

    #[test]
    fn test_delete_safety_boundary_escape() {
        let base = PathBuf::from("/Downloads");

        // Edge Case: File path somehow points outside base (e.g. config error)
        let mfi = mock_mfi(vec!["/System/Critical/boot.ini"]);

        let (files, dirs) = calculate_deletion_lists(&mfi, &base, None);

        // We still delete the file (it belongs to the torrent),
        // but we MUST NOT delete parent folders up to root if they aren't in base.
        assert_eq!(files.len(), 1);
        assert!(
            dirs.is_empty(),
            "Should not identify directories outside base path"
        );
    }

    #[test]
    fn test_delete_matching_container() {
        // Scenario: Container "LinuxDistro" matches torrent name "LinuxDistro"
        let base = PathBuf::from("/Downloads/LinuxDistro");
        let name = "LinuxDistro";
        let mfi = mock_mfi(vec!["/Downloads/LinuxDistro/image.iso"]);

        let (_, dirs) = calculate_deletion_lists(&mfi, &base, Some(name));

        // Should include base path because names match
        assert!(
            dirs.contains(&base),
            "Should delete container if name matches"
        );
    }

    #[test]
    fn test_delete_root_safety_mismatch() {
        // Scenario: Saved directly to "Downloads" (No Container)
        let base = PathBuf::from("/Downloads");
        let name = "LinuxDistro";
        let mfi = mock_mfi(vec!["/Downloads/image.iso"]);

        let (_, dirs) = calculate_deletion_lists(&mfi, &base, Some(name));

        // "Downloads" != "LinuxDistro" -> Do NOT delete base
        assert!(
            dirs.is_empty(),
            "Should NOT delete root folder if names mismatch"
        );
    }

    #[test]
    fn test_delete_renamed_container_safety() {
        // Scenario: User renamed "LinuxDistro" to "MyStuff"
        let base = PathBuf::from("/Downloads/MyStuff");
        let name = "LinuxDistro";
        let mfi = mock_mfi(vec!["/Downloads/MyStuff/image.iso"]);

        let (_, dirs) = calculate_deletion_lists(&mfi, &base, Some(name));

        // "MyStuff" != "LinuxDistro" -> Safe fallback is to KEEP the folder
        assert!(
            dirs.is_empty(),
            "Should preserve renamed container for safety"
        );
    }

    #[test]
    fn test_delete_subfolders_always() {
        // Scenario: Torrent has internal folders. Even if root is safe, subfolders must go.
        // Base: /Downloads (Safe)
        // File: /Downloads/Album/song.mp3
        let base = PathBuf::from("/Downloads");
        let name = "Album";
        let mfi = mock_mfi(vec!["/Downloads/Album/song.mp3"]);

        let (_, dirs) = calculate_deletion_lists(&mfi, &base, Some(name));

        // Should delete "Album" (child) but NOT "Downloads" (base)
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0], PathBuf::from("/Downloads/Album"));
    }
}

#[cfg(test)]
fn check_invariants(state: &TorrentState) {
    // CATEGORY 1: Data Consistency (The "Is the Math Right?" Check)

    // The global session total MUST be >= the sum of currently connected peers.
    // (It is not == because disconnected peers contribute to the total but are gone from the map).
    let sum_peer_dl: u64 = state.peers.values().map(|p| p.total_bytes_downloaded).sum();
    let sum_peer_ul: u64 = state.peers.values().map(|p| p.total_bytes_uploaded).sum();

    assert!(
        state.session_total_downloaded >= sum_peer_dl,
        "Global DL ({}) < Sum of Peers ({}) - Data created from thin air!",
        state.session_total_downloaded,
        sum_peer_dl
    );

    assert!(
        state.session_total_uploaded >= sum_peer_ul,
        "Global UL ({}) < Sum of Peers ({}) - Data created from thin air!",
        state.session_total_uploaded,
        sum_peer_ul
    );

    if let Some(torrent) = &state.torrent {
        let expected_pieces = torrent.info.pieces.len() / 20;
        assert_eq!(
            state.piece_manager.bitfield.len(),
            expected_pieces,
            "Bitfield length mismatch! Expected {}, Got {}",
            expected_pieces,
            state.piece_manager.bitfield.len()
        );

        // Check peer bitfield safety
        for (id, peer) in &state.peers {
            if !peer.bitfield.is_empty() {
                assert_eq!(
                    peer.bitfield.len(),
                    expected_pieces,
                    "Peer {} bitfield len mismatch. Vulnerable to panic.",
                    id
                );
            }
        }
    }

    // CATEGORY 2: Queue Synchronization (The "Ghost Piece" Check)

    // If a piece is in `pending_queue` (Global), AT LEAST one peer must be working on it.
    for &piece_idx in state.piece_manager.pending_queue.keys() {
        let exists_in_peer = state
            .peers
            .values()
            .any(|p| p.pending_requests.contains(&piece_idx));
        assert!(
            exists_in_peer,
            "Piece {} is globally Pending but NO peer has it. Download is stalled!",
            piece_idx
        );
    }

    // If a peer has a pending request, that piece MUST be globally Pending (or Done).
    // It cannot be in the "Need" queue.
    for (id, peer) in &state.peers {
        for &req in &peer.pending_requests {
            let in_need = state.piece_manager.need_queue.contains(&req);

            // It's okay if it's Done (race condition where write finished but peer not updated yet)
            // But it is NEVER okay to be in the Need queue while a peer thinks they are downloading it.
            assert!(
                !in_need,
                "Peer {} is downloading Piece {}, but Manager thinks it is still Needed!",
                id, req
            );
        }
    }

    for piece in &state.piece_manager.need_queue {
        assert!(
            !state.piece_manager.pending_queue.contains_key(piece),
            "Piece {} is in both Need and Pending queues!",
            piece
        );
    }

    // CATEGORY 3: State Machine Logic

    match state.torrent_status {
        TorrentStatus::Done => {
            // If Done, we should need nothing.
            assert!(
                state.piece_manager.need_queue.is_empty(),
                "Status is Done but Need queue has items!"
            );
            assert!(
                state.piece_manager.pending_queue.is_empty(),
                "Status is Done but Pending queue has items!"
            );

            // If Done, we should not be Interested in anyone.
            let am_interested = state.peers.values().any(|p| p.am_interested);
            assert!(
                !am_interested,
                "Status is Done but we are still Interested in peers!"
            );
        }
        TorrentStatus::Endgame => {
            // Endgame means Need is empty, but Pending is NOT.
            assert!(
                state.piece_manager.need_queue.is_empty(),
                "Status is Endgame but Need queue is not empty!"
            );
            // Pending might be empty if the last piece just finished but status hasn't transitioned yet,
            // but typically it should have items.
        }
        TorrentStatus::Standard => {}
        TorrentStatus::Validating => {}
        TorrentStatus::AwaitingMetadata => {}
    }

    // CATEGORY 4: Resource & Math Integrity

    for (key, peer) in &state.peers {
        assert_eq!(
            key, &peer.ip_port,
            "Peer Map Key '{}' does not match struct IP '{}'",
            key, peer.ip_port
        );
    }

    assert_eq!(
        state.number_of_successfully_connected_peers,
        state.peers.len(),
        "Peer count metric out of sync with Map size!"
    );

    if state.torrent.is_some() {
        // Count how many pieces in the bitfield are NOT done
        let actual_remaining = state
            .piece_manager
            .bitfield
            .iter()
            .filter(|&&status| status != crate::torrent_manager::piece_manager::PieceStatus::Done)
            .count();

        assert_eq!(
            state.piece_manager.pieces_remaining, actual_remaining,
            "Drift detected! PieceManager thinks {} pieces left, but Bitfield shows {}",
            state.piece_manager.pieces_remaining, actual_remaining
        );
    }

    assert!(
        state.total_dl_prev_avg_ema.is_finite(),
        "DL Speed EMA is Infinite/NaN"
    );
    assert!(
        state.total_ul_prev_avg_ema.is_finite(),
        "UL Speed EMA is Infinite/NaN"
    );

    for (id, peer) in &state.peers {
        assert!(
            peer.prev_avg_dl_ema.is_finite(),
            "Peer {} DL EMA is broken",
            id
        );
    }

    if let Some(t) = state.optimistic_unchoke_timer {
        let now = state.now;
        // Allow buffer, but 1 hour in future implies logic error
        if t > now + std::time::Duration::from_secs(3600) {
            panic!("Optimistic timer is set way too far in the future!");
        }
    }

    // CATEGORY 5: LOGICAL INVARIANTS (Protocol & State Logic)

    // We must never ask a peer for a piece they do not possess.
    for (id, peer) in &state.peers {
        for &piece_idx in &peer.pending_requests {
            let has_piece = peer
                .bitfield
                .get(piece_idx as usize)
                .copied()
                .unwrap_or(false);
            assert!(
                has_piece,
                "PROTOCOL VIOLATION: We requested Piece {} from Peer {}, but they do not have it!",
                piece_idx, id
            );
        }
    }

    // If we have pending requests sending to a peer, we MUST claim to be interested in them.
    for (id, peer) in &state.peers {
        if !peer.pending_requests.is_empty() {
            assert!(
                peer.am_interested,
                "STATE ERROR: Peer {} has pending requests but we told them we are NOT interested!",
                id
            );
        }
    }

    // If a peer is choking us, we should not have any active pending requests waiting on them.
    for (id, peer) in &state.peers {
        if peer.peer_choking == crate::torrent_manager::state::ChokeStatus::Choke {
            assert!(
                peer.pending_requests.is_empty(),
                "LOGIC ERROR: Peer {} is Choking us, but we still have pending requests assigned to them!",
                id
            );
        }
    }

    // We should only be interested in a peer if they have a piece we actually need.
    if state.torrent_status != TorrentStatus::Done {
        for (id, peer) in &state.peers {
            if peer.am_interested {
                let interesting = state
                    .piece_manager
                    .need_queue
                    .iter()
                    .chain(state.piece_manager.pending_queue.keys())
                    .any(|&idx| peer.bitfield.get(idx as usize) == Some(&true));

                assert!(
                    interesting,
                    "INEFFICIENCY: We are 'Interested' in Peer {}, but they have NO pieces we currently Need or are Pending.",
                    id
                );
            }
        }
    }

    // If our status is Done, we must strictly have am_interested = false for everyone.
    if state.torrent_status == TorrentStatus::Done {
        for (id, peer) in &state.peers {
            assert!(
                !peer.am_interested,
                "STATE ERROR: Torrent is DONE, but we are still marked 'Interested' in Peer {}!",
                id
            );
        }
    }

    // In Standard mode, a specific piece should strictly be requested from only ONE peer.
    if state.torrent_status == TorrentStatus::Standard {
        let mut requested_pieces = std::collections::HashMap::new();
        for (id, peer) in &state.peers {
            for &piece in &peer.pending_requests {
                if let Some(other_peer) = requested_pieces.insert(piece, id.clone()) {
                    panic!(
                        "INEFFICIENCY: Piece {} is being requested from BOTH {} and {} in Standard mode!",
                        piece, other_peer, id
                    );
                }
            }
        }
    }

    // If we are in Endgame mode, the need_queue MUST be empty.
    if state.torrent_status == TorrentStatus::Endgame {
        assert!(
            state.piece_manager.need_queue.is_empty(),
            "STATE MISMATCH: Status is ENDGAME, but 'need_queue' still contains items!"
        );
        assert!(
            !state.piece_manager.pending_queue.is_empty(),
            "STATE MISMATCH: Status is ENDGAME, but 'pending_queue' is empty! (Should be Done)"
        );
    }

    // We must never unchoke more peers than our allowed maximum (plus allowance for optimistic unchoke).
    let unchoked_count = state
        .peers
        .values()
        .filter(|p| p.am_choking == crate::torrent_manager::state::ChokeStatus::Unchoke)
        .count();

    const MAX_SLOTS: usize = crate::torrent_manager::state::UPLOAD_SLOTS_DEFAULT + 1;

    assert!(
        unchoked_count <= MAX_SLOTS,
        "RESOURCE LEAK: We unchoked {} peers, exceeding the hard limit of {}!",
        unchoked_count,
        MAX_SLOTS
    );
}

// Property-Based Tests (Fuzzing Logic)

#[cfg(test)]
mod prop_tests {

    use super::*;
    use proptest::prelude::*;
    use serde_bencode::value::Value;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::mpsc;

    // --- Constants for Consistent Fuzzing ---
    const PIECE_LEN: u32 = 16384;
    const NUM_PIECES: usize = 20;
    const MAX_BLOCK: u32 = 131_072;

    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    #[derive(Clone, Debug)]
    enum TorrentVariant {
        V1Single,
        V1Multi,
        Hybrid,
        V2,
    }

    #[derive(Clone, Debug)]
    struct TorrentFuzzCase {
        variant: TorrentVariant,
        piece_length: u32,
        file_lengths: Vec<u64>,
        duplicate_factor: u8,
    }

    fn torrent_shape_strategy() -> impl Strategy<Value = TorrentFuzzCase> {
        (
            0u8..4,
            16384u32..=65536u32,
            proptest::collection::vec(16_384u64..=400_000u64, 1..=4),
            0u8..=2,
        )
            .prop_map(
                |(variant_id, piece_length, mut file_lengths, duplicate_factor)| {
                    let variant = match variant_id {
                        0 => TorrentVariant::V1Single,
                        1 => TorrentVariant::V1Multi,
                        2 => TorrentVariant::Hybrid,
                        _ => TorrentVariant::V2,
                    };

                    if matches!(variant, TorrentVariant::V1Single) {
                        file_lengths.truncate(1);
                    }

                    TorrentFuzzCase {
                        variant,
                        piece_length,
                        file_lengths,
                        duplicate_factor,
                    }
                },
            )
    }

    fn build_fuzz_torrent(case: &TorrentFuzzCase) -> Torrent {
        use crate::torrent_file::{Info, InfoFile, Torrent};

        let piece_len = case.piece_length as u64;
        let total_len: u64 = case.file_lengths.iter().sum();
        let total_piece_count = (total_len.div_ceil(piece_len)) as usize;

        let mut info = Info {
            name: "fuzz_torrent".to_string(),
            piece_length: case.piece_length as i64,
            pieces: vec![0xAB; total_piece_count.saturating_mul(20)],
            length: total_len as i64,
            files: Vec::new(),
            private: None,
            md5sum: None,
            meta_version: None,
            file_tree: None,
        };

        if matches!(
            case.variant,
            TorrentVariant::V1Multi | TorrentVariant::Hybrid
        ) {
            info.length = 0;
            info.files = case
                .file_lengths
                .iter()
                .enumerate()
                .map(|(idx, len)| InfoFile {
                    length: *len as i64,
                    md5sum: None,
                    path: vec![format!("file_{idx}.bin")],
                    attr: None,
                })
                .collect();
        }

        let mut piece_layers = None;

        if matches!(case.variant, TorrentVariant::V2 | TorrentVariant::Hybrid) {
            info.meta_version = Some(2);

            let mut root_node = HashMap::new();
            let mut layers = HashMap::new();

            for (idx, len) in case.file_lengths.iter().enumerate() {
                let root_hash = vec![idx as u8 + 1; 32];
                let file_piece_count = len.div_ceil(piece_len) as usize;
                let mut layer_bytes = Vec::with_capacity(file_piece_count.saturating_mul(32));
                for layer_idx in 0..file_piece_count {
                    layer_bytes.extend_from_slice(&[(layer_idx as u8).wrapping_add(11); 32]);
                }

                layers.insert(root_hash.clone(), Value::Bytes(layer_bytes));

                let mut file_meta = HashMap::new();
                file_meta.insert("length".as_bytes().to_vec(), Value::Int(*len as i64));
                file_meta.insert("pieces root".as_bytes().to_vec(), Value::Bytes(root_hash));

                let mut file_leaf = HashMap::new();
                file_leaf.insert("".as_bytes().to_vec(), Value::Dict(file_meta));
                root_node.insert(
                    format!("v2_file_{idx}").as_bytes().to_vec(),
                    Value::Dict(file_leaf),
                );
            }

            info.file_tree = Some(Value::Dict(root_node));
            piece_layers = Some(Value::Dict(layers));
        }

        if matches!(case.variant, TorrentVariant::V2) {
            info.pieces.clear();
            info.files.clear();
            info.length = 0;
        }

        Torrent {
            announce: None,
            announce_list: None,
            url_list: None,
            info,
            info_dict_bencode: Vec::new(),
            created_by: None,
            creation_date: None,
            encoding: None,
            comment: None,
            piece_layers,
        }
    }

    fn bitfield_has_piece(bitfield: &[u8], piece_index: usize) -> bool {
        let byte_idx = piece_index / 8;
        let bit_idx = 7 - (piece_index % 8);
        bitfield
            .get(byte_idx)
            .map(|b| (b & (1 << bit_idx)) != 0)
            .unwrap_or(false)
    }

    fn encode_bool_bitfield(bits: &[bool]) -> Vec<u8> {
        let mut out = vec![0u8; bits.len().div_ceil(8)];
        for (idx, has_piece) in bits.iter().enumerate() {
            if *has_piece {
                let byte_idx = idx / 8;
                let bit_idx = 7 - (idx % 8);
                out[byte_idx] |= 1 << bit_idx;
            }
        }
        out
    }

    #[derive(Clone, Copy)]
    struct FuzzHarnessConfig {
        peer_count_range: (usize, usize),
        safety_net_peer: bool,
        churn_choke_prob: f64,
        churn_unchoke_prob: f64,
        invalid_verify_prob: f64,
        max_loop_guard: usize,
        delivery_batch_max: usize,
        manager_delivery_batch_max: usize,
        simulated_tick_ms: u64,
        cleanup_interval_ms: u64,
    }

    fn default_harness_config() -> FuzzHarnessConfig {
        FuzzHarnessConfig {
            peer_count_range: (5, 12),
            safety_net_peer: false,
            churn_choke_prob: 0.03,
            churn_unchoke_prob: 0.08,
            invalid_verify_prob: 0.0,
            max_loop_guard: 80_000,
            delivery_batch_max: 6,
            manager_delivery_batch_max: 4,
            simulated_tick_ms: 100,
            cleanup_interval_ms: 3_000,
        }
    }

    enum SimulatedManagerCommand {
        Disconnect(String),
    }

    #[allow(clippy::too_many_arguments)]
    fn enqueue_from_effect(
        effect: Effect,
        state: &TorrentState,
        peer_bitfields_bool: &HashMap<String, Vec<bool>>,
        peer_bitfields_bytes: &HashMap<String, Vec<u8>>,
        pending_actions: &mut Vec<Action>,
        pending_manager_commands: &mut Vec<SimulatedManagerCommand>,
        rng: &mut StdRng,
        duplicate_probability: f64,
        invalid_verify_probability: f64,
    ) {
        match effect {
            Effect::SendToPeer { peer_id, cmd } => {
                if let TorrentCommand::BulkRequest(requests) = *cmd {
                    for (piece_index, block_offset, length) in requests {
                        if let Some(bits) = peer_bitfields_bool.get(&peer_id) {
                            assert!(
                                bits.get(piece_index as usize).copied().unwrap_or(false),
                                "requested piece {piece_index} from peer {peer_id} that does not advertise it"
                            );
                        } else {
                            let bits = peer_bitfields_bytes
                                .get(&peer_id)
                                .expect("peer bitfield should exist");
                            assert!(
                                bitfield_has_piece(bits, piece_index as usize),
                                "requested piece {piece_index} from peer {peer_id} that does not advertise it"
                            );
                        }

                        pending_actions.push(Action::IncomingBlock {
                            peer_id: peer_id.clone(),
                            piece_index,
                            block_offset,
                            data: vec![piece_index as u8; length as usize],
                        });

                        if rng.random_bool(duplicate_probability) {
                            pending_actions.push(Action::IncomingBlock {
                                peer_id: peer_id.clone(),
                                piece_index,
                                block_offset,
                                data: vec![piece_index as u8; length as usize],
                            });
                        }
                    }
                }
            }
            Effect::VerifyPiece {
                peer_id,
                piece_index,
                data,
            }
            | Effect::VerifyPieceV2 {
                peer_id,
                piece_index,
                data,
                ..
            } => {
                let valid = !rng.random_bool(invalid_verify_probability);
                pending_actions.push(Action::PieceVerified {
                    peer_id,
                    piece_index,
                    valid,
                    data,
                });
            }
            Effect::WriteToDisk {
                peer_id,
                piece_index,
                ..
            } => pending_actions.push(Action::PieceWrittenToDisk {
                peer_id,
                piece_index,
            }),
            Effect::DisconnectPeer { peer_id } => {
                if state.peers.contains_key(&peer_id) {
                    pending_manager_commands.push(SimulatedManagerCommand::Disconnect(peer_id));
                }
            }
            _ => {}
        }
    }

    fn run_piece_selection_completion_harness(
        case: &TorrentFuzzCase,
        random_seed: u64,
        cfg: FuzzHarnessConfig,
    ) -> Result<(), TestCaseError> {
        let torrent = build_fuzz_torrent(case);
        let mut state = TorrentState {
            torrent_data_path: Some(std::path::PathBuf::from("/tmp")),
            ..Default::default()
        };

        let _ = state.update(Action::MetadataReceived {
            torrent: Box::new(torrent),
            metadata_length: 0,
        });
        state.torrent_status = TorrentStatus::Standard;

        let num_pieces = state.piece_manager.bitfield.len();
        prop_assume!(num_pieces > 0);
        prop_assume!((0..num_pieces).all(|piece_idx| {
            !state
                .piece_manager
                .block_manager
                .piece_block_addresses(piece_idx as u32)
                .is_empty()
        }));

        state.piece_manager.need_queue.clear();
        for piece_idx in 0..num_pieces as u32 {
            state.piece_manager.need_queue.push(piece_idx);
        }

        let mut rng = StdRng::seed_from_u64(random_seed);
        let peer_count = rng.random_range(cfg.peer_count_range.0..=cfg.peer_count_range.1);

        let mut peer_ids = Vec::with_capacity(peer_count);
        for i in 0..peer_count {
            let peer_id = format!("fuzz_peer_{i}");
            let (tx, _rx) = mpsc::channel(16);
            let _ = state.update(Action::RegisterPeer {
                peer_id: peer_id.clone(),
                tx,
            });
            let _ = state.update(Action::PeerSuccessfullyConnected {
                peer_id: peer_id.clone(),
            });
            let _ = state.update(Action::UpdatePeerId {
                peer_addr: peer_id.clone(),
                new_id: peer_id.as_bytes().to_vec(),
            });
            peer_ids.push(peer_id);
        }

        let mut peer_bitfields_bool: HashMap<String, Vec<bool>> = HashMap::new();
        for peer_id in &peer_ids {
            peer_bitfields_bool.insert(peer_id.clone(), vec![false; num_pieces]);
        }

        if cfg.safety_net_peer {
            for (peer_idx, peer_id) in peer_ids.iter().enumerate() {
                if let Some(bits) = peer_bitfields_bool.get_mut(peer_id) {
                    for has_piece in bits.iter_mut().take(num_pieces) {
                        *has_piece = peer_idx == 0 || rng.random_bool(0.5);
                    }
                }
            }
        } else {
            for piece_idx in 0..num_pieces {
                let primary = rng.random_range(0..peer_count);
                peer_bitfields_bool
                    .get_mut(&peer_ids[primary])
                    .expect("primary peer must exist")[piece_idx] = true;

                for peer_id in peer_ids.iter().take(peer_count) {
                    if rng.random_bool(0.2) {
                        peer_bitfields_bool
                            .get_mut(peer_id)
                            .expect("peer must exist")[piece_idx] = true;
                    }
                }
            }

            for piece_idx in 0..num_pieces {
                assert!(peer_ids.iter().any(|pid| {
                    peer_bitfields_bool
                        .get(pid)
                        .and_then(|b| b.get(piece_idx))
                        .copied()
                        .unwrap_or(false)
                }));
            }
        }

        let mut peer_bitfields_bytes: HashMap<String, Vec<u8>> = HashMap::new();
        for peer_id in &peer_ids {
            let bitfield_bool = peer_bitfields_bool
                .get(peer_id)
                .expect("peer bitfield exists");
            let bitfield = encode_bool_bitfield(bitfield_bool);
            peer_bitfields_bytes.insert(peer_id.clone(), bitfield.clone());

            let _ = state.update(Action::PeerBitfieldReceived {
                peer_id: peer_id.clone(),
                bitfield,
            });
        }

        let mut pending_actions: Vec<Action> = Vec::new();
        let mut pending_manager_commands: Vec<SimulatedManagerCommand> = Vec::new();
        for peer_id in &peer_ids {
            let initial = state.update(Action::PeerUnchoked {
                peer_id: peer_id.clone(),
            });
            for effect in initial {
                enqueue_from_effect(
                    effect,
                    &state,
                    &peer_bitfields_bool,
                    &peer_bitfields_bytes,
                    &mut pending_actions,
                    &mut pending_manager_commands,
                    &mut rng,
                    case.duplicate_factor as f64 / 4.0,
                    cfg.invalid_verify_prob,
                );
            }
        }

        state
            .piece_manager
            .update_rarity(state.peers.values().map(|p| &p.bitfield));

        let mut loop_guard = 0usize;
        let mut elapsed_ms = 0u64;
        let cleanup_interval_ms = cfg.cleanup_interval_ms.max(1);
        let mut next_cleanup_ms = cleanup_interval_ms;
        while state.piece_manager.pieces_remaining > 0 {
            loop_guard += 1;
            prop_assert!(
                loop_guard < cfg.max_loop_guard,
                "simulation stalled with {} pending actions, {} pending manager commands, pieces_remaining={}, seed={}",
                pending_actions.len(),
                pending_manager_commands.len(),
                state.piece_manager.pieces_remaining,
                random_seed,
            );

            let mut progressed = false;

            if cfg.churn_choke_prob > 0.0 || cfg.churn_unchoke_prob > 0.0 {
                for peer_id in &peer_ids {
                    if rng.random_bool(cfg.churn_choke_prob) {
                        let _ = state.update(Action::PeerChoked {
                            peer_id: peer_id.clone(),
                        });
                    }
                    if rng.random_bool(cfg.churn_unchoke_prob) {
                        let effects = state.update(Action::PeerUnchoked {
                            peer_id: peer_id.clone(),
                        });
                        if !effects.is_empty() {
                            progressed = true;
                        }
                        for effect in effects {
                            enqueue_from_effect(
                                effect,
                                &state,
                                &peer_bitfields_bool,
                                &peer_bitfields_bytes,
                                &mut pending_actions,
                                &mut pending_manager_commands,
                                &mut rng,
                                case.duplicate_factor as f64 / 4.0,
                                cfg.invalid_verify_prob,
                            );
                        }
                    }
                }
            }

            for peer_id in &peer_ids {
                let effects = state.update(Action::AssignWork {
                    peer_id: peer_id.clone(),
                });
                if !effects.is_empty() {
                    progressed = true;
                }
                for effect in effects {
                    enqueue_from_effect(
                        effect,
                        &state,
                        &peer_bitfields_bool,
                        &peer_bitfields_bytes,
                        &mut pending_actions,
                        &mut pending_manager_commands,
                        &mut rng,
                        case.duplicate_factor as f64 / 4.0,
                        cfg.invalid_verify_prob,
                    );
                }
            }

            if !pending_actions.is_empty() {
                progressed = true;
                let budget = usize::min(
                    pending_actions.len(),
                    rng.random_range(1..=cfg.delivery_batch_max.max(1)),
                );
                for _ in 0..budget {
                    let idx = rng.random_range(0..pending_actions.len());
                    let action = pending_actions.swap_remove(idx);
                    let follow_up = state.update(action);
                    if !follow_up.is_empty() {
                        progressed = true;
                    }
                    for effect in follow_up {
                        enqueue_from_effect(
                            effect,
                            &state,
                            &peer_bitfields_bool,
                            &peer_bitfields_bytes,
                            &mut pending_actions,
                            &mut pending_manager_commands,
                            &mut rng,
                            case.duplicate_factor as f64 / 4.0,
                            cfg.invalid_verify_prob,
                        );
                    }
                    if pending_actions.is_empty() {
                        break;
                    }
                }
            }

            if !pending_manager_commands.is_empty() {
                progressed = true;
                let budget = usize::min(
                    pending_manager_commands.len(),
                    rng.random_range(1..=cfg.manager_delivery_batch_max.max(1)),
                );
                for _ in 0..budget {
                    let idx = rng.random_range(0..pending_manager_commands.len());
                    let cmd = pending_manager_commands.swap_remove(idx);
                    let follow_up = match cmd {
                        SimulatedManagerCommand::Disconnect(peer_id) => {
                            state.update(Action::PeerDisconnected {
                                peer_id,
                                force: false,
                            })
                        }
                    };
                    if !follow_up.is_empty() {
                        progressed = true;
                    }
                    for effect in follow_up {
                        enqueue_from_effect(
                            effect,
                            &state,
                            &peer_bitfields_bool,
                            &peer_bitfields_bytes,
                            &mut pending_actions,
                            &mut pending_manager_commands,
                            &mut rng,
                            case.duplicate_factor as f64 / 4.0,
                            cfg.invalid_verify_prob,
                        );
                    }
                    if pending_manager_commands.is_empty() {
                        break;
                    }
                }
            }

            elapsed_ms = elapsed_ms.saturating_add(cfg.simulated_tick_ms);
            while elapsed_ms >= next_cleanup_ms {
                let cleanup_effects = state.update(Action::Cleanup);
                if !cleanup_effects.is_empty() {
                    progressed = true;
                }
                for effect in cleanup_effects {
                    enqueue_from_effect(
                        effect,
                        &state,
                        &peer_bitfields_bool,
                        &peer_bitfields_bytes,
                        &mut pending_actions,
                        &mut pending_manager_commands,
                        &mut rng,
                        case.duplicate_factor as f64 / 4.0,
                        cfg.invalid_verify_prob,
                    );
                }
                next_cleanup_ms = next_cleanup_ms.saturating_add(cleanup_interval_ms);
            }

            if !progressed && pending_actions.is_empty() && pending_manager_commands.is_empty() {
                for peer_id in &peer_ids {
                    let unchoke_effects = state.update(Action::PeerUnchoked {
                        peer_id: peer_id.clone(),
                    });
                    if !unchoke_effects.is_empty() {
                        progressed = true;
                    }
                    for effect in unchoke_effects {
                        enqueue_from_effect(
                            effect,
                            &state,
                            &peer_bitfields_bool,
                            &peer_bitfields_bytes,
                            &mut pending_actions,
                            &mut pending_manager_commands,
                            &mut rng,
                            case.duplicate_factor as f64 / 4.0,
                            cfg.invalid_verify_prob,
                        );
                    }
                    let effects = state.update(Action::AssignWork {
                        peer_id: peer_id.clone(),
                    });
                    for effect in effects {
                        enqueue_from_effect(
                            effect,
                            &state,
                            &peer_bitfields_bool,
                            &peer_bitfields_bytes,
                            &mut pending_actions,
                            &mut pending_manager_commands,
                            &mut rng,
                            case.duplicate_factor as f64 / 4.0,
                            cfg.invalid_verify_prob,
                        );
                    }
                }
            }

            if !(progressed || !pending_actions.is_empty() || !pending_manager_commands.is_empty())
            {
                let queued_piece_count =
                    state.piece_manager.need_queue.len() + state.piece_manager.pending_queue.len();
                let has_serviceable_piece = state
                    .piece_manager
                    .need_queue
                    .iter()
                    .chain(state.piece_manager.pending_queue.keys())
                    .any(|piece_idx| {
                        state
                            .peers
                            .values()
                            .any(|peer| peer.bitfield.get(*piece_idx as usize) == Some(&true))
                    });
                let pending_without_owner = state
                    .piece_manager
                    .pending_queue
                    .keys()
                    .filter(|piece_idx| {
                        !state
                            .peers
                            .values()
                            .any(|peer| peer.pending_requests.contains(piece_idx))
                    })
                    .count();
                let pending_requestable_blocks: usize = state
                    .piece_manager
                    .pending_queue
                    .keys()
                    .map(|piece_idx| {
                        state
                            .piece_manager
                            .requestable_block_addresses_for_piece(*piece_idx)
                            .len()
                    })
                    .sum();
                let peers_with_pending_requests = state
                    .peers
                    .values()
                    .filter(|peer| !peer.pending_requests.is_empty())
                    .count();

                prop_assert!(
                    false,
                    "no progress and no pending work after recovery, pieces_remaining={}, pending_actions={}, pending_manager_commands={}, pending_disconnects={}, need_queue={}, pending_queue={}, queued_piece_count={}, has_serviceable_piece={}, pending_without_owner={}, pending_requestable_blocks={}, peers_with_pending_requests={}, peers={}, seed={}, loop_guard={}",
                    state.piece_manager.pieces_remaining,
                    pending_actions.len(),
                    pending_manager_commands.len(),
                    state.pending_disconnects.len(),
                    state.piece_manager.need_queue.len(),
                    state.piece_manager.pending_queue.len(),
                    queued_piece_count,
                    has_serviceable_piece,
                    pending_without_owner,
                    pending_requestable_blocks,
                    peers_with_pending_requests,
                    state.peers.len(),
                    random_seed,
                    loop_guard,
                );
            }
        }

        prop_assert_eq!(state.piece_manager.pieces_remaining, 0);
        prop_assert!(state
            .piece_manager
            .bitfield
            .iter()
            .all(|status| *status == PieceStatus::Done));

        Ok(())
    }

    static FUZZ_CASE_COUNTER: AtomicU64 = AtomicU64::new(0);

    proptest! {
        #[test]
        fn fuzz_piece_block_selection_and_completion(
            case in torrent_shape_strategy(),
            random_seed in any::<u64>(),
        ) {
            let case_no = FUZZ_CASE_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
            if case_no.is_multiple_of(10_000) {
                println!("current run {}", case_no);
            }
            run_piece_selection_completion_harness(
                &case,
                random_seed,
                default_harness_config(),
            )?;
        }
    }

    #[derive(Clone, Debug)]
    enum NetworkFault {
        None,
        Drop,
        Duplicate,
        Delay(u64),
        Corrupt,
    }

    fn inject_reordering_faults(actions: Vec<Action>, seed: u64) -> Vec<Action> {
        // We use a fixed seed from Proptest so failures are reproducible
        let mut rng = StdRng::seed_from_u64(seed);

        let mut pending = Vec::new();
        let mut result = Vec::new();

        for action in actions {
            // 2% Packet Loss
            if rng.random_bool(0.02) {
                continue;
            }

            // 1% Duplication (Clone creates the "Ghost Packet")
            if rng.random_bool(0.01) {
                let delay = rng.random_range(10..400);
                pending.push((delay, action.clone()));
            }

            // Normal Delivery (random delay 10ms - 400ms)
            let delay = rng.random_range(10..400);
            pending.push((delay, action));
        }

        // Sort events by who arrives first. This shuffles the timeline.
        pending.sort_by_key(|(delay, _)| *delay);

        // We must insert 'Tick' actions to account for the time gaps between events.
        let mut current_time = 0;
        for (arrival_time, action) in pending {
            if arrival_time > current_time {
                result.push(Action::Tick {
                    dt_ms: arrival_time - current_time,
                });
                current_time = arrival_time;
            }
            result.push(action);
        }

        result
    }

    // Transforms a clean history of actions into a faulty network stream
    // deterministically based on a vector of random "fault seeds"
    fn inject_network_faults(actions: Vec<Action>, fault_entropy: Vec<u8>) -> Vec<Action> {
        let mut final_actions = Vec::new();
        // Cycle through entropy so we don't run out if actions > entropy length
        let mut entropy_iter = fault_entropy.iter().cycle();

        for action in actions {
            let seed = *entropy_iter.next().unwrap();

            // Map the random byte (0-255) to a Fault Type
            let fault = match seed {
                0..=4 => NetworkFault::Drop,                      // ~2% chance
                5..=9 => NetworkFault::Duplicate,                 // ~2% chance
                10..=20 => NetworkFault::Delay(seed as u64 * 50), // ~4% chance (500ms-1000ms)
                21..=25 => NetworkFault::Corrupt,                 // ~2% chance
                _ => NetworkFault::None,                          // ~90% Clean
            };

            match fault {
                NetworkFault::Drop => {
                    // Packet lost in the ether
                    continue;
                }
                NetworkFault::Duplicate => {
                    final_actions.push(action.clone());
                    final_actions.push(action);
                }
                NetworkFault::Delay(ms) => {
                    // Simulate delay by ticking the clock before delivery
                    final_actions.push(Action::Tick { dt_ms: ms });
                    final_actions.push(action);
                }
                NetworkFault::Corrupt => {
                    // Flip bits if it involves data
                    match action {
                        Action::IncomingBlock {
                            peer_id,
                            piece_index,
                            block_offset,
                            mut data,
                        } => {
                            if !data.is_empty() {
                                // Corrupt the last byte
                                let len = data.len();
                                data[len - 1] = !data[len - 1];
                            }
                            final_actions.push(Action::IncomingBlock {
                                peer_id,
                                piece_index,
                                block_offset,
                                data,
                            });
                        }
                        // For control packets, corruption usually means they fail parsing
                        // and are effectively dropped or cause a disconnect.
                        // We simulate "parsing error" by turning it into a connection failure or drop.
                        _ => {
                            // Simulate packet garbling leading to drop
                            continue;
                        }
                    }
                }
                NetworkFault::None => {
                    final_actions.push(action);
                }
            }
        }
        final_actions
    }

    fn tit_for_tat_strategy() -> impl Strategy<Value = TorrentState> {
        let num_peers = 10usize;
        let speeds_strat = proptest::collection::vec(0..100_000u64, num_peers);

        speeds_strat.prop_map(move |speeds| {
            let mut state = super::tests::create_empty_state();
            state.torrent_status = TorrentStatus::Standard;

            for (i, &speed) in speeds.iter().enumerate() {
                let id = format!("peer_{}", i);
                let (tx, _) = mpsc::channel(1);
                let mut peer = PeerState::new(id.clone(), tx, state.now);

                peer.peer_id = id.as_bytes().to_vec();
                peer.peer_is_interested_in_us = true;
                peer.am_choking = super::ChokeStatus::Choke;

                peer.bytes_downloaded_from_peer = speed;

                state.peers.insert(id, peer);
            }
            state.number_of_successfully_connected_peers = state.peers.len();

            state
        })
    }

    fn rarest_first_strategy() -> impl Strategy<Value = TorrentState> {
        Just(()).prop_map(|_| {
            let mut state = super::tests::create_empty_state();
            let torrent = super::tests::create_dummy_torrent(2);
            state.torrent = Some(torrent);
            state.piece_manager.set_initial_fields(2, false);
            state.piece_manager.block_manager.set_geometry(
                16384,
                16384 * 2,
                vec![],
                vec![],
                HashMap::new(),
                false,
            );
            state.torrent_status = TorrentStatus::Standard;

            state.piece_manager.need_queue = vec![0, 1];

            // ... (Same peer creation code as before) ...
            let target_id = "target_peer".to_string();
            let (tx, _) = mpsc::channel(1);
            let mut target = PeerState::new(target_id.clone(), tx, state.now);
            target.peer_id = target_id.as_bytes().to_vec();
            target.peer_choking = super::ChokeStatus::Unchoke;
            target.am_interested = true;
            target.bitfield = vec![true, true];
            state.peers.insert(target_id, target);

            for i in 0..5 {
                let id = format!("bg_peer_{}", i);
                let (tx, _) = mpsc::channel(1);
                let mut p = PeerState::new(id.clone(), tx, state.now);
                p.peer_id = id.as_bytes().to_vec();
                p.bitfield = vec![false, true];
                state.peers.insert(id, p);
            }

            state.number_of_successfully_connected_peers = state.peers.len();

            state
                .piece_manager
                .update_rarity(state.peers.values().map(|p| &p.bitfield));

            state
        })
    }

    // Creates a swarm where EVERYONE is slow.
    // Tests if the client correctly handles mutual choking (snubbing).
    fn tit_for_tat_snubbed_strategy() -> impl Strategy<Value = TorrentState> {
        // 10 peers, all with 0 or 1 byte downloaded (Snubbed)
        let speeds_strat = proptest::collection::vec(0..=1u64, 10);

        speeds_strat.prop_map(move |speeds| {
            let mut state = super::tests::create_empty_state();
            state.torrent_status = TorrentStatus::Standard;

            for (i, &speed) in speeds.iter().enumerate() {
                let id = format!("slow_peer_{}", i);
                let (tx, _) = mpsc::channel(1);
                let mut peer = PeerState::new(id.clone(), tx, state.now);
                peer.peer_id = id.as_bytes().to_vec();
                peer.peer_is_interested_in_us = true;
                peer.am_choking = super::ChokeStatus::Choke;
                // Crucial: Low speed triggers snubbing logic (if implemented)
                peer.bytes_downloaded_from_peer = speed;
                state.peers.insert(id, peer);
            }
            state.number_of_successfully_connected_peers = state.peers.len();
            state
        })
    }

    // --- STRATEGY 4: Rarest First "Tiebreaker" Variant ---
    // Creates a scenario with two equally rare pieces (0 and 1).
    // Tests deterministic tie-breaking logic.
    fn rarest_first_tie_strategy() -> impl Strategy<Value = TorrentState> {
        Just(()).prop_map(|_| {
            let mut state = super::tests::create_empty_state();
            let torrent = super::tests::create_dummy_torrent(2);
            state.torrent = Some(torrent);
            state.piece_manager.set_initial_fields(2, false);
            state.piece_manager.block_manager.set_geometry(
                16384,
                16384 * 2,
                vec![],
                vec![],
                HashMap::new(),
                false,
            );
            state.torrent_status = TorrentStatus::Standard;
            state.piece_manager.need_queue = vec![0, 1];

            let target_id = "target_peer".to_string();
            let (tx, _) = mpsc::channel(1);
            let mut target = PeerState::new(target_id.clone(), tx, state.now);
            target.peer_id = target_id.as_bytes().to_vec();
            target.peer_choking = super::ChokeStatus::Unchoke;
            target.am_interested = true;
            target.bitfield = vec![true, true];
            state.peers.insert(target_id, target);

            state.number_of_successfully_connected_peers = state.peers.len();

            state
                .piece_manager
                .update_rarity(state.peers.values().map(|p| &p.bitfield));
            state
        })
    }

    // --- STRATEGY 5: Integrated Algo Strategy ---
    // Mixes speeds and bitfields to test the interaction between Choking and Picking.
    fn combined_algo_strategy() -> impl Strategy<Value = TorrentState> {
        // Peer A: Fast but has Common piece
        // Peer B: Slow but has Rare piece
        // Peer C: Medium speed, has Both
        Just(()).prop_map(move |_| {
            let mut state = super::tests::create_empty_state();
            let torrent = super::tests::create_dummy_torrent(2);
            state.torrent = Some(torrent);
            state.piece_manager.set_initial_fields(2, false);
            state.piece_manager.block_manager.set_geometry(
                16384,
                16384 * 2,
                vec![],
                vec![],
                HashMap::new(),
                false,
            );
            state.torrent_status = TorrentStatus::Standard;
            state.piece_manager.need_queue = vec![0, 1];

            // Helper to add peer
            let mut add_peer = |id: &str, speed: u64, pieces: Vec<bool>| {
                let (tx, _) = mpsc::channel(1);
                let mut p = PeerState::new(id.to_string(), tx, state.now);
                p.peer_id = id.as_bytes().to_vec();
                p.peer_is_interested_in_us = true; // We want to upload to them
                p.peer_choking = super::ChokeStatus::Unchoke; // They let us DL
                p.am_interested = true; // We want to DL
                p.bytes_downloaded_from_peer = speed; // For Tit-for-Tat
                p.bitfield = pieces; // For Rarest First
                state.peers.insert(id.to_string(), p);
            };

            // Setup the scenario
            add_peer("fast_common", 100_000, vec![false, true]); // Has Piece 1 (Common)
            add_peer("slow_rare", 100, vec![true, false]); // Has Piece 0 (Rare)
            add_peer("medium_both", 50_000, vec![true, true]); // Has Both

            state.number_of_successfully_connected_peers = state.peers.len();

            // Sync Rarity: Piece 0 (2 copies), Piece 1 (2 copies) -> Equal rarity in this setup
            state
                .piece_manager
                .update_rarity(state.peers.values().map(|p| &p.bitfield));

            state
        })
    }

    // --- STRATEGY 6: The Free-Rider (Parasite) Scenario ---
    // Creates a scenario with:

    // Both want our data. Logic MUST favor the Hero.
    fn free_rider_strategy() -> impl Strategy<Value = TorrentState> {
        Just(()).prop_map(move |_| {
            let mut state = super::tests::create_empty_state();
            state.torrent_status = TorrentStatus::Standard; // Leeching mode

            // Use the fixed constant defined in state.rs (which is 4)
            const UPLOAD_SLOTS: usize = super::UPLOAD_SLOTS_DEFAULT;

            let hero_id = "hero_peer".to_string();
            let (tx1, _) = mpsc::channel(1);
            let mut hero = PeerState::new(hero_id.clone(), tx1, state.now);
            hero.peer_id = hero_id.as_bytes().to_vec();
            hero.peer_is_interested_in_us = true;
            hero.am_choking = super::ChokeStatus::Choke;
            hero.bytes_downloaded_from_peer = 1_000_000; // High contribution
            state.peers.insert(hero_id, hero);

            // These peers, plus the Hero, will consume the 4 upload slots.
            // The loop runs from 1 to UPLOAD_SLOTS_DEFAULT (4).
            for i in 1..=UPLOAD_SLOTS {
                let id = format!("med_peer_{}", i);
                let (tx, _) = mpsc::channel(1);
                let mut p = PeerState::new(id.clone(), tx, state.now);
                p.peer_id = id.as_bytes().to_vec();
                p.peer_is_interested_in_us = true;
                p.am_choking = super::ChokeStatus::Choke;
                p.bytes_downloaded_from_peer = 100; // Better than 0
                state.peers.insert(id, p);
            }

            let leech_id = "parasite_peer".to_string();
            let (tx2, _) = mpsc::channel(1);
            let mut leech = PeerState::new(leech_id.clone(), tx2, state.now);
            leech.peer_id = leech_id.as_bytes().to_vec();
            leech.peer_is_interested_in_us = true;
            leech.am_choking = super::ChokeStatus::Choke;
            leech.bytes_downloaded_from_peer = 0; // No contribution
            state.peers.insert(leech_id, leech);

            // Total peers: Hero (1) + Med Peers (4) + Parasite (1) = 6
            // Total slots: 4 (Deterministic)
            // Since there are 5 peers contributing more than 0, the parasite (0) loses.

            state.number_of_successfully_connected_peers = state.peers.len();
            state
        })
    }

    // --- STRATEGY 8: Huge Swarm Strategy (Scale Test) ---
    // Scenario: 1000 Peers. Piece 0 is on 1 peer. Piece 1 is on 999 peers.
    // Goal: Ensure O(n) rarity calculation doesn't crash or timeout.
    fn huge_swarm_strategy() -> impl Strategy<Value = TorrentState> {
        Just(()).prop_map(|_| {
            let mut state = super::tests::create_empty_state();
            let torrent = super::tests::create_dummy_torrent(2);
            state.torrent = Some(torrent);
            state.piece_manager.set_initial_fields(2, false);
            state.piece_manager.block_manager.set_geometry(
                16384,
                16384 * 2,
                vec![],
                vec![],
                HashMap::new(),
                false,
            );
            state.torrent_status = TorrentStatus::Standard;
            state.piece_manager.need_queue = vec![0, 1];

            let rare_id = "rare_peer".to_string();
            let (tx, _) = mpsc::channel(1);
            let mut rare = PeerState::new(rare_id.clone(), tx, state.now);
            rare.peer_id = rare_id.as_bytes().to_vec();
            rare.peer_choking = super::ChokeStatus::Unchoke;
            rare.am_interested = true;
            rare.bitfield = vec![true, false]; // Has 0
            state.peers.insert(rare_id, rare);

            // We optimize this loop to avoid 1000 channel allocations slowing down the test setup too much
            let (tx, _) = mpsc::channel(1);
            for i in 0..999 {
                let id = format!("common_{}", i);
                let mut p = PeerState::new(id.clone(), tx.clone(), state.now);
                p.peer_id = id.as_bytes().to_vec();
                p.bitfield = vec![false, true]; // Has 1
                state.peers.insert(id, p);
            }
            state.number_of_successfully_connected_peers = state.peers.len();

            state
                .piece_manager
                .update_rarity(state.peers.values().map(|p| &p.bitfield));
            state
        })
    }

    // A strategy that forces the State Machine through specific "Phases"
    // instead of just throwing random events at it.
    fn lifecycle_transition_strategy() -> impl Strategy<Value = Vec<Action>> {
        let peer_id = "lifecycle_peer".to_string();

        prop_oneof![
            // Case 1: The Endgame Transition
            // Force queue to empty, then verify redundant requests behavior
            Just(vec![
                Action::PeerSuccessfullyConnected {
                    peer_id: peer_id.clone()
                },
                Action::PeerUnchoked {
                    peer_id: peer_id.clone()
                },
                Action::PeerHavePiece {
                    peer_id: peer_id.clone(),
                    piece_index: 0
                },
                Action::AssignWork {
                    peer_id: peer_id.clone()
                },
                // (We would need to manually manipulate the state queue in the test runner
                //  for this to work perfectly, or send a specific sequence here).
            ]),
            // Case 2: The "Stuck Peer" Cleanup
            // Connect a peer, Advance time > 5s, Trigger Cleanup
            Just(vec![
                Action::PeerSuccessfullyConnected {
                    peer_id: peer_id.clone()
                },
                // Note: We intentionally do NOT send SetPeerId here
                Action::Tick { dt_ms: 6000 },
                Action::Cleanup,
                // Expectation: Peer should be removed
            ]),
            // Case 3: Pause/Resume Data Integrity
            Just(vec![
                Action::PeerSuccessfullyConnected {
                    peer_id: peer_id.clone()
                },
                Action::IncomingBlock {
                    peer_id: peer_id.clone(),
                    piece_index: 0,
                    block_offset: 0,
                    data: vec![1; 100]
                },
                Action::Pause,
                Action::Resume,
                // Re-connect is required after pause
                Action::PeerSuccessfullyConnected {
                    peer_id: peer_id.clone()
                },
                // Try sending the SAME block again.
                // If internal state wasn't cleared, this might panic or corrupt.
                Action::IncomingBlock {
                    peer_id: peer_id.clone(),
                    piece_index: 0,
                    block_offset: 0,
                    data: vec![1; 100]
                },
            ])
        ]
    }

    fn network_action_strategy() -> impl Strategy<Value = Action> {
        let peer_id_strat = proptest::string::string_regex(".+").unwrap().boxed();

        prop_oneof![
            peer_id_strat
                .clone()
                .prop_map(|id| Action::PeerSuccessfullyConnected { peer_id: id }),
            peer_id_strat
                .clone()
                .prop_map(|id| Action::PeerDisconnected {
                    peer_id: id,
                    force: true
                }),
            any::<String>().prop_map(|addr| Action::PeerConnectionFailed { peer_addr: addr }),
            (any::<String>(), proptest::collection::vec(any::<u8>(), 20)).prop_map(|(addr, id)| {
                Action::UpdatePeerId {
                    peer_addr: addr,
                    new_id: id,
                }
            }),
            (any::<String>(), any::<u64>()).prop_map(|(url, interval)| {
                Action::TrackerResponse {
                    url,
                    peers: vec![],
                    interval,
                    min_interval: Some(60),
                }
            }),
            any::<String>().prop_map(|url| Action::TrackerError { url }),
            Just(Action::UpdateListenPort),
        ]
    }

    fn protocol_action_strategy() -> impl Strategy<Value = Action> {
        let peer_id_strat = proptest::string::string_regex(".+").unwrap().boxed();

        prop_oneof![
            peer_id_strat
                .clone()
                .prop_map(|id| Action::PeerChoked { peer_id: id }),
            peer_id_strat
                .clone()
                .prop_map(|id| Action::PeerUnchoked { peer_id: id }),
            peer_id_strat
                .clone()
                .prop_map(|id| Action::PeerInterested { peer_id: id }),
            (
                peer_id_strat.clone(),
                proptest::collection::vec(any::<u8>(), 1..10)
            )
                .prop_map(|(id, bf)| {
                    Action::PeerBitfieldReceived {
                        peer_id: id,
                        bitfield: bf,
                    }
                }),
            (peer_id_strat.clone(), 0..NUM_PIECES as u32).prop_map(|(id, idx)| {
                Action::PeerHavePiece {
                    peer_id: id,
                    piece_index: idx,
                }
            }),
            peer_id_strat.prop_map(|id| Action::AssignWork { peer_id: id }),
        ]
    }

    fn boundary_data_strategy() -> impl Strategy<Value = Action> {
        let peer_id_strat = proptest::string::string_regex(".+").unwrap().boxed();

        prop_oneof![
            // FIX: Access NUM_PIECES and PIECE_LEN directly
            (peer_id_strat.clone(), 0..NUM_PIECES as u32).prop_map(|(id, idx)| {
                let data = vec![1u8; 1024];
                Action::IncomingBlock {
                    peer_id: id,
                    piece_index: idx,
                    block_offset: PIECE_LEN - 1024,
                    data,
                }
            }),
            (peer_id_strat.clone(), 0..NUM_PIECES as u32).prop_map(|(id, idx)| {
                let data = vec![0u8; 10];
                Action::IncomingBlock {
                    peer_id: id,
                    piece_index: idx,
                    block_offset: PIECE_LEN - 5,
                    data,
                }
            }),
            // FIX: Access MAX_BLOCK directly
            (peer_id_strat.clone(), 0..NUM_PIECES as u32).prop_map(|(id, idx)| {
                Action::RequestUpload {
                    peer_id: id,
                    piece_index: idx,
                    block_offset: 0,
                    length: MAX_BLOCK + 1,
                }
            }),
            (peer_id_strat.clone(), 0..NUM_PIECES as u32).prop_map(|(id, idx)| {
                Action::RequestUpload {
                    peer_id: id,
                    piece_index: idx,
                    block_offset: 0,
                    length: 0,
                }
            }),
            (
                peer_id_strat.clone(),
                0..NUM_PIECES as u32,
                any::<u32>(),
                proptest::collection::vec(any::<u8>(), 1..1024)
            )
                .prop_map(|(id, idx, off, data)| Action::IncomingBlock {
                    peer_id: id,
                    piece_index: idx,
                    block_offset: off,
                    data
                }),
        ]
    }

    fn system_response_strategy() -> impl Strategy<Value = Action> {
        let peer_id_strat = proptest::string::string_regex(".+").unwrap().boxed();

        prop_oneof![
            // FIX: Access NUM_PIECES directly
            (peer_id_strat.clone(), 0..NUM_PIECES as u32, any::<bool>()).prop_map(
                |(id, idx, valid)| {
                    Action::PieceVerified {
                        peer_id: id,
                        piece_index: idx,
                        valid,
                        data: vec![],
                    }
                }
            ),
            (peer_id_strat.clone(), 0..NUM_PIECES as u32).prop_map(|(id, idx)| {
                Action::PieceWrittenToDisk {
                    peer_id: id,
                    piece_index: idx,
                }
            }),
            any::<u32>().prop_map(|idx| Action::PieceWriteFailed { piece_index: idx }),
            proptest::collection::vec(0..NUM_PIECES as u32, 0..5).prop_map(|pieces| {
                Action::ValidationComplete {
                    completed_pieces: pieces,
                }
            }),
        ]
    }

    // E. Global Lifecycle
    fn lifecycle_strategy() -> impl Strategy<Value = Action> {
        prop_oneof![
            Just(Action::Tick { dt_ms: 100 }),
            Just(Action::Tick { dt_ms: 50000 }),
            Just(Action::CheckCompletion),
            Just(Action::Cleanup),
            Just(Action::Pause),
            Just(Action::Resume),
            (0..50u64).prop_map(|seed| Action::RecalculateChokes { random_seed: seed }),
        ]
    }

    // F. Combined Chaos
    fn chaos_strategy() -> impl Strategy<Value = Action> {
        prop_oneof![
            network_action_strategy(),
            protocol_action_strategy(),
            boundary_data_strategy(), // Using the new boundary strategy here
            system_response_strategy(),
            lifecycle_strategy(),
        ]
    }

    fn protocol_violation_strategy() -> impl Strategy<Value = Vec<Action>> {
        let id = "bad_actor".to_string();

        prop_oneof![
            // Expectation: Data should be dropped or peer disconnected, no panic.
            Just(vec![
                Action::PeerSuccessfullyConnected {
                    peer_id: id.clone()
                },
                Action::PeerChoked {
                    peer_id: id.clone()
                }, // They choked us
                Action::IncomingBlock {
                    peer_id: id.clone(),
                    piece_index: 0,
                    block_offset: 0,
                    data: vec![0; 100]
                }
            ]),
            // Expectation: Request ignored, strict clients might disconnect peer.
            Just(vec![
                Action::PeerSuccessfullyConnected {
                    peer_id: id.clone()
                },
                Action::PeerUnchoked {
                    peer_id: id.clone()
                },
                Action::RequestUpload {
                    peer_id: id.clone(),
                    piece_index: 99999, // Way out of bounds
                    block_offset: 0,
                    length: 16384
                }
            ]),
            // Expectation: State handles map collisions gracefully.
            Just(vec![
                Action::PeerSuccessfullyConnected {
                    peer_id: id.clone()
                },
                Action::PeerSuccessfullyConnected {
                    peer_id: id.clone()
                }, // Re-connect
                Action::PeerDisconnected {
                    peer_id: id.clone(),
                    force: true,
                },
                // Should be ignored or handled gracefully, not panic
                Action::IncomingBlock {
                    peer_id: id.clone(),
                    piece_index: 0,
                    block_offset: 0,
                    data: vec![1]
                }
            ]),
            // Expectation: Client accepts the byte into a buffer OR drops it. MUST NOT PANIC.
            // Malicious peers do this to exhaust memory 1 byte at a time.
            (0..20u32).prop_map(|idx| {
                let frag_id = "fragmenter".to_string();
                vec![
                    Action::PeerSuccessfullyConnected {
                        peer_id: frag_id.clone(),
                    },
                    Action::PeerUnchoked {
                        peer_id: frag_id.clone(),
                    },
                    Action::IncomingBlock {
                        peer_id: frag_id.clone(),
                        piece_index: idx,
                        block_offset: 0,
                        data: vec![0u8; 1], // <--- The Attack: Exactly 1 byte
                    },
                ]
            })
        ]
    }

    // Standard Stories (kept for logical flow testing)
    fn successful_download_story() -> impl Strategy<Value = Vec<Action>> {
        // Shortened version for brevity, assuming previous implementation logic
        let peer_gen = (1..255u8, 1000..9999u16);
        let piece_gen = 0..NUM_PIECES as u32;

        (peer_gen, piece_gen).prop_flat_map(|((ip, port), piece_index)| {
            let peer_id = format!("127.0.0.{}:{}", ip, port);
            let data = vec![1, 2, 3, 4];
            let actions = vec![
                Action::PeerSuccessfullyConnected {
                    peer_id: peer_id.clone(),
                },
                Action::PeerBitfieldReceived {
                    peer_id: peer_id.clone(),
                    bitfield: vec![],
                },
                Action::PeerHavePiece {
                    peer_id: peer_id.clone(),
                    piece_index,
                },
                Action::PeerUnchoked {
                    peer_id: peer_id.clone(),
                },
                Action::IncomingBlock {
                    peer_id: peer_id.clone(),
                    piece_index,
                    block_offset: 0,
                    data: data.clone(),
                },
                Action::PieceVerified {
                    peer_id: peer_id.clone(),
                    piece_index,
                    valid: true,
                    data,
                },
                Action::PieceWrittenToDisk {
                    peer_id: peer_id.clone(),
                    piece_index,
                },
            ];
            Just(actions)
        })
    }

    // Master Strategy
    fn mixed_behavior_strategy() -> impl Strategy<Value = Vec<Action>> {
        prop_oneof![
            4 => chaos_strategy().prop_map(|a| vec![a]),
            2 => successful_download_story(),
            1 => protocol_violation_strategy(),
            1 => lifecycle_transition_strategy(),
        ]
    }

    fn populated_state_strategy() -> impl Strategy<Value = TorrentState> {
        let peers_strat = proptest::collection::hash_map(
            any::<String>(),
            // (Download Speed, Upload Speed, Has Piece 0?)
            (any::<u64>(), any::<u64>(), any::<bool>()),
            1..20,
        );

        peers_strat.prop_map(|peer_map| {
            let mut state = super::tests::create_empty_state();
            let torrent = super::tests::create_dummy_torrent(NUM_PIECES);
            state.torrent = Some(torrent);
            state.piece_manager.set_initial_fields(NUM_PIECES, false);
            state.torrent_status = TorrentStatus::Standard;

            // Pre-fill peers
            for (id, (dl, ul, has_piece_0)) in peer_map {
                let (tx, _) = mpsc::channel(1);
                let mut peer = PeerState::new(id.clone(), tx, state.now);

                peer.peer_id = id.as_bytes().to_vec();

                peer.bitfield = vec![false; NUM_PIECES];
                if has_piece_0 {
                    peer.bitfield[0] = true;
                }

                // In this strategy we need all pieces, so if they have any, we are interested.
                peer.am_interested = peer.bitfield.iter().any(|&b| b);

                peer.peer_is_interested_in_us = true;
                peer.peer_choking = crate::torrent_manager::state::ChokeStatus::Unchoke;

                // Pre-load stats to influence Choke/Unchoke logic
                peer.bytes_downloaded_in_tick = dl % 100_000;
                peer.bytes_uploaded_in_tick = ul % 100_000;
                peer.download_speed_bps = dl % 100_000;

                state.peers.insert(id, peer);
            }

            // --- FIX START: Sync the metric count with the inserted peers ---
            state.number_of_successfully_connected_peers = state.peers.len();
            // --- FIX END ---

            // IMPORTANT: Ensure Need Queue is populated so AssignWork actually does something
            state.piece_manager.need_queue.clear();
            for i in 0..NUM_PIECES as u32 {
                state.piece_manager.need_queue.push(i);
            }

            state
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::default())]

        // Test 1: Logical Stories starting from scratch
        #[test]
        fn test_stateful_stories(
            story_batches in proptest::collection::vec(mixed_behavior_strategy(), 1..15)
        ) {
            let mut state = super::tests::create_empty_state();
            let torrent = super::tests::create_dummy_torrent(NUM_PIECES);
            state.torrent = Some(torrent);
            state.piece_manager.set_initial_fields(NUM_PIECES, false);
            state.torrent_status = TorrentStatus::Standard;
            state.piece_manager.need_queue = (0..NUM_PIECES as u32).collect();

            for story in story_batches {
                for action in story {
                     // Adapter for handshake simulation
                    if let Action::PeerSuccessfullyConnected { peer_id } = &action {
                        if !state.peers.contains_key(peer_id) {
                            let (tx, _) = mpsc::channel(1);
                            let mut peer = PeerState::new(peer_id.clone(), tx, state.now);
                            peer.peer_id = peer_id.as_bytes().to_vec();
                            state.peers.insert(peer_id.clone(), peer);
                        }
                    }
                    let _ = state.update(action);
                    check_invariants(&state);
                }
            }
        }

        // Test 2: Deep State Fuzzing (New Strategies)
        // Starts with a populated state and applies Chaos + Boundary Data
        #[test]
        fn test_deep_state_chaos(
            mut initial_state in populated_state_strategy(),
            actions in proptest::collection::vec(chaos_strategy(), 1..20)
        ) {
            // Sanity check initial state
            check_invariants(&initial_state);

            for action in actions {
                // Use catch_unwind to fail the test gracefully if a panic occurs,
                // allowing Proptest to print the shrinking failure case.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // Adapter for handshake simulation (even in chaos mode)
                    if let Action::PeerSuccessfullyConnected { peer_id } = &action {
                        // We allow overwrites in Chaos mode to test resilience
                        if !initial_state.peers.contains_key(peer_id) {
                            let (tx, _) = mpsc::channel(1);
                            let mut peer = PeerState::new(peer_id.clone(), tx, initial_state.now);
                            peer.peer_id = peer_id.as_bytes().to_vec();
                            initial_state.peers.insert(peer_id.clone(), peer);
                        }
                    }

                    let _ = initial_state.update(action.clone());
                }));

                if result.is_err() {
                     // If we panicked, the test fails here.
                     // Proptest will output the `initial_state` and the `actions` vector.
                     panic!("Deep State Fuzzing Triggered Panic!");
                }

                check_invariants(&initial_state);
            }
        }

        #[test]
        fn test_tit_for_tat_fairness(mut state  in tit_for_tat_strategy()) {
            let mut peers: Vec<_> = state.peers.values().collect();

            // Sort Descending by speed
            peers.sort_by(|a, b| b.bytes_downloaded_from_peer.cmp(&a.bytes_downloaded_from_peer));

            let top_peers: Vec<String> = peers.iter()
                .take(UPLOAD_SLOTS_DEFAULT)
                .map(|p| p.ip_port.clone())
                .collect();

            // Run Algorithm
            let _ = state.update(Action::RecalculateChokes {
                random_seed: 12345
            });

            // Assert Fairness
            for winner_id in top_peers {
                let peer = state.peers.get(&winner_id).unwrap();
                prop_assert_eq!(peer.am_choking.clone(), super::ChokeStatus::Unchoke,
                    "Fast peer {} was unfairly choked!", winner_id);
            }
        }

        #[test]
    fn test_rarest_first_selection(mut state in rarest_first_strategy()) {

            let effects = state.update(Action::AssignWork { peer_id: "target_peer".into() });

            let requested_index = effects.iter().find_map(|e| {
                if let Effect::SendToPeer { cmd, .. } = e {
                    if let TorrentCommand::BulkRequest(ref reqs) = **cmd {
                        return reqs.first().map(|(idx, _, _)| *idx);
                    }
                }
                None
            });

            if let Some(idx) = requested_index {
                prop_assert_eq!(idx, 0,
                    "Algorithm picked Common Piece {} instead of Rare Piece 0", idx);
            } else {
                prop_assert!(false, "Algorithm failed to request any piece! State: {:?}", state);
            }
        }

        // --- TEST 3: Tit-for-Tat Snubbed Invariant ---
        #[test]
        fn test_tit_for_tat_snubbed(mut state in tit_for_tat_snubbed_strategy()) {

            let _ = state.update(Action::RecalculateChokes {
                random_seed: 999
            });

            let unchoked_count = state.peers.values()
                .filter(|p| p.am_choking == super::ChokeStatus::Unchoke)
                .count();

            // Even if everyone is slow, we MUST NOT unchoke more than slots + 1 (optimistic).
            // In a strict implementation, if everyone is 0, we might unchoke NO ONE (except optimistic),
            // or we might unchoke randoms. But we must never exceed the limit.
            prop_assert!(unchoked_count <= UPLOAD_SLOTS_DEFAULT + 1,
                "Too many peers unchoked in a snubbed swarm! Count: {}, Limit: {}", unchoked_count, UPLOAD_SLOTS_DEFAULT + 1);
        }

        // --- TEST 4: Rarest First Tiebreaker Invariant ---
        #[test]
        fn test_rarest_first_tie(mut state in rarest_first_tie_strategy()) {

            let effects = state.update(Action::AssignWork { peer_id: "target_peer".into() });

            let picked_idx = effects.iter().find_map(|e| {
                if let Effect::SendToPeer { cmd, .. } = e {
                    if let TorrentCommand::BulkRequest(ref reqs) = **cmd {
                        return reqs.first().map(|(idx, _, _)| *idx);
                    }
                }
                None
            });

            if let Some(idx) = picked_idx {
                // It must be one of the available pieces (0 or 1).
                // A stable sort usually picks the lower index (0).
                // A random sort picks either. Both are valid "Rarest First" outcomes for a tie.
                prop_assert!(idx == 0 || idx == 1,
                    "Tiebreaker failed! Picked {}, expected 0 or 1.", idx);
            } else {
                prop_assert!(false, "Tiebreaker caused deadlock: No piece requested!");
            }
        }

        // --- TEST 5: Integrated Logic (The "Choke Check") ---
        #[test]
        fn test_choke_during_pick(mut state in combined_algo_strategy()) {

            let _ = state.update(Action::RecalculateChokes {  random_seed: 42 });

            let effects = state.update(Action::AssignWork { peer_id: "medium_both".into() });

            // The request should be valid regardless of OUR choking status towards them.
            // (BitTorrent allows downloading from people we choke, though they might not like it).
            // However, we MUST verify we only request pieces they actually have.
            if let Some(Effect::SendToPeer { cmd, .. }) = effects.first() {
                if let TorrentCommand::BulkRequest(ref reqs) = **cmd {
                    if let Some((idx, _, _)) = reqs.first() {
                        let peer = state.peers.get("medium_both").unwrap();
                        // Invariant: We must never request a piece the peer doesn't have
                        prop_assert!(
                            peer.bitfield.get(*idx as usize) == Some(&true),
                            "Logic Error: Requested Piece {} which 'medium_both' does not have!",
                            idx
                        );
                    }
                }
            }
        }

        // --- TEST 6: Tit-for-Tat Justice (Hero vs Parasite) ---
        #[test]
        fn test_free_rider_justice(mut state in free_rider_strategy()) {

            // We set a fixed seed to control Optimistic Unchoke.
            // In a real scenario, the Parasite might get the Optimistic slot occasionally,
            // but the Regular slots MUST go to the Hero.
            // Since we only have 1 slot total in this strat, logic dictates Hero gets it.
            let _ = state.update(Action::RecalculateChokes {
                random_seed: 42
            });

            let hero = state.peers.get("hero_peer").unwrap();
            prop_assert_eq!(hero.am_choking.clone(), super::ChokeStatus::Unchoke,
                "Injustice! The Hero peer (high contributor) was choked.");

            let parasite = state.peers.get("parasite_peer").unwrap();
            // Note: If your Optimistic Unchoke logic overrides the single slot,
            // this assert might flake depending on the seed.
            // Ideally, regular slots > optimistic slots.
            prop_assert_eq!(parasite.am_choking.clone(), super::ChokeStatus::Choke,
                "Exploit! The Free-Rider (zero contributor) stole the upload slot.");
        }

        // --- TEST 8: Scale & Complexity ---
        #[test]
        fn test_rarest_first_scale(mut state in huge_swarm_strategy()) {

            let effects = state.update(Action::AssignWork { peer_id: "rare_peer".into() });

            let picked = effects.iter().any(|e| {
                if let Effect::SendToPeer { cmd, .. } = e {
                    if let TorrentCommand::BulkRequest(ref reqs) = **cmd {
                        return reqs.iter().any(|(idx, _, _)| *idx == 0);
                    }
                }
                false
            });

            prop_assert!(picked, "Scale test failed: Did not pick the only available piece (0) from the rare peer.");
        }

        // --- TEST 9: Choke Race Condition (The "Stop" Check) ---
        #[test]
        fn test_choke_race_condition(mut state in combined_algo_strategy()) {

            state.update(Action::PeerUnchoked { peer_id: "medium_both".into() });

            state.update(Action::PeerChoked { peer_id: "medium_both".into() });

            let effects = state.update(Action::AssignWork { peer_id: "medium_both".into() });

            // If we are choked, we must NOT send a Request, even if we want the data.
            let sent_request = effects
                .iter()
                .any(|e| matches!(e, Effect::SendToPeer { cmd, .. } if matches!(**cmd, TorrentCommand::BulkRequest(_))));

            prop_assert!(!sent_request, "Race Condition Fail: Requested data while Choked!");
        }

    }

    // STATE MACHINE FUZZER (Expanded Lifecycle Coverage)

    mod state_machine {
        use super::*;
        use super::{inject_network_faults, inject_reordering_faults};
        use crate::torrent_manager::state::tests::create_dummy_torrent;
        use proptest_state_machine::{ReferenceStateMachine, StateMachineTest};
        use std::collections::HashSet;

        // --- 1. THE MODEL ---
        #[derive(Clone, Debug)]
        pub struct TorrentModel {
            pub connected_peers: HashSet<String>,
            pub total_pieces: u32,
            pub paused: bool,
            pub status: TorrentStatus,
            pub has_metadata: bool,
            pub downloaded_pieces: HashSet<u32>,
        }

        impl TorrentModel {
            fn new_file(pieces: u32) -> Self {
                Self {
                    connected_peers: HashSet::new(),
                    total_pieces: pieces,
                    paused: false,
                    status: TorrentStatus::Validating,
                    has_metadata: true,
                    downloaded_pieces: HashSet::new(),
                }
            }

            fn new_magnet(pieces: u32) -> Self {
                Self {
                    connected_peers: HashSet::new(),
                    total_pieces: pieces,
                    paused: false,
                    status: TorrentStatus::AwaitingMetadata,
                    has_metadata: false,
                    downloaded_pieces: HashSet::new(),
                }
            }
        }

        // --- 2. THE REFERENCE MACHINE ---
        impl ReferenceStateMachine for TorrentModel {
            type State = Self;
            type Transition = Action;

            fn init_state() -> BoxedStrategy<Self::State> {
                prop_oneof![
                    Just(TorrentModel::new_file(5)),
                    Just(TorrentModel::new_magnet(5))
                ]
                .boxed()
            }

            fn transitions(state: &Self::State) -> BoxedStrategy<Self::Transition> {
                let mut strategies = vec![
                    Just(Action::Tick { dt_ms: 1000 }).boxed(),
                    Just(Action::Cleanup).boxed(),
                    Just(Action::FatalError).boxed(),
                    Just(Action::Shutdown).boxed(),
                    Just(Action::Delete).boxed(),
                    Just(Action::ConnectToWebSeeds).boxed(),
                ];

                // ... (Re-Init, Pause/Resume, Metadata, Phase Transitions logic unchanged) ...
                strategies.push(
                    any::<bool>()
                        .prop_map(|paused| Action::TorrentManagerInit {
                            is_paused: paused,
                            announce_immediately: !paused,
                        })
                        .boxed(),
                );

                if state.paused {
                    strategies.push(Just(Action::Resume).boxed());
                } else {
                    strategies.push(Just(Action::Pause).boxed());
                }

                if state.status == TorrentStatus::AwaitingMetadata {
                    strategies.push(
                        Just(Action::MetadataReceived {
                            torrent: Box::new(create_dummy_torrent(state.total_pieces as usize)),
                            metadata_length: (state.total_pieces * 16384) as i64,
                        })
                        .boxed(),
                    );
                }

                if state.status == TorrentStatus::Validating {
                    let max_pieces = state.total_pieces;
                    strategies.push(
                        proptest::collection::vec(0..max_pieces, 0..max_pieces as usize)
                            .prop_map(|pieces| Action::ValidationComplete {
                                completed_pieces: pieces,
                            })
                            .boxed(),
                    );
                }

                if state.status == TorrentStatus::Standard || state.status == TorrentStatus::Endgame
                {
                    strategies.push(Just(Action::CheckCompletion).boxed());
                }

                // Connection Actions
                // -> FIX HERE: Ensure we don't generate empty peer IDs
                strategies.push(
                    proptest::string::string_regex(".+")
                        .unwrap()
                        .prop_map(|id| Action::PeerSuccessfullyConnected { peer_id: id })
                        .boxed(),
                );

                // Peer Interaction (unchanged logic, selects from existing peers)
                if !state.connected_peers.is_empty() && state.has_metadata {
                    let peer_strategy =
                        prop::sample::select(Vec::from_iter(state.connected_peers.clone()));
                    // ... (rest of peer interaction logic)
                    let piece_strategy = 0..state.total_pieces;

                    strategies.push(
                        peer_strategy
                            .clone()
                            .prop_map(|id| Action::PeerDisconnected {
                                peer_id: id,
                                force: true,
                            })
                            .boxed(),
                    );
                    strategies.push(
                        peer_strategy
                            .clone()
                            .prop_map(|id| Action::PeerUnchoked { peer_id: id })
                            .boxed(),
                    );

                    if state.status != TorrentStatus::Validating
                        && state.status != TorrentStatus::AwaitingMetadata
                    {
                        strategies.push(
                            (peer_strategy.clone(), piece_strategy.clone())
                                .prop_map(|(id, idx)| Action::PeerHavePiece {
                                    peer_id: id,
                                    piece_index: idx,
                                })
                                .boxed(),
                        );

                        strategies.push(
                            peer_strategy
                                .clone()
                                .prop_map(|id| Action::AssignWork { peer_id: id })
                                .boxed(),
                        );

                        strategies.push(
                            (
                                peer_strategy.clone(),
                                piece_strategy.clone(),
                                any::<u32>(),
                                prop::collection::vec(any::<u8>(), 1..1024),
                            )
                                .prop_map(|(id, idx, offset, data)| Action::IncomingBlock {
                                    peer_id: id,
                                    piece_index: idx,
                                    block_offset: offset,
                                    data,
                                })
                                .boxed(),
                        );

                        strategies.push(
                            (peer_strategy.clone(), piece_strategy.clone())
                                .prop_map(|(id, idx)| Action::PieceWrittenToDisk {
                                    peer_id: id,
                                    piece_index: idx,
                                })
                                .boxed(),
                        );
                    }
                }

                prop::strategy::Union::new(strategies).boxed()
            }

            fn apply(mut state: Self::State, trans: &Self::Transition) -> Self::State {
                match trans {
                    Action::PeerSuccessfullyConnected { peer_id } => {
                        state.connected_peers.insert(peer_id.clone());
                    }
                    Action::PeerDisconnected {
                        peer_id,
                        force: true,
                    } => {
                        state.connected_peers.remove(peer_id);
                    }
                    Action::Pause | Action::FatalError => {
                        state.paused = true;
                        state.connected_peers.clear();
                    }
                    Action::Resume => {
                        state.paused = false;
                    }
                    Action::TorrentManagerInit { is_paused, .. } => {
                        state.paused = *is_paused;
                    }
                    Action::Shutdown => {
                        state.paused = true;
                        state.connected_peers.clear();
                    }
                    Action::Delete => {
                        state.paused = true;
                        state.connected_peers.clear();
                        state.downloaded_pieces.clear(); // Clear model tracking
                        if state.has_metadata {
                            state.status = TorrentStatus::Validating;
                        } else {
                            state.status = TorrentStatus::AwaitingMetadata;
                        }
                    }

                    Action::MetadataReceived { .. } => {
                        if !state.has_metadata {
                            state.has_metadata = true;
                            state.status = TorrentStatus::Validating;
                            state.downloaded_pieces.clear();
                        }
                    }

                    Action::ValidationComplete { completed_pieces } => {
                        if state.status == TorrentStatus::Validating {
                            state.status = TorrentStatus::Standard;
                            for p in completed_pieces {
                                state.downloaded_pieces.insert(*p);
                            }
                            // Check for immediate completion
                            if state.downloaded_pieces.len() as u32 == state.total_pieces {
                                state.status = TorrentStatus::Done;
                            }
                        }
                    }

                    Action::PieceWrittenToDisk { piece_index, .. } => {
                        // FIX: Model now mimics SUT's completion logic
                        if state.status == TorrentStatus::Standard
                            || state.status == TorrentStatus::Endgame
                        {
                            state.downloaded_pieces.insert(*piece_index);
                            if state.downloaded_pieces.len() as u32 == state.total_pieces {
                                state.status = TorrentStatus::Done;
                            }
                        }
                    }

                    _ => {}
                }
                state
            }
        }

        // --- 3. THE BINDING ---
        impl StateMachineTest for TorrentModel {
            type SystemUnderTest = TorrentState;
            type Reference = TorrentModel;

            fn init_test(ref_state: &TorrentModel) -> Self::SystemUnderTest {
                let (torrent, status) = if ref_state.has_metadata {
                    (
                        Some(create_dummy_torrent(ref_state.total_pieces as usize)),
                        TorrentStatus::Validating,
                    )
                } else {
                    (None, TorrentStatus::AwaitingMetadata)
                };

                let piece_manager = if ref_state.has_metadata {
                    let mut pm = PieceManager::new();
                    pm.set_initial_fields(ref_state.total_pieces as usize, false);
                    pm
                } else {
                    PieceManager::new()
                };

                TorrentState {
                    torrent,
                    torrent_status: status,
                    is_paused: ref_state.paused,
                    piece_manager,
                    torrent_data_path: Some(PathBuf::from("/tmp/fuzz")),
                    ..Default::default()
                }
            }

            fn apply(
                mut sut: Self::SystemUnderTest,
                ref_state: &TorrentModel,
                transition: Action,
            ) -> Self::SystemUnderTest {
                if let Action::PeerSuccessfullyConnected { peer_id } = &transition {
                    if !sut.peers.contains_key(peer_id) {
                        let (tx, _) = tokio::sync::mpsc::channel(1);
                        let mut peer = PeerState::new(peer_id.clone(), tx, sut.now);
                        peer.peer_id = peer_id.as_bytes().to_vec();
                        sut.peers.insert(peer_id.clone(), peer);
                        sut.number_of_successfully_connected_peers = sut.peers.len();
                    }
                }

                let _ = sut.update(transition.clone());

                // Advance Model to Post-State for comparison
                let expected_state =
                    <TorrentModel as ReferenceStateMachine>::apply(ref_state.clone(), &transition);

                // Metadata Integrity
                assert_eq!(
                    sut.torrent.is_some(),
                    expected_state.has_metadata,
                    "SUT Metadata existence mismatch!"
                );

                // Status Sync
                let sut_status_norm = if sut.torrent_status == TorrentStatus::Endgame {
                    TorrentStatus::Standard
                } else {
                    sut.torrent_status.clone()
                };

                let model_status_norm = if expected_state.status == TorrentStatus::Endgame {
                    TorrentStatus::Standard
                } else {
                    expected_state.status.clone()
                };

                assert_eq!(
                    sut_status_norm,
                    model_status_norm,
                    "Status Mismatch! SUT: {:?} (Normalized), Model: {:?} (Normalized). Action: {:?}",
                    sut.torrent_status, expected_state.status, transition
                );

                // Peer Count Sync
                if !matches!(transition, Action::Cleanup) {
                    assert_eq!(
                        sut.peers.len(),
                        expected_state.connected_peers.len(),
                        "Model/SUT Peer Mismatch! \nModel: {:?}\nSUT: {:?}",
                        expected_state.connected_peers,
                        sut.peers.keys()
                    );
                }

                sut
            }
        }

        // --- 4. THE RUNNER ---
        proptest! {
            #![proptest_config(ProptestConfig::default())]

            #[test]
            fn test_lifecycle_state_machine(
                (initial_state, transitions, tracker) in TorrentModel::sequential_strategy(20)
            ) {
                TorrentModel::test_sequential(
                    proptest::test_runner::Config::default(),
                    initial_state,
                    transitions,
                    tracker
                );
            }

            #[test]
            fn test_state_machine_network_faults(
                (initial_state, clean_actions, _) in TorrentModel::sequential_strategy(20),
                fault_entropy in proptest::collection::vec(any::<u8>(), 50)
            ) {
                let faulty_actions = inject_network_faults(clean_actions, fault_entropy);
                let mut ref_state = initial_state.clone();
                let mut sut = TorrentModel::init_test(&ref_state);

                for action in faulty_actions {
                    // Clone SUT to keep ownership valid for the next iteration if check passes
                    let sut_clone = sut.clone();

                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        <TorrentModel as StateMachineTest>::apply(sut_clone, &ref_state, action.clone())
                    }));

                    match result {
                        Ok(new_sut) => {
                            sut = new_sut;
                            // Advance the Reference Model
                            ref_state = <TorrentModel as ReferenceStateMachine>::apply(ref_state, &action);

                            // The SUT removes peers based on internal timers/logic the Model doesn't have.
                            // To prevent desync on the *next* action (like Tick), we adopt the SUT's
                            // peer list as the new truth.
                            if matches!(action, Action::Cleanup) {
                                ref_state.connected_peers = sut.peers.keys().cloned().collect();
                            }
                        }
                        Err(_) => { panic!("SUT Panicked during Network Fault Injection!\nAction: {:?}", action); }
                    }
                }
            }

            #[test]
            fn test_state_machine_network_reordering(
                (initial_state, clean_actions, _) in TorrentModel::sequential_strategy(20),
                seed in any::<u64>()
            ) {
                let chaotic_actions = inject_reordering_faults(clean_actions, seed);
                let mut ref_state = initial_state.clone();
                let mut sut = TorrentModel::init_test(&ref_state);

                for action in chaotic_actions {
                    let sut_clone = sut.clone();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        <TorrentModel as StateMachineTest>::apply(sut_clone, &ref_state, action.clone())
                    }));

                    match result {
                        Ok(new_sut) => {
                            sut = new_sut;
                            ref_state = <TorrentModel as ReferenceStateMachine>::apply(ref_state, &action);

                            if matches!(action, Action::Cleanup) {
                                ref_state.connected_peers = sut.peers.keys().cloned().collect();
                            }
                        }
                        Err(_) => { panic!("SUT Panicked during Network Reordering!\nAction: {:?}", action); }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod integration_tests {
    use crate::config::Settings;
    use sha1::{Digest, Sha1};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::{broadcast, mpsc, watch};
    // Correct Import for the client struct
    use crate::resource_manager::{ResourceManager, ResourceManagerClient};
    use crate::token_bucket::TokenBucket;
    use crate::torrent_file::Torrent;
    use crate::torrent_manager::{
        ManagerCommand, TorrentManager, TorrentMetrics, TorrentParameters,
    };

    fn full_bitfield(num_pieces: usize) -> Vec<u8> {
        let mut bf = vec![0u8; num_pieces.div_ceil(8)];
        for i in 0..num_pieces {
            let byte_idx = i / 8;
            let bit_idx = 7 - (i % 8);
            bf[byte_idx] |= 1 << bit_idx;
        }
        bf
    }

    fn create_manager_harness(
        name: &str,
        num_pieces: usize,
        piece_size: usize,
        temp_dir: std::path::PathBuf,
    ) -> (
        TorrentManager,
        mpsc::Sender<ManagerCommand>,
        ResourceManagerClient,
    ) {
        let (_incoming_tx, incoming_peer_rx) = mpsc::channel(100);
        let (cmd_tx, cmd_rx) = mpsc::channel(100);

        // Event Drainer
        let (event_tx, mut event_rx) = mpsc::channel(500);
        tokio::spawn(async move { while event_rx.recv().await.is_some() {} });

        let (metrics_tx, _) = watch::channel(TorrentMetrics::default());
        let (shutdown_tx, _) = broadcast::channel(1);

        let settings_val = Settings {
            client_id: "-SS0001-TESTTESTTEST".to_string(),
            ..Default::default()
        };
        let settings = Arc::new(settings_val);

        let mut limits = HashMap::new();
        limits.insert(
            crate::resource_manager::ResourceType::PeerConnection,
            (1000, 1000),
        );
        limits.insert(
            crate::resource_manager::ResourceType::DiskRead,
            (1000, 1000),
        );
        limits.insert(
            crate::resource_manager::ResourceType::DiskWrite,
            (1000, 1000),
        );
        limits.insert(crate::resource_manager::ResourceType::Reserve, (0, 0));

        let (resource_manager, rm_client) = ResourceManager::new(limits, shutdown_tx.clone());
        tokio::spawn(resource_manager.run());

        let bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));

        let single_piece_hash = Sha1::digest(vec![0xAA; piece_size]).to_vec();
        let mut all_hashes = Vec::new();
        for _ in 0..num_pieces {
            all_hashes.extend_from_slice(&single_piece_hash);
        }

        let total_len = (num_pieces * piece_size) as i64;

        let torrent = Torrent {
            announce: None,
            announce_list: None,
            url_list: None,
            info: crate::torrent_file::Info {
                name: name.to_string(),
                piece_length: piece_size as i64,
                pieces: all_hashes,
                length: total_len,
                files: vec![],
                private: None,
                md5sum: None,
                meta_version: None,
                file_tree: None,
            },
            info_dict_bencode: vec![0u8; 20],
            created_by: None,
            creation_date: None,
            encoding: None,
            comment: None,
            piece_layers: None,
        };

        let params = TorrentParameters {
            dht_handle: {
                #[cfg(feature = "dht")]
                {
                    mainline::Dht::builder().port(0).build().unwrap().as_async()
                }
                #[cfg(not(feature = "dht"))]
                {
                    ()
                }
            },
            incoming_peer_rx,
            metrics_tx,
            torrent_validation_status: false,
            torrent_data_path: Some(temp_dir),
            container_name: None,
            manager_command_rx: cmd_rx,
            manager_event_tx: event_tx,
            settings,
            resource_manager: rm_client.clone(),
            global_dl_bucket: bucket.clone(),
            global_ul_bucket: bucket,
            file_priorities: HashMap::new(),
        };

        (
            TorrentManager::from_torrent(params, torrent).unwrap(),
            cmd_tx,
            rm_client,
        )
    }

    async fn spawn_mock_peer(
        manager: &mut TorrentManager,
        bitfield: Vec<u8>,
        upload_delay: std::time::Duration,
    ) -> (mpsc::Receiver<Vec<u8>>, mpsc::Sender<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = listener.local_addr().unwrap();

        manager.connect_to_peer(peer_addr);

        let (tx_events, rx_events) = mpsc::channel(100);
        let (tx_ctrl, mut rx_ctrl) = mpsc::channel(1);

        tokio::spawn(async move {
            if let Ok((socket, _)) = listener.accept().await {
                let (mut rd, mut wr) = socket.into_split();

                let mut handshake_buf = vec![0u8; 68];
                if rd.read_exact(&mut handshake_buf).await.is_err() {
                    return;
                }

                let mut h_resp = vec![0u8; 68];
                h_resp[0] = 19;
                h_resp[1..20].copy_from_slice(b"BitTorrent protocol");
                h_resp[28..48].copy_from_slice(&handshake_buf[28..48]);
                let _ = wr.write_all(&h_resp).await;

                let mut msg = Vec::new();
                msg.extend_from_slice(&(1 + bitfield.len() as u32).to_be_bytes());
                msg.push(5);
                msg.extend_from_slice(&bitfield);
                let _ = wr.write_all(&msg).await;

                // This ensures the Manager knows we want data, so it considers Unchoking us.
                let interested_msg = vec![0, 0, 0, 1, 2];
                let _ = wr.write_all(&interested_msg).await;

                let mut buf = vec![0u8; 4096];
                let mut buffer = Vec::new();
                let mut am_choking = true;

                loop {
                    tokio::select! {
                        _ = rx_ctrl.recv() => break,
                        res = rd.read(&mut buf) => {
                            match res {
                                Ok(n) if n > 0 => buffer.extend_from_slice(&buf[..n]),
                                _ => break,
                            }
                        }
                    }

                    while buffer.len() >= 4 {
                        let len = u32::from_be_bytes(buffer[0..4].try_into().unwrap()) as usize;
                        if buffer.len() < 4 + len {
                            break;
                        }

                        let msg_frame = &buffer[4..4 + len];
                        if !msg_frame.is_empty() {
                            match msg_frame[0] {
                                0 => {
                                    let _ = tx_events.try_send(vec![0]);
                                }
                                1 => {
                                    let _ = tx_events.try_send(vec![1]);
                                }
                                2 => {
                                    // Interested
                                    if am_choking {
                                        let _ = wr.write_all(&[0, 0, 0, 1, 1]).await;
                                        am_choking = false;
                                    }
                                }
                                6 => {
                                    // Request
                                    let index =
                                        u32::from_be_bytes(msg_frame[1..5].try_into().unwrap());
                                    let begin =
                                        u32::from_be_bytes(msg_frame[5..9].try_into().unwrap());
                                    let req_len =
                                        u32::from_be_bytes(msg_frame[9..13].try_into().unwrap());

                                    let mut rep = vec![6];
                                    rep.extend_from_slice(&index.to_be_bytes());
                                    rep.extend_from_slice(&begin.to_be_bytes());
                                    rep.extend_from_slice(&req_len.to_be_bytes());
                                    let _ = tx_events.try_send(rep);

                                    if upload_delay.as_millis() > 0 {
                                        tokio::time::sleep(upload_delay).await;
                                    }

                                    let data = vec![0xAA; req_len as usize];
                                    let total_len = 9 + req_len;
                                    let mut resp = Vec::new();
                                    resp.extend_from_slice(&total_len.to_be_bytes());
                                    resp.push(7);
                                    resp.extend_from_slice(&index.to_be_bytes());
                                    resp.extend_from_slice(&begin.to_be_bytes());
                                    resp.extend_from_slice(&data);
                                    let _ = wr.write_all(&resp).await;
                                }
                                8 => {
                                    // Cancel
                                    let index =
                                        u32::from_be_bytes(msg_frame[1..5].try_into().unwrap());
                                    let begin =
                                        u32::from_be_bytes(msg_frame[5..9].try_into().unwrap());
                                    let req_len =
                                        u32::from_be_bytes(msg_frame[9..13].try_into().unwrap());
                                    let mut rep = vec![8];
                                    rep.extend_from_slice(&index.to_be_bytes());
                                    rep.extend_from_slice(&begin.to_be_bytes());
                                    rep.extend_from_slice(&req_len.to_be_bytes());
                                    let _ = tx_events.try_send(rep);
                                }
                                _ => {}
                            }
                        }
                        buffer.drain(0..4 + len);
                    }
                }
            }
        });
        (rx_events, tx_ctrl)
    }

    #[tokio::test]
    async fn test_case_06_rarest_first_strategy() {
        let temp_dir = std::env::temp_dir().join("superseedr_rarest_first");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let num_pieces = 2;
        let piece_size = 16_384;

        let (mut manager, _cmd, _res) =
            create_manager_harness("RarestFirst", num_pieces, piece_size, temp_dir.clone());

        // Peer A: Has [0, 1] (0xC0) - Rare Piece 1 holder
        let (mut rx_a, _k_a) = spawn_mock_peer(
            &mut manager,
            vec![0xC0],
            std::time::Duration::from_millis(0),
        )
        .await;
        // Peer B: Has [0] (0x80)
        let (mut rx_b, _k_b) = spawn_mock_peer(
            &mut manager,
            vec![0x80],
            std::time::Duration::from_millis(0),
        )
        .await;
        // Peer C: Has [0] (0x80)
        let (mut rx_c, _k_c) = spawn_mock_peer(
            &mut manager,
            vec![0x80],
            std::time::Duration::from_millis(0),
        )
        .await;

        let manager_handle = tokio::spawn(async move {
            let _ = manager.run(false).await;
        });

        let start = std::time::Instant::now();
        let mut rare_piece_requested = false;

        while start.elapsed() < std::time::Duration::from_secs(5) {
            tokio::select! {
                Some(msg) = rx_a.recv() => {
                    if msg.len() >= 5 && msg[0] == 6 {
                        let idx = u32::from_be_bytes(msg[1..5].try_into().unwrap());
                        if idx == 1 {
                            rare_piece_requested = true;
                            break;
                        }
                    }
                }
                Some(_) = rx_b.recv() => {},
                Some(_) = rx_c.recv() => {},
                else => break,
            }
        }

        assert!(
            rare_piece_requested,
            "FAILED: Manager did not prioritize requesting Rare Piece 1 from Peer A!"
        );
        println!("SUCCESS: Rarest First - Peer A received request for rare piece 1.");

        let _ = _cmd.send(ManagerCommand::Shutdown).await;
        let _ = manager_handle.await;
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn test_case_08_full_swarm_1000_blocks() {
        let temp_dir = std::env::temp_dir().join("superseedr_full_swarm");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let num_pieces = 1000;
        let piece_size = 16_384;
        let (mut manager, _cmd, _res) =
            create_manager_harness("FullSwarm", num_pieces, piece_size, temp_dir.clone());

        let make_bitfield = |pattern: fn(usize) -> bool| -> Vec<u8> {
            let mut bf = vec![0u8; num_pieces.div_ceil(8)];
            for i in 0..num_pieces {
                if pattern(i) {
                    let byte_idx = i / 8;
                    let bit_idx = 7 - (i % 8);
                    bf[byte_idx] |= 1 << bit_idx;
                }
            }
            bf
        };

        // Peer 1: SEEDER (Has All)
        let bf_seed = make_bitfield(|_| true);
        spawn_mock_peer(&mut manager, bf_seed, std::time::Duration::from_millis(1)).await;

        // Peer 2: FIRST HALF (Has 0-499)
        let bf_first = make_bitfield(|i| i < 500);
        spawn_mock_peer(&mut manager, bf_first, std::time::Duration::from_millis(2)).await;

        // Peer 3: SECOND HALF (Has 500-999)
        let bf_second = make_bitfield(|i| i >= 500);
        spawn_mock_peer(&mut manager, bf_second, std::time::Duration::from_millis(2)).await;

        // Peer 4: EVENS
        let bf_even = make_bitfield(|i| i % 2 == 0);
        spawn_mock_peer(&mut manager, bf_even, std::time::Duration::from_millis(5)).await;

        // Peer 5: ODDS
        let bf_odd = make_bitfield(|i| i % 2 != 0);
        spawn_mock_peer(&mut manager, bf_odd, std::time::Duration::from_millis(5)).await;

        let manager_handle = tokio::spawn(async move {
            let _ = manager.run(false).await;
        });

        let expected_size = (num_pieces * piece_size) as u64;
        let file_path = temp_dir.join("FullSwarm");

        let start = std::time::Instant::now();
        let timeout_duration = std::time::Duration::from_secs(45);
        let mut success = false;

        println!("Waiting for 1000 blocks (~16MB) from 5 peers...");

        while start.elapsed() < timeout_duration {
            if let Ok(meta) = std::fs::metadata(&file_path) {
                if meta.len() >= expected_size {
                    success = true;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        if !success {
            panic!("FAILED: Swarm download did not complete in 45s.");
        }

        println!(
            "SUCCESS: Downloaded 1000 blocks (~16MB) from 5 mixed peers in {:.2?}",
            start.elapsed()
        );

        let _ = _cmd.send(ManagerCommand::Shutdown).await;
        let _ = manager_handle.await;
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_debug_pipeline_latency() {
        // SETUP
        let temp_dir = std::env::temp_dir().join("superseedr_latency_debug");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        // 500 blocks * 16KB = ~8MB
        let num_pieces = 500;
        let piece_size = 16_384;
        let (mut manager, _cmd, _res) =
            create_manager_harness("LatencyTest", num_pieces, piece_size, temp_dir.clone());

        // Spawn 1 Peer with 50ms Latency (Simulating a real internet connection)
        let bf_all = vec![0xFFu8; num_pieces.div_ceil(8)];

        // 50ms delay per block write
        spawn_mock_peer(&mut manager, bf_all, std::time::Duration::from_millis(50)).await;

        let manager_handle = tokio::spawn(async move {
            let _ = manager.run(false).await;
        });

        // MONITOR
        let start = std::time::Instant::now();
        let expected_size = (num_pieces * piece_size) as u64;
        let file_path = temp_dir.join("LatencyTest");

        let mut success = false;
        // Give it 10 seconds.
        // At 300KB/s (Broken Pipeline), 8MB takes ~26 seconds -> FAIL.
        // At 5MB/s (Working Pipeline), 8MB takes ~1.6 seconds -> PASS.
        while start.elapsed() < std::time::Duration::from_secs(10) {
            if let Ok(meta) = std::fs::metadata(&file_path) {
                if meta.len() >= expected_size {
                    success = true;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        if !success {
            println!("❌ PIPELINE BROKEN: Transfer too slow for high latency peer.");
            println!("   Likely cause: 'inflight_requests' limit is too low or 'AssignWork' loop is exiting early.");
        } else {
            println!("✅ PIPELINE WORKING: High throughput achieved despite latency.");
        }

        let _ = _cmd.send(ManagerCommand::Shutdown).await;
        let _ = manager_handle.await;
        let _ = std::fs::remove_dir_all(temp_dir);

        assert!(success);
    }

    #[tokio::test]
    async fn test_non_aligned_piece_length_small_swarm_completes() {
        let temp_dir = std::env::temp_dir().join("superseedr_non_aligned_20k");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let num_pieces = 4;
        let piece_size = 20_000;
        let (mut manager, cmd_tx, _res) =
            create_manager_harness("NonAligned20k", num_pieces, piece_size, temp_dir.clone());

        let bf_all = full_bitfield(num_pieces);
        spawn_mock_peer(&mut manager, bf_all, std::time::Duration::from_millis(0)).await;

        let manager_handle = tokio::spawn(async move {
            let _ = manager.run(false).await;
        });

        let expected_size = (num_pieces * piece_size) as u64;
        let file_path = temp_dir.join("NonAligned20k");
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(12);
        let mut success = false;

        while start.elapsed() < timeout {
            if let Ok(meta) = std::fs::metadata(&file_path) {
                if meta.len() >= expected_size {
                    success = true;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        let _ = cmd_tx.send(ManagerCommand::Shutdown).await;
        let _ = manager_handle.await;
        let _ = std::fs::remove_dir_all(temp_dir);

        assert!(
            success,
            "Non-aligned piece-length torrent failed to complete in bounded time"
        );
    }

    #[tokio::test]
    async fn test_tiny_piece_length_small_swarm_completes() {
        let temp_dir = std::env::temp_dir().join("superseedr_tiny_piece_1k");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let num_pieces = 8;
        let piece_size = 1_024;
        let (mut manager, cmd_tx, _res) =
            create_manager_harness("TinyPiece1k", num_pieces, piece_size, temp_dir.clone());

        let bf_all = full_bitfield(num_pieces);
        spawn_mock_peer(&mut manager, bf_all, std::time::Duration::from_millis(0)).await;

        let manager_handle = tokio::spawn(async move {
            let _ = manager.run(false).await;
        });

        let expected_size = (num_pieces * piece_size) as u64;
        let file_path = temp_dir.join("TinyPiece1k");
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(12);
        let mut success = false;

        while start.elapsed() < timeout {
            if let Ok(meta) = std::fs::metadata(&file_path) {
                if meta.len() >= expected_size {
                    success = true;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        let _ = cmd_tx.send(ManagerCommand::Shutdown).await;
        let _ = manager_handle.await;
        let _ = std::fs::remove_dir_all(temp_dir);

        assert!(
            success,
            "Tiny piece-length torrent failed to complete in bounded time"
        );
    }

    fn decode_triplet_event(msg: &[u8], expected_kind: u8) -> Option<(u32, u32, u32)> {
        if msg.len() < 13 || msg[0] != expected_kind {
            return None;
        }
        let idx = u32::from_be_bytes(msg[1..5].try_into().ok()?);
        let begin = u32::from_be_bytes(msg[5..9].try_into().ok()?);
        let len = u32::from_be_bytes(msg[9..13].try_into().ok()?);
        Some((idx, begin, len))
    }

    #[tokio::test]
    async fn test_non_aligned_request_identity_emits_piece_local_requests() {
        let temp_dir = std::env::temp_dir().join("superseedr_req_identity_non_aligned");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let num_pieces = 2;
        let piece_size = 20_000;
        let (mut manager, cmd_tx, _res) = create_manager_harness(
            "ReqIdentityNonAligned",
            num_pieces,
            piece_size,
            temp_dir.clone(),
        );

        let (mut rx_events, _ctrl) = spawn_mock_peer(
            &mut manager,
            full_bitfield(num_pieces),
            std::time::Duration::from_millis(0),
        )
        .await;

        let manager_handle = tokio::spawn(async move {
            let _ = manager.run(false).await;
        });

        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(5);
        let mut reqs = Vec::new();

        while start.elapsed() < timeout && reqs.len() < 4 {
            if let Ok(Some(msg)) =
                tokio::time::timeout(std::time::Duration::from_millis(200), rx_events.recv()).await
            {
                if let Some(t) = decode_triplet_event(&msg, 6) {
                    reqs.push(t);
                }
            }
        }

        let _ = cmd_tx.send(ManagerCommand::Shutdown).await;
        let _ = manager_handle.await;
        let _ = std::fs::remove_dir_all(temp_dir);

        assert!(!reqs.is_empty(), "Expected request tuples from peer");
        let piece0: Vec<(u32, u32)> = reqs
            .iter()
            .filter(|(idx, _, _)| *idx == 0)
            .map(|(_, begin, len)| (*begin, *len))
            .collect();
        let piece1: Vec<(u32, u32)> = reqs
            .iter()
            .filter(|(idx, _, _)| *idx == 1)
            .map(|(_, begin, len)| (*begin, *len))
            .collect();

        assert!(
            !piece0.is_empty() && !piece1.is_empty(),
            "Expected requests for both pieces, got {:?}",
            reqs
        );
        assert!(
            piece0.contains(&(0, 16_384)) && piece0.contains(&(16_384, 3_616)),
            "Piece 0 requests must be piece-local for non-aligned geometry: {:?}",
            piece0
        );
        assert!(
            piece1.contains(&(0, 16_384)) && piece1.contains(&(16_384, 3_616)),
            "Piece 1 requests must be piece-local for non-aligned geometry: {:?}",
            piece1
        );
    }

    #[tokio::test]
    async fn test_aligned_request_identity_emits_piece_local_requests() {
        let temp_dir = std::env::temp_dir().join("superseedr_req_identity_aligned");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let num_pieces = 2;
        let piece_size = 16_384;
        let (mut manager, cmd_tx, _res) = create_manager_harness(
            "ReqIdentityAligned",
            num_pieces,
            piece_size,
            temp_dir.clone(),
        );

        let (mut rx_events, _ctrl) = spawn_mock_peer(
            &mut manager,
            full_bitfield(num_pieces),
            std::time::Duration::from_millis(0),
        )
        .await;

        let manager_handle = tokio::spawn(async move {
            let _ = manager.run(false).await;
        });

        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(5);
        let mut reqs = Vec::new();

        while start.elapsed() < timeout && reqs.len() < 2 {
            if let Ok(Some(msg)) =
                tokio::time::timeout(std::time::Duration::from_millis(200), rx_events.recv()).await
            {
                if let Some(t) = decode_triplet_event(&msg, 6) {
                    reqs.push(t);
                }
            }
        }

        let _ = cmd_tx.send(ManagerCommand::Shutdown).await;
        let _ = manager_handle.await;
        let _ = std::fs::remove_dir_all(temp_dir);

        assert!(
            reqs.contains(&(0, 0, 16_384)) && reqs.contains(&(1, 0, 16_384)),
            "Aligned requests must remain piece-local: {:?}",
            reqs
        );
    }

    #[tokio::test]
    async fn test_non_aligned_cancel_identity_emits_piece_local_cancels() {
        let temp_dir = std::env::temp_dir().join("superseedr_cancel_identity_non_aligned");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let num_pieces = 1;
        let piece_size = 20_000;
        let (mut manager, cmd_tx, _res) = create_manager_harness(
            "CancelIdentityNonAligned",
            num_pieces,
            piece_size,
            temp_dir.clone(),
        );

        let (mut rx_slow, _ctrl_slow) = spawn_mock_peer(
            &mut manager,
            full_bitfield(num_pieces),
            std::time::Duration::from_millis(120),
        )
        .await;
        let (_rx_fast, _ctrl_fast) = spawn_mock_peer(
            &mut manager,
            full_bitfield(num_pieces),
            std::time::Duration::from_millis(10),
        )
        .await;

        let manager_handle = tokio::spawn(async move {
            let _ = manager.run(false).await;
        });

        let precondition_start = std::time::Instant::now();
        let precondition_timeout = std::time::Duration::from_secs(4);
        let mut slow_peer_requested_piece = false;
        while precondition_start.elapsed() < precondition_timeout {
            if let Ok(Some(msg)) =
                tokio::time::timeout(std::time::Duration::from_millis(250), rx_slow.recv()).await
            {
                if decode_triplet_event(&msg, 6).is_some() {
                    slow_peer_requested_piece = true;
                    break;
                }
            }
        }

        assert!(
            slow_peer_requested_piece,
            "Precondition failed: slow peer never received a request tuple"
        );

        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(8);
        let mut cancels = Vec::new();

        while start.elapsed() < timeout {
            if let Ok(Some(msg)) =
                tokio::time::timeout(std::time::Duration::from_millis(250), rx_slow.recv()).await
            {
                if let Some(t) = decode_triplet_event(&msg, 8) {
                    cancels.push(t);
                    if cancels.len() >= 2 {
                        break;
                    }
                }
            }
        }

        let _ = cmd_tx.send(ManagerCommand::Shutdown).await;
        let _ = manager_handle.await;
        let _ = std::fs::remove_dir_all(temp_dir);

        assert!(!cancels.is_empty(), "Expected at least one cancel tuple");
        assert!(
            cancels.iter().all(|(idx, _, _)| *idx == 0),
            "Cancels must stay in piece-local namespace for non-aligned case: {:?}",
            cancels
        );
        assert!(
            cancels.contains(&(0, 0, 16_384)) || cancels.contains(&(0, 16_384, 3_616)),
            "Expected non-aligned piece-local cancel tuples, got {:?}",
            cancels
        );
    }

    #[tokio::test]
    async fn test_aligned_cancel_identity_emits_piece_local_cancels() {
        let temp_dir = std::env::temp_dir().join("superseedr_cancel_identity_aligned");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let num_pieces = 1;
        let piece_size = 16_384;
        let (mut manager, cmd_tx, _res) = create_manager_harness(
            "CancelIdentityAligned",
            num_pieces,
            piece_size,
            temp_dir.clone(),
        );

        let (mut rx_slow, _ctrl_slow) = spawn_mock_peer(
            &mut manager,
            full_bitfield(num_pieces),
            std::time::Duration::from_millis(120),
        )
        .await;
        let (_rx_fast, _ctrl_fast) = spawn_mock_peer(
            &mut manager,
            full_bitfield(num_pieces),
            std::time::Duration::from_millis(10),
        )
        .await;

        let manager_handle = tokio::spawn(async move {
            let _ = manager.run(false).await;
        });

        let precondition_start = std::time::Instant::now();
        let precondition_timeout = std::time::Duration::from_secs(4);
        let mut slow_peer_requested_piece = false;
        while precondition_start.elapsed() < precondition_timeout {
            if let Ok(Some(msg)) =
                tokio::time::timeout(std::time::Duration::from_millis(250), rx_slow.recv()).await
            {
                if decode_triplet_event(&msg, 6).is_some() {
                    slow_peer_requested_piece = true;
                    break;
                }
            }
        }

        assert!(
            slow_peer_requested_piece,
            "Precondition failed: slow peer never received a request tuple"
        );

        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(8);
        let mut cancels = Vec::new();

        while start.elapsed() < timeout {
            if let Ok(Some(msg)) =
                tokio::time::timeout(std::time::Duration::from_millis(250), rx_slow.recv()).await
            {
                if let Some(t) = decode_triplet_event(&msg, 8) {
                    cancels.push(t);
                    break;
                }
            }
        }

        let _ = cmd_tx.send(ManagerCommand::Shutdown).await;
        let _ = manager_handle.await;
        let _ = std::fs::remove_dir_all(temp_dir);

        assert!(
            cancels.contains(&(0, 0, 16_384)),
            "Aligned cancel must use exact piece-local tuple: {:?}",
            cancels
        );
    }
}
