// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::PeerInfo;
use crate::app::TorrentState;
use crate::resource_manager::ResourceManagerClient;

use crate::networking::ConnectionType;

use crate::token_bucket::TokenBucket;

use crate::torrent_manager::DiskIoOperation;

use crate::config::Settings;

use crate::torrent_manager::piece_manager::PieceStatus;
use crate::torrent_manager::state::ChokeStatus;
use crate::torrent_manager::state::PeerState;
use crate::torrent_manager::state::TorrentActivity;

use crate::torrent_manager::state::TorrentStatus;
use crate::torrent_manager::state::TrackerState;
use crate::torrent_manager::ManagerCommand;
use crate::torrent_manager::ManagerEvent;

use crate::torrent_manager::piece_manager::PieceManager;

use crate::errors::StorageError;
use crate::storage::create_and_allocate_files;
use crate::storage::read_data_from_disk;
use crate::storage::write_data_to_disk;
use crate::storage::MultiFileInfo;

use crate::command::TorrentCommand;
use crate::command::TorrentCommandSummary;

use crate::networking::session::PeerSessionParameters;
use crate::networking::BlockInfo;
use crate::networking::PeerSession;

use crate::tracker::client::{
    announce_completed, announce_periodic, announce_started, announce_stopped,
};

use rand::prelude::IndexedRandom;
use rand::Rng;

use crate::torrent_file::Torrent;

use std::error::Error;

use tracing::{event, Level};

#[cfg(feature = "dht")]
use mainline::async_dht::AsyncDht;
#[cfg(feature = "dht")]
use mainline::Id;
#[cfg(not(feature = "dht"))]
type AsyncDht = ();

use std::time::Duration;
use std::time::Instant;

use magnet_url::Magnet;

use urlencoding::decode;

use data_encoding::BASE32;

use sha1::{Digest, Sha1};
use tokio::fs;
use tokio::net::TcpStream;
use tokio::signal;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_stream::StreamExt;

use std::collections::HashMap;
use std::collections::HashSet;
use std::net::SocketAddrV4;
use std::path::PathBuf;
use std::sync::Arc;

use crate::torrent_manager::TorrentParameters;

const HASH_LENGTH: usize = 20;
const MAX_CONCURRENT_VALIDATIONS: usize = 64;
const MAX_BLOCK_SIZE: u32 = 131_072;
const CLIENT_LEECHING_FALLBACK_INTERVAL: u64 = 60; // 60 seconds
const FALLBACK_ANNOUNCE_INTERVAL: u64 = 1800; // 30 minutes

const BASE_COOLDOWN_SECS: u64 = 10;
const MAX_COOLDOWN_SECS: u64 = 1800; 
const MAX_TIMEOUT_COUNT: u32 = 10;

pub struct TorrentManager {
    info_hash: Vec<u8>,
    torrent_metadata_length: Option<i64>,
    torrent: Option<Torrent>,

    root_download_path: PathBuf,
    multi_file_info: Option<MultiFileInfo>,

    is_paused: bool,

    trackers: HashMap<String, TrackerState>,

    torrent_status: TorrentStatus,

    number_of_successfully_connected_peers: usize,
    torrent_validation_status: bool,

    dht_handle: AsyncDht,

    last_known_peers: HashSet<String>,

    peers_map: HashMap<String, PeerState>,
    timed_out_peers: HashMap<String, (u32, Instant)>,
    torrent_manager_tx: Sender<TorrentCommand>,

    #[cfg(feature = "dht")]
    dht_tx: Sender<Vec<SocketAddrV4>>,
    #[cfg(not(feature = "dht"))]
    dht_tx: Sender<()>,

    metrics_tx: Sender<TorrentState>,
    manager_event_tx: Sender<ManagerEvent>,
    shutdown_tx: broadcast::Sender<()>,

    torrent_manager_rx: Receiver<TorrentCommand>,

    #[cfg(feature = "dht")]
    dht_rx: Receiver<Vec<SocketAddrV4>>,
    #[cfg(not(feature = "dht"))]
    dht_rx: Receiver<()>,

    incoming_peer_rx: Receiver<(TcpStream, Vec<u8>)>,
    manager_command_rx: Receiver<ManagerCommand>,

    session_total_uploaded: u64,
    session_total_downloaded: u64,
    bytes_downloaded_in_interval: u64,
    bytes_uploaded_in_interval: u64,
    total_dl_prev_avg_ema: f64,
    total_ul_prev_avg_ema: f64,

    piece_manager: PieceManager,

    optimistic_unchoke_timer: Instant,

    has_made_first_connection: bool,

    in_flight_uploads: HashMap<String, HashMap<BlockInfo, JoinHandle<()>>>,

    #[cfg(feature = "dht")]
    dht_trigger_tx: watch::Sender<()>,
    #[cfg(not(feature = "dht"))]
    dht_trigger_tx: (),

    settings: Arc<Settings>,
    resource_manager: ResourceManagerClient,

    last_activity: TorrentActivity,

    global_dl_bucket: Arc<Mutex<TokenBucket>>,
    global_ul_bucket: Arc<Mutex<TokenBucket>>,
}

impl TorrentManager {
    pub fn from_torrent(
        torrent_parameters: TorrentParameters,
        torrent: Torrent,
    ) -> Result<Self, String> {
        let TorrentParameters {
            dht_handle,
            incoming_peer_rx,
            metrics_tx,
            torrent_validation_status,
            download_dir,
            manager_command_rx,
            manager_event_tx,
            settings,
            resource_manager,
            global_dl_bucket,
            global_ul_bucket,
        } = torrent_parameters;

        let bencoded_data = serde_bencode::to_bytes(&torrent)
            .map_err(|e| format!("Failed to re-encode torrent struct: {}", e))?;

        let torrent_length = bencoded_data.len();

        let mut trackers = HashMap::new();
        if let Some(ref announce) = torrent.announce {
            trackers.insert(
                announce.clone(),
                TrackerState {
                    next_announce_time: Instant::now(), // Announce immediately
                    leeching_interval: None,
                    seeding_interval: None,
                },
            );
        }

        let mut info_dict_hasher = Sha1::new();
        info_dict_hasher.update(&torrent.info_dict_bencode);
        let info_hash = info_dict_hasher.finalize();

        let (torrent_manager_tx, torrent_manager_rx) = mpsc::channel::<TorrentCommand>(100);
        let (shutdown_tx, _) = broadcast::channel(1);

        #[cfg(feature = "dht")]
        let (dht_tx, dht_rx) = mpsc::channel::<Vec<SocketAddrV4>>(10);
        #[cfg(not(feature = "dht"))]
        let (dht_tx, dht_rx) = mpsc::channel::<()>(1);

        #[cfg(feature = "dht")]
        let (dht_trigger_tx, _) = watch::channel(());
        #[cfg(not(feature = "dht"))]
        let dht_trigger_tx = ();

        let pieces_len = torrent.info.pieces.len();

        let mut piece_manager = PieceManager::new();
        piece_manager.set_initial_fields(pieces_len / 20, torrent_validation_status);

        let multi_file_info = MultiFileInfo::new(
            &download_dir,
            &torrent.info.name,
            // Check for multi-file torrents
            if torrent.info.files.is_empty() {
                None
            } else {
                Some(&torrent.info.files)
            },
            // Provide length for single-file torrents
            if torrent.info.files.is_empty() {
                Some(torrent.info.length as u64)
            } else {
                None
            },
        )
        .map_err(|e| format!("Failed to initialize file manager: {}", e))?;

        Ok(Self {
            torrent: Some(torrent),
            torrent_metadata_length: Some(torrent_length as i64),
            root_download_path: download_dir,
            multi_file_info: Some(multi_file_info),
            is_paused: false,
            info_hash: info_hash.to_vec(),
            peers_map: HashMap::new(),
            timed_out_peers: HashMap::new(),
            trackers,
            torrent_status: TorrentStatus::Standard,
            torrent_manager_tx,
            torrent_manager_rx,
            number_of_successfully_connected_peers: 0,
            dht_handle,
            dht_tx,
            dht_rx,
            incoming_peer_rx,
            metrics_tx,
            shutdown_tx,
            torrent_validation_status,
            session_total_uploaded: 0,
            session_total_downloaded: 0,
            bytes_downloaded_in_interval: 0,
            bytes_uploaded_in_interval: 0,
            total_dl_prev_avg_ema: 0.0,
            total_ul_prev_avg_ema: 0.0,
            manager_command_rx,
            manager_event_tx,
            last_known_peers: HashSet::new(),
            piece_manager,
            optimistic_unchoke_timer: Instant::now(),
            has_made_first_connection: false,
            in_flight_uploads: HashMap::new(),
            dht_trigger_tx,
            settings,
            resource_manager,
            last_activity: TorrentActivity::Initializing,
            global_dl_bucket,
            global_ul_bucket,
        })
    }

