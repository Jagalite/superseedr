// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::protocol::{
    reader_task, writer_task, BlockInfo, ClientExtendedId, ExtendedHandshakePayload, Message,
    MetadataMessage, METADATA_PIECE_SIZE,
};

#[cfg(feature = "pex")]
use super::protocol::PexMessage;

use crate::token_bucket::TokenBucket;

use crate::torrent_manager::command::TorrentCommand;

use std::collections::HashMap;
use std::collections::HashSet;
use std::error::Error as StdError;
use std::sync::Arc;

#[cfg(feature = "pex")]
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::split;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio::time::Duration;
use tokio::time::Instant;

use tracing::{event, instrument, Level};

use crate::torrent_manager::state::MAX_PIPELINE_DEPTH;

const PEER_BLOCK_IN_FLIGHT_LIMIT: usize = 8;
const MAX_WINDOW: usize = MAX_PIPELINE_DEPTH;
const PEER_FLOOD_WINDOW: Duration = Duration::from_secs(1);
const PEER_FLOOD_DISCONNECT_BUDGET_PER_WINDOW: u32 = 131_072;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerFloodAction {
    Allow,
    DisconnectAndLog,
}

#[derive(Clone, Copy)]
struct PeerFloodGate {
    window_started_at: Instant,
    used_budget: u32,
}

impl PeerFloodGate {
    fn new(now: Instant) -> Self {
        Self {
            window_started_at: now,
            used_budget: 0,
        }
    }

    fn check(&mut self, now: Instant, cost: u32) -> PeerFloodAction {
        if now.duration_since(self.window_started_at) >= PEER_FLOOD_WINDOW {
            self.window_started_at = now;
            self.used_budget = 0;
        }

        if cost == 0 {
            return PeerFloodAction::Allow;
        }

        self.used_budget = self.used_budget.saturating_add(cost);

        if self.used_budget > PEER_FLOOD_DISCONNECT_BUDGET_PER_WINDOW {
            return PeerFloodAction::DisconnectAndLog;
        }

        PeerFloodAction::Allow
    }
}

struct DisconnectGuard {
    peer_ip_port: String,
    manager_tx: Sender<TorrentCommand>,
    network_scope_id: Option<crate::networking::NetworkScopeId>,
}

impl Drop for DisconnectGuard {
    fn drop(&mut self) {
        let disconnect = match self.network_scope_id {
            Some(scope_id) => TorrentCommand::DisconnectGeneration {
                peer_id: self.peer_ip_port.clone(),
                scope_id,
            },
            None => TorrentCommand::Disconnect(self.peer_ip_port.clone()),
        };
        match self.manager_tx.try_send(disconnect) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(disconnect)) => {
                let manager_tx = self.manager_tx.clone();
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(async move {
                        let _ = manager_tx.send(disconnect).await;
                    });
                }
            }
        }
    }
}