    pub fn from_magnet(
        torrent_parameters: TorrentParameters,
        magnet: Magnet,
    ) -> Result<Self, String> {
        assert_eq!(magnet.hash_type(), Some("btih"));

        let TorrentParameters {
            dht_handle,
            incoming_peer_rx,
            metrics_tx,
            torrent_validation_status,
            download_dir,
            manager_command_rx,
            manager_event_tx,
            settings,
            resource_manager,
            global_dl_bucket,
            global_ul_bucket,
        } = torrent_parameters;

        let hash_string = magnet
            .hash()
            .ok_or_else(|| "Magnet link does not contain info hash".to_string())?;

        // Apply the same Hex/Base32 logic here
        let info_hash = if hash_string.len() == 40 {
            hex::decode(hash_string).map_err(|e| e.to_string())
        } else if hash_string.len() == 32 {
            BASE32
                .decode(hash_string.to_uppercase().as_bytes())
                .map_err(|e| e.to_string())
        } else {
            Err(format!("Invalid info_hash length: {}", hash_string.len()))
        }?;
        event!(Level::DEBUG, "INFO HASH {:?}", info_hash);

        // TODO: Handle UDP Trackers
        let trackers_set: HashSet<String> = magnet
            .trackers()
            .iter()
            .filter(|t| t.starts_with("http"))
            .map(|t| {
                decode(t)
                    .expect("Failed to decode tracker URL")
                    .into_owned()
            })
            .collect();
        let mut trackers = HashMap::new();
        for url in trackers_set {
            trackers.insert(
                url.clone(),
                TrackerState {
                    next_announce_time: Instant::now(), // Announce immediately
                    leeching_interval: None,
                    seeding_interval: None,
                },
            );
        }

        let (torrent_manager_tx, torrent_manager_rx) = mpsc::channel::<TorrentCommand>(100);
        let (shutdown_tx, _) = broadcast::channel(1);

        #[cfg(feature = "dht")]
        let (dht_tx, dht_rx) = mpsc::channel::<Vec<SocketAddrV4>>(10);
        #[cfg(not(feature = "dht"))]
        let (dht_tx, dht_rx) = mpsc::channel::<()>(1);

        #[cfg(feature = "dht")]
        let (dht_trigger_tx, _) = watch::channel(());
        #[cfg(not(feature = "dht"))]
        let dht_trigger_tx = ();

        Ok(Self {
            torrent: None,
            torrent_metadata_length: None,
            root_download_path: download_dir,
            multi_file_info: None,
            is_paused: false,
            info_hash,
            trackers,
            peers_map: HashMap::new(),
            timed_out_peers: HashMap::new(),
            torrent_status: TorrentStatus::Standard,
            torrent_manager_tx,
            torrent_manager_rx,
            number_of_successfully_connected_peers: 0,
            dht_handle,
            dht_tx,
            dht_rx,
            shutdown_tx,
            incoming_peer_rx,
            metrics_tx,
            torrent_validation_status,
            session_total_uploaded: 0,
            session_total_downloaded: 0,
            bytes_downloaded_in_interval: 0,
            bytes_uploaded_in_interval: 0,
            total_dl_prev_avg_ema: 0.0,
            total_ul_prev_avg_ema: 0.0,
            manager_command_rx,
            manager_event_tx,
            last_known_peers: HashSet::new(),
            piece_manager: PieceManager::new(),
            optimistic_unchoke_timer: Instant::now(),
            has_made_first_connection: false,
            in_flight_uploads: HashMap::new(),
            dht_trigger_tx,
            settings,
            resource_manager,
            last_activity: TorrentActivity::Initializing,
            global_dl_bucket,
            global_ul_bucket,
        })
    }

    fn recalculate_chokes(&mut self) {
        // The choke/unchoke logic is a core part of the BitTorrent protocol's tit-for-tat strategy.
        // 1. Identify interested peers.
        // 2. Sort them based on their upload/download rates (seeding vs. leeching).
        // 3. Unchoke the top peers, allowing them to download from us.
        // 4. Implement an "optimistic unchoke" to give new peers a chance.
        // 5. Choke the remaining peers to manage upload slots.
        let mut interested_peers: Vec<_> = self
            .peers_map
            .values_mut()
            .filter(|p| p.peer_is_interested_in_us)
            .collect();

        if self.torrent_status == TorrentStatus::Done {
            interested_peers
                .sort_by(|a, b| b.bytes_uploaded_to_peer.cmp(&a.bytes_uploaded_to_peer));
        } else {
            interested_peers.sort_by(|a, b| {
                b.bytes_downloaded_from_peer
                    .cmp(&a.bytes_downloaded_from_peer)
            });
        }

        let mut unchoke_candidates: HashSet<String> = interested_peers
            .iter()
            .take(self.settings.upload_slots)
            .map(|p| p.ip_port.clone())
            .collect();

        if self.optimistic_unchoke_timer.elapsed() > Duration::from_secs(30) {
            let optimistic_candidates: Vec<_> = interested_peers
                .iter()
                .filter(|p| !unchoke_candidates.contains(&p.ip_port))
                .collect();

            if let Some(optimistic_peer) = optimistic_candidates.choose(&mut rand::rng()) {
                unchoke_candidates.insert(optimistic_peer.ip_port.clone());
            }
            self.optimistic_unchoke_timer = Instant::now();
        }

        for (peer_id, peer) in self.peers_map.iter_mut() {
            if unchoke_candidates.contains(peer_id) {
                if peer.am_choking == ChokeStatus::Choke {
                    peer.am_choking = ChokeStatus::Unchoke;
                    let peer_tx = peer.peer_tx.clone();
                    let _ = peer_tx.try_send(TorrentCommand::PeerUnchoke);
                }
            } else if peer.am_choking == ChokeStatus::Unchoke {
                peer.am_choking = ChokeStatus::Choke;
                let peer_tx = peer.peer_tx.clone();
                let _ = peer_tx.try_send(TorrentCommand::PeerChoke);
            }
        }

        for peer in self.peers_map.values_mut() {
            peer.bytes_downloaded_from_peer = 0;
            peer.bytes_uploaded_to_peer = 0;
        }
    }

    fn generate_bitfield(&mut self) -> Vec<u8> {
        // Creates a bitfield representing the pieces we have.
        // The bitfield is a byte array where each bit corresponds to a piece.
        let num_pieces = self.piece_manager.bitfield.len();
        let num_bytes = num_pieces.div_ceil(8);
        let mut bitfield_bytes = vec![0u8; num_bytes];

        for (piece_index, status) in self.piece_manager.bitfield.iter().enumerate() {
            if *status == PieceStatus::Done {
                let byte_index = piece_index / 8;
                let bit_index_in_byte = piece_index % 8;
                let mask = 1 << (7 - bit_index_in_byte);
                bitfield_bytes[byte_index] |= mask;
            }
        }

        bitfield_bytes
    }

    fn check_for_completion(&mut self) {
        let _torrent = self.torrent.clone().expect("Torrent metadata not ready.");

        if self.torrent_status != TorrentStatus::Done
            && self
                .piece_manager
                .bitfield
                .iter()
                .all(|status| *status == PieceStatus::Done)
        {
            self.torrent_status = TorrentStatus::Done;

            for url in self.trackers.keys() {
                let url_clone = url.clone();
                let info_hash_clone = self.info_hash.clone();
                let client_port_clone = self.settings.client_port;
                let client_id_clone = self.settings.client_id.clone();
                let session_total_uploaded_clone = self.session_total_uploaded as usize;
                let session_total_downloaded_clone = self.session_total_downloaded as usize;
                tokio::spawn(async move {
                    let _ = announce_completed(
                        url_clone,
                        &info_hash_clone,
                        client_id_clone,
                        client_port_clone,
                        session_total_uploaded_clone,
                        session_total_downloaded_clone,
                    )
                    .await;
                });
            }
            for tracker in self.trackers.values_mut() {
                tracker.next_announce_time = Instant::now();
            }

            for peer in self.peers_map.values_mut() {
                peer.am_interested = false;
                let peer_tx_cloned = peer.peer_tx.clone();
                let _ = peer_tx_cloned.try_send(TorrentCommand::NotInterested);
            }
        }
    }

    fn find_and_assign_work(&mut self, peer_id: String) {
        if self.piece_manager.need_queue.is_empty() && self.piece_manager.pending_queue.is_empty() {
            return;
        }

        let torrent = self.torrent.clone().expect("Torrent metadata not ready.");
        let multi_file_info = self.multi_file_info.as_ref().expect("File info not ready.");

        if let Some(peer) = self.peers_map.get_mut(&peer_id) {
            if peer.bitfield.is_empty() || peer.peer_choking == ChokeStatus::Choke {
                if peer.peer_choking == ChokeStatus::Choke
                    && !peer.am_interested
                    && self
                        .piece_manager
                        .need_queue
                        .iter()
                        .any(|&p| peer.bitfield.get(p as usize) == Some(&true))
                {
                    peer.am_interested = true;
                    peer.peer_choking = ChokeStatus::Pending;
                    let peer_tx_cloned = peer.peer_tx.clone();
                    let _ = peer_tx_cloned.try_send(TorrentCommand::ClientInterested);
                }
                return;
            }

            let piece_to_assign = self.piece_manager.choose_piece_for_peer(
                &peer.bitfield,
                &peer.pending_requests,
                &self.torrent_status,
            );

            if let Some(piece_index) = piece_to_assign {
                event!(Level::DEBUG, peer = %peer_id, piece = piece_index, "Assigning rarest piece.");

                peer.pending_requests.insert(piece_index);
                self.piece_manager
                    .mark_as_pending(piece_index, peer_id.clone());

                if self.piece_manager.need_queue.is_empty()
                    && self.torrent_status != TorrentStatus::Endgame
                {
                    event!(Level::DEBUG, "All pieces requested, entering ENDGAME mode!");
                    self.torrent_status = TorrentStatus::Endgame;
                }

                let torrent_size = multi_file_info.total_size as i64;
                let peer_tx_cloned = peer.peer_tx.clone();
                let _ = peer_tx_cloned.try_send(TorrentCommand::RequestDownload(
                    piece_index,
                    torrent.info.piece_length,
                    torrent_size,
                ));
            }
        }
    }

    pub async fn connect_to_peer(&mut self, peer_ip: String, peer_port: u16) {
        if self.is_paused {
            return;
        }
        let peer_ip_port = format!("{}:{}", peer_ip, peer_port);

        if let Some((failure_count, next_attempt_time)) = self.timed_out_peers.get(&peer_ip_port) {
            if Instant::now() < *next_attempt_time {
                event!(Level::INFO, peer = %peer_ip_port, failures = %failure_count, "Ignoring connection attempt, peer is on exponential backoff.");
                return;
            }
        }

        if self.peers_map.contains_key(&peer_ip_port) {
            event!(
                Level::TRACE,
                peer_ip_port,
                "PEER SESSION ALREADY ESTABLISHED"
            );
            return;
        }

        // --- All your variable clones ---
        let torrent_manager_tx_clone = self.torrent_manager_tx.clone();
        let resource_manager_clone = self.resource_manager.clone();
        let global_dl_bucket_clone = self.global_dl_bucket.clone();
        let global_ul_bucket_clone = self.global_ul_bucket.clone();
        let info_hash_clone = self.info_hash.clone();
        let torrent_metadata_length_clone = self.torrent_metadata_length;
        let peer_ip_port_clone = peer_ip_port.clone();

        // 1. Get TWO shutdown receivers
        let mut shutdown_rx_permit = self.shutdown_tx.subscribe();
        let mut shutdown_rx_session = self.shutdown_tx.subscribe();
        let shutdown_tx = self.shutdown_tx.clone();

        let (peer_session_tx, peer_session_rx) = mpsc::channel::<TorrentCommand>(10);
        self.peers_map.insert(
            peer_ip_port.clone(),
            PeerState::new(peer_ip_port.clone(), peer_session_tx),
        );

        let bitfield = match self.torrent {
            None => None,
            _ => Some(self.generate_bitfield()),
        };

        let client_id_clone = self.settings.client_id.clone();
        tokio::spawn(async move {
            let session_permit = tokio::select! {
                permit_result = resource_manager_clone.acquire_peer_connection() => {
                    match permit_result {
                        Ok(permit) => Some(permit), // Got it
                        Err(_) => {
                            event!(Level::DEBUG, "Failed to acquire permit. Manager shut down?");
                            None // Failed, will exit
                        }
                    }
                }
                _ = shutdown_rx_permit.recv() => {
                    event!(Level::DEBUG, "PEER SESSION {}: Shutting down before permit acquired.", &peer_ip_port_clone);
                    None // Shutting down, will exit
                }
            };

            if let Some(session_permit) = session_permit {
                let connection_result = timeout(
                    Duration::from_secs(2),
                    TcpStream::connect(&peer_ip_port_clone),
                )
                .await;

                if let Ok(Ok(stream)) = connection_result {
                    let _held_session_permit = session_permit;
                    let session = PeerSession::new(PeerSessionParameters {
                        info_hash: info_hash_clone,
                        torrent_metadata_length: torrent_metadata_length_clone,
                        connection_type: ConnectionType::Outgoing,
                        torrent_manager_rx: peer_session_rx,
                        torrent_manager_tx: torrent_manager_tx_clone.clone(),
                        peer_ip_port: peer_ip_port_clone.clone(),
                        client_id: client_id_clone.into(),
                        global_dl_bucket: global_dl_bucket_clone,
                        global_ul_bucket: global_ul_bucket_clone,
                        shutdown_tx,
                    });

                    tokio::select! {
                        session_result = session.run(stream, Vec::new(), bitfield) => {
                            if let Err(e) = session_result {
                                event!(
                                    Level::DEBUG,
                                    "PEER SESSION {}: ENDED IN ERROR: {}",
                                    &peer_ip_port_clone,
                                    e
                                );
                            }
                        }
                        _ = shutdown_rx_session.recv() => {
                            event!(
                                Level::DEBUG,
                                "PEER SESSION {}: Shutting down due to manager signal.",
                                &peer_ip_port_clone
                            );
                        }
                    }
                } else {
                    let _ = torrent_manager_tx_clone.try_send(TorrentCommand::UnresponsivePeer(peer_ip_port));
                    event!(Level::DEBUG, peer = %peer_ip_port_clone, "PEER TIMEOUT or connection refused");
                }
            }

            let _ = torrent_manager_tx_clone
                .send(TorrentCommand::Disconnect(peer_ip_port_clone))
                .await;
        });
    }

    pub async fn connect_to_tracker_peers(&mut self) {
        let torrent_size_left = self
            .multi_file_info
            .as_ref()
            .map_or(1, |info| info.total_size as usize);

        let mut peers = HashSet::new();

        for url in self.trackers.keys() {
            let info_hash_clone = self.info_hash.clone();
            let client_port_clone = self.settings.client_port;
            let client_id_clone = self.settings.client_id.clone();
            let tracker_response = announce_started(
                url.to_string(),
                &info_hash_clone,
                client_id_clone,
                client_port_clone,
                torrent_size_left,
            )
            .await;

            match tracker_response {
                Ok(value) => {
                    for peer in value.peers {
                        peers.insert((peer.ip, peer.port));
                    }
                }
                Err(e) => {
                    event!(Level::DEBUG, ?e);
                }
            }
        }

        for peer in peers {
            self.connect_to_peer(peer.0, peer.1).await;
        }
    }

    pub async fn validate_local_file(&mut self) -> Result<(), StorageError> {
        let torrent = self.torrent.clone().expect("Torrent metadata not ready.");

        event!(
            Level::INFO,
            "Validating Local File STARTED: {} ",
            torrent.info.name
        );

        let multi_file_info = match &self.multi_file_info {
            Some(info) => info.clone(),
            None => return Ok(()),
        };
        create_and_allocate_files(&multi_file_info).await?;

        let mut join_set = JoinSet::new();
        let piece_length_u64 = torrent.info.piece_length as u64;

        let mut piece_indices = (0..self.piece_manager.bitfield.len()).peekable();

        while piece_indices.peek().is_some() || !join_set.is_empty() {
            while join_set.len() < MAX_CONCURRENT_VALIDATIONS {
                if let Some(piece_index) = piece_indices.next() {
                    if self.torrent_validation_status {
                        self.piece_manager
                            .mark_as_complete(piece_index.try_into().unwrap());
                        continue;
                    }

                    let start_offset = (piece_index as u64) * piece_length_u64;
                    let len_this_piece = self.get_piece_size(piece_index as u32);

                    if len_this_piece == 0 {
                        continue;
                    }

                    let start_hash_index = piece_index * HASH_LENGTH;
                    let end_hash_index = start_hash_index + HASH_LENGTH;
                    let expected_hash = torrent
                        .info
                        .pieces
                        .get(start_hash_index..end_hash_index)
                        .map(|s| s.to_vec());

                    let multi_file_info_clone = multi_file_info.clone();

                    let resource_manager_clone = self.resource_manager.clone();
                    join_set.spawn(async move {
                        if let Ok(_permit) = resource_manager_clone.acquire_disk_read().await {
                            let piece_data = match read_data_from_disk(&multi_file_info_clone, start_offset, len_this_piece).await {
                                Ok(data) => data,
                                Err(e) => {
                                    event!(Level::WARN, piece = piece_index, error = %e, "Read from disk failed during validation");
                                    return (piece_index, false);
                                }
                            };
                            let validation_result = tokio::task::spawn_blocking(move || {
                                if let Some(expected) = expected_hash {
                                    sha1::Sha1::digest(&piece_data).as_slice() == expected.as_slice()
                                } else { false }
                           }).await;
                            (piece_index, validation_result.unwrap_or(false))
                        } else {
                            event!(Level::DEBUG, "Failed to acquire disk read permit. Resource manager might be shut down.");
                            (piece_index, false)
                        }
                    });
                } else {
                    // No more pieces to spawn, break the inner loop
                    break;
                }
            }

            if let Some(Ok((piece_index, is_valid))) = join_set.join_next().await {
                if is_valid {
                    self.piece_manager
                        .mark_as_complete(piece_index.try_into().unwrap());
                }
                if piece_index % 20 == 0 {
                    if let Some(ref torrent) = self.torrent {
                        let metrics_tx_clone = self.metrics_tx.clone();
                        let info_hash_clone = self.info_hash.clone();
                        let torrent_name_clone = torrent.info.name.clone();
                        let number_of_pieces_total = (torrent.info.pieces.len() / 20) as u32;
                        let number_of_pieces_completed =
                            number_of_pieces_total - self.piece_manager.pieces_remaining as u32;

                        // Create a minimal TorrentState for validation progress
                        let torrent_state = TorrentState {
                            info_hash: info_hash_clone,
                            torrent_name: torrent_name_clone,
                            number_of_pieces_total,
                            number_of_pieces_completed,
                            activity_message: "Validating local files...".to_string(),
                            ..Default::default()
                        };

                        if let Err(e) = metrics_tx_clone.try_send(torrent_state) {
                            tracing::event!(
                                Level::ERROR,
                                "Failed to send validation metrics to TUI: {}",
                                e
                            );
                        }
                    }
                }
            }
        }

        self.check_for_completion();

        event!(
            Level::INFO,
            "Validating Local File DONE: {} ",
            torrent.info.name
        );

        Ok(())
    }