struct AbortOnDrop(JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ConnectionType {
    Outgoing,
    Incoming,
}

pub struct PeerSessionParameters {
    pub info_hash: Vec<u8>,
    pub torrent_metadata_length: Option<i64>,
    pub connection_type: ConnectionType,
    pub torrent_manager_rx: Receiver<TorrentCommand>,
    pub torrent_manager_tx: Sender<TorrentCommand>,
    pub peer_ip_port: String,
    pub client_id: Vec<u8>,
    pub global_dl_bucket: Arc<TokenBucket>,
    pub global_ul_bucket: Arc<TokenBucket>,
    pub shutdown_tx: broadcast::Sender<()>,
    pub network_scope_id: Option<crate::networking::NetworkScopeId>,
    pub session_cancel: watch::Receiver<bool>,
}

pub struct PeerSession {
    info_hash: Vec<u8>,
    peer_session_established: bool,
    torrent_metadata_length: Option<i64>,
    connection_type: ConnectionType,
    torrent_manager_rx: Receiver<TorrentCommand>,
    torrent_manager_tx: Sender<TorrentCommand>,
    client_id: Vec<u8>,
    peer_ip_port: String,
    #[cfg(feature = "webtorrent")]
    expected_peer_id: Option<[u8; 20]>,

    writer_rx: Option<Receiver<Message>>,
    writer_tx: Sender<Message>,

    block_tracker: Arc<Mutex<HashSet<BlockInfo>>>,
    block_request_limit_semaphore: Arc<Semaphore>,

    peer_extended_id_mappings: HashMap<String, u8>,
    peer_extended_handshake_payload: Option<ExtendedHandshakePayload>,
    peer_torrent_metadata_piece_count: usize,
    peer_torrent_metadata_pieces: Vec<u8>,

    global_dl_bucket: Arc<TokenBucket>,
    global_ul_bucket: Arc<TokenBucket>,

    shutdown_tx: broadcast::Sender<()>,
    network_scope_id: Option<crate::networking::NetworkScopeId>,
    session_cancel: watch::Receiver<bool>,

    current_window_size: usize,
    blocks_received_interval: usize,
    prev_speed: f64,
    pending_window_shrink: usize,
    peer_flood_gate: PeerFloodGate,
    last_piece_received: Instant,
    last_payload_activity: Instant,

    #[cfg(test)]
    testing_window_monitor: Option<Arc<AtomicUsize>>,

    #[cfg(test)]
    testing_window_events: Option<mpsc::UnboundedSender<WindowAdaptationEvent>>,
}

async fn wait_for_session_cancel(session_cancel: &mut watch::Receiver<bool>) {
    if *session_cancel.borrow() {
        return;
    }
    loop {
        if session_cancel.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        if *session_cancel.borrow_and_update() {
            return;
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowAdaptationEvent {
    Grew { new_size: usize },
    Shrunk { new_size: usize },
    Reset { new_size: usize },
}

impl PeerSession {
    pub fn new(params: PeerSessionParameters) -> Self {
        // Increased channel size to prevent internal bottlenecks
        let (writer_tx, writer_rx) = mpsc::channel::<Message>(1000);
        let now = Instant::now();

        Self {
            info_hash: params.info_hash,
            peer_session_established: false,
            torrent_metadata_length: params.torrent_metadata_length,
            connection_type: params.connection_type,
            torrent_manager_rx: params.torrent_manager_rx,
            torrent_manager_tx: params.torrent_manager_tx,
            client_id: params.client_id,
            peer_ip_port: params.peer_ip_port,
            #[cfg(feature = "webtorrent")]
            expected_peer_id: None,
            writer_rx: Some(writer_rx),
            writer_tx,
            block_tracker: Arc::new(Mutex::new(HashSet::new())),
            block_request_limit_semaphore: Arc::new(Semaphore::new(PEER_BLOCK_IN_FLIGHT_LIMIT)),

            peer_extended_id_mappings: HashMap::new(),
            peer_extended_handshake_payload: None,
            peer_torrent_metadata_piece_count: 0,
            peer_torrent_metadata_pieces: Vec::new(),
            global_dl_bucket: params.global_dl_bucket,
            global_ul_bucket: params.global_ul_bucket,
            shutdown_tx: params.shutdown_tx,
            network_scope_id: params.network_scope_id,
            session_cancel: params.session_cancel,

            current_window_size: PEER_BLOCK_IN_FLIGHT_LIMIT,
            blocks_received_interval: 0,
            prev_speed: 0.0,
            pending_window_shrink: 0,
            peer_flood_gate: PeerFloodGate::new(now),
            last_piece_received: now,
            last_payload_activity: now,

            #[cfg(test)]
            testing_window_monitor: None,

            #[cfg(test)]
            testing_window_events: None,
        }
    }

    #[cfg(feature = "webtorrent")]
    pub(crate) fn with_expected_peer_id(mut self, expected_peer_id: [u8; 20]) -> Self {
        self.expected_peer_id = Some(expected_peer_id);
        self
    }

    #[instrument(skip(self, stream, handshake_response, current_bitfield))]
    pub async fn run<S>(
        mut self,
        stream: S,
        handshake_response: Vec<u8>,
        current_bitfield: Option<Vec<u8>>,
    ) -> Result<(), Box<dyn StdError + Send + Sync>>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let _guard = DisconnectGuard {
            peer_ip_port: self.peer_ip_port.clone(),
            manager_tx: self.torrent_manager_tx.clone(),
            network_scope_id: self.network_scope_id,
        };
        let mut session_cancel = self.session_cancel.clone();
        if *session_cancel.borrow_and_update() {
            return Ok(());
        }

        let (mut stream_read_half, stream_write_half) = split(stream);
        let (writer_result_tx, mut writer_result_rx) = oneshot::channel();

        let global_ul_bucket_clone = self.global_ul_bucket.clone();
        let writer_shutdown_rx = self.shutdown_tx.subscribe();
        let writer_rx = self.writer_rx.take().ok_or("Writer RX missing")?;
        let mut shutdown_rx = self.shutdown_tx.subscribe();

        let writer_handle = tokio::spawn(writer_task(
            stream_write_half,
            writer_rx,
            writer_result_tx,
            global_ul_bucket_clone,
            writer_shutdown_rx,
        ));
        let _writer_abort_guard = AbortOnDrop(writer_handle);

        // We do this BEFORE spawning the reader task so we can validate the connection.
        let handshake_response = match self.connection_type {
            ConnectionType::Outgoing => {
                let _ = self.writer_tx.try_send(Message::Handshake(
                    self.info_hash.clone(),
                    self.client_id.clone(),
                ));
                let mut buffer = vec![0u8; 68];
                tokio::select! {
                    biased;
                    _ = wait_for_session_cancel(&mut session_cancel) => return Ok(()),
                    _ = shutdown_rx.recv() => return Ok(()),
                    writer_result = &mut writer_result_rx => {
                        return match writer_result {
                            Ok(result) => result,
                            Err(_) => Err("Writer panicked".into()),
                        };
                    }
                    result = async {
                        #[cfg(feature = "webtorrent")]
                        if self.expected_peer_id.is_some() {
                            return tokio::time::timeout(Duration::from_secs(20), stream_read_half.read_exact(&mut buffer))
                                .await.map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "WebRTC peer handshake timed out"))?;
                        }
                        stream_read_half.read_exact(&mut buffer).await
                    } => {
                        result?;
                    }
                }
                buffer
            }
            ConnectionType::Incoming => {
                let _ = self.writer_tx.try_send(Message::Handshake(
                    self.info_hash.clone(),
                    self.client_id.clone(),
                ));
                handshake_response
            }
        };

        let peer_info_hash = &handshake_response[28..48];
        if self.info_hash != peer_info_hash {
            return Err("Info hash mismatch".into());
        }

        let peer_id = handshake_response[48..68].to_vec();
        #[cfg(feature = "webtorrent")]
        if self
            .expected_peer_id
            .is_some_and(|expected| peer_id.as_slice() != expected)
        {
            return Err("WebRTC signaling peer ID does not match the BitTorrent handshake".into());
        }
        let _ = self
            .torrent_manager_tx
            .try_send(TorrentCommand::PeerId(self.peer_ip_port.clone(), peer_id));

        if (handshake_response[25] & 0x10) != 0 {
            let meta_len = self.torrent_metadata_length;
            let _ = self
                .writer_tx
                .try_send(Message::ExtendedHandshake(meta_len));
        }

        if let Some(bitfield) = current_bitfield {
            self.peer_session_established = true;
            let _ = self.writer_tx.try_send(Message::Bitfield(bitfield));
        }
        let _ = self
            .torrent_manager_tx
            .try_send(TorrentCommand::SuccessfullyConnected(
                self.peer_ip_port.clone(),
            ));

        let (peer_msg_tx, mut peer_msg_rx) = mpsc::channel::<Message>(100);
        let reader_shutdown = self.shutdown_tx.subscribe();
        let dl_bucket = self.global_dl_bucket.clone();
        let reader_handle = tokio::spawn(reader_task(
            stream_read_half,
            peer_msg_tx,
            dl_bucket,
            reader_shutdown,
        ));
        let _reader_abort_guard = AbortOnDrop(reader_handle);

        let mut keep_alive_timer = tokio::time::interval(Duration::from_secs(60));
        let mut speed_adjustment_timer = tokio::time::interval(Duration::from_secs(1));
        speed_adjustment_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let manager_tx = self.torrent_manager_tx.clone();

        let result: Result<(), Box<dyn StdError + Send + Sync>> = 'session: loop {
            tokio::select! {
                // KeepAlive
                _ = keep_alive_timer.tick() => { let _ = self.writer_tx.try_send(Message::KeepAlive); },

                _ = speed_adjustment_timer.tick() => {
                    if self.last_payload_activity.elapsed() > Duration::from_secs(120) {
                        break 'session Err("Timeout".into());
                    }
                    if !self.adjust_window_size() {
                        break 'session Ok(());
                    }
                },

                // INCOMING MESSAGES (From Reader Task)
                msg = peer_msg_rx.recv() => {
                    let Some(msg) = msg else {
                        break 'session Ok(());
                    };
                    self.last_payload_activity = Instant::now();
                    match self.incoming_peer_message_flood_action() {
                        PeerFloodAction::Allow => {}
                        PeerFloodAction::DisconnectAndLog => {
                            tracing::warn!(
                                "Peer {} exceeded inbound message budget (limit: {}/s). Disconnecting after {}.",
                                self.peer_ip_port,
                                PEER_FLOOD_DISCONNECT_BUDGET_PER_WINDOW,
                                Self::dropped_peer_message_label(&msg)
                            );
                            break 'session Ok(());
                        }
                    }

                    match msg {
                        Message::Piece(index, begin, data) => {
                            let block_len = data.len() as u32;
                            let info = BlockInfo {
                                piece_index: index,
                                offset: begin,
                                length: block_len,
                            };

                            let was_expected = self.block_tracker.lock().await.remove(&info);

                            if was_expected {
                                self.blocks_received_interval += 1;
                                self.last_piece_received = Instant::now();

                                if self.pending_window_shrink > 0 {
                                    self.pending_window_shrink -= 1;
                                    // We do NOT call add_permits(1).
                                    // This effectively destroys the permit associated with this block,
                                    // realizing the window shrinkage.
                                } else {
                                    self.block_request_limit_semaphore.add_permits(1);
                                }

                                let cmd = TorrentCommand::Block(self.peer_ip_port.clone(), index, begin, data);

                                loop {
                                    tokio::select! {
                                        permit_res = manager_tx.reserve() => {
                                            match permit_res {
                                                Ok(permit) => {
                                                    permit.send(cmd);
                                                    break;
                                                }
                                                Err(_) => break 'session Err("Manager Closed".into()),
                                            }
                                        }
                                        // Still process Manager commands while waiting to send (Avoid Deadlock)
                                        Some(cmd) = self.torrent_manager_rx.recv() => {
                                            if !self.process_manager_command(cmd)? {
                                                break 'session Ok(());
                                            }
                                        },
                                        _ = wait_for_session_cancel(&mut session_cancel) => break 'session Ok(()),
                                        _ = shutdown_rx.recv() => break 'session Ok(()),
                                    }
                                }
                            } else {
                                event!(Level::TRACE, "Session: Dropped cancelled/unsolicited block {}@{}", index, begin);
                            }
                        }
                        Message::Choke => {
                            self.block_tracker.lock().await.clear();

                            self.pending_window_shrink = 0;

                            self.current_window_size = PEER_BLOCK_IN_FLIGHT_LIMIT;

                            #[cfg(test)]
                            if let Some(monitor) = &self.testing_window_monitor {
                                monitor.store(self.current_window_size, Ordering::Relaxed);
                            }

                            #[cfg(test)]
                            self.emit_window_event(WindowAdaptationEvent::Reset {
                                new_size: self.current_window_size,
                            });

                            let current = self.block_request_limit_semaphore.available_permits();
                            if current < self.current_window_size {
                                self.block_request_limit_semaphore.add_permits(self.current_window_size - current);
                            }

                            let _ = self.torrent_manager_tx.try_send(TorrentCommand::Choke(self.peer_ip_port.clone()));
                        }
                        Message::Unchoke => { let _ = self.torrent_manager_tx.try_send(TorrentCommand::Unchoke(self.peer_ip_port.clone())); }
                        Message::Interested => { let _ = self.torrent_manager_tx.try_send(TorrentCommand::PeerInterested(self.peer_ip_port.clone())); }
                        Message::NotInterested => {}
                        Message::Have(idx) => { let _ = self.torrent_manager_tx.try_send(TorrentCommand::Have(self.peer_ip_port.clone(), idx)); }
                        Message::Bitfield(bf) => { let _ = self.torrent_manager_tx.try_send(TorrentCommand::PeerBitfield(self.peer_ip_port.clone(), bf)); }
                        Message::Request(i, b, l) => {
                            let _ = self.torrent_manager_tx.try_send(
                                TorrentCommand::RequestUpload(self.peer_ip_port.clone(), i, b, l)
                            );
                        }

                        Message::Cancel(i, b, l) => { let _ = self.torrent_manager_tx.try_send(TorrentCommand::CancelUpload(self.peer_ip_port.clone(), i, b, l)); }
                        Message::Extended(id, p) => { self.handle_extended_message(id, p).await?; }
                        Message::KeepAlive => {}
                        Message::Port(_) => {}
                        Message::Handshake(..) => {}
                        Message::ExtendedHandshake(_) => {}

                        Message::HashRequest(root, base, offset, length, proof_layers) => {
                            let _ = self.torrent_manager_tx.try_send(TorrentCommand::GetHashes {
                                peer_id: self.peer_ip_port.clone(),
                                file_root: root.clone(),
                                base_layer: base,
                                index: offset,
                                length,
                                proof_layers,
                            });
                            tracing::trace!("Peer requested hashes for Root: {:?}", hex::encode(&root));
                        }

                        Message::HashPiece(root, base, offset, proof) => {
                            let _ = self.torrent_manager_tx.try_send(
                                TorrentCommand::MerkleHashData {
                                    peer_id: self.peer_ip_port.clone(),
                                    root: root.clone(),
                                    piece_index: offset,
                                    base_layer: base,
                                    length: proof.len() as u32 / 32,
                                    proof,
                                }
                            );
                            tracing::debug!("Received HashPiece for Root: {:?}", hex::encode(&root));
                        }

                        Message::HashReject(root, _, offset, _, _proof_layers) => {
                            tracing::info!("Peer {} rejected hash request for Root {:?} @ Offset {}",
                                self.peer_ip_port, hex::encode(&root), offset);
                        }
                    }
                },

                // OUTGOING COMMANDS (From Manager)
                Some(cmd) = self.torrent_manager_rx.recv() => {
                    if !self.process_manager_command(cmd)? { break 'session Ok(()); }
                },

                _ = wait_for_session_cancel(&mut session_cancel) => break 'session Ok(()),

                // WRITER ERRORS
                writer_result = &mut writer_result_rx => {
                    break 'session match writer_result {
                        Ok(result) => result,
                        Err(_) => Err("Writer panicked".into()),
                    };
                },

                // SHUTDOWN
                msg = shutdown_rx.recv() => {
                    match msg {
                        Ok(()) => break 'session Ok(()),
                        Err(_) => continue,
                    }
                },
            }
        };

        result
    }

    fn process_manager_command(
        &mut self,
        command: TorrentCommand,
    ) -> Result<bool, Box<dyn StdError + Send + Sync>> {
        match command {
            TorrentCommand::Disconnect(_) => return Ok(false),

            TorrentCommand::PeerChoke | TorrentCommand::Choke(_) => {
                self.last_payload_activity = Instant::now();
                let _ = self.writer_tx.try_send(Message::Choke);
            }
            TorrentCommand::PeerUnchoke | TorrentCommand::Unchoke(_) => {
                self.last_payload_activity = Instant::now();
                let _ = self.writer_tx.try_send(Message::Unchoke);
            }
            TorrentCommand::ClientInterested => {
                self.last_payload_activity = Instant::now();
                let _ = self.writer_tx.try_send(Message::Interested);
            }
            TorrentCommand::NotInterested => {
                self.last_payload_activity = Instant::now();
                let _ = self.writer_tx.try_send(Message::NotInterested);
            }

            // --- BULK REQUEST WITH ZOMBIE REAPER ---
            TorrentCommand::BulkRequest(requests) => {
                self.last_payload_activity = Instant::now();
                let writer = self.writer_tx.clone();
                let sem = self.block_request_limit_semaphore.clone();
                let tracker = self.block_tracker.clone();
                let mut shutdown = self.shutdown_tx.subscribe();

                tokio::spawn(async move {
                    for (index, begin, length) in requests {
                        let permit_option = tokio::select! {
                            permit_result = timeout(Duration::from_secs(10), sem.clone().acquire_owned()) => {
                                match permit_result {
                                    Ok(Ok(permit)) => Some(permit),
                                    _ => None,
                                }
                            }
                            _ = shutdown.recv() => None
                        };

                        if let Some(permit) = permit_option {
                            let info = BlockInfo {
                                piece_index: index,
                                offset: begin,
                                length,
                            };

                            {
                                let mut t = tracker.lock().await;
                                t.insert(info.clone());
                            }

                            if writer
                                .send(Message::Request(index, begin, length))
                                .await
                                .is_ok()
                            {
                                permit.forget();
                            } else {
                                {
                                    let mut t = tracker.lock().await;
                                    t.remove(&info);
                                }
                                break;
                            }
                        }
                    }
                });
            }

            TorrentCommand::BulkCancel(cancels) => {
                self.last_payload_activity = Instant::now();
                for (index, begin, len) in &cancels {
                    let _ = self
                        .writer_tx
                        .try_send(Message::Cancel(*index, *begin, *len));
                }

                let tracker = self.block_tracker.clone();
                let sem = self.block_request_limit_semaphore.clone();

                tokio::spawn(async move {
                    let mut tracker_guard = tracker.lock().await;
                    let mut permits_to_add = 0;
                    for (index, begin, length) in cancels {
                        let info = BlockInfo {
                            piece_index: index,
                            offset: begin,
                            length,
                        };
                        if tracker_guard.remove(&info) {
                            permits_to_add += 1;
                        }
                    }
                    if permits_to_add > 0 {
                        sem.add_permits(permits_to_add);
                    }
                });
            }

            TorrentCommand::Upload(index, begin, data) => {
                self.last_payload_activity = Instant::now();
                let _ = self.writer_tx.try_send(Message::Piece(index, begin, data));
            }
            #[cfg(feature = "webtorrent")]
            TorrentCommand::RejectMetadata { piece } if self.expected_peer_id.is_some() => {
                if let Some(metadata_id) =
                    self.peer_advertised_extension_id(ClientExtendedId::UtMetadata)
                {
                    let payload = serde_bencode::to_bytes(&MetadataMessage {
                        msg_type: 2,
                        piece,
                        total_size: None,
                    })?;
                    self.writer_tx
                        .try_send(Message::Extended(metadata_id, payload))
                        .map_err(|_| "metadata writer queue unavailable")?;
                }
            }
            #[cfg(feature = "webtorrent")]
            TorrentCommand::UploadMetadata {
                piece,
                total_size,
                data,
            } if self.expected_peer_id.is_some()
                && data.len() <= METADATA_PIECE_SIZE
                && total_size >= data.len() =>
            {
                let header = MetadataMessage {
                    msg_type: 1,
                    piece,
                    total_size: Some(total_size),
                };
                if let (Some(metadata_id), Ok(mut payload)) = (
                    self.peer_advertised_extension_id(ClientExtendedId::UtMetadata),
                    serde_bencode::to_bytes(&header),
                ) {
                    payload.extend_from_slice(&data);
                    self.writer_tx
                        .try_send(Message::Extended(metadata_id, payload))
                        .map_err(|_| "metadata writer queue unavailable")?;
                }
            }
            TorrentCommand::PeerBitfield(_, bf) => {
                self.last_payload_activity = Instant::now();
                let _ = self.writer_tx.try_send(Message::Bitfield(bf));
            }
            #[cfg(feature = "pex")]
            TorrentCommand::SendPexPeers(peers) => {
                self.handle_pex(peers);
            }
            TorrentCommand::Have(_, idx) => {
                self.last_payload_activity = Instant::now();
                let _ = self.writer_tx.try_send(Message::Have(idx));
            }
            TorrentCommand::SendHashPiece {
                root,
                base_layer,
                index,
                proof,
                ..
            } => {
                self.last_payload_activity = Instant::now();
                let _ = self
                    .writer_tx
                    .try_send(Message::HashPiece(root, base_layer, index, proof));
            }

            TorrentCommand::SendHashReject {
                root,
                base_layer,
                index,
                length,
                ..
            } => {
                self.last_payload_activity = Instant::now();
                let _ = self
                    .writer_tx
                    .try_send(Message::HashReject(root, base_layer, index, length, 0));
            }

            TorrentCommand::GetHashes {
                file_root,
                base_layer,
                index,
                length,
                proof_layers,
                ..
            } => {
                self.last_payload_activity = Instant::now();
                let _ = self.writer_tx.try_send(Message::HashRequest(
                    file_root.clone(),
                    base_layer,
                    index,
                    length,
                    proof_layers,
                ));

                tracing::debug!(
                    "Sent HashRequest to {}: Root={:?}, Base={}, Idx={}, Len={}",
                    self.peer_ip_port,
                    hex::encode(&file_root),
                    base_layer,
                    index,
                    length
                );
            }

            _ => {}
        }
        Ok(true)
    }

    fn incoming_peer_message_flood_action(&mut self) -> PeerFloodAction {
        self.peer_flood_gate.check(Instant::now(), 1)
    }

    fn dropped_peer_message_label(message: &Message) -> &'static str {
        match message {
            Message::Request(..) => "request",
            Message::Cancel(..) => "cancel",
            Message::Piece(..) => "piece",
            Message::Choke => "choke",
            Message::Unchoke => "unchoke",
            Message::Interested => "interested",
            Message::NotInterested => "not interested",
            Message::Have(..) => "have",
            Message::Bitfield(..) => "bitfield",
            Message::Extended(..) => "extended",
            Message::KeepAlive => "keep-alive",
            Message::Port(..) => "port",
            Message::Handshake(..) => "handshake",
            Message::ExtendedHandshake(..) => "extended handshake",
            Message::HashRequest(..) => "hash request",
            Message::HashPiece(..) => "hash piece",
            Message::HashReject(..) => "hash reject",
        }
    }

    #[cfg(feature = "pex")]
    fn handle_pex(&self, peers_list: Vec<String>) {
        if let Some(pex_id) = self.peer_advertised_extension_id(ClientExtendedId::UtPex) {
            let self_addr = Self::peer_key_socket_addr(&self.peer_ip_port);
            let peers: Vec<SocketAddr> = peers_list
                .iter()
                .filter_map(|peer_key| {
                    let addr = Self::peer_key_socket_addr(peer_key)?;
                    if *peer_key == self.peer_ip_port || Some(addr) == self_addr {
                        None
                    } else {
                        Some(addr)
                    }
                })
                .collect();

            let mut added = Vec::new();
            let mut added6 = Vec::new();
            for addr in peers {
                match addr {
                    SocketAddr::V4(v4) => {
                        added.extend_from_slice(&v4.ip().octets());
                        added.extend_from_slice(&v4.port().to_be_bytes());
                    }
                    SocketAddr::V6(v6) => {
                        added6.extend_from_slice(&v6.ip().octets());
                        added6.extend_from_slice(&v6.port().to_be_bytes());
                    }
                }
            }

            if !added.is_empty() || !added6.is_empty() {
                let added_flags_len = added.len() / 6;
                let added6_flags_len = added6.len() / 18;
                let msg = PexMessage {
                    added,
                    added_f: vec![0; added_flags_len],
                    added6_f: vec![0; added6_flags_len],
                    added6,
                    ..Default::default()
                };
                if let Ok(payload) = serde_bencode::to_bytes(&msg) {
                    let _ = self.writer_tx.try_send(Message::Extended(pex_id, payload));
                }
            }
        }
    }

    #[cfg(feature = "pex")]
    fn peer_key_socket_addr(peer_key: &str) -> Option<SocketAddr> {
        let socket_addr = peer_key
            .split_once("://")
            .map_or(peer_key, |(_, socket_addr)| socket_addr);
        socket_addr.parse::<SocketAddr>().ok()
    }

    fn peer_advertised_extension_id(&self, extension: ClientExtendedId) -> Option<u8> {
        self.peer_extended_id_mappings
            .get(extension.as_str())
            .copied()
            .filter(|id| *id != ClientExtendedId::Handshake.id())
    }

    async fn handle_extended_message(
        &mut self,
        extended_id: u8,
        payload: Vec<u8>,
    ) -> Result<(), Box<dyn StdError + Send + Sync>> {
        if extended_id == ClientExtendedId::Handshake.id() {
            if let Ok(handshake_data) =
                serde_bencode::from_bytes::<ExtendedHandshakePayload>(&payload)
            {
                #[cfg(feature = "webtorrent")]
                if self.expected_peer_id.is_some() {
                    if handshake_data
                        .metadata_size
                        .is_some_and(|size| !(1..=16 * 1024 * 1024).contains(&size))
                    {
                        return Err("WebRTC metadata size is outside supported bounds".into());
                    }
                    if !self.peer_torrent_metadata_pieces.is_empty()
                        && self
                            .peer_extended_handshake_payload
                            .as_ref()
                            .and_then(|h| h.metadata_size)
                            != handshake_data.metadata_size
                    {
                        return Err("WebRTC metadata size changed during transfer".into());
                    }
                }
                self.peer_extended_id_mappings = handshake_data.m.clone();
                if !handshake_data.m.is_empty() {
                    self.peer_extended_handshake_payload = Some(handshake_data.clone());
                    if !self.peer_session_established {
                        if let Some(_torrent_metadata_len) = handshake_data.metadata_size {
                            let request = MetadataMessage {
                                msg_type: 0,
                                piece: 0,
                                total_size: None,
                            };
                            if let (Some(metadata_id), Ok(payload_bytes)) = (
                                self.peer_advertised_extension_id(ClientExtendedId::UtMetadata),
                                serde_bencode::to_bytes(&request),
                            ) {
                                let _ = self
                                    .writer_tx
                                    .try_send(Message::Extended(metadata_id, payload_bytes));
                            }
                        }
                    }
                }
            }
            return Ok(());
        }

        #[cfg(feature = "pex")]
        {
            if extended_id == ClientExtendedId::UtPex.id() {
                if let Ok(pex_data) = serde_bencode::from_bytes::<PexMessage>(&payload) {
                    let mut new_peers = Vec::new();
                    for chunk in pex_data.added.chunks_exact(6) {
                        let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
                        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
                        new_peers.push(SocketAddr::from((ip, port)));
                    }
                    for chunk in pex_data.added6.chunks_exact(18) {
                        let mut addr = [0u8; 16];
                        addr.copy_from_slice(&chunk[..16]);
                        let ip = Ipv6Addr::from(addr);
                        let port = u16::from_be_bytes([chunk[16], chunk[17]]);
                        new_peers.push(SocketAddr::from((ip, port)));
                    }
                    if !new_peers.is_empty() {
                        let _ = self
                            .torrent_manager_tx
                            .try_send(TorrentCommand::AddPexPeers(
                                self.peer_ip_port.clone(),
                                new_peers,
                            ));
                    }
                }
            }
        }

        #[cfg(feature = "webtorrent")]
        if extended_id == ClientExtendedId::UtMetadata.id() && self.expected_peer_id.is_some() {
            let mut cursor = std::io::Cursor::new(&payload[..payload.len().min(1024)]);
            let header: MetadataMessage = serde::Deserialize::deserialize(
                &mut serde_bencode::Deserializer::new(&mut cursor),
            )?;
            let header_length = cursor.position() as usize;
            if header.msg_type == 0 {
                if header_length != payload.len() {
                    return Err("metadata request has trailing data".into());
                }
                if self.peer_session_established {
                    self.torrent_manager_tx
                        .try_send(TorrentCommand::RequestMetadata {
                            peer_id: self.peer_ip_port.clone(),
                            piece: header.piece,
                        })
                        .map_err(|_| "metadata manager queue unavailable")?;
                } else {
                    self.process_manager_command(TorrentCommand::RejectMetadata {
                        piece: header.piece,
                    })?;
                }
                return Ok(());
            }
            if self.peer_session_established {
                return Ok(());
            }
            if header.msg_type == 2 {
                return Err("peer rejected the metadata request".into());
            }
            let total = self
                .peer_extended_handshake_payload
                .as_ref()
                .and_then(|h| h.metadata_size)
                .and_then(|size| usize::try_from(size).ok())
                .filter(|size| (1..=16 * 1024 * 1024).contains(size))
                .ok_or("metadata data arrived without a valid size")?;
            let start = self
                .peer_torrent_metadata_piece_count
                .saturating_mul(METADATA_PIECE_SIZE);
            let expected_length = METADATA_PIECE_SIZE.min(total.saturating_sub(start));
            if header.msg_type != 1
                || header.piece != self.peer_torrent_metadata_piece_count
                || header.total_size != Some(total)
                || expected_length == 0
                || payload.len().saturating_sub(header_length) != expected_length
                || self
                    .peer_torrent_metadata_pieces
                    .len()
                    .saturating_add(expected_length)
                    > total
            {
                return Err("invalid WebRTC metadata piece index, size, or payload".into());
            }
        }

        if extended_id == ClientExtendedId::UtMetadata.id() && !self.peer_session_established {
            if let Some(ref handshake_data) = self.peer_extended_handshake_payload {
                if let Some(torrent_metadata_len) = handshake_data.metadata_size {
                    let torrent_metadata_len_usize = torrent_metadata_len as usize;
                    let current_offset =
                        self.peer_torrent_metadata_piece_count * METADATA_PIECE_SIZE;
                    let expected_data_len = std::cmp::min(
                        METADATA_PIECE_SIZE,
                        torrent_metadata_len_usize.saturating_sub(current_offset),
                    );

                    if payload.len() >= expected_data_len {
                        let header_len = payload.len() - expected_data_len;
                        let metadata_binary = &payload[header_len..];
                        self.peer_torrent_metadata_pieces.extend(metadata_binary);

                        if torrent_metadata_len_usize == self.peer_torrent_metadata_pieces.len() {
                            match crate::torrent_file::parser::from_info_bytes(
                                &self.peer_torrent_metadata_pieces,
                            ) {
                                Ok(torrent) => {
                                    let _ = self.torrent_manager_tx.try_send(
                                        TorrentCommand::MetadataTorrent(
                                            Box::new(torrent),
                                            torrent_metadata_len,
                                        ),
                                    );
                                }
                                Err(e) => {
                                    tracing::error!(
                                        "METADATA FAILURE: Parser rejected info dict: {:?}",
                                        e
                                    );
                                }
                            }
                        } else {
                            self.peer_torrent_metadata_piece_count += 1;
                            let request = MetadataMessage {
                                msg_type: 0,
                                piece: self.peer_torrent_metadata_piece_count,
                                total_size: None,
                            };
                            if let (Some(metadata_id), Ok(payload_bytes)) = (
                                self.peer_advertised_extension_id(ClientExtendedId::UtMetadata),
                                serde_bencode::to_bytes(&request),
                            ) {
                                let _ = self
                                    .writer_tx
                                    .try_send(Message::Extended(metadata_id, payload_bytes));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn adjust_window_size(&mut self) -> bool {
        let available_permits = self.block_request_limit_semaphore.available_permits();
        let in_flight = self.current_window_size.saturating_sub(available_permits);

        if in_flight > 0 && self.last_piece_received.elapsed() > Duration::from_secs(20) {
            tracing::error!(
                "Peer {} stalled ({} blocks in flight, no data for 20s). Disconnecting.",
                self.peer_ip_port,
                in_flight
            );
            return false;
        }

        let speed = self.blocks_received_interval as f64;
        self.blocks_received_interval = 0; // Reset counter for the next 1s tick

        let is_saturated = available_permits <= 2;
        if is_saturated {
            if speed > self.prev_speed * 1.1 {
                if self.current_window_size < MAX_WINDOW {
                    self.current_window_size += 1;
                    self.block_request_limit_semaphore.add_permits(1);

                    #[cfg(test)]
                    self.emit_window_event(WindowAdaptationEvent::Grew {
                        new_size: self.current_window_size,
                    });

                    tracing::debug!(
                        "Speed Up: Peer {} -> {:.2} blocks/s (was {:.2}). Window: {}",
                        self.peer_ip_port,
                        speed,
                        self.prev_speed,
                        self.current_window_size
                    );
                }
            } else if speed < self.prev_speed * 0.9 {
                self.shrink_window();
            }
        } else if available_permits > (self.current_window_size / 2) {
            self.shrink_window();
        }

        #[cfg(test)]
        if let Some(monitor) = &self.testing_window_monitor {
            monitor.store(self.current_window_size, Ordering::Relaxed);
        }

        if self.prev_speed == 0.0 || speed > 0.0 {
            self.prev_speed = speed;
        }

        true
    }

    fn shrink_window(&mut self) {
        if self.current_window_size > PEER_BLOCK_IN_FLIGHT_LIMIT {
            self.current_window_size -= 1;

            #[cfg(test)]
            self.emit_window_event(WindowAdaptationEvent::Shrunk {
                new_size: self.current_window_size,
            });

            if let Ok(permit) = self.block_request_limit_semaphore.try_acquire() {
                permit.forget();
            } else {
                self.pending_window_shrink += 1;
            }

            tracing::debug!(
                "Shrinking: Peer {} Limit reduced to {}",
                self.peer_ip_port,
                self.current_window_size
            );
        }
    }

    #[cfg(test)]
    fn emit_window_event(&self, event: WindowAdaptationEvent) {
        if let Some(window_events) = &self.testing_window_events {
            let _ = window_events.send(event);
        }
    }

    #[cfg(test)]
    pub fn with_window_monitor(mut self, monitor: Arc<AtomicUsize>) -> Self {
        self.testing_window_monitor = Some(monitor);
        self
    }

    #[cfg(test)]
    fn with_window_events(
        mut self,
        window_events: mpsc::UnboundedSender<WindowAdaptationEvent>,
    ) -> Self {
        self.testing_window_events = Some(window_events);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::networking::protocol::{generate_message, Message};
    use crate::torrent_file::Torrent;

    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};
    use tokio::sync::{broadcast, mpsc, watch};

    async fn parse_message<R>(stream: &mut R) -> Result<Message, std::io::Error>
    where
        R: AsyncReadExt + Unpin,
    {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let message_len = u32::from_be_bytes(len_buf);

        let mut message_buf = if message_len > 0 {
            let payload_len = message_len as usize;
            let mut temp_buf = vec![0; payload_len];
            stream.read_exact(&mut temp_buf).await?;
            temp_buf
        } else {
            vec![]
        };

        let mut full_message = len_buf.to_vec();
        full_message.append(&mut message_buf);
        let mut cursor = std::io::Cursor::new(&full_message);
        crate::networking::protocol::parse_message_from_bytes(&mut cursor)
    }

    // --- Helper: Spawn Session with Window Monitor ---
    async fn spawn_test_session() -> (
        tokio::io::DuplexStream,        // Network (Mock Peer)
        mpsc::Sender<TorrentCommand>,   // Client Command Tx
        mpsc::Receiver<TorrentCommand>, // Manager Event Rx
        Arc<AtomicUsize>,               // <--- The Window Monitor
    ) {
        let (network, cmd_tx, manager_rx, window_monitor, _window_event_rx) =
            spawn_test_session_with_window_events().await;
        (network, cmd_tx, manager_rx, window_monitor)
    }

    async fn spawn_test_session_with_window_events() -> (
        tokio::io::DuplexStream,        // Network (Mock Peer)
        mpsc::Sender<TorrentCommand>,   // Client Command Tx
        mpsc::Receiver<TorrentCommand>, // Manager Event Rx
        Arc<AtomicUsize>,               // <--- The Window Monitor
        mpsc::UnboundedReceiver<WindowAdaptationEvent>,
    ) {
        let (client_socket, mock_peer_socket) = duplex(64 * 1024 * 1024);
        let infinite_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
        let (manager_tx, manager_rx) = mpsc::channel(1000);
        let (cmd_tx, cmd_rx) = mpsc::channel(1000);
        let (shutdown_tx, _) = broadcast::channel(1);
        let (window_event_tx, window_event_rx) = mpsc::unbounded_channel();

        let params = PeerSessionParameters {
            info_hash: [0u8; 20].to_vec(),
            torrent_metadata_length: None,
            connection_type: ConnectionType::Outgoing,
            torrent_manager_rx: cmd_rx,
            torrent_manager_tx: manager_tx,
            peer_ip_port: "virtual-peer:1337".to_string(),
            client_id: b"-SS1000-TESTTESTTEST".to_vec(),
            global_dl_bucket: infinite_bucket.clone(),
            global_ul_bucket: infinite_bucket.clone(),
            shutdown_tx,
            network_scope_id: None,
            session_cancel: watch::channel(false).1,
        };

        // Create the Atomic Monitor
        let window_monitor = Arc::new(AtomicUsize::new(PEER_BLOCK_IN_FLIGHT_LIMIT));
        let monitor_clone = window_monitor.clone();

        tokio::spawn(async move {
            // Inject monitor using the builder pattern
            let session = PeerSession::new(params)
                .with_window_monitor(monitor_clone)
                .with_window_events(window_event_tx);

            if let Err(e) = session.run(client_socket, vec![], Some(vec![])).await {
                eprintln!("Test Session ended: {:?}", e);
            }
        });

        (
            mock_peer_socket,
            cmd_tx,
            manager_rx,
            window_monitor,
            window_event_rx,
        )
    }

    #[tokio::test]
    async fn disconnect_guard_retries_when_manager_queue_is_full() {
        let (manager_tx, mut manager_rx) = mpsc::channel(1);
        manager_tx
            .send(TorrentCommand::NotInterested)
            .await
            .unwrap();

        drop(DisconnectGuard {
            peer_ip_port: "closed-peer".to_string(),
            manager_tx,
            network_scope_id: Some(crate::networking::NetworkScopeId::for_test(17)),
        });

        assert!(matches!(
            manager_rx.recv().await,
            Some(TorrentCommand::NotInterested)
        ));
        let cleanup = timeout(Duration::from_secs(1), manager_rx.recv())
            .await
            .expect("disconnect cleanup should retry after backpressure clears");
        assert!(matches!(
            cleanup,
            Some(TorrentCommand::DisconnectGeneration { peer_id, scope_id })
                if peer_id == "closed-peer"
                    && scope_id == crate::networking::NetworkScopeId::for_test(17)
        ));
    }

    #[tokio::test]
    async fn session_cancel_interrupts_an_outgoing_handshake() {
        let (client_socket, mut mock_peer_socket) = duplex(1024);
        let infinite_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
        let (manager_tx, mut manager_rx) = mpsc::channel(16);
        let (_cmd_tx, cmd_rx) = mpsc::channel(16);
        let (shutdown_tx, _) = broadcast::channel(1);
        let (session_cancel_tx, session_cancel_rx) = watch::channel(false);

        let session = PeerSession::new(PeerSessionParameters {
            info_hash: [0u8; 20].to_vec(),
            torrent_metadata_length: None,
            connection_type: ConnectionType::Outgoing,
            torrent_manager_rx: cmd_rx,
            torrent_manager_tx: manager_tx,
            peer_ip_port: "cancellation-peer:1337".to_string(),
            client_id: b"-SS1000-CANCELTEST00".to_vec(),
            global_dl_bucket: infinite_bucket.clone(),
            global_ul_bucket: infinite_bucket,
            shutdown_tx,
            network_scope_id: None,
            session_cancel: session_cancel_rx,
        });
        let session_task = tokio::spawn(session.run(client_socket, Vec::new(), None));

        let mut handshake = vec![0u8; 68];
        mock_peer_socket
            .read_exact(&mut handshake)
            .await
            .expect("read outgoing handshake");
        session_cancel_tx.send_replace(true);

        let result = tokio::time::timeout(Duration::from_secs(1), session_task)
            .await
            .expect("session cancellation should not wait for peer I/O")
            .expect("session task should not panic");
        assert!(result.is_ok());

        assert!(matches!(
            manager_rx.try_recv(),
            Ok(TorrentCommand::Disconnect(peer)) if peer == "cancellation-peer:1337"
        ));
        assert!(manager_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn metadata_only_session_reports_success_after_valid_handshake() {
        let (client_socket, mut mock_peer_socket) = duplex(1024);
        let infinite_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
        let (manager_tx, mut manager_rx) = mpsc::channel(16);
        let (_cmd_tx, cmd_rx) = mpsc::channel(16);
        let (shutdown_tx, _) = broadcast::channel(1);
        let peer_key = "metadata-peer:1337";

        let session = PeerSession::new(PeerSessionParameters {
            info_hash: [0u8; 20].to_vec(),
            torrent_metadata_length: None,
            connection_type: ConnectionType::Outgoing,
            torrent_manager_rx: cmd_rx,
            torrent_manager_tx: manager_tx,
            peer_ip_port: peer_key.to_string(),
            client_id: b"-SS1013-METADATA0000".to_vec(),
            global_dl_bucket: infinite_bucket.clone(),
            global_ul_bucket: infinite_bucket,
            shutdown_tx: shutdown_tx.clone(),
            network_scope_id: None,
            session_cancel: watch::channel(false).1,
        });
        let session_task = tokio::spawn(session.run(client_socket, Vec::new(), None));

        let mut handshake = vec![0u8; 68];
        mock_peer_socket
            .read_exact(&mut handshake)
            .await
            .expect("read outgoing handshake");
        handshake[48..68].copy_from_slice(b"-SS1013-REMOTEPEER00");
        mock_peer_socket
            .write_all(&handshake)
            .await
            .expect("write valid handshake response");

        let connected_peer = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match manager_rx.recv().await {
                    Some(TorrentCommand::SuccessfullyConnected(peer)) => break peer,
                    Some(_) => continue,
                    None => panic!("metadata-only session closed its manager channel"),
                }
            }
        })
        .await
        .expect("metadata-only session should report handshake success");
        assert_eq!(connected_peer, peer_key);

        let _ = shutdown_tx.send(());
        tokio::time::timeout(Duration::from_secs(1), session_task)
            .await
            .expect("metadata-only session should stop after shutdown")
            .expect("metadata-only session task should not panic")
            .expect("metadata-only session should exit cleanly");
    }

    #[cfg(feature = "webtorrent")]
    #[tokio::test]
    async fn feature_enabled_native_session_does_not_serve_webtorrent_metadata() {
        let infinite_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
        let (manager_tx, mut manager_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (shutdown_tx, _) = broadcast::channel(1);
        let mut session = PeerSession::new(PeerSessionParameters {
            info_hash: [0_u8; 20].to_vec(),
            torrent_metadata_length: Some(32),
            connection_type: ConnectionType::Outgoing,
            torrent_manager_rx: command_rx,
            torrent_manager_tx: manager_tx,
            peer_ip_port: "tcp://192.0.2.1:6881".to_string(),
            client_id: b"-SSWT00-NATIVE000000".to_vec(),
            global_dl_bucket: infinite_bucket.clone(),
            global_ul_bucket: infinite_bucket,
            shutdown_tx,
            network_scope_id: None,
            session_cancel: watch::channel(false).1,
        });
        session.peer_session_established = true;
        session
            .peer_extended_id_mappings
            .insert(ClientExtendedId::UtMetadata.as_str().to_string(), 2);

        let request = serde_bencode::to_bytes(&MetadataMessage {
            msg_type: 0,
            piece: 0,
            total_size: None,
        })
        .unwrap();
        session
            .handle_extended_message(ClientExtendedId::UtMetadata.id(), request)
            .await
            .unwrap();
        assert!(manager_rx.try_recv().is_err());

        session
            .process_manager_command(TorrentCommand::UploadMetadata {
                piece: 0,
                total_size: 32,
                data: vec![0_u8; 32],
            })
            .unwrap();
        assert!(session.writer_rx.as_mut().unwrap().try_recv().is_err());
    }

    #[cfg(feature = "webtorrent")]
    #[tokio::test]
    async fn webtorrent_session_rejects_a_signaling_and_wire_peer_id_mismatch() {
        let (client_socket, mut mock_peer_socket) = duplex(1024);
        let infinite_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
        let (manager_tx, _manager_rx) = mpsc::channel(16);
        let (_cmd_tx, cmd_rx) = mpsc::channel(16);
        let (shutdown_tx, _) = broadcast::channel(1);

        let session = PeerSession::new(PeerSessionParameters {
            info_hash: [0_u8; 20].to_vec(),
            torrent_metadata_length: None,
            connection_type: ConnectionType::Outgoing,
            torrent_manager_rx: cmd_rx,
            torrent_manager_tx: manager_tx,
            peer_ip_port: "webrtc://expected-peer".to_string(),
            client_id: b"-SSWT00-LOCAL0000000".to_vec(),
            global_dl_bucket: infinite_bucket.clone(),
            global_ul_bucket: infinite_bucket,
            shutdown_tx,
            network_scope_id: None,
            session_cancel: watch::channel(false).1,
        })
        .with_expected_peer_id(*b"-SSWT00-EXPECTED0000");
        let session_task = tokio::spawn(session.run(client_socket, Vec::new(), None));

        let mut handshake = vec![0_u8; 68];
        mock_peer_socket
            .read_exact(&mut handshake)
            .await
            .expect("read outgoing handshake");
        handshake[48..68].copy_from_slice(b"-SSWT00-FORGED000000");
        mock_peer_socket
            .write_all(&handshake)
            .await
            .expect("write mismatched handshake response");

        let error = tokio::time::timeout(Duration::from_secs(1), session_task)
            .await
            .expect("mismatched session should stop promptly")
            .expect("mismatched session task should not panic")
            .expect_err("mismatched signaling identity must be rejected");
        assert!(error.to_string().contains("signaling peer ID"));
    }

    #[cfg(feature = "webtorrent")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn peer_sessions_exchange_a_requested_block_over_webrtc() {
        use crate::networking::webtorrent::rtc::{answer_offer, create_offer, WebRtcSessionConfig};

        let offer = create_offer(WebRtcSessionConfig::loopback())
            .await
            .expect("create WebRTC offer");
        let answer = answer_offer(WebRtcSessionConfig::loopback(), offer.sdp().to_string())
            .await
            .expect("create WebRTC answer");
        let answer_sdp = answer.sdp().to_string();
        let (offerer_stream, answerer_stream) =
            tokio::try_join!(offer.accept_answer(answer_sdp), answer.into_stream(),)
                .expect("open WebRTC byte streams");

        let info_hash = [0x2a_u8; 20].to_vec();
        let infinite_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
        let (shutdown_tx, _) = broadcast::channel(2);

        let (offerer_manager_tx, mut offerer_manager_rx) = mpsc::channel(64);
        let (offerer_command_tx, offerer_command_rx) = mpsc::channel(64);
        let offerer_session = PeerSession::new(PeerSessionParameters {
            info_hash: info_hash.clone(),
            torrent_metadata_length: Some(64),
            connection_type: ConnectionType::Outgoing,
            torrent_manager_rx: offerer_command_rx,
            torrent_manager_tx: offerer_manager_tx,
            peer_ip_port: "webrtc://synthetic-answerer/0001".to_string(),
            client_id: b"-SSWT00-ALPHA0000000".to_vec(),
            global_dl_bucket: infinite_bucket.clone(),
            global_ul_bucket: infinite_bucket.clone(),
            shutdown_tx: shutdown_tx.clone(),
            network_scope_id: None,
            session_cancel: watch::channel(false).1,
        })
        .with_expected_peer_id(*b"-SSWT00-BETA00000000");

        let (answerer_manager_tx, mut answerer_manager_rx) = mpsc::channel(64);
        let (answerer_command_tx, answerer_command_rx) = mpsc::channel(64);
        let answerer_session = PeerSession::new(PeerSessionParameters {
            info_hash,
            torrent_metadata_length: Some(64),
            connection_type: ConnectionType::Outgoing,
            torrent_manager_rx: answerer_command_rx,
            torrent_manager_tx: answerer_manager_tx,
            peer_ip_port: "webrtc://synthetic-offerer/0001".to_string(),
            client_id: b"-SSWT00-BETA00000000".to_vec(),
            global_dl_bucket: infinite_bucket.clone(),
            global_ul_bucket: infinite_bucket,
            shutdown_tx: shutdown_tx.clone(),
            network_scope_id: None,
            session_cancel: watch::channel(false).1,
        })
        .with_expected_peer_id(*b"-SSWT00-ALPHA0000000");

        let offerer_task =
            tokio::spawn(offerer_session.run(offerer_stream, Vec::new(), Some(vec![0x80])));
        let answerer_task =
            tokio::spawn(answerer_session.run(answerer_stream, Vec::new(), Some(vec![0x80])));

        async fn wait_connected(receiver: &mut mpsc::Receiver<TorrentCommand>) -> String {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match receiver.recv().await {
                        Some(TorrentCommand::SuccessfullyConnected(peer_id)) => break peer_id,
                        Some(_) => {}
                        None => panic!("peer session closed before connecting"),
                    }
                }
            })
            .await
            .expect("BitTorrent handshake over WebRTC timed out")
        }

        assert_eq!(
            wait_connected(&mut offerer_manager_rx).await,
            "webrtc://synthetic-answerer/0001"
        );
        assert_eq!(
            wait_connected(&mut answerer_manager_rx).await,
            "webrtc://synthetic-offerer/0001"
        );

        let payload: Vec<u8> = (0..16_384)
            .map(|index| ((index * 17 + 3) % 251) as u8)
            .collect();
        offerer_command_tx
            .send(TorrentCommand::BulkRequest(vec![(
                0,
                0,
                payload.len() as u32,
            )]))
            .await
            .expect("request synthetic block");

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match answerer_manager_rx.recv().await {
                    Some(TorrentCommand::RequestUpload(peer_id, 0, 0, length)) => {
                        assert_eq!(peer_id, "webrtc://synthetic-offerer/0001");
                        assert_eq!(length, payload.len() as u32);
                        break;
                    }
                    Some(_) => {}
                    None => panic!("answerer session closed before upload request"),
                }
            }
        })
        .await
        .expect("upload request over WebRTC timed out");

        answerer_command_tx
            .send(TorrentCommand::Upload(0, 0, payload.clone()))
            .await
            .expect("send synthetic block");

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match offerer_manager_rx.recv().await {
                    Some(TorrentCommand::Block(peer_id, 0, 0, received)) => {
                        assert_eq!(peer_id, "webrtc://synthetic-answerer/0001");
                        assert_eq!(received, payload);
                        break;
                    }
                    Some(_) => {}
                    None => panic!("offerer session closed before receiving block"),
                }
            }
        })
        .await
        .expect("block transfer over WebRTC timed out");

        let _ = shutdown_tx.send(());
        for task in [offerer_task, answerer_task] {
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("peer session did not stop")
                .expect("peer session task panicked")
                .expect("peer session returned an error");
        }
    }

    #[cfg(feature = "webtorrent")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn metadata_only_peer_session_fetches_info_dictionary_over_webrtc() {
        use crate::networking::webtorrent::rtc::{answer_offer, create_offer, WebRtcSessionConfig};

        let offer = create_offer(WebRtcSessionConfig::loopback())
            .await
            .expect("create WebRTC offer");
        let answer = answer_offer(WebRtcSessionConfig::loopback(), offer.sdp().to_string())
            .await
            .expect("create WebRTC answer");
        let answer_sdp = answer.sdp().to_string();
        let (metadata_stream, seeder_stream) =
            tokio::try_join!(offer.accept_answer(answer_sdp), answer.into_stream(),)
                .expect("open WebRTC byte streams");

        let info_bytes =
            b"d6:lengthi16384e4:name16:web_peer_fixture12:piece lengthi16384e6:pieces20:00000000000000000000ee"
                .to_vec();
        let info_hash = [0x5a_u8; 20].to_vec();
        let infinite_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
        let (shutdown_tx, _) = broadcast::channel(2);

        let (metadata_manager_tx, mut metadata_manager_rx) = mpsc::channel(64);
        let (_metadata_command_tx, metadata_command_rx) = mpsc::channel(64);
        let metadata_session = PeerSession::new(PeerSessionParameters {
            info_hash: info_hash.clone(),
            torrent_metadata_length: None,
            connection_type: ConnectionType::Outgoing,
            torrent_manager_rx: metadata_command_rx,
            torrent_manager_tx: metadata_manager_tx,
            peer_ip_port: "webrtc://synthetic-seeder/0002".to_string(),
            client_id: b"-SSWT00-META00000000".to_vec(),
            global_dl_bucket: infinite_bucket.clone(),
            global_ul_bucket: infinite_bucket.clone(),
            shutdown_tx: shutdown_tx.clone(),
            network_scope_id: None,
            session_cancel: watch::channel(false).1,
        })
        .with_expected_peer_id(*b"-SSWT00-SEED00000000");

        let (seeder_manager_tx, mut seeder_manager_rx) = mpsc::channel(64);
        let (seeder_command_tx, seeder_command_rx) = mpsc::channel(64);
        let seeder_session = PeerSession::new(PeerSessionParameters {
            info_hash,
            torrent_metadata_length: Some(info_bytes.len() as i64),
            connection_type: ConnectionType::Outgoing,
            torrent_manager_rx: seeder_command_rx,
            torrent_manager_tx: seeder_manager_tx,
            peer_ip_port: "webrtc://synthetic-metadata-peer/0002".to_string(),
            client_id: b"-SSWT00-SEED00000000".to_vec(),
            global_dl_bucket: infinite_bucket.clone(),
            global_ul_bucket: infinite_bucket,
            shutdown_tx: shutdown_tx.clone(),
            network_scope_id: None,
            session_cancel: watch::channel(false).1,
        })
        .with_expected_peer_id(*b"-SSWT00-META00000000");

        let metadata_task = tokio::spawn(metadata_session.run(metadata_stream, Vec::new(), None));
        let seeder_task =
            tokio::spawn(seeder_session.run(seeder_stream, Vec::new(), Some(vec![0x80])));

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match seeder_manager_rx.recv().await {
                    Some(TorrentCommand::RequestMetadata { peer_id, piece }) => {
                        assert_eq!(peer_id, "webrtc://synthetic-metadata-peer/0002");
                        assert_eq!(piece, 0);
                        break;
                    }
                    Some(_) => {}
                    None => panic!("metadata seeder closed before receiving the BEP 9 request"),
                }
            }
        })
        .await
        .expect("metadata request over WebRTC timed out");

        seeder_command_tx
            .send(TorrentCommand::UploadMetadata {
                piece: 0,
                total_size: info_bytes.len(),
                data: info_bytes.clone(),
            })
            .await
            .expect("serve metadata piece");

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match metadata_manager_rx.recv().await {
                    Some(TorrentCommand::MetadataTorrent(torrent, metadata_length)) => {
                        assert_eq!(metadata_length, info_bytes.len() as i64);
                        assert_eq!(torrent.info_dict_bencode, info_bytes);
                        assert_eq!(torrent.info.name, "web_peer_fixture");
                        break;
                    }
                    Some(_) => {}
                    None => panic!("metadata-only session closed before parsing metadata"),
                }
            }
        })
        .await
        .expect("metadata transfer over WebRTC timed out");

        let _ = shutdown_tx.send(());
        for task in [metadata_task, seeder_task] {
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("peer session did not stop")
                .expect("peer session task panicked")
                .expect("peer session returned an error");
        }
    }

    #[tokio::test]
    async fn reader_eof_disconnects_peer_without_waiting_for_idle_timeout() {
        let (client_socket, mock_peer_socket) = duplex(1024);
        let (mut mock_peer_read, mut mock_peer_write) = split(mock_peer_socket);
        let infinite_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
        let (manager_tx, mut manager_rx) = mpsc::channel(16);
        let (_cmd_tx, cmd_rx) = mpsc::channel(16);
        let (shutdown_tx, _) = broadcast::channel(1);
        let peer_key = "closing-peer:1337";

        let session = PeerSession::new(PeerSessionParameters {
            info_hash: [0u8; 20].to_vec(),
            torrent_metadata_length: None,
            connection_type: ConnectionType::Outgoing,
            torrent_manager_rx: cmd_rx,
            torrent_manager_tx: manager_tx,
            peer_ip_port: peer_key.to_string(),
            client_id: b"-SS1000-CLOSETEST000".to_vec(),
            global_dl_bucket: infinite_bucket.clone(),
            global_ul_bucket: infinite_bucket,
            shutdown_tx,
            network_scope_id: None,
            session_cancel: watch::channel(false).1,
        });
        let session_task = tokio::spawn(session.run(client_socket, Vec::new(), None));

        let mut handshake = vec![0u8; 68];
        mock_peer_read
            .read_exact(&mut handshake)
            .await
            .expect("read outgoing handshake");
        handshake[25] &= !0x10;
        handshake[48..68].copy_from_slice(b"-SS1013-CLOSINGPEER0");
        mock_peer_write
            .write_all(&handshake)
            .await
            .expect("write valid handshake response");

        loop {
            match manager_rx.recv().await {
                Some(TorrentCommand::SuccessfullyConnected(peer)) => {
                    assert_eq!(peer, peer_key);
                    break;
                }
                Some(_) => continue,
                None => panic!("session closed its manager channel before connecting"),
            }
        }

        mock_peer_write
            .shutdown()
            .await
            .expect("close peer write side");
        tokio::time::timeout(Duration::from_secs(1), session_task)
            .await
            .expect("reader EOF should terminate the session immediately")
            .expect("session task should not panic")
            .expect("reader EOF should be a clean session close");

        assert!(matches!(
            manager_rx.recv().await,
            Some(TorrentCommand::Disconnect(peer)) if peer == peer_key
        ));
    }

    fn build_session_for_extended_message_tests() -> (PeerSession, mpsc::Receiver<TorrentCommand>) {
        let infinite_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
        let (manager_tx, manager_rx) = mpsc::channel(16);
        let (_cmd_tx, cmd_rx) = mpsc::channel(16);
        let (shutdown_tx, _) = broadcast::channel(1);

        let params = PeerSessionParameters {
            info_hash: [0u8; 20].to_vec(),
            torrent_metadata_length: None,
            connection_type: ConnectionType::Outgoing,
            torrent_manager_rx: cmd_rx,
            torrent_manager_tx: manager_tx,
            peer_ip_port: "extended-id-peer:1337".to_string(),
            client_id: b"-SS1000-EXTENDEDTEST".to_vec(),
            global_dl_bucket: infinite_bucket.clone(),
            global_ul_bucket: infinite_bucket,
            shutdown_tx,
            network_scope_id: None,
            session_cancel: watch::channel(false).1,
        };

        (PeerSession::new(params), manager_rx)
    }

    #[cfg(feature = "pex")]
    fn build_session_for_pex_tests(peer_ip_port: &str) -> PeerSession {
        let infinite_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
        let (manager_tx, _manager_rx) = mpsc::channel(16);
        let (_cmd_tx, cmd_rx) = mpsc::channel(16);
        let (shutdown_tx, _) = broadcast::channel(1);

        let params = PeerSessionParameters {
            info_hash: [0u8; 20].to_vec(),
            torrent_metadata_length: None,
            connection_type: ConnectionType::Outgoing,
            torrent_manager_rx: cmd_rx,
            torrent_manager_tx: manager_tx,
            peer_ip_port: peer_ip_port.to_string(),
            client_id: b"-SS1000-PEXTEST0000".to_vec(),
            global_dl_bucket: infinite_bucket.clone(),
            global_ul_bucket: infinite_bucket,
            shutdown_tx,
            network_scope_id: None,
            session_cancel: watch::channel(false).1,
        };

        PeerSession::new(params)
    }

    #[cfg(feature = "pex")]
    fn compact_ipv4_peer(octets: [u8; 4], port: u16) -> Vec<u8> {
        let mut encoded = Vec::from(octets);
        encoded.extend_from_slice(&port.to_be_bytes());
        encoded
    }

    #[test]
    #[cfg(feature = "pex")]
    fn peer_key_socket_addr_accepts_plain_and_transport_qualified_keys() {
        let plain: SocketAddr = "127.0.0.1:6881".parse().unwrap();

        assert_eq!(
            PeerSession::peer_key_socket_addr("127.0.0.1:6881"),
            Some(plain)
        );
        assert_eq!(
            PeerSession::peer_key_socket_addr("tcp://127.0.0.1:6881"),
            Some(plain)
        );
        assert_eq!(
            PeerSession::peer_key_socket_addr("utp://127.0.0.1:6881"),
            Some(plain)
        );
        assert_eq!(PeerSession::peer_key_socket_addr("not-a-peer"), None);
    }

    #[cfg(feature = "webtorrent")]
    #[tokio::test]
    async fn webtorrent_rejects_metadata_requests_until_metadata_is_available() {
        let (session, mut manager_rx) = build_session_for_extended_message_tests();
        let mut session = session.with_expected_peer_id([7; 20]);
        session
            .peer_extended_id_mappings
            .insert("ut_metadata".into(), 3);
        let payload = serde_bencode::to_bytes(&MetadataMessage {
            msg_type: 0,
            piece: 5,
            total_size: None,
        })
        .unwrap();
        session
            .handle_extended_message(ClientExtendedId::UtMetadata.id(), payload)
            .await
            .unwrap();
        let Message::Extended(id, response) =
            session.writer_rx.as_mut().unwrap().recv().await.unwrap()
        else {
            panic!("expected a metadata response")
        };
        assert_eq!(id, 3);
        let header: MetadataMessage = serde_bencode::from_bytes(&response).unwrap();
        assert_eq!(header.msg_type, 2);
        assert_eq!(header.piece, 5);
        assert!(manager_rx.try_recv().is_err());
    }

    #[cfg(feature = "webtorrent")]
    #[tokio::test]
    async fn webtorrent_rejects_unbounded_metadata_and_wrong_piece_indices() {
        for size in [-1, 0, 16 * 1024 * 1024 + 1] {
            let (session, _) = build_session_for_extended_message_tests();
            let mut session = session.with_expected_peer_id([7; 20]);
            let handshake = ExtendedHandshakePayload {
                m: HashMap::from([("ut_metadata".into(), 3)]),
                metadata_size: Some(size),
                lt_v2: None,
            };
            assert!(session
                .handle_extended_message(0, serde_bencode::to_bytes(&handshake).unwrap())
                .await
                .is_err());
        }
        let (session, mut manager_rx) = build_session_for_extended_message_tests();
        let mut session = session.with_expected_peer_id([7; 20]);
        session.peer_extended_handshake_payload = Some(ExtendedHandshakePayload {
            m: HashMap::new(),
            metadata_size: Some(1),
            lt_v2: None,
        });
        let mut payload = serde_bencode::to_bytes(&MetadataMessage {
            msg_type: 1,
            piece: 1,
            total_size: Some(1),
        })
        .unwrap();
        payload.push(b'e');
        assert!(session
            .handle_extended_message(ClientExtendedId::UtMetadata.id(), payload)
            .await
            .is_err());
        assert!(session.peer_torrent_metadata_pieces.is_empty());
        assert!(manager_rx.try_recv().is_err());
    }

    #[tokio::test]
    #[cfg(feature = "pex")]
    async fn handle_pex_advertises_transport_qualified_peer_keys() {
        let mut session = build_session_for_pex_tests("tcp://127.0.0.1:5000");
        session
            .peer_extended_id_mappings
            .insert(ClientExtendedId::UtPex.as_str().to_string(), 9);
        let mut writer_rx = session.writer_rx.take().expect("writer rx");

        session.handle_pex(vec![
            "tcp://127.0.0.1:5000".to_string(),
            "tcp://127.0.0.2:6000".to_string(),
            "127.0.0.3:6001".to_string(),
        ]);

        let Message::Extended(extension_id, payload) = writer_rx.recv().await.expect("pex message")
        else {
            panic!("expected extended pex message");
        };
        assert_eq!(extension_id, 9);

        let decoded: PexMessage = serde_bencode::from_bytes(&payload).expect("decode pex");
        assert_eq!(decoded.added.len(), 12);
        assert!(decoded
            .added
            .chunks_exact(6)
            .any(|chunk| chunk == compact_ipv4_peer([127, 0, 0, 2], 6000)));
        assert!(decoded
            .added
            .chunks_exact(6)
            .any(|chunk| chunk == compact_ipv4_peer([127, 0, 0, 3], 6001)));
    }

    struct WindowDriveHarness<'a> {
        client_cmd_tx: &'a mpsc::Sender<TorrentCommand>,
        manager_event_rx: &'a mut mpsc::Receiver<TorrentCommand>,
        window_event_rx: &'a mut mpsc::UnboundedReceiver<WindowAdaptationEvent>,
        request_id: u32,
        inflight: usize,
    }

    impl WindowDriveHarness<'_> {
        async fn drive_until(
            &mut self,
            step: Duration,
            max_steps: usize,
            predicate: impl Fn(WindowAdaptationEvent) -> bool,
        ) -> Option<WindowAdaptationEvent> {
            for _ in 0..max_steps {
                while self.inflight < 150 {
                    self.client_cmd_tx
                        .send(TorrentCommand::BulkRequest(vec![(
                            self.request_id,
                            0,
                            16384,
                        )]))
                        .await
                        .expect("failed to send bulk request");
                    self.request_id += 1;
                    self.inflight += 1;
                }

                tokio::task::yield_now().await;
                tokio::time::advance(step).await;
                tokio::task::yield_now().await;

                while let Ok(command) = self.manager_event_rx.try_recv() {
                    if matches!(command, TorrentCommand::Block(..)) && self.inflight > 0 {
                        self.inflight = self.inflight.saturating_sub(1);
                    }
                }

                while let Ok(event) = self.window_event_rx.try_recv() {
                    if predicate(event) {
                        return Some(event);
                    }
                }
            }

            None
        }
    }

    // --- Standard Handshake Helper ---
    async fn perform_handshake(network: &mut tokio::io::DuplexStream) {
        let mut handshake_buf = vec![0u8; 68];
        network.read_exact(&mut handshake_buf).await.unwrap();
        let mut response = vec![0u8; 68];
        response[0] = 19;
        response[1..20].copy_from_slice(b"BitTorrent protocol");
        response[20..28].copy_from_slice(&[0, 0, 0, 0, 0, 0x10, 0, 0]);
        network.write_all(&response).await.unwrap();
    }

    #[tokio::test]
    async fn metadata_request_uses_peer_advertised_extension_id() {
        let (mut session, _manager_rx) = build_session_for_extended_message_tests();
        let mut extensions = HashMap::new();
        extensions.insert(ClientExtendedId::UtMetadata.as_str().to_string(), 7);
        let handshake = ExtendedHandshakePayload {
            m: extensions,
            metadata_size: Some(1),
            lt_v2: None,
        };

        session
            .handle_extended_message(
                ClientExtendedId::Handshake.id(),
                serde_bencode::to_bytes(&handshake).unwrap(),
            )
            .await
            .unwrap();

        let outbound = session
            .writer_rx
            .as_mut()
            .unwrap()
            .recv()
            .await
            .expect("expected metadata request");

        match outbound {
            Message::Extended(7, payload) => {
                let request: MetadataMessage = serde_bencode::from_bytes(&payload).unwrap();
                assert_eq!(request.msg_type, 0);
                assert_eq!(request.piece, 0);
                assert_eq!(request.total_size, None);
            }
            other => panic!("expected metadata request on peer-advertised id, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn metadata_extension_id_zero_is_ignored() {
        let (mut session, mut manager_rx) = build_session_for_extended_message_tests();
        let mut extensions = HashMap::new();
        extensions.insert(ClientExtendedId::UtMetadata.as_str().to_string(), 0);
        let handshake = ExtendedHandshakePayload {
            m: extensions,
            metadata_size: Some(1),
            lt_v2: None,
        };

        session
            .handle_extended_message(
                ClientExtendedId::Handshake.id(),
                serde_bencode::to_bytes(&handshake).unwrap(),
            )
            .await
            .unwrap();

        assert!(session.writer_rx.as_mut().unwrap().try_recv().is_err());
        assert!(session.peer_torrent_metadata_pieces.is_empty());

        let metadata_header = MetadataMessage {
            msg_type: 1,
            piece: 0,
            total_size: Some(1),
        };
        let mut metadata_payload = serde_bencode::to_bytes(&metadata_header).unwrap();
        metadata_payload.push(b'x');

        session
            .handle_extended_message(ClientExtendedId::Handshake.id(), metadata_payload)
            .await
            .unwrap();

        assert!(session.peer_torrent_metadata_pieces.is_empty());
        assert!(manager_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn metadata_piece_on_local_extension_id_is_accepted() {
        let (mut session, mut manager_rx) = build_session_for_extended_message_tests();
        let info_bytes =
            b"d6:lengthi16384e4:name13:dup_meta_test12:piece lengthi16384e6:pieces20:00000000000000000000ee"
                .to_vec();
        let mut extensions = HashMap::new();
        extensions.insert(ClientExtendedId::UtMetadata.as_str().to_string(), 7);
        let handshake = ExtendedHandshakePayload {
            m: extensions,
            metadata_size: Some(info_bytes.len() as i64),
            lt_v2: None,
        };

        session
            .handle_extended_message(
                ClientExtendedId::Handshake.id(),
                serde_bencode::to_bytes(&handshake).unwrap(),
            )
            .await
            .unwrap();

        let _initial_request = session.writer_rx.as_mut().unwrap().recv().await;

        let metadata_header = MetadataMessage {
            msg_type: 1,
            piece: 0,
            total_size: Some(info_bytes.len()),
        };
        let mut metadata_payload = serde_bencode::to_bytes(&metadata_header).unwrap();
        metadata_payload.extend_from_slice(&info_bytes);

        session
            .handle_extended_message(7, metadata_payload.clone())
            .await
            .unwrap();
        assert!(manager_rx.try_recv().is_err());
        assert!(session.peer_torrent_metadata_pieces.is_empty());

        session
            .handle_extended_message(ClientExtendedId::UtMetadata.id(), metadata_payload)
            .await
            .unwrap();

        match manager_rx
            .recv()
            .await
            .expect("expected metadata torrent command")
        {
            TorrentCommand::MetadataTorrent(torrent, metadata_len) => {
                let Torrent { info, .. } = *torrent;
                assert_eq!(metadata_len, info_bytes.len() as i64);
                assert_eq!(info.name, "dup_meta_test");
                assert_eq!(info.piece_length, 16_384);
            }
            other => panic!("expected metadata torrent command, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_pipeline_saturation_with_virtual_time() {
        let (mut network, client_cmd_tx, _manager_event_rx, _) = spawn_test_session().await;

        // --- Step 1: Handshake ---
        let mut handshake_buf = vec![0u8; 68];
        network
            .read_exact(&mut handshake_buf)
            .await
            .expect("Failed to read client handshake");

        let mut response = vec![0u8; 68];
        response[0] = 19;
        response[1..20].copy_from_slice(b"BitTorrent protocol");
        response[20..28].copy_from_slice(&[0, 0, 0, 0, 0, 0x10, 0, 0]);
        network
            .write_all(&response)
            .await
            .expect("Failed to write handshake");

        // Consume Initial Messages (Bitfield, Extended Handshake, etc.)
        // We read until we stop getting messages for a short duration
        let start_drain = Instant::now();
        while start_drain.elapsed() < Duration::from_millis(500) {
            if let Ok(Ok(_)) = timeout(Duration::from_millis(50), parse_message(&mut network)).await
            {
                continue;
            } else {
                break; // No more immediate messages
            }
        }

        // --- Step 2: The Saturation Test ---
        // Send 5 requests in a single bulk command.
        let requests: Vec<_> = (0..5).map(|i| (0, i * 16384, 16384)).collect();
        client_cmd_tx
            .send(TorrentCommand::BulkRequest(requests))
            .await
            .expect("Failed to send bulk command");

        // ASSERTION: Immediate Burst
        let mut requests_received = HashSet::new();

        // Give 5 seconds for all async tasks to spawn and flush
        let overall_timeout = Duration::from_secs(5);
        let start = Instant::now();

        while requests_received.len() < 5 {
            if start.elapsed() > overall_timeout {
                break; // Stop loop, assert later
            }

            // Per-message timeout
            match timeout(Duration::from_secs(1), parse_message(&mut network)).await {
                Ok(Ok(Message::Request(idx, begin, len))) => {
                    assert_eq!(idx, 0);
                    assert_eq!(len, 16384);
                    requests_received.insert(begin);
                }
                Ok(Ok(_)) => {}      // Ignore KeepAlives or late Metadata messages
                Ok(Err(_)) => break, // Socket closed
                Err(_) => {}         // Timeout, keep retrying until overall_timeout
            }
        }

        assert_eq!(
            requests_received.len(),
            5,
            "Failed to receive all 5 requests in burst. Got: {:?}",
            requests_received
        );
    }

    #[tokio::test]
    async fn test_fragmented_pipeline_saturation() {
        let (mut network, client_cmd_tx, _manager_event_rx, _) = spawn_test_session().await;

        let mut handshake_buf = vec![0u8; 68];
        network.read_exact(&mut handshake_buf).await.unwrap();
        let mut response = vec![0u8; 68];
        response[0] = 19;
        response[1..20].copy_from_slice(b"BitTorrent protocol");
        response[20..28].copy_from_slice(&[0, 0, 0, 0, 0, 0x10, 0, 0]);
        network.write_all(&response).await.unwrap();

        // Drain setup
        let start_drain = Instant::now();
        while start_drain.elapsed() < Duration::from_millis(500) {
            if let Ok(Ok(_)) = timeout(Duration::from_millis(50), parse_message(&mut network)).await
            {
                continue;
            } else {
                break;
            }
        }

        // Send 5 separate commands for 5 separate pieces in a single bulk command
        let requests: Vec<_> = (0..5).map(|i| (i as u32, 0, 16384)).collect();
        client_cmd_tx
            .send(TorrentCommand::BulkRequest(requests))
            .await
            .expect("Failed to send bulk command");

        let mut requested_pieces = HashSet::new();
        let start = Instant::now();

        while requested_pieces.len() < 5 {
            if start.elapsed() > Duration::from_secs(5) {
                break;
            }

            if let Ok(Ok(Message::Request(idx, _, _))) =
                timeout(Duration::from_secs(1), parse_message(&mut network)).await
            {
                requested_pieces.insert(idx);
            }
        }

        assert_eq!(
            requested_pieces.len(),
            5,
            "Failed to receive all 5 fragmented requests. Got: {:?}",
            requested_pieces
        );
    }

    #[tokio::test]
    async fn test_requests_continue_after_cancels() {
        let (mut network, _client_cmd_tx, mut manager_rx, _) = spawn_test_session().await;

        perform_handshake(&mut network).await;

        let start_drain = Instant::now();
        while start_drain.elapsed() < Duration::from_millis(500) {
            match timeout(Duration::from_millis(50), manager_rx.recv()).await {
                Ok(Some(_)) => continue,
                _ => break,
            }
        }

        for i in 0..MAX_WINDOW {
            let request =
                generate_message(Message::Request(0, (i as u32) * 16_384, 16_384)).unwrap();
            network.write_all(&request).await.unwrap();
        }

        let mut forwarded_requests = 0;
        while forwarded_requests < MAX_WINDOW {
            match timeout(Duration::from_secs(1), manager_rx.recv()).await {
                Ok(Some(TorrentCommand::RequestUpload(_, piece_index, block_offset, length))) => {
                    assert_eq!(piece_index, 0);
                    assert_eq!(block_offset, (forwarded_requests as u32) * 16_384);
                    assert_eq!(length, 16_384);
                    forwarded_requests += 1;
                }
                Ok(Some(_)) => continue,
                Ok(None) => panic!("Session died while forwarding upload requests"),
                Err(_) => panic!(
                    "Timed out waiting for RequestUpload {}/{}",
                    forwarded_requests, MAX_WINDOW
                ),
            }
        }

        for i in 0..MAX_WINDOW {
            let cancel = generate_message(Message::Cancel(0, (i as u32) * 16_384, 16_384)).unwrap();
            network.write_all(&cancel).await.unwrap();
        }

        let mut forwarded_cancels = 0;
        while forwarded_cancels < MAX_WINDOW {
            match timeout(Duration::from_secs(1), manager_rx.recv()).await {
                Ok(Some(TorrentCommand::CancelUpload(_, piece_index, block_offset, length))) => {
                    assert_eq!(piece_index, 0);
                    assert_eq!(block_offset, (forwarded_cancels as u32) * 16_384);
                    assert_eq!(length, 16_384);
                    forwarded_cancels += 1;
                }
                Ok(Some(_)) => continue,
                Ok(None) => panic!("Session died while forwarding upload cancels"),
                Err(_) => panic!(
                    "Timed out waiting for CancelUpload {}/{}",
                    forwarded_cancels, MAX_WINDOW
                ),
            }
        }

        let fresh_request =
            generate_message(Message::Request(1, 0, 16_384)).expect("fresh request message");
        network.write_all(&fresh_request).await.unwrap();

        match timeout(Duration::from_millis(250), manager_rx.recv()).await {
            Ok(Some(TorrentCommand::RequestUpload(_, piece_index, block_offset, length))) => {
                assert_eq!(piece_index, 1);
                assert_eq!(block_offset, 0);
                assert_eq!(length, 16_384);
            }
            Ok(Some(other)) => panic!("Expected RequestUpload after cancels, got {:?}", other),
            Ok(None) => panic!("Session died before forwarding fresh request"),
            Err(_) => panic!("Fresh request was not forwarded after all cancels"),
        }
    }

    #[test]
    fn test_peer_flood_gate_resets_after_window_rollover() {
        let now = Instant::now();
        let mut gate = PeerFloodGate::new(now);

        assert_eq!(
            gate.check(now, PEER_FLOOD_DISCONNECT_BUDGET_PER_WINDOW),
            PeerFloodAction::Allow
        );
        assert_eq!(
            gate.check(now + PEER_FLOOD_WINDOW, 1),
            PeerFloodAction::Allow
        );
    }

    #[test]
    fn test_peer_flood_gate_disconnects_after_disconnect_budget() {
        let now = Instant::now();
        let mut gate = PeerFloodGate::new(now);

        assert_eq!(
            gate.check(now, PEER_FLOOD_DISCONNECT_BUDGET_PER_WINDOW),
            PeerFloodAction::Allow
        );
        assert_eq!(gate.check(now, 1), PeerFloodAction::DisconnectAndLog);
    }

    #[tokio::test]
    async fn test_performance_1000_blocks_sliding_window() {
        let (mut network, client_cmd_tx, mut manager_event_rx, _) = spawn_test_session().await;

        let mut handshake_buf = vec![0u8; 68];
        network
            .read_exact(&mut handshake_buf)
            .await
            .expect("Handshake read failed");

        let mut response = vec![0u8; 68];
        response[0] = 19;
        response[1..20].copy_from_slice(b"BitTorrent protocol");
        response[20..28].copy_from_slice(&[0, 0, 0, 0, 0, 0x10, 0, 0]);
        network
            .write_all(&response)
            .await
            .expect("Handshake write failed");

        let (mut peer_read, mut peer_write) = tokio::io::split(network);

        tokio::spawn(async move {
            let mut am_choking = true;

            while let Ok(Ok(msg)) =
                timeout(Duration::from_secs(5), parse_message(&mut peer_read)).await
            {
                match msg {
                    Message::Interested if am_choking => {
                        let unchoke = generate_message(Message::Unchoke).unwrap();
                        peer_write.write_all(&unchoke).await.unwrap();
                        am_choking = false;
                    }
                    Message::Request(index, begin, _len) if !am_choking => {
                        let data = vec![1u8; 16384];
                        let piece = generate_message(Message::Piece(index, begin, data)).unwrap();
                        if peer_write.write_all(&piece).await.is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });

        let mut session_ready = false;
        while !session_ready {
            match timeout(Duration::from_secs(1), manager_event_rx.recv()).await {
                Ok(Some(TorrentCommand::SuccessfullyConnected(_))) => session_ready = true,
                Ok(Some(TorrentCommand::PeerBitfield(_, _))) => session_ready = true,
                Ok(Some(_)) => continue,
                _ => panic!("Session failed to connect"),
            }
        }

        client_cmd_tx
            .send(TorrentCommand::ClientInterested)
            .await
            .unwrap();

        let mut is_unchoked = false;
        while !is_unchoked {
            if let Ok(Some(cmd)) = timeout(Duration::from_secs(1), manager_event_rx.recv()).await {
                if let TorrentCommand::Unchoke(_) = cmd {
                    is_unchoked = true;
                }
            } else {
                panic!("Peer never unchoked us!");
            }
        }

        const TOTAL_BLOCKS: u32 = 1000;
        const WINDOW_SIZE: u32 = 20;
        const BLOCK_SIZE: usize = 16384;

        let start_time = Instant::now();
        let mut blocks_requested = 0;
        let mut blocks_received = 0;

        // Fill window
        let requests: Vec<_> = (0..WINDOW_SIZE)
            .map(|i| (i, 0, BLOCK_SIZE as u32))
            .collect();
        client_cmd_tx
            .send(TorrentCommand::BulkRequest(requests))
            .await
            .unwrap();
        blocks_requested += WINDOW_SIZE;

        // Process loop
        while blocks_received < TOTAL_BLOCKS {
            match timeout(Duration::from_secs(5), manager_event_rx.recv()).await {
                Ok(Some(TorrentCommand::Block(..))) => {
                    blocks_received += 1;
                    if blocks_requested < TOTAL_BLOCKS {
                        client_cmd_tx
                            .send(TorrentCommand::BulkRequest(vec![(
                                blocks_requested,
                                0,
                                BLOCK_SIZE as u32,
                            )]))
                            .await
                            .unwrap();
                        blocks_requested += 1;
                    }
                }
                Ok(Some(_)) => continue,
                Ok(None) => panic!("Session died"),
                Err(_) => panic!("Stalled at {}/{}", blocks_received, TOTAL_BLOCKS),
            }
        }

        let elapsed = start_time.elapsed();
        let total_mb = (TOTAL_BLOCKS * BLOCK_SIZE as u32) as f64 / 1_000_000.0;
        println!(
            "Success: {:.2} MB in {:.2?} ({:.2} MB/s)",
            total_mb,
            elapsed,
            total_mb / elapsed.as_secs_f64()
        );
    }

    #[tokio::test]
    async fn test_bug_repro_unsolicited_forwarding() {
        let (mut network, _client_cmd_tx, mut manager_rx, _) = spawn_test_session().await;

        let mut handshake_buf = vec![0u8; 68];
        network.read_exact(&mut handshake_buf).await.unwrap();
        let mut response = vec![0u8; 68];
        response[0] = 19;
        response[1..20].copy_from_slice(b"BitTorrent protocol");
        response[20..28].copy_from_slice(&[0, 0, 0, 0, 0, 0x10, 0, 0]);
        network.write_all(&response).await.unwrap();

        // Drain setup messages on the network side
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(200) {
            if let Ok(Ok(_)) = timeout(Duration::from_millis(10), parse_message(&mut network)).await
            {
                continue;
            } else {
                break;
            }
        }

        // Piece 999 is definitely not in the session's tracker.
        let data = vec![0xAA; 16384];
        let piece_msg = generate_message(Message::Piece(999, 0, data)).unwrap();
        network.write_all(&piece_msg).await.unwrap();

        // We listen to the Manager channel for a fixed window.
        // We MUST loop because the Session sends 'PeerId', 'SuccessfullyConnected', etc.
        // first. If we only recv() once, we pop 'PeerId', ignore it, and exit early
        // (passing the test falsely).

        let listen_duration = Duration::from_millis(500);
        let start_listen = Instant::now();

        while start_listen.elapsed() < listen_duration {
            // Short timeout per recv to allow checking the total elapsed time
            match timeout(Duration::from_millis(50), manager_rx.recv()).await {
                Ok(Some(TorrentCommand::Block(peer_id, index, begin, _))) => {
                    panic!(
                        "TEST FAILED (BUG CONFIRMED): Session forwarded unsolicited block {}@{} from {}! \
                        It should have been dropped because it was not in the tracker.", 
                        index, begin, peer_id
                    );
                }
                Ok(Some(_cmd)) => {
                    // Continue loop, draining unrelated startup events (PeerId, Bitfield, etc.)
                    continue;
                }
                Ok(None) => panic!("Session died unexpectedly"),
                Err(_) => continue, // Timeout on individual recv, keep listening until total time is up
            }
        }

        println!("SUCCESS: Session filtered out the unsolicited block.");
    }

    async fn spawn_debug_session() -> (
        tokio::io::DuplexStream,
        mpsc::Sender<TorrentCommand>,
        mpsc::Receiver<TorrentCommand>,
        tokio::task::JoinHandle<()>, // <--- Return the handle
    ) {
        // Use a large buffer to prevent blocking
        let (client_socket, mock_peer_socket) = duplex(64 * 1024 * 1024);
        let infinite_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
        let (manager_tx, manager_rx) = mpsc::channel(1000);
        let (cmd_tx, cmd_rx) = mpsc::channel(1000);
        let (shutdown_tx, _) = broadcast::channel(1);

        let params = PeerSessionParameters {
            info_hash: [0u8; 20].to_vec(),
            torrent_metadata_length: None,
            connection_type: ConnectionType::Outgoing,
            torrent_manager_rx: cmd_rx,
            torrent_manager_tx: manager_tx,
            peer_ip_port: "virtual-peer:1337".to_string(),
            client_id: b"-SS1000-TESTTESTTEST".to_vec(),
            global_dl_bucket: infinite_bucket.clone(),
            global_ul_bucket: infinite_bucket.clone(),
            shutdown_tx,
            network_scope_id: None,
            session_cancel: watch::channel(false).1,
        };

        let handle = tokio::spawn(async move {
            let session = PeerSession::new(params);
            match session.run(client_socket, vec![], Some(vec![])).await {
                Ok(_) => println!("DEBUG: Session exited cleanly"),
                Err(e) => {
                    // This print is CRITICAL for seeing why it died
                    println!("DEBUG: Session CRASHED with error: {:?}", e);
                    // Force a panic here so the JoinHandle reports it as a panic to the test
                    panic!("Session crashed: {:?}", e);
                }
            }
        });

        (mock_peer_socket, cmd_tx, manager_rx, handle)
    }

    #[tokio::test]
    async fn test_heavy_load_20k_blocks_sliding_window() {
        const TOTAL_BLOCKS: u32 = 20_000;
        const PIPELINE_DEPTH: u32 = 128;
        const BLOCK_SIZE: usize = 16384;

        let (mut network, client_cmd_tx, mut manager_event_rx, session_handle) =
            spawn_debug_session().await;

        let mut handshake_buf = vec![0u8; 68];
        network
            .read_exact(&mut handshake_buf)
            .await
            .expect("Handshake read failed");
        let mut response = vec![0u8; 68];
        response[0] = 19;
        response[1..20].copy_from_slice(b"BitTorrent protocol");
        response[20..28].copy_from_slice(&[0, 0, 0, 0, 0, 0x10, 0, 0]);
        network
            .write_all(&response)
            .await
            .expect("Handshake write failed");

        let (mut peer_read, mut peer_write) = tokio::io::split(network);
        tokio::spawn(async move {
            let mut am_choking = true;
            let dummy_data = vec![0xAA; BLOCK_SIZE];
            while let Ok(Ok(msg)) =
                timeout(Duration::from_secs(30), parse_message(&mut peer_read)).await
            {
                match msg {
                    Message::Interested if am_choking => {
                        let unchoke = generate_message(Message::Unchoke).unwrap();
                        if peer_write.write_all(&unchoke).await.is_err() {
                            break;
                        }
                        am_choking = false;
                    }
                    Message::Request(index, begin, _len) if !am_choking => {
                        let piece_msg =
                            generate_message(Message::Piece(index, begin, dummy_data.clone()))
                                .unwrap();
                        if peer_write.write_all(&piece_msg).await.is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });

        // We add a check for the session handle here too, in case it dies during startup
        loop {
            tokio::select! {
                res = manager_event_rx.recv() => match res {
                    Some(TorrentCommand::SuccessfullyConnected(_)) => break,
                    Some(TorrentCommand::PeerBitfield(..)) => break,
                    Some(_) => continue,
                    None => {
                        println!("Session died during startup. checking handle...");
                        let _ = session_handle.await;
                        panic!("Session died during startup (Manager RX Closed)");
                    }
                },
                _ = tokio::time::sleep(Duration::from_secs(2)) => panic!("Timeout waiting for connect"),
            }
        }

        client_cmd_tx
            .send(TorrentCommand::ClientInterested)
            .await
            .unwrap();

        // Wait for Unchoke
        loop {
            tokio::select! {
                res = manager_event_rx.recv() => match res {
                    Some(TorrentCommand::Unchoke(_)) => break,
                    Some(_) => continue,
                    None => {
                        let _ = session_handle.await;
                        panic!("Session died waiting for Unchoke");
                    }
                },
                _ = tokio::time::sleep(Duration::from_secs(2)) => panic!("Timeout waiting for Unchoke"),
            }
        }

        println!("Starting transfer of {} blocks...", TOTAL_BLOCKS);
        tokio::task::yield_now().await;

        let start_time = Instant::now();
        let mut blocks_requested = 0;
        let mut blocks_received = 0;

        let initial_batch: Vec<_> = (0..PIPELINE_DEPTH)
            .map(|i| {
                blocks_requested += 1;
                (i, 0, BLOCK_SIZE as u32)
            })
            .collect();

        client_cmd_tx
            .send(TorrentCommand::BulkRequest(initial_batch))
            .await
            .expect("Failed to send initial batch");

        while blocks_received < TOTAL_BLOCKS {
            tokio::select! {
                res = manager_event_rx.recv() => match res {
                    Some(TorrentCommand::Block(..)) => {
                        blocks_received += 1;
                        if blocks_requested < TOTAL_BLOCKS {
                            let req = vec![(blocks_requested, 0, BLOCK_SIZE as u32)];
                            if client_cmd_tx.send(TorrentCommand::BulkRequest(req)).await.is_err() {
                                break; // Session dead
                            }
                            blocks_requested += 1;
                        }
                        if blocks_received % 5000 == 0 {
                            println!("Progress: {}/{}", blocks_received, TOTAL_BLOCKS);
                        }
                    },
                    Some(_) => continue,
                    None => {
                        println!("!!! SESSION DIED PREMATURELY - Awaiting Handle for Panic Info !!!");
                        // Await the handle to print the panic message from the spawned task
                        if let Err(e) = session_handle.await {
                            if e.is_panic() {
                                std::panic::resume_unwind(e.into_panic());
                            } else {
                                panic!("Session task cancelled or failed: {:?}", e);
                            }
                        }
                        panic!("Session closed manager channel but exited cleanly?");
                    }
                },
                _ = tokio::time::sleep(Duration::from_secs(10)) => {
                    panic!("Stalled: No blocks received for 10s");
                }
            }
        }

        // Assert success
        assert_eq!(blocks_received, TOTAL_BLOCKS);
        let elapsed = start_time.elapsed();
        let mb = (TOTAL_BLOCKS as f64 * BLOCK_SIZE as f64) / 1024.0 / 1024.0;
        println!(
            "DONE: {:.2} MB in {:.2?} ({:.2} MB/s)",
            mb,
            elapsed,
            mb / elapsed.as_secs_f64()
        );
    }

    // TEST 1: ROCKET (Growth to Max)

    #[tokio::test(start_paused = true)]
    async fn test_dynamic_window_growth_to_max() {
        let (mut network, client_cmd_tx, mut manager_event_rx, window_monitor, mut window_event_rx) =
            spawn_test_session_with_window_events().await;
        perform_handshake(&mut network).await;

        let (mut peer_read, mut peer_write) = tokio::io::split(network);
        tokio::spawn(async move {
            let dummy_data = vec![0xAA; 16384];
            while let Ok(Ok(msg)) =
                timeout(Duration::from_secs(30), parse_message(&mut peer_read)).await
            {
                match msg {
                    Message::Interested => {
                        let _ = peer_write
                            .write_all(&generate_message(Message::Unchoke).unwrap())
                            .await;
                    }
                    Message::Request(i, b, _) => {
                        tokio::time::sleep(Duration::from_millis(2)).await;
                        let piece =
                            generate_message(Message::Piece(i, b, dummy_data.clone())).unwrap();
                        let _ = peer_write.write_all(&piece).await;
                    }
                    _ => {}
                }
            }
        });

        client_cmd_tx
            .send(TorrentCommand::ClientInterested)
            .await
            .expect("failed to send interested command");

        for _ in 0..20 {
            tokio::task::yield_now().await;
            if let Ok(TorrentCommand::Unchoke(_)) = manager_event_rx.try_recv() {
                break;
            }
            tokio::time::advance(Duration::from_millis(100)).await;
        }

        let mut drive = WindowDriveHarness {
            client_cmd_tx: &client_cmd_tx,
            manager_event_rx: &mut manager_event_rx,
            window_event_rx: &mut window_event_rx,
            request_id: 0,
            inflight: 0,
        };
        let growth_event = drive
            .drive_until(Duration::from_millis(100), 120, |event| {
                matches!(event, WindowAdaptationEvent::Grew { .. })
            })
            .await;

        match growth_event {
            Some(WindowAdaptationEvent::Grew { .. }) => {}
            _ => panic!(
                "Window never grew under paused-time load (observed={}, base={})",
                window_monitor.load(Ordering::Relaxed),
                PEER_BLOCK_IN_FLIGHT_LIMIT
            ),
        }

        let _ = drive
            .drive_until(Duration::from_millis(100), 20, |_| false)
            .await;

        let final_window = window_monitor.load(Ordering::Relaxed);
        println!("Rocket Test: Final Window Size = {}", final_window);

        assert!(
            final_window > PEER_BLOCK_IN_FLIGHT_LIMIT,
            "Window should have grown (Current: {}, Start: {})",
            final_window,
            PEER_BLOCK_IN_FLIGHT_LIMIT
        );
    }

    // TEST 2: CONGESTION (Increase then Decrease)

    #[tokio::test(start_paused = true)]
    async fn test_dynamic_window_congestion_control() {
        let (mut network, client_cmd_tx, mut manager_event_rx, window_monitor, mut window_event_rx) =
            spawn_test_session_with_window_events().await;
        perform_handshake(&mut network).await;

        let is_congested = Arc::new(AtomicBool::new(false));
        let is_congested_clone = is_congested.clone();

        let (mut peer_read, mut peer_write) = tokio::io::split(network);
        tokio::spawn(async move {
            let dummy_data = vec![0xAA; 16384];
            let start_time = Instant::now();
            while let Ok(Ok(msg)) =
                timeout(Duration::from_secs(30), parse_message(&mut peer_read)).await
            {
                match msg {
                    Message::Interested => {
                        let _ = peer_write
                            .write_all(&generate_message(Message::Unchoke).unwrap())
                            .await;
                    }
                    Message::Request(i, b, _) => {
                        if is_congested_clone.load(Ordering::Relaxed) {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        } else if start_time.elapsed() < Duration::from_secs(2) {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        } else {
                            tokio::time::sleep(Duration::from_millis(2)).await;
                        }

                        let piece =
                            generate_message(Message::Piece(i, b, dummy_data.clone())).unwrap();
                        let _ = peer_write.write_all(&piece).await;
                    }
                    _ => {}
                }
            }
        });

        client_cmd_tx
            .send(TorrentCommand::ClientInterested)
            .await
            .expect("failed to send interested command");

        for _ in 0..20 {
            tokio::task::yield_now().await;
            if let Ok(TorrentCommand::Unchoke(_)) = manager_event_rx.try_recv() {
                break;
            }
            tokio::time::advance(Duration::from_millis(100)).await;
        }

        let mut drive = WindowDriveHarness {
            client_cmd_tx: &client_cmd_tx,
            manager_event_rx: &mut manager_event_rx,
            window_event_rx: &mut window_event_rx,
            request_id: 0,
            inflight: 0,
        };
        let growth_event = drive
            .drive_until(Duration::from_millis(100), 120, |event| {
                matches!(event, WindowAdaptationEvent::Grew { .. })
            })
            .await;

        match growth_event {
            Some(WindowAdaptationEvent::Grew { .. }) => {}
            _ => panic!(
                "Window never grew under paused-time load (observed={}, base={})",
                window_monitor.load(Ordering::Relaxed),
                PEER_BLOCK_IN_FLIGHT_LIMIT
            ),
        }

        let _ = drive
            .drive_until(Duration::from_millis(100), 20, |_| false)
            .await;

        let peak_window = window_monitor.load(Ordering::Relaxed);
        while drive.window_event_rx.try_recv().is_ok() {}

        println!("Phase 1 Peak Window: {}", peak_window);
        assert!(
            peak_window > PEER_BLOCK_IN_FLIGHT_LIMIT,
            "Window failed to grow (peak={}, base={})",
            peak_window,
            PEER_BLOCK_IN_FLIGHT_LIMIT
        );

        is_congested.store(true, Ordering::Relaxed);

        let shrink_event = drive
            .drive_until(Duration::from_millis(100), 200, |event| {
                matches!(event, WindowAdaptationEvent::Shrunk { new_size } if new_size < peak_window)
            })
            .await;

        let final_window = match shrink_event {
            Some(WindowAdaptationEvent::Shrunk { new_size }) => new_size,
            _ => panic!(
                "Window never shrank after congestion under paused time (observed={}, peak={})",
                window_monitor.load(Ordering::Relaxed),
                peak_window
            ),
        };

        println!("Phase 2 Final Window: {}", final_window);
        assert!(
            final_window < peak_window,
            "Window failed to shrink on congestion (Peak: {}, Final: {})",
            peak_window,
            final_window
        );
    }

    // TEST 3: SUSTAIN (Steady State)

    #[tokio::test]
    async fn test_dynamic_window_steady_state() {
        let (mut network, client_cmd_tx, mut manager_event_rx, window_monitor) =
            spawn_test_session().await;
        perform_handshake(&mut network).await;

        // Mock Peer: Fixed Rate (10ms delay)
        let (mut peer_read, mut peer_write) = tokio::io::split(network);
        tokio::spawn(async move {
            let dummy_data = vec![0xAA; 16384];
            while let Ok(Ok(msg)) =
                timeout(Duration::from_secs(30), parse_message(&mut peer_read)).await
            {
                match msg {
                    Message::Interested => {
                        let _ = peer_write
                            .write_all(&generate_message(Message::Unchoke).unwrap())
                            .await;
                    }
                    Message::Request(i, b, _) => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        let piece =
                            generate_message(Message::Piece(i, b, dummy_data.clone())).unwrap();
                        let _ = peer_write.write_all(&piece).await;
                    }
                    _ => {}
                }
            }
        });

        let _ = client_cmd_tx.send(TorrentCommand::ClientInterested).await;
        loop {
            if let Ok(Some(TorrentCommand::Unchoke(_))) =
                timeout(Duration::from_secs(1), manager_event_rx.recv()).await
            {
                break;
            }
        }

        // Run for a longer duration to check stability
        let mut completed = 0;
        let mut inflight = 0;

        // Process ~400 blocks (should take ~4 seconds minimum purely by delay, likely more)
        while completed < 400 {
            // Keep pipe full
            while inflight < 100 {
                let _ = client_cmd_tx
                    .send(TorrentCommand::BulkRequest(vec![(
                        completed + inflight,
                        0,
                        16384,
                    )]))
                    .await;
                inflight += 1;
            }

            if let Some(TorrentCommand::Block(..)) = manager_event_rx.recv().await {
                completed += 1;
                if inflight > 0 {
                    inflight = inflight.saturating_sub(1);
                }
            }
        }
        let final_window = window_monitor.load(Ordering::Relaxed);
        println!("Steady State Window: {}", final_window);

        assert!(
            final_window >= PEER_BLOCK_IN_FLIGHT_LIMIT,
            "Window collapsed unexpectedly"
        );
        assert!(final_window < 255, "Window overflowed");
    }

    #[tokio::test(start_paused = true)]
    async fn test_dynamic_window_reset_on_choke() {
        let (mut network, client_cmd_tx, mut manager_event_rx, window_monitor, mut window_event_rx) =
            spawn_test_session_with_window_events().await;
        perform_handshake(&mut network).await;

        let should_choke = Arc::new(AtomicBool::new(false));
        let should_choke_clone = should_choke.clone();

        let (mut peer_read, mut peer_write) = tokio::io::split(network);
        tokio::spawn(async move {
            let mut am_choking = true;
            let dummy_data = vec![0xAA; 16384];
            let start_time = Instant::now();

            while let Ok(Ok(msg)) =
                timeout(Duration::from_secs(30), parse_message(&mut peer_read)).await
            {
                if should_choke_clone.load(Ordering::Relaxed) && !am_choking {
                    let choke_msg = generate_message(Message::Choke).unwrap();
                    let _ = peer_write.write_all(&choke_msg).await;
                    tokio::time::sleep(Duration::from_millis(500)).await;

                    let unchoke_msg = generate_message(Message::Unchoke).unwrap();
                    let _ = peer_write.write_all(&unchoke_msg).await;
                    am_choking = false;
                    should_choke_clone.store(false, Ordering::Relaxed);
                }

                match msg {
                    Message::Interested if am_choking => {
                        let unchoke = generate_message(Message::Unchoke).unwrap();
                        let _ = peer_write.write_all(&unchoke).await;
                        am_choking = false;
                    }
                    Message::Request(i, b, _) if !am_choking => {
                        if start_time.elapsed() < Duration::from_secs(2) {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        } else {
                            tokio::time::sleep(Duration::from_millis(2)).await;
                        }

                        let piece =
                            generate_message(Message::Piece(i, b, dummy_data.clone())).unwrap();
                        let _ = peer_write.write_all(&piece).await;
                    }
                    _ => {}
                }
            }
        });

        client_cmd_tx
            .send(TorrentCommand::ClientInterested)
            .await
            .expect("failed to send interested command");

        for _ in 0..20 {
            tokio::task::yield_now().await;
            if let Ok(TorrentCommand::Unchoke(_)) = manager_event_rx.try_recv() {
                break;
            }
            tokio::time::advance(Duration::from_millis(100)).await;
        }

        let mut drive = WindowDriveHarness {
            client_cmd_tx: &client_cmd_tx,
            manager_event_rx: &mut manager_event_rx,
            window_event_rx: &mut window_event_rx,
            request_id: 0,
            inflight: 0,
        };

        let growth_event = drive
            .drive_until(Duration::from_millis(100), 120, |event| {
                matches!(event, WindowAdaptationEvent::Grew { .. })
            })
            .await;

        match growth_event {
            Some(WindowAdaptationEvent::Grew { new_size }) => {
                println!("Peak Window before Choke: {}", new_size);
                assert!(
                    new_size > PEER_BLOCK_IN_FLIGHT_LIMIT,
                    "Window did not grow enough to test reset (Got {}, want > {})",
                    new_size,
                    PEER_BLOCK_IN_FLIGHT_LIMIT
                );
            }
            _ => panic!(
                "Window never grew before choke under paused time (observed={}, base={})",
                window_monitor.load(Ordering::Relaxed),
                PEER_BLOCK_IN_FLIGHT_LIMIT
            ),
        }

        while drive.window_event_rx.try_recv().is_ok() {}

        should_choke.store(true, Ordering::Relaxed);

        let reset_event = drive
            .drive_until(Duration::from_millis(100), 40, |event| {
                matches!(
                    event,
                    WindowAdaptationEvent::Reset {
                        new_size: PEER_BLOCK_IN_FLIGHT_LIMIT,
                    }
                )
            })
            .await;

        match reset_event {
            Some(WindowAdaptationEvent::Reset { new_size }) => {
                println!("Window after Choke: {}", new_size);
                assert_eq!(
                    new_size, PEER_BLOCK_IN_FLIGHT_LIMIT,
                    "Window failed to reset to default on Choke!"
                );
            }
            _ => panic!(
                "Window never reset on choke under paused time (observed={}, base={})",
                window_monitor.load(Ordering::Relaxed),
                PEER_BLOCK_IN_FLIGHT_LIMIT
            ),
        }
    }
}