    fn get_piece_size(&self, piece_index: u32) -> usize {
        let torrent = self.torrent.clone().expect("Torrent metadata not ready.");
        let multi_file_info = self.multi_file_info.as_ref().expect("File info not ready.");

        let total_length_u64 = multi_file_info.total_size;
        let piece_length_u64 = torrent.info.piece_length as u64;
        let piece_index_u64 = piece_index as u64;
        let start_offset = piece_index_u64 * piece_length_u64;
        let bytes_remaining = total_length_u64.saturating_sub(start_offset);

        std::cmp::min(piece_length_u64, bytes_remaining) as usize
    }
    fn generate_activity_message(&self, dl_speed: u64, ul_speed: u64) -> String {
        // Generates a human-readable status message based on the torrent's current state.
        if self.is_paused {
            return "Paused".to_string();
        }

        if self.torrent_status == TorrentStatus::Done {
            return if ul_speed > 0 {
                "Seeding".to_string()
            } else {
                "Finished".to_string()
            };
        }

        if dl_speed > 0 {
            return match &self.last_activity {
                TorrentActivity::DownloadingPiece(p) => format!("Receiving piece #{}", p),
                TorrentActivity::VerifyingPiece(p) => format!("Verifying piece #{}", p),
                _ => "Downloading".to_string(),
            };
        }

        if ul_speed > 0 {
            return match &self.last_activity {
                TorrentActivity::SendingPiece(p) => format!("Sending piece #{}", p),
                _ => "Uploading".to_string(),
            };
        }

        if !self.peers_map.is_empty() {
            return "Stalled".to_string();
        }

        match &self.last_activity {
            #[cfg(feature = "dht")]
            TorrentActivity::SearchingDht => "Searching DHT for peers...".to_string(),
            TorrentActivity::AnnouncingToTracker => "Contacting tracker...".to_string(),
            _ => "Connecting to peers...".to_string(),
        }
    }
    fn send_metrics(&mut self) {
        if let Some(ref torrent) = self.torrent {
            let multi_file_info = self.multi_file_info.as_ref().expect("File info not ready.");

            let next_announce_in = self
                .trackers
                .values()
                .map(|t| t.next_announce_time)
                .min()
                .map_or(Duration::MAX, |t| {
                    t.saturating_duration_since(Instant::now())
                });

            // Calculates and sends metrics to the TUI.
            let inst_total_dl_speed = self.bytes_downloaded_in_interval * 8;
            let inst_total_ul_speed = self.bytes_uploaded_in_interval * 8;
            let bytes_downloaded_this_tick = self.bytes_downloaded_in_interval;
            let bytes_uploaded_this_tick = self.bytes_uploaded_in_interval;

            let total_seconds = next_announce_in.as_secs();
            let minutes_part = (total_seconds / 60) % 60;
            if bytes_downloaded_this_tick == 0 && bytes_uploaded_this_tick == 0 && minutes_part == 0
            {
                return;
            }

            const TOTAL_EMA_PERIOD: f64 = 5.0;
            let alpha = 2.0 / (TOTAL_EMA_PERIOD + 1.0);

            let new_total_avg_dl =
                (inst_total_dl_speed as f64 * alpha) + (self.total_dl_prev_avg_ema * (1.0 - alpha));
            self.total_dl_prev_avg_ema = new_total_avg_dl;
            let smoothed_total_dl_speed = new_total_avg_dl as u64;

            let new_total_avg_ul =
                (inst_total_ul_speed as f64 * alpha) + (self.total_ul_prev_avg_ema * (1.0 - alpha));
            self.total_ul_prev_avg_ema = new_total_avg_ul;
            let smoothed_total_ul_speed = new_total_avg_ul as u64;

            let activity_message =
                self.generate_activity_message(smoothed_total_dl_speed, smoothed_total_ul_speed);

            self.bytes_downloaded_in_interval = 0;
            self.bytes_uploaded_in_interval = 0;

            let metrics_tx_clone = self.metrics_tx.clone();
            let info_hash_clone = self.info_hash.clone();
            let torrent_name_clone = torrent.info.name.clone();
            let number_of_pieces_total = (torrent.info.pieces.len() / 20) as u32;
            let number_of_pieces_completed =
                number_of_pieces_total - self.piece_manager.pieces_remaining as u32;
            let number_of_successfully_connected_peers = self.peers_map.len();

            let eta = if self.piece_manager.pieces_remaining == 0 {
                Duration::from_secs(0)
            } else if smoothed_total_dl_speed == 0 {
                Duration::MAX
            } else {
                let total_size_bytes = multi_file_info.total_size;
                let bytes_completed = (torrent.info.piece_length as u64).saturating_mul(
                    self.piece_manager
                        .bitfield
                        .iter()
                        .filter(|&s| *s == PieceStatus::Done)
                        .count() as u64,
                );
                let bytes_remaining = total_size_bytes.saturating_sub(bytes_completed);
                let eta_seconds = (bytes_remaining * 8) / smoothed_total_dl_speed;
                Duration::from_secs(eta_seconds)
            };

            let peers_info: Vec<PeerInfo> = self
                .peers_map
                .values()
                .map(|p| {
                    let base_action_str = match &p.last_action {
                        TorrentCommand::SuccessfullyConnected(id) if id.is_empty() => {
                            "Connecting...".to_string()
                        }
                        TorrentCommand::SuccessfullyConnected(_) => {
                            "Exchanged Handshake".to_string()
                        }
                        TorrentCommand::PeerBitfield(_, _) => "Exchanged Bitfield".to_string(),
                        TorrentCommand::Choke(_) => "Choked Us".to_string(),
                        TorrentCommand::Unchoke(_) => "Unchoked Us".to_string(),
                        TorrentCommand::Disconnect(_) => "Disconnected".to_string(),
                        TorrentCommand::Have(_, _) => "Peer Has New Piece".to_string(),
                        TorrentCommand::Block(_, _, _, _) => "Receiving From Peer".to_string(),
                        TorrentCommand::RequestUpload(_, _, _, _) => {
                            "Peer is Requesting".to_string()
                        }
                        TorrentCommand::Cancel(_) => "Peer Canceling Request".to_string(),
                        _ => "Idle".to_string(),
                    };
                    let discriminant = std::mem::discriminant(&p.last_action);
                    let count = p.action_counts.get(&discriminant).unwrap_or(&0);
                    let final_action_str = if *count > 0 {
                        format!("{} (x{})", base_action_str, count)
                    } else {
                        base_action_str
                    };

                    PeerInfo {
                        address: p.ip_port.clone(),
                        peer_id: p.peer_id.clone(),
                        am_choking: p.am_choking != ChokeStatus::Unchoke,
                        peer_choking: p.peer_choking != ChokeStatus::Unchoke,
                        am_interested: p.am_interested,
                        peer_interested: p.peer_is_interested_in_us,
                        bitfield: p.bitfield.clone(),
                        download_speed_bps: p.download_speed_bps,
                        upload_speed_bps: p.upload_speed_bps,
                        total_downloaded: p.total_bytes_downloaded,
                        total_uploaded: p.total_bytes_uploaded,
                        last_action: final_action_str,
                    }
                })
                .collect();

            let torrent_state = TorrentState {
                info_hash: info_hash_clone,
                torrent_name: torrent_name_clone,
                number_of_successfully_connected_peers,
                number_of_pieces_total,
                number_of_pieces_completed,
                download_speed_bps: smoothed_total_dl_speed,
                upload_speed_bps: smoothed_total_ul_speed,
                bytes_downloaded_this_tick,
                bytes_uploaded_this_tick,
                eta,
                peers: peers_info,
                activity_message,
                next_announce_in,
                ..Default::default()
            };
            tokio::spawn(async move {
                if let Err(e) = metrics_tx_clone.try_send(torrent_state) {
                    tracing::event!(Level::ERROR, "Failed to send metrics to TUI: {}", e);
                }
            });
        }
    }

    pub async fn run(mut self, is_paused: bool) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.is_paused = is_paused;

        if self.torrent.is_some() {
            if let Err(error) = self.validate_local_file().await {
                match error {
                    StorageError::Io(e) => {
                        eprintln!("Error calling validate local file: {}", e);
                    }
                }
            }
        }

        if !self.is_paused {
            event!(
                Level::DEBUG,
                "Performing initial 'started' announce to trackers..."
            );
            let torrent_size_left = self
                .multi_file_info
                .as_ref()
                .map_or(0, |mfi| mfi.total_size as usize);

            for url in self.trackers.keys() {
                let torrent_manager_tx_clone = self.torrent_manager_tx.clone();
                let url_clone = url.clone();
                let info_hash_clone = self.info_hash.clone();
                let client_port_clone = self.settings.client_port;

                let client_id_clone = self.settings.client_id.clone();

                tokio::spawn(async move {
                    let response = announce_started(
                        url_clone.clone(),
                        &info_hash_clone,
                        client_id_clone,
                        client_port_clone,
                        torrent_size_left,
                    )
                    .await;

                    match response {
                        Ok(resp) => {
                            let _ = torrent_manager_tx_clone
                                .send(TorrentCommand::AnnounceResponse(url_clone, resp))
                                .await;
                        }
                        Err(e) => {
                            let _ = torrent_manager_tx_clone
                                .send(TorrentCommand::AnnounceFailed(url_clone, e.to_string()))
                                .await;
                        }
                    }
                });
            }
        }

        #[cfg(feature = "dht")]
        {
            let dht_tx_clone = self.dht_tx.clone();
            let dht_handle_clone = self.dht_handle.clone();
            let mut dht_trigger_rx = self.dht_trigger_tx.subscribe();
            let mut shutdown_rx = self.shutdown_tx.subscribe();
            if let Ok(info_hash_id) = Id::from_bytes(self.info_hash.clone()) {
                tokio::spawn(async move {
                    loop {
                        let mut peers_stream = dht_handle_clone.get_peers(info_hash_id);
                        tokio::select! {
                            _ = shutdown_rx.recv() => {
                                event!(Level::DEBUG, "DHT task shutting down.");
                                break;
                            }

                            _ = async {
                                while let Some(peer) = peers_stream.next().await {
                                    if dht_tx_clone.send(peer).await.is_err() {
                                        return;
                                    }
                                }
                            } => {}
                        }

                        tokio::select! {
                            _ = shutdown_rx.recv() => {
                                event!(Level::DEBUG, "DHT task shutting down.");
                                break;
                            }
                            _ = tokio::time::sleep(Duration::from_secs(300)) => {}
                            _ = dht_trigger_rx.changed() => {}
                        }
                    }
                });
            }
        }

        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut cleanup_timer = tokio::time::interval(Duration::from_secs(3));
        let mut pex_timer = tokio::time::interval(Duration::from_secs(75));
        let mut choke_timer = tokio::time::interval(Duration::from_secs(10));
        loop {
            tokio::select! {
                _ = signal::ctrl_c() => {
                    println!("Ctrl+C received, initiating clean shutdown...");
                    break Ok(());
                }
                _ = cleanup_timer.tick(), if !self.is_paused => {
                    self.timed_out_peers.retain(|_, (retry_count, _)| *retry_count < MAX_TIMEOUT_COUNT);

                    if self.torrent_status == TorrentStatus::Done {
                        for peer in self.peers_map.values() {
                            let peer_is_fully_seeded = peer.bitfield.iter().all(|&has| has);

                            if peer_is_fully_seeded {
                                let manager_tx_clone = self.torrent_manager_tx.clone();
                                let peer_id_clone = peer.ip_port.clone();
                                tokio::spawn(async move {
                                    let _ = manager_tx_clone.send(TorrentCommand::Disconnect(peer_id_clone)).await;
                                });
                            }
                        }
                    }
                }
                _ = tick.tick(), if !self.is_paused => {

                    let now = Instant::now();
                    let mut trackers_to_announce = Vec::new();

                    for (url, tracker_state) in &self.trackers {
                        if now >= tracker_state.next_announce_time {
                            trackers_to_announce.push(url.clone());
                        }
                    }

                    if !trackers_to_announce.is_empty() {
                        let mut torrent_size_left = 1;
                        if let Some(torrent) = &self.torrent {
                            torrent_size_left = torrent.info.length as usize;
                            if !torrent.info.files.is_empty() {
                                torrent_size_left = torrent.info.files.iter().map(|file| file.length as usize).sum();
                            }
                        }
                        for url in trackers_to_announce {
                            if let Some(tracker_state) = self.trackers.get_mut(&url) {
                                // Set a temporary lock to prevent re-announcing
                                tracker_state.next_announce_time = now + Duration::from_secs(2048 * 2);
                                let torrent_manager_tx_clone = self.torrent_manager_tx.clone();
                                let url_clone = url.clone();
                                let info_hash_clone = self.info_hash.clone();
                                let client_port_clone = self.settings.client_port;
                                let client_id_clone = self.settings.client_id.clone();
                                let session_total_uploaded_clone = self.session_total_uploaded as usize;
                                let session_total_downloaded_clone = self.session_total_downloaded as usize;
                                tokio::spawn(async move {
                                    let tracker_response = announce_periodic(
                                        url.to_string(),
                                        &info_hash_clone,
                                        client_id_clone,
                                        client_port_clone,
                                        session_total_uploaded_clone,
                                        session_total_downloaded_clone,
                                        torrent_size_left,
                                    ).await;

                                    match tracker_response {
                                        Ok(response) => {
                                            let _ = torrent_manager_tx_clone.send(TorrentCommand::AnnounceResponse(url_clone, response)).await;
                                        },
                                        Err(e) => {
                                            let _ = torrent_manager_tx_clone.send(TorrentCommand::AnnounceFailed(url_clone, e.to_string())).await;
                                        }
                                    }
                                });
                            }
                        }
                    }


                    if self.torrent_status == TorrentStatus::Endgame {
                        let peer_ids: Vec<String> = self.peers_map.keys().cloned().collect();
                        for peer_id in peer_ids {
                            if let Some(peer) = self.peers_map.get(&peer_id) {
                                if peer.pending_requests.is_empty() {
                                    self.find_and_assign_work(peer_id.clone());
                                }
                            }
                        }
                    }

                    const PEER_EMA_PERIOD: f64 = 5.0;
                    let alpha = 2.0 / (PEER_EMA_PERIOD + 1.0);

                    for peer in self.peers_map.values_mut() {
                        let inst_dl_speed = peer.bytes_downloaded_in_tick * 8;
                        let inst_ul_speed = peer.bytes_uploaded_in_tick * 8;

                        let new_avg_dl = (inst_dl_speed as f64 * alpha) + (peer.prev_avg_dl_ema * (1.0 - alpha));
                        peer.prev_avg_dl_ema = new_avg_dl;
                        peer.download_speed_bps = new_avg_dl as u64;

                        let new_avg_ul = (inst_ul_speed as f64 * alpha) + (peer.prev_avg_ul_ema * (1.0 - alpha));
                        peer.prev_avg_ul_ema = new_avg_ul;
                        peer.upload_speed_bps = new_avg_ul as u64;

                        peer.bytes_downloaded_in_tick = 0;
                        peer.bytes_uploaded_in_tick = 0;
                    }

                    self.send_metrics();
                }

                 _ = choke_timer.tick(), if !self.is_paused => {

                    if self.torrent_status != TorrentStatus::Done {
                        let peer_bitfields = self.peers_map.values().map(|p| &p.bitfield);
                        self.piece_manager.update_rarity(peer_bitfields);
                    }
                    self.recalculate_chokes();
                }

                _ = pex_timer.tick(), if !self.is_paused => {
                    if self.peers_map.len() < 2 {
                        continue;
                    }

                    let all_peer_ips: Vec<String> = self.peers_map.keys().cloned().collect();

                    for peer_state in self.peers_map.values() {
                        let peer_tx = peer_state.peer_tx.clone();
                        let peers_list = all_peer_ips.clone();

                        let _ = peer_tx.try_send(
                            TorrentCommand::SendPexPeers(peers_list)
                        );
                    }
                }

                Some(manager_command) = self.manager_command_rx.recv() => {
                    event!(Level::TRACE, ?manager_command);
                    match manager_command {
                        ManagerCommand::Pause => {
                            self.last_activity = TorrentActivity::Paused;
                            self.is_paused = true;

                            for peer in self.peers_map.values() {
                                let peer_tx = peer.peer_tx.clone();
                                let peer_ip_port = peer.ip_port.clone();
                                let _ = peer_tx.try_send(TorrentCommand::Disconnect(peer_ip_port));
                            }

                            self.last_known_peers = self.peers_map.keys().cloned().collect();
                            self.peers_map.clear();

                            self.send_metrics();

                            event!(Level::INFO, info_hash = %BASE32.encode(&self.info_hash), "Torrent paused. Disconnected from all peers.");

                        },
                        ManagerCommand::Resume => {
                            self.last_activity = TorrentActivity::ConnectingToPeers;
                            self.is_paused = false;
                            event!(Level::INFO, info_hash = %BASE32.encode(&self.info_hash), "Torrent resumed. Re-announcing to trackers.");

                            #[cfg(feature = "dht")]
                            let _ = self.dht_trigger_tx.send(());

                            for peer_addr in std::mem::take(&mut self.last_known_peers) {
                                if let Ok(socket_addr) = peer_addr.parse::<std::net::SocketAddr>() {
                                    self.connect_to_peer(socket_addr.ip().to_string(), socket_addr.port()).await;
                                }
                            }
                            for tracker_state in self.trackers.values_mut() {
                                tracker_state.next_announce_time = Instant::now();
                            }
                        },
                        ManagerCommand::Shutdown => {
                            event!(Level::INFO, info_hash = %BASE32.encode(&self.info_hash), "Torrent shutting down.");
                            self.is_paused = true;
                            let _ = self.shutdown_tx.send(());

                            if let (Some(torrent), Some(multi_file_info)) = (&self.torrent, &self.multi_file_info) {
                                let total_size_bytes = multi_file_info.total_size;
                                let bytes_completed = (torrent.info.piece_length as u64).saturating_mul(
                                    self.piece_manager
                                        .bitfield
                                        .iter()
                                        .filter(|&s| *s == PieceStatus::Done)
                                        .count() as u64,
                                );
                                let bytes_left = total_size_bytes.saturating_sub(bytes_completed);
                                for url in self.trackers.keys() {
                                    let url_clone = url.clone();
                                    let info_hash_clone = self.info_hash.clone();
                                    let client_port_clone = self.settings.client_port;
                                    let client_id_clone = self.settings.client_id.clone();

                                let session_total_uploaded_clone = self.session_total_uploaded as usize;
                                let session_total_downloaded_clone = self.session_total_downloaded as usize;
                                    tokio::spawn(async move {
                                        announce_stopped(
                                            url_clone,
                                            &info_hash_clone,
                                            client_id_clone,
                                            client_port_clone,

                                        session_total_uploaded_clone,
                                        session_total_downloaded_clone,
                                            bytes_left as usize,
                                        )
                                        .await;
                                    });
                                }
                            }

                            self.peers_map.clear();
                            let _ = self.manager_event_tx.send(ManagerEvent::DeletionComplete(self.info_hash.clone(), Ok(()))).await;
                            break Ok(());
                        },
                        ManagerCommand::DeleteFile => {
                            let torrent = self.torrent.clone().expect("Torrent metadata not ready.");
                            self.peers_map.clear();
                            let mut event_result = Ok(());

                            if let Some(multi_file_info) = &self.multi_file_info {
                                for file_info in &multi_file_info.files {
                                    event!(Level::INFO, "Deleting file: {:?}", &file_info.path);
                                    if let Err(e) = fs::remove_file(&file_info.path).await {
                                        if e.kind() != std::io::ErrorKind::NotFound {
                                            let error_msg = format!("Failed to delete torrent file {:?}: {}", &file_info.path, e);
                                            event!(Level::ERROR, "{}", error_msg);
                                            event_result = Err(error_msg);
                                            break;
                                        }
                                    }
                                }
                                if event_result.is_ok() && multi_file_info.files.len() > 1 {
                                    let content_dir = self.root_download_path.join(&torrent.info.name);
                                    event!(Level::INFO, "Attempting to clean up directory: {:?}", &content_dir);
                                    let _ = fs::remove_dir(&content_dir).await.ok();
                                }
                            } else {
                                let error_msg = "Could not delete files: torrent metadata unavailable.".to_string();
                                event!(Level::WARN, "{}", error_msg);
                                event_result = Err(error_msg);
                            }
                            let _ = self.manager_event_tx.send(ManagerEvent::DeletionComplete(self.info_hash.clone(), event_result)).await;
                            break Ok(());
                        },
                    }
                }

                maybe_peers = async {
                    #[cfg(feature = "dht")]
                    {
                        self.dht_rx.recv().await
                    }
                    #[cfg(not(feature = "dht"))]
                    {
                        std::future::pending().await
                    }
                }, if !self.is_paused => {
                    #[cfg(feature = "dht")]
                    {
                        if let Some(peers) = maybe_peers {
                            self.last_activity = TorrentActivity::SearchingDht;
                            for peer in peers {
                                event!(Level::DEBUG, "PEER FROM DHT {}", peer);
                                self.connect_to_peer(peer.ip().to_string(), peer.port()).await;
                            }
                        } else {
                            event!(Level::WARN, "DHT channel closed. No longer receiving DHT peers.");
                        }
                    }
                }

                Some((stream, handshake_response)) = self.incoming_peer_rx.recv(), if !self.is_paused => {
                    if let Ok(peer_addr) = stream.peer_addr() {
                        let peer_ip_port = peer_addr.to_string();
                        event!(Level::DEBUG, peer_addr = %peer_ip_port, "NEW INCOMING PEER CONNECTION");
                        let torrent_manager_tx_clone = self.torrent_manager_tx.clone();
                        let (peer_session_tx, peer_session_rx) = mpsc::channel::<TorrentCommand>(10);

                        if self.peers_map.contains_key(&peer_ip_port) {
                            event!(Level::WARN, peer_ip = %peer_ip_port, "Already connected to this peer. Dropping incoming connection.");
                            continue;
                        }

                        self.peers_map.insert(
                            peer_ip_port.clone(),
                            PeerState::new(peer_ip_port.clone(), peer_session_tx),
                        );

                        let bitfield = match self.torrent {
                            None => None,
                            _ => Some(self.generate_bitfield())
                        };
                        let info_hash_clone = self.info_hash.clone();
                        let torrent_metadata_length_clone = self.torrent_metadata_length;
                        let global_dl_bucket_clone = self.global_dl_bucket.clone();
                        let global_ul_bucket_clone = self.global_ul_bucket.clone();
                        let mut shutdown_rx_manager = self.shutdown_tx.subscribe();
                        let shutdown_tx = self.shutdown_tx.clone();
                        let client_id_clone = self.settings.client_id.clone();
                        tokio::spawn(async move {
                            let session = PeerSession::new(PeerSessionParameters {
                                info_hash: info_hash_clone,
                                torrent_metadata_length: torrent_metadata_length_clone,
                                connection_type: ConnectionType::Incoming,
                                torrent_manager_rx: peer_session_rx,
                                torrent_manager_tx: torrent_manager_tx_clone,
                                peer_ip_port: peer_ip_port.clone(),
                                client_id: client_id_clone.into(),
                                global_dl_bucket: global_dl_bucket_clone,
                                global_ul_bucket: global_ul_bucket_clone,
                                shutdown_tx,
                            });

                            tokio::select! {
                                session_result = session.run(stream, handshake_response, bitfield) => {
                                    if let Err(e) = session_result {
                                        event!(Level::ERROR, peer_ip = %peer_ip_port, error = %e, "Incoming peer session ended with error.");
                                    }
                                }
                                _ = shutdown_rx_manager.recv() => {
                                    event!(
                                        Level::DEBUG,
                                        "INCOMING PEER SESSION {}: Shutting down due to manager signal.",
                                        &peer_ip_port
                                    );
                                }
                            }
                        });
                    } else {
                        event!(Level::INFO, "ERROR GETTING PEER ADDRESS FROM STREAM");
                    }
                }

                Some(command) = self.torrent_manager_rx.recv() => {

                    event!(Level::DEBUG, command_summary = ?TorrentCommandSummary(&command));
                    event!(Level::TRACE, ?command);

                    let peer_id_for_action = match &command {
                        TorrentCommand::SuccessfullyConnected(id) => Some(id),
                        TorrentCommand::PeerBitfield(id, _) => Some(id),
                        TorrentCommand::Choke(id) => Some(id),
                        TorrentCommand::Unchoke(id) => Some(id),
                        TorrentCommand::Have(id, _) => Some(id),
                        TorrentCommand::Block(id, _, _, _) => Some(id),
                        TorrentCommand::RequestUpload(id, _, _, _) => Some(id),
                        TorrentCommand::Disconnect(id) => Some(id),
                        _ => None,
                    };
                    if let Some(id) = peer_id_for_action {
                        if let Some(peer) = self.peers_map.get_mut(id) {
                            peer.last_action = command.clone();
                            let discriminant = std::mem::discriminant(&command);
                            *peer.action_counts.entry(discriminant).or_insert(0) += 1;
                        }
                    }

                    match command {
                        TorrentCommand::SuccessfullyConnected(peer_id) => {

                            if !self.has_made_first_connection {
                                self.has_made_first_connection = true;
                                event!(Level::DEBUG, "Made first successful peer connection. Proactive recovery is now armed.");
                            }

                            if self.timed_out_peers.remove(&peer_id).is_some() {
                                event!(Level::DEBUG, peer = %peer_id, "Peer connected successfully, resetting backoff.");
                            }

                            self.number_of_successfully_connected_peers += 1;
                            self.find_and_assign_work(peer_id);
                        },
                        TorrentCommand::PeerId(peer_ip_port, peer_id) => {
                            if let Some(peer) = self.peers_map.get_mut(&peer_ip_port) {
                                peer.peer_id = peer_id;
                            }
                        }
                        TorrentCommand::AddPexPeers(_peer_id, new_peers) => {
                            for peer_tuple in new_peers {
                                self.connect_to_peer(peer_tuple.0, peer_tuple.1).await;
                            }
                        },
                        TorrentCommand::PeerBitfield(peer_id, value) => {
                            if let Some(peer) = self.peers_map.get_mut(&peer_id) {
                                peer.bitfield = value.iter()
                                    .flat_map(|&byte| {
                                        (0..8).map(move |i| (byte >> (7 - i)) & 1 == 1)
                                    })
                                    .collect();
                                if let Some(ref torrent) = self.torrent {
                                    let total_pieces = torrent.info.pieces.len() / 20;
                                    peer.bitfield.resize(total_pieces, false);
                                    self.find_and_assign_work(peer_id);
                                } else {
                                    event!(Level::DEBUG, peer_id = %peer_id, "Storing raw bitfield, metadata not yet available.");
                                }
                            }
                        },
                        TorrentCommand::Choke(peer_id) => {
                            if let Some(peer) = self.peers_map.get_mut(&peer_id) {
                                peer.peer_choking = ChokeStatus::Choke;
                            }
                        }
                        TorrentCommand::PeerInterested(peer_id) => {
                            if let Some(peer) = self.peers_map.get_mut(&peer_id) {
                                peer.peer_is_interested_in_us = true;
                            }
                        }
                        TorrentCommand::Disconnect(peer_id) => {
                            if let Some(removed_peer) = self.peers_map.remove(&peer_id) {
                                for piece_index in removed_peer.pending_requests {
                                    if self.piece_manager.bitfield[piece_index as usize] != PieceStatus::Done {
                                        event!(Level::DEBUG, piece = piece_index, peer = %peer_id, "Peer disconnected, requeueing abandoned piece.");
                                        self.piece_manager.requeue_pending_to_need(piece_index);
                                    }
                                }

                                if self.number_of_successfully_connected_peers > 0 {
                                    self.number_of_successfully_connected_peers -= 1;
                                };
                            }
                        }
                        TorrentCommand::Unchoke(peer_id) => {
                            if self.torrent_status != TorrentStatus::Done {
                                if let Some(peer) = self.peers_map.get_mut(&peer_id) {
                                    peer.peer_choking = ChokeStatus::Unchoke;
                                    self.find_and_assign_work(peer_id);
                                }
                            }
                        }
                        TorrentCommand::Have(peer_id, piece_index) => {
                            if let Some(peer) = self.peers_map.get_mut(&peer_id) {
                                if peer.bitfield.len() > piece_index as usize {
                                    peer.bitfield[piece_index as usize] = true;
                                }
                            }
                        },
                        TorrentCommand::Block(peer_id, piece_index, block_offset, block_data) => {
                            self.last_activity = TorrentActivity::DownloadingPiece(piece_index);
                            event!(
                                Level::DEBUG,
                                peer = %peer_id,
                                piece = piece_index,
                                offset = block_offset,
                                len = block_data.len(),
                                "Block received"
                            );

                            if self.piece_manager.bitfield.get(piece_index as usize) == Some(&PieceStatus::Done) {
                                continue;
                            }

                            self.bytes_downloaded_in_interval += block_data.len() as u64;
                            self.session_total_downloaded += block_data.len() as u64;
                            if let Some(peer) = self.peers_map.get_mut(&peer_id) {
                                peer.bytes_downloaded_from_peer += block_data.len() as u64;
                                peer.bytes_downloaded_in_tick += block_data.len() as u64;
                                peer.total_bytes_downloaded += block_data.len() as u64
                            }

                            let piece_size = self.get_piece_size(piece_index);

                            if let Some(complete_piece_data) = self.piece_manager.handle_block(piece_index, block_offset, &block_data, piece_size) {

                                let torrent = self.torrent.clone().expect("Torrent metadata not ready for verification.");
                                let start_hash_index = piece_index as usize * HASH_LENGTH;
                                let end_hash_index = start_hash_index + HASH_LENGTH;
                                let expected_hash = torrent.info.pieces.get(start_hash_index..end_hash_index).map(|s| s.to_vec());
                                let torrent_manager_tx = self.torrent_manager_tx.clone();
                                let peer_id_clone = peer_id.clone();
                                tokio::spawn(async move {
                                    let verification_result = tokio::task::spawn_blocking(move || {
                                        if let Some(expected) = expected_hash {
                                            let actual_hash = sha1::Sha1::digest(&complete_piece_data);
                                            if actual_hash.as_slice() == expected.as_slice() {
                                                return Ok(complete_piece_data);
                                            }
                                        }
                                        Err(())
                                    }).await.unwrap_or(Err(()));

                                    let _ = torrent_manager_tx.send(TorrentCommand::PieceVerified {
                                        piece_index,
                                        peer_id: peer_id_clone,
                                        verification_result,
                                    }).await;
                                });
                            }

                        },
                        TorrentCommand::PieceVerified { piece_index, peer_id, verification_result } => {
                            self.last_activity = TorrentActivity::VerifyingPiece(piece_index);

                            let torrent = self.torrent.clone().expect("Torrent metadata not ready for verification.");
                            match verification_result {
                                Ok(verified_piece_data) => {
                                    event!(
                                        Level::DEBUG,
                                        piece = piece_index,
                                        peer = %peer_id,
                                        mode = ?self.torrent_status,
                                        "Piece verified successfully. Queue Status: Need={}, Pending={}",
                                        self.piece_manager.need_queue.len(),
                                        self.piece_manager.pending_queue.len(),
                                    );

                                    if let Some(peer) = self.peers_map.get_mut(&peer_id) {
                                        peer.pending_requests.remove(&piece_index);
                                    }


                                    let channels: Vec<Sender<TorrentCommand>> = self
                                        .peers_map
                                        .values()
                                        .filter(|peer| {
                                            peer.ip_port != peer_id &&
                                            (piece_index as usize) < peer.bitfield.len() &&
                                            !peer.bitfield[piece_index as usize]
                                        })
                                        .map(|p| p.peer_tx.clone())
                                        .collect();
                                    for tx in channels {
                                        let _ = tx.try_send(TorrentCommand::PieceAcquired(piece_index));
                                    }

                                    let multi_file_info_clone = self
                                        .multi_file_info
                                        .clone()
                                        .expect("File info should be available when writing pieces");
                                    let piece_length = torrent.info.piece_length as u64;
                                    let global_offset = piece_index as u64 * piece_length;

                                    let manager_event_tx_clone = self.manager_event_tx.clone();
                                    let info_hash_clone = self.info_hash.clone();

                                    let resource_manager_clone = self.resource_manager.clone();
                                    let torrent_manager_tx_clone = self.torrent_manager_tx.clone();
                                    let peer_id_clone = peer_id.clone();
                                    tokio::spawn(async move {
                                                                            let operation = DiskIoOperation {
                                                                                piece_index,
                                                                                offset: global_offset,
                                                                                length: verified_piece_data.len(),
                                                                            };
                                                                            let _ = manager_event_tx_clone.send(ManagerEvent::DiskWriteStarted { info_hash: info_hash_clone, op: operation }).await;

                                                                            if let Ok(_permit) = resource_manager_clone.acquire_disk_write().await {

                                                                                const MAX_RETRIES: u32 = 5;
                                                                                const BASE_BACKOFF_MS: u64 = 50;
                                                                                const JITTER_MS: u64 = 100;

                                                                                for attempt in 0..MAX_RETRIES {

                                                                                    match write_data_to_disk(
                                                                                        &multi_file_info_clone,
                                                                                        global_offset,
                                                                                        &verified_piece_data,
                                                                                    )
                                                                                    .await
                                                                                    {
                                                                                        Ok(()) => {
                                                                                            let _ = torrent_manager_tx_clone.send(TorrentCommand::PieceWrittenToDisk { peer_id: peer_id_clone, piece_index }).await;
                                                                                            let _ = manager_event_tx_clone.send(ManagerEvent::DiskWriteFinished).await;
                                                                                            return;
                                                                                        }
                                                                                        Err(e) => {
                                                                                            if attempt == MAX_RETRIES - 1 {
                                                                                                event!(Level::ERROR,
                                                                                                    piece = piece_index,
                                                                                                    error = %e,
                                                                                                    "Failed to write piece to disk after {} attempts. Abandoning piece.",
                                                                                                    MAX_RETRIES
                                                                                                );
                                                                                                break;
                                                                                            }

                                                                                            let backoff_duration_ms = BASE_BACKOFF_MS.saturating_mul(2u64.pow(attempt));
                                                                                            let jitter = rand::rng().random_range(0..=JITTER_MS);
                                                                                            let total_delay = Duration::from_millis(backoff_duration_ms + jitter);

                                                                                            event!(
                                                                                                Level::DEBUG,
                                                                                                error = %e,
                                                                                                piece = piece_index,
                                                                                                "Disk write failed. Retrying in {:?} (Attempt {}/{})",
                                                                                                total_delay,
                                                                                                attempt + 1,
                                                                                                MAX_RETRIES
                                                                                            );
                                                                                            tokio::time::sleep(total_delay).await;
                                                                                        }
                                                                                    }
                                                                                }
                                                                            } else {
                                                                                event!(Level::DEBUG, "Failed to acquire disk write permit. Resource manager might be shut down.");
                                                                            }
                                                                            let _ = torrent_manager_tx_clone.send(TorrentCommand::PieceWriteFailed { piece_index }).await;
                                                                            let _ = manager_event_tx_clone.send(ManagerEvent::DiskWriteFinished).await;
                                                                        });
                                                                        self.check_for_completion();
                                                                        self.find_and_assign_work(peer_id);
                                                                    },
                                                                    Err(_) => {
                                                                        event!(Level::WARN, piece = piece_index, bad_peer = %peer_id, "Piece validation failed.");
                                                                        self.piece_manager.reset_piece_assembly(piece_index);

                                                                        if let Some(peer) = self.peers_map.get_mut(&peer_id) {
                                                                            event!(Level::WARN, peer = %peer_id, "Disconnecting from peer due to sending corrupt piece.");
                                                                            let peer_tx = peer.peer_tx.clone();
                                                                            let _ = peer_tx.try_send(TorrentCommand::Disconnect(peer_id));
                                                                        }
                                                                    }
                                                                }
                                                            },

                        TorrentCommand::PieceWrittenToDisk { peer_id, piece_index } => {
                            for peer_id_to_cancel in self.piece_manager.mark_as_complete(piece_index) {
                                if peer_id_to_cancel != peer_id {
                                    if let Some(peer) = self.peers_map.get_mut(&peer_id_to_cancel) {
                                        event!(
                                            Level::DEBUG,
                                            "ENDGAME: Cancelling redundant request for piece {} from {}",
                                            piece_index,
                                            peer_id_to_cancel
                                        );
                                        let peer_tx = peer.peer_tx.clone();
                                        peer.pending_requests.remove(&piece_index);
                                        let _ = peer_tx.try_send(TorrentCommand::Cancel(piece_index));
                                    }
                                    self.find_and_assign_work(peer_id_to_cancel);
                                }
                            }

                            // Send Have messages to all peers
                            for peer in self.peers_map.values() {
                                let peer_tx = peer.peer_tx.clone();
                                let _ = peer_tx.try_send(TorrentCommand::PieceAcquired(piece_index));
                            }

                            self.check_for_completion();
                        },
                        TorrentCommand::PieceWriteFailed { piece_index } => {
                            event!(Level::WARN, piece = piece_index, "Re-queuing piece for download after disk write failure.");
                            self.piece_manager.requeue_pending_to_need(piece_index);
                        },
                        TorrentCommand::RequestUpload(peer_id, piece_index, block_offset, block_length) => {
                            if self.torrent.is_none() {
                                continue;
                            }
                            self.last_activity = TorrentActivity::SendingPiece(piece_index);

                            let torrent = self.torrent.clone().expect("Torrent metadata not ready.");

                            if block_length > MAX_BLOCK_SIZE {
                                event!(
                                    Level::WARN,
                                    peer_id = %peer_id,
                                    requested_length = block_length,
                                    "Peer requested an invalid block size. Ignoring."
                                );
                                continue;
                            }

                            self.bytes_uploaded_in_interval += block_length as u64;
                            self.session_total_uploaded += block_length as u64;
                            if let (Some(peer), Some(multi_file_info)) = (self.peers_map.get_mut(&peer_id), &self.multi_file_info) {
                                peer.bytes_uploaded_to_peer += block_length as u64;
                                peer.bytes_uploaded_in_tick += block_length as u64;
                                peer.total_bytes_uploaded += block_length as u64;

                                if peer.am_choking == ChokeStatus::Unchoke && (piece_index as usize) < self.piece_manager.bitfield.len() && self.piece_manager.bitfield[piece_index as usize] == PieceStatus::Done {
                                    let block_info = BlockInfo { piece_index, offset: block_offset, length: block_length };

                                    let multi_file_info_clone = multi_file_info.clone();
                                    let peer_tx = peer.peer_tx.clone();
                                    let global_offset = (piece_index as u64 * torrent.info.piece_length as u64) + block_offset as u64;

                                    let manager_event_tx_clone = self.manager_event_tx.clone();
                                    let info_hash_clone = self.info_hash.clone();

                                    let manager_tx_for_cleanup = self.torrent_manager_tx.clone();
                                    let peer_id_clone_for_cleanup = peer_id.clone();
                                    let block_info_clone = block_info.clone();
                                    let peer_semaphore = peer.upload_slots_semaphore.clone();

                                    let resource_manager_clone = self.resource_manager.clone();
                                    let handle = tokio::spawn(async move {
                                        let operation = DiskIoOperation {
                                            piece_index,
                                            offset: global_offset,
                                            length: block_length as usize,
                                        };
                                        let _ = manager_event_tx_clone.send(ManagerEvent::DiskReadStarted { info_hash: info_hash_clone, op: operation }).await;

                                        if let (Ok(_peer_permit), Ok(_disk_permit)) = (
                                            peer_semaphore.acquire_owned().await,
                                            resource_manager_clone.acquire_disk_read().await
                                        ) {
                                            const MAX_RETRIES: u32 = 3;
                                            const BASE_BACKOFF_MS: u64 = 50;
                                            const JITTER_MS: u64 = 100;

                                            let mut piece_data_result = Err(StorageError::Io(std::io::Error::other("Read not attempted")));

                                            for attempt in 0..MAX_RETRIES {
                                                match read_data_from_disk(&multi_file_info_clone, global_offset, block_length as usize).await {
                                                    Ok(piece_data) => {
                                                        piece_data_result = Ok(piece_data);
                                                        break;
                                                    }
                                                    Err(e) => {
                                                        piece_data_result = Err(e);
                                                        if attempt == MAX_RETRIES - 1 {
                                                            break;
                                                        }

                                                        let backoff_duration_ms = BASE_BACKOFF_MS.saturating_mul(2u64.pow(attempt));
                                                        let jitter = rand::rng().random_range(0..=JITTER_MS);
                                                        let total_delay = Duration::from_millis(backoff_duration_ms + jitter);

                                                        event!(
                                                            Level::DEBUG,
                                                            error = ?piece_data_result.as_ref().err(),
                                                            piece = piece_index,
                                                            "Disk read failed. Retrying in {:?} (Attempt {}/{})",
                                                            total_delay,
                                                            attempt + 1,
                                                            MAX_RETRIES
                                                        );
                                                        tokio::time::sleep(total_delay).await;
                                                    }
                                                }
                                            }

                                            match piece_data_result {
                                                Ok(piece_data) => {
                                                    let _ = peer_tx.send(TorrentCommand::Upload(piece_index, block_offset, piece_data)).await;
                                                }
                                                Err(e) => {
                                                    event!(Level::ERROR, error = ?e, piece = piece_index, "Failed to read from local disk for upload after {} attempts", MAX_RETRIES);
                                                }
                                            }
                                        } else {
                                            event!(Level::ERROR, "Failed to acquire resources for upload. Peer semaphore or resource manager might be closed.");
                                        }

                                        let _ = manager_tx_for_cleanup.send(TorrentCommand::UploadTaskCompleted {
                                            peer_id: peer_id_clone_for_cleanup,
                                            block_info: block_info_clone,
                                        }).await;
                                        let _ = manager_event_tx_clone.send(ManagerEvent::DiskReadFinished).await;
                                    });
                                    self.in_flight_uploads
                                        .entry(peer_id.clone())
                                        .or_default()
                                        .insert(block_info, handle);


                                }
                            }
                        },
                        TorrentCommand::CancelUpload(peer_id, piece_index, block_offset, block_length) => {
                            let block_to_cancel = BlockInfo { piece_index, offset: block_offset, length: block_length };
                            if let Some(peer_uploads) = self.in_flight_uploads.get_mut(&peer_id) {
                                if let Some(handle) = peer_uploads.remove(&block_to_cancel) {
                                    handle.abort();
                                    event!(Level::TRACE, peer = %peer_id, ?block_to_cancel, "Aborted in-flight upload task.");
                                }
                            }
                        },
                        TorrentCommand::UploadTaskCompleted { peer_id, block_info } => {
                            if let Some(peer_uploads) = self.in_flight_uploads.get_mut(&peer_id) {
                                peer_uploads.remove(&block_info);
                            }
                        },
                        TorrentCommand::DhtTorrent(torrent, torrent_metadata_length) => {
                            if self.torrent.is_none() {
                                let mut info_dict_hasher = Sha1::new();
                                info_dict_hasher.update(torrent.clone().info_dict_bencode);
                                let dht_info_hash = info_dict_hasher.finalize();

                                if *self.info_hash == *dht_info_hash {

                                    #[cfg(all(feature = "dht", feature = "pex"))]
                                    {
                                        // Check if the 'private' key exists and is set to 1
                                        if torrent.info.private == Some(1) {
                                            event!(Level::ERROR, info_hash = %BASE32.encode(&self.info_hash), "Rejecting private torrent (from metadata) in normal build.");

                                            let _ = self.manager_event_tx.send(ManagerEvent::DeletionComplete(self.info_hash.clone(), Ok(()))).await;
                                            break Ok(());
                                        }
                                    }

                                    self.torrent = Some(torrent.clone());
                                    self.torrent_metadata_length = Some(torrent_metadata_length);

                                    let multi_file_info = MultiFileInfo::new(
                                        &self.root_download_path,
                                        &torrent.info.name,
                                        if torrent.info.files.is_empty() { None } else { Some(&torrent.info.files) },
                                        if torrent.info.files.is_empty() { Some(torrent.info.length as u64) } else { None },
                                    )
                                    .expect("Failed to create multi-file info from DHT metadata");
                                    self.multi_file_info = Some(multi_file_info);

                                    let pieces_len = torrent.info.pieces.len();
                                    let total_pieces = pieces_len / 20;

                                    self.piece_manager.set_initial_fields(pieces_len / 20, self.torrent_validation_status);
                                    let bitfield = self.generate_bitfield();

                                    let _ = self.validate_local_file().await;

                                    if let Some(announce) = torrent.announce {
                                        self.trackers.insert(announce.clone(), TrackerState {
                                            next_announce_time: Instant::now(), // Announce immediately
                                            leeching_interval: None,
                                            seeding_interval: None,
                                        });
                                    }
                                    self.connect_to_tracker_peers().await;

                                    for peer in self.peers_map.values_mut() {
                                        peer.bitfield.resize(total_pieces, false);
                                        let peer_tx_cloned = peer.peer_tx.clone();
                                        let bitfield_clone = bitfield.clone();
                                        let torrent_metadata_length_clone = self.torrent_metadata_length;
                                        let _ =
                                            peer_tx_cloned.try_send(TorrentCommand::ClientBitfield(bitfield_clone, torrent_metadata_length_clone));
                                    }
                                }
                            }
                        }
                        TorrentCommand::AnnounceResponse(url, response) => {
                            self.last_activity = TorrentActivity::AnnouncingToTracker;
                            for peer in response.peers {
                                self.connect_to_peer(peer.ip, peer.port).await;
                            }

                            if let Some(tracker) = self.trackers.get_mut(&url) {
                                let seeding_interval_secs = if response.interval > 0 { response.interval as u64 } else { FALLBACK_ANNOUNCE_INTERVAL };
                                tracker.seeding_interval = Some(Duration::from_secs(seeding_interval_secs));

                                let leeching_interval_secs = match response.min_interval {
                                    Some(min) if min > 0 => min as u64,
                                    _ => CLIENT_LEECHING_FALLBACK_INTERVAL,
                                };
                                tracker.leeching_interval = Some(Duration::from_secs(leeching_interval_secs));

                                let next_interval = if self.torrent_status != TorrentStatus::Done {
                                    tracker.leeching_interval.unwrap()
                                } else {
                                    tracker.seeding_interval.unwrap()
                                };

                                tracker.next_announce_time = Instant::now() + next_interval;
                                event!(Level::DEBUG, tracker = %url, next_announce_in_secs = next_interval.as_secs(), "Announce successful. STATUS {:?}", self.torrent_status);
                            }
                        },

                        TorrentCommand::AnnounceFailed(url, error_message) => {
                            if let Some(tracker) = self.trackers.get_mut(&url) {
                                let current_interval = tracker.seeding_interval.unwrap_or(Duration::from_secs(FALLBACK_ANNOUNCE_INTERVAL));

                                let backoff_secs = (current_interval.as_secs() * 2).min(FALLBACK_ANNOUNCE_INTERVAL * 2);
                                let backoff_duration = Duration::from_secs(backoff_secs);

                                tracker.next_announce_time = Instant::now() + backoff_duration;
                                event!(Level::DEBUG, tracker = %url, error = %error_message, retry_in_secs = backoff_secs, "Announce failed.");
                            }
                        },

                        TorrentCommand::UnresponsivePeer(peer_ip_port) => {
                            let now = Instant::now();

                            let (failure_count, _) = self.timed_out_peers.get(&peer_ip_port).cloned().unwrap_or((0, now));
                            let new_failure_count = (failure_count + 1).min(10);
                            let backoff_duration_secs = (BASE_COOLDOWN_SECS * 2u64.pow(new_failure_count - 1))
                                                        .min(MAX_COOLDOWN_SECS);
                            let next_attempt_time = now + Duration::from_secs(backoff_duration_secs);

                            event!(Level::DEBUG,
                                peer = %peer_ip_port,
                                failures = new_failure_count,
                                cooldown_secs = backoff_duration_secs,
                                "Peer timed out. Applying exponential backoff."
                            );
                            self.timed_out_peers.insert(peer_ip_port.clone(), (new_failure_count, next_attempt_time));

                            let _ = self.torrent_manager_tx.try_send(TorrentCommand::Disconnect(peer_ip_port));
                        }
                        _ => {
                            println!("UNIMPLEMENTED TORRENT COMMEND {:?}",  command);
                        }
                    }
                }
            }
        }
    }
}
