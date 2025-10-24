// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::torrent_file::Info;
use crate::torrent_file::Torrent;

use super::protocol::{
    calculate_blocks_for_piece, parse_message, writer_task, BlockInfo, ClientExtendedId,
    ExtendedHandshakePayload, Message, MessageSummary, MetadataMessage, PexMessage,
};

use crate::token_bucket::consume_tokens;
use crate::token_bucket::TokenBucket;

use crate::command::TorrentCommand;

use tokio::sync::Mutex;

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::collections::HashSet;
use std::error::Error as StdError;
use std::net::Ipv4Addr;
use std::sync::Arc;

use tokio::io::AsyncReadExt;

use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::oneshot;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio::time::Duration;
use tokio::time::Instant;

use tracing::{event, instrument, Level};

const PEER_BLOCK_IN_FLIGHT_LIMIT: usize = 5;

struct DisconnectGuard {
    peer_ip_port: String,
    manager_tx: Sender<TorrentCommand>,
}

impl Drop for DisconnectGuard {
    fn drop(&mut self) {
        let manager_tx = self.manager_tx.clone();
        let peer_ip_port = self.peer_ip_port.clone();

        tokio::spawn(async move {
            let _ = manager_tx
                .try_send(TorrentCommand::Disconnect(peer_ip_port));
        });
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
    pub global_dl_bucket: Arc<Mutex<TokenBucket>>,
    pub global_ul_bucket: Arc<Mutex<TokenBucket>>,
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

    writer_rx: Receiver<Message>,
    writer_tx: Sender<Message>,

    block_tracker: HashMap<u32, HashSet<BlockInfo>>,
    block_request_buffer: Vec<u8>,
    block_request_limit_semaphore: Arc<Semaphore>,
    block_request_joinset: JoinSet<()>,
    block_requests_remaining: usize,
    block_upload_limit_semaphore: Arc<Semaphore>,

    peer_extended_id_mappings: HashMap<String, u8>,
    peer_extended_handshake_payload: Option<ExtendedHandshakePayload>,
    peer_torrent_metadata_piece_count: usize,
    peer_torrent_metadata_pieces: Vec<u8>,

    global_dl_bucket: Arc<Mutex<TokenBucket>>,
    global_ul_bucket: Arc<Mutex<TokenBucket>>,
}

impl PeerSession {
    pub fn new(params: PeerSessionParameters) -> Self {
        let (writer_tx, writer_rx) = mpsc::channel::<Message>(100);

        Self {
            info_hash: params.info_hash,
            peer_session_established: false,
            torrent_metadata_length: params.torrent_metadata_length,
            connection_type: params.connection_type,
            torrent_manager_rx: params.torrent_manager_rx,
            torrent_manager_tx: params.torrent_manager_tx,
            client_id: params.client_id,
            peer_ip_port: params.peer_ip_port,
            writer_rx,
            writer_tx,
            block_tracker: HashMap::new(),
            block_request_buffer: Vec::new(),
            block_request_limit_semaphore: Arc::new(Semaphore::new(PEER_BLOCK_IN_FLIGHT_LIMIT)),
            block_request_joinset: JoinSet::new(),
            block_requests_remaining: 0,
            block_upload_limit_semaphore: Arc::new(Semaphore::new(PEER_BLOCK_IN_FLIGHT_LIMIT)),
            peer_extended_id_mappings: HashMap::new(),
            peer_extended_handshake_payload: None,
            peer_torrent_metadata_piece_count: 0,
            peer_torrent_metadata_pieces: Vec::new(),
            global_dl_bucket: params.global_dl_bucket,
            global_ul_bucket: params.global_ul_bucket,
        }
    }

    #[instrument(skip(self, current_bitfield))]
    pub async fn run(
        mut self,
        stream: TcpStream,
        handshake_response: Vec<u8>,
        current_bitfield: Option<Vec<u8>>,
    ) -> Result<(), Box<dyn StdError + Send + Sync>> {
        let _guard = DisconnectGuard {
            peer_ip_port: self.peer_ip_port.clone(),
            manager_tx: self.torrent_manager_tx.clone(),
        };

        let (mut stream_read_half, stream_write_half) = stream.into_split();
        let (error_tx, mut error_rx) = oneshot::channel();

        let global_ul_bucket_clone = self.global_ul_bucket.clone();
        let writer_handle = tokio::spawn(writer_task(
            stream_write_half,
            self.writer_rx,
            error_tx,
            global_ul_bucket_clone,
        ));

        let handshake_response = match self.connection_type {
            ConnectionType::Outgoing => {
                let _ = self
                    .writer_tx
                    .send(Message::Handshake(
                        self.info_hash.clone(),
                        self.client_id.clone(),
                    ))
                    .await;

                let mut buffer = vec![0u8; 68];
                stream_read_half.read_exact(&mut buffer).await?;
                buffer
            }
            ConnectionType::Incoming => {
                let _ = self
                    .writer_tx
                    .send(Message::Handshake(
                        self.info_hash.clone(),
                        self.client_id.clone(),
                    ))
                    .await;
                handshake_response
            }
        };

        // TODO: Remove duplicate processing
        let peer_info_hash = &handshake_response[28..48];
        if self.info_hash != peer_info_hash {
            event!(
                Level::DEBUG,
                "Our hash: {:?} - peer hash: {:?}",
                self.info_hash,
                peer_info_hash
            );
            return Err("Info hash mismatch with peer".into());
        }

        let peer_id = handshake_response[48..68].to_vec();
        self.torrent_manager_tx
            .send(TorrentCommand::PeerId(self.peer_ip_port.clone(), peer_id))
            .await?;

        let reserved_bytes = &handshake_response[20..28];
        const EXTENSION_FLAG: u8 = 0x10;
        let peer_supports_extended = (reserved_bytes[5] & EXTENSION_FLAG) != 0;
        if peer_supports_extended {
            let mut torrent_metadata_len = None;
            if let Some(torrent_metadata_length) = self.torrent_metadata_length {
                torrent_metadata_len = Some(torrent_metadata_length);
            }
            self.writer_tx
                .send(Message::ExtendedHandshake(torrent_metadata_len))
                .await?;
        }

        if let Some(bitfield) = current_bitfield {
            self.peer_session_established = true;
            let _ = self
                .writer_tx
                .send(Message::Bitfield(bitfield.clone()))
                .await;
            let _ = self
                .torrent_manager_tx
                .send(TorrentCommand::SuccessfullyConnected(
                    self.peer_ip_port.clone(),
                ))
                .await;
        }

        let mut keep_alive_timer = tokio::time::interval(Duration::from_secs(60));

        let inactivity_timeout = tokio::time::sleep(Duration::from_secs(120));
        tokio::pin!(inactivity_timeout);

        let _result: Result<(), Box<dyn StdError + Send + Sync>> = 'session: loop {
            const READ_TIMEOUT: Duration = Duration::from_secs(2);

            tokio::select! {
                _ = &mut inactivity_timeout => {
                    event!(Level::DEBUG, "Peer timed out due to inactivity. Disconnecting.");
                    break 'session Err("Peer connection timed out".into());
                },

                _ = keep_alive_timer.tick() => {
                    let writer_tx_clone = self.writer_tx.clone();
                    tokio::spawn(async move {
                        writer_tx_clone
                            .try_send(Message::KeepAlive) 
                    });
                    event!(Level::TRACE, "Sent periodic Keep-Alive.");
                },

                Ok(message_from_peer) = timeout(READ_TIMEOUT, parse_message(&mut stream_read_half)) => {
                    if let Ok(ref message) = message_from_peer {
                        inactivity_timeout.as_mut().reset(Instant::now() + Duration::from_secs(120));
                        match message {
                            Message::KeepAlive => {
                                event!(Level::TRACE, ?message);
                            }
                            _ => {
                                event!(Level::TRACE, ?message);
                                event!(Level::DEBUG, message_summary = ?MessageSummary(message));
                            }
                        }
                    }

                    match message_from_peer {
                        Ok(Message::Bitfield(value)) => {
                            let peer_ip_port_clone = self.peer_ip_port.clone();
                            let torrent_manager_tx_clone = self.torrent_manager_tx.clone();
                            tokio::spawn(async move {
                            let _ = torrent_manager_tx_clone
                                .try_send(TorrentCommand::PeerBitfield(peer_ip_port_clone, value));
                            });
                        }
                        Ok(Message::NotInterested) => {}
                        Ok(Message::Interested) => {
                            let peer_ip_port_clone = self.peer_ip_port.clone();
                            let torrent_manager_tx_clone = self.torrent_manager_tx.clone();
                            tokio::spawn(async move {
                                let _ =
                                    torrent_manager_tx_clone
                                    .try_send(TorrentCommand::PeerInterested(peer_ip_port_clone));
                            });
                        }
                        Ok(Message::Choke) => {
                            let peer_ip_port_clone = self.peer_ip_port.clone();
                            let torrent_manager_tx_clone = self.torrent_manager_tx.clone();
                            tokio::spawn(async move {
                                let _ =
                                    torrent_manager_tx_clone
                                    .try_send(TorrentCommand::Choke(peer_ip_port_clone));
                            });
                        }
                        Ok(Message::Unchoke) => {
                            let peer_ip_port_clone = self.peer_ip_port.clone();
                            let torrent_manager_tx_clone = self.torrent_manager_tx.clone();
                            tokio::spawn(async move {
                                let _ =
                                    torrent_manager_tx_clone
                                    .try_send(TorrentCommand::Unchoke(peer_ip_port_clone));
                            });
                        },
                        Ok(Message::Piece(piece_index, block_offset , block_data)) => {
                            consume_tokens(&self.global_dl_bucket, block_data.len() as f64).await;
                            self.block_request_limit_semaphore.add_permits(1);

                            let received_block = BlockInfo {
                                piece_index,
                                offset: block_offset,
                                length: block_data.len() as u32,
                            };

                            if let Entry::Occupied(mut entry) = self.block_tracker.entry(piece_index) {
                                let blocks_for_piece = entry.get_mut();
                                blocks_for_piece.remove(&received_block);
                                if blocks_for_piece.is_empty() {
                                    entry.remove();
                                }
                            }

                            let peer_ip_port_clone = self.peer_ip_port.clone();
                            let torrent_manager_tx_clone = self.torrent_manager_tx.clone();
                            let _block_request_buffer_clone = self.block_request_buffer.clone();
                            self.block_request_joinset.spawn(async move {
                                let _ = torrent_manager_tx_clone
                                    .send(TorrentCommand::Block(peer_ip_port_clone, piece_index, block_offset, block_data))
                                    .await;
                            });
                        },
                        Ok(Message::Request(piece_index, block_offset, block_length)) => {
                            let peer_ip_port_clone = self.peer_ip_port.clone();
                            let torrent_manager_tx_clone = self.torrent_manager_tx.clone();
                            tokio::spawn(async move {
                                let _ = torrent_manager_tx_clone
                                    .try_send(TorrentCommand::RequestUpload(peer_ip_port_clone, piece_index, block_offset, block_length));
                            });
                        },
                        Ok(Message::Cancel(piece_index, block_offset, block_length)) => {
                            let peer_ip_port_clone = self.peer_ip_port.clone();
                            let torrent_manager_tx_clone = self.torrent_manager_tx.clone();
                            tokio::spawn(async move {
                                let _ = torrent_manager_tx_clone
                                    .try_send(TorrentCommand::CancelUpload(peer_ip_port_clone, piece_index, block_offset, block_length));
                            });
                        },
                        Ok(Message::Have(piece_index)) => {
                            let peer_ip_port_clone = self.peer_ip_port.clone();
                            let torrent_manager_tx_clone = self.torrent_manager_tx.clone();
                            tokio::spawn(async move {
                                let _ = torrent_manager_tx_clone
                                    .try_send(TorrentCommand::Have(peer_ip_port_clone, piece_index));
                            });
                        }
                        Ok(Message::Extended(extended_id, payload)) => {

                            // TODO: Config file - make PEX optional
                            if extended_id == ClientExtendedId::Handshake.id() {
                                if let Ok(handshake_data) = serde_bencode::from_bytes::<ExtendedHandshakePayload>(&payload) {

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
                                                match serde_bencode::to_bytes(&request) {
                                                    Ok(payload_bytes) => {
                                                        let _ = self.writer_tx.send(
                                                            Message::Extended(ClientExtendedId::UtMetadata.id(), payload_bytes)
                                                        ).await;
                                                    }
                                                    Err(e) => {
                                                        event!(Level::ERROR, "Failed to serialize metadata request: {}", e);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            if extended_id == ClientExtendedId::UtPex.id() {
                                if let Ok(pex_data) = serde_bencode::from_bytes::<PexMessage>(&payload) {
                                    let mut new_peers = Vec::new();
                                    for chunk in pex_data.added.chunks_exact(6) {
                                        let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
                                        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
                                        new_peers.push((ip.to_string(), port));
                                    }
                                    if !new_peers.is_empty() {
                                        let peer_ip_port_clone = self.peer_ip_port.clone();
                                        let torrent_manager_tx_clone = self.torrent_manager_tx.clone();
                                        tokio::spawn(async move {
                                            let _ = torrent_manager_tx_clone
                                                .try_send(TorrentCommand::AddPexPeers(peer_ip_port_clone, new_peers));
                                        });
                                    }
                                }
                            }
                            if extended_id == ClientExtendedId::UtMetadata.id()
                                && !self.peer_session_established {
                                    if let Some(ref handshake_data) = self.peer_extended_handshake_payload {
                                        if let Some(torrent_metadata_len) = handshake_data.metadata_size {
                                            let torrent_metadata_len_usize = torrent_metadata_len as usize;

                                            let current_offset = self.peer_torrent_metadata_piece_count * 16384;
                                            let expected_data_len = std::cmp::min(16384, torrent_metadata_len_usize - current_offset);
                                            let header_len = payload.len() - expected_data_len;
                                            let metadata_binary = &payload[header_len..];
                                            self.peer_torrent_metadata_pieces.extend(metadata_binary);

                                            if torrent_metadata_len_usize == self.peer_torrent_metadata_pieces.len() {

                                                let dht_info_result: Result<Info, _> = serde_bencode::from_bytes(&self.peer_torrent_metadata_pieces[..]);

                                                match dht_info_result {
                                                    Ok(dht_info) => {
                                                        let torrent_manager_tx_clone = self.torrent_manager_tx.clone();
                                                        let torrent_metadata_length_clone = torrent_metadata_len;
                                                        let peer_torrent_metadata_pieces_clone = self.peer_torrent_metadata_pieces.clone();
                                                        tokio::spawn(async move {
                                                            let _ = torrent_manager_tx_clone
                                                                .try_send(TorrentCommand::DhtTorrent(
                                                                    Torrent {
                                                                        info_dict_bencode: peer_torrent_metadata_pieces_clone,
                                                                        info: dht_info,
                                                                        announce: None,
                                                                        announce_list: None,
                                                                        creation_date: None,
                                                                        comment: None,
                                                                        created_by: None,
                                                                        encoding: None
                                                                    },
                                                                    torrent_metadata_length_clone
                                                                ));
                                                        });
                                                    }
                                                    Err(e) => {
                                                        event!(Level::WARN, "Failed to decode torrent metadata from peer: {}", e);
                                                        return Err("Peer sent invalid torrent metadata".into());
                                                    }
                                                }

                                            } else {
                                                self.peer_torrent_metadata_piece_count += 1;
                                                let request = MetadataMessage {
                                                    msg_type: 0,
                                                    piece: self.peer_torrent_metadata_piece_count,
                                                    total_size: None,
                                                };
                                                match serde_bencode::to_bytes(&request) {
                                                    Ok(payload_bytes) => {
                                                        let _ = self.writer_tx.send(
                                                            Message::Extended(ClientExtendedId::UtMetadata.id(), payload_bytes)
                                                        ).await;
                                                    }
                                                    Err(e) => {
                                                        event!(Level::ERROR, "Failed to serialize metadata request: {}", e);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                        }
                        Ok(Message::KeepAlive) => {
                            event!(Level::TRACE, "PEER KEEP SENT KEEP ALIVE.");
                        }
                        Ok(Message::Port(_)) => {}
                        Err(e) => {
                            break 'session Err(e.into());
                        }
                        _ => {
                            event!(Level::WARN, "UNIMPLEMENTED MESSAGE {:?}", message_from_peer);
                        },
                    }
                },

                Some(command) = self.torrent_manager_rx.recv() => {
                    event!(Level::TRACE, ?command);
                    match command {
                        TorrentCommand::ClientBitfield(bitfield, torrent_metadata_length) => {
                            self.peer_session_established = true;
                            self.torrent_metadata_length = torrent_metadata_length;

                            let peer_ip_port_clone = self.peer_ip_port.clone();
                            let torrent_manager_tx_clone = self.torrent_manager_tx.clone();
                            let writer_tx_clone = self.writer_tx.clone();
                            let bitfield_clone = bitfield.clone();
                            tokio::spawn(async move {
                                let _ = writer_tx_clone.try_send(Message::Bitfield(bitfield_clone));
                                let _ = torrent_manager_tx_clone
                                    .try_send(TorrentCommand::SuccessfullyConnected(peer_ip_port_clone));
                            });
                        }
                        TorrentCommand::SendPexPeers(peers_list) => {
                            if let Some(pex_id) = self.peer_extended_id_mappings.get(ClientExtendedId::UtPex.as_str()).copied() {
                                let pex_list_for_this_peer: Vec<u8> = peers_list.iter()
                                    .filter(|&peer_ip| *peer_ip != self.peer_ip_port)
                                    .filter_map(|ip_port| ip_port.parse::<std::net::SocketAddr>().ok())
                                    .filter_map(|addr| {
                                        if let std::net::SocketAddr::V4(v4_addr) = addr {
                                            let mut peer_bytes = Vec::with_capacity(6);
                                            peer_bytes.extend_from_slice(&v4_addr.ip().octets());
                                            peer_bytes.extend_from_slice(&v4_addr.port().to_be_bytes());
                                            Some(peer_bytes)
                                        } else {
                                            None
                                        }
                                    })
                                    .flatten()
                                    .collect();

                                if pex_list_for_this_peer.is_empty() {
                                    continue;
                                }

                                let pex_message = PexMessage {
                                    added: pex_list_for_this_peer,
                                    ..Default::default()
                                };

                                if let Ok(bencoded_payload) = serde_bencode::to_bytes(&pex_message) {
                                    let writer_tx_clone = self.writer_tx.clone();
                                    tokio::spawn(async move {
                                        let _ = writer_tx_clone.try_send(
                                            Message::Extended(pex_id, bencoded_payload)
                                        );
                                    });
                                }
                            }

                        }
                        TorrentCommand::PeerUnchoke => {
                            let writer_tx_clone = self.writer_tx.clone();
                            tokio::spawn(async move {
                                writer_tx_clone
                                    .try_send(Message::Unchoke)
                            });
                        }
                        TorrentCommand::PeerChoke => {
                            let writer_tx_clone = self.writer_tx.clone();
                            tokio::spawn(async move {
                                writer_tx_clone
                                    .try_send(Message::Choke)
                            });
                        }
                        TorrentCommand::Disconnect(_) => {
                            break 'session Err("DISCONNECTING PEER".into());
                        }
                        TorrentCommand::ClientInterested => {
                            let writer_tx_clone = self.writer_tx.clone();
                            tokio::spawn(async move {
                                writer_tx_clone
                                    .try_send(Message::Interested)
                            });
                        }
                        TorrentCommand::NotInterested => {
                            let writer_tx_clone = self.writer_tx.clone();
                            tokio::spawn(async move {
                                writer_tx_clone
                                    .try_send(Message::NotInterested)
                            });
                        }
                        TorrentCommand::Cancel(piece_index) => {
                            if let Some(blocks) = self.block_tracker.remove(&piece_index) {
                                for block in blocks {
                                    if self.block_request_limit_semaphore.available_permits() < PEER_BLOCK_IN_FLIGHT_LIMIT {
                                        self.block_request_limit_semaphore.add_permits(1);
                                    }

                                    let writer_tx_clone = self.writer_tx.clone();
                                    tokio::spawn(async move {
                                        writer_tx_clone
                                            .try_send(Message::Cancel(
                                                block.piece_index,
                                                block.offset,
                                                block.length
                                            ))
                                    });
                                }
                            }

                            // TODO: Wrap this joinset and blockinfo into one struct and drop both
                            self.block_request_joinset = JoinSet::new();

                        }
                        TorrentCommand::PieceAcquired(piece_index) => {
                            let writer_tx_clone = self.writer_tx.clone();
                            tokio::spawn(async move {
                                writer_tx_clone
                                    .try_send(Message::Have(piece_index))
                            });
                        }
                        TorrentCommand::RequestDownload(piece_index, piece_length, torrent_size) => {
                            let piece_start = piece_index as u64 * piece_length as u64;
                            let remaining_bytes = (torrent_size as u64).saturating_sub(piece_start);
                            let piece_size = std::cmp::min(piece_length as u64, remaining_bytes) as u32;

                            self.block_request_buffer = vec![0; piece_size as usize];

                            let blocks = calculate_blocks_for_piece(
                                piece_index,
                                piece_size
                            );
                            self.block_tracker.insert(piece_index, blocks.clone());
                            self.block_requests_remaining = blocks.len();
                            for block in blocks.into_iter() {
                                let writer_tx_clone = self.writer_tx.clone();
                                let semaphore_clone = self.block_request_limit_semaphore.clone();
                                tokio::spawn(async move {
                                    let permit = match semaphore_clone.acquire().await {
                                        Ok(p) => p,
                                        Err(_) => return,
                                    };
                                    let send_result = writer_tx_clone
                                        .try_send(Message::Request(
                                            block.piece_index,
                                            block.offset,
                                            block.length,
                                        ));
                                    if send_result.is_ok() {
                                        permit.forget(); //more permits in tokio::select! - PIECE
                                    }
                                });
                            }
                        }
                        TorrentCommand::Upload(piece_index, block_offset, block_data) => {
                            let writer_tx_clone = self.writer_tx.clone();
                            let _semaphore_clone = self.block_upload_limit_semaphore.clone();
                            tokio::spawn(async move {
                                let _ = writer_tx_clone
                                    .try_send(Message::Piece(
                                        piece_index,
                                        block_offset,
                                        block_data,
                                    ));
                            });
                        }
                        _ => {
                            event!(Level::WARN, "UNIMPLEMENTED TORRENT COMMAND {:?}", command);
                        }
                    }
                }

                writer_error = &mut error_rx => {
                    match writer_error {
                        Ok(err) => {
                            break 'session Err(err);
                        },
                        Err(_) => {
                            break 'session Err("Writer task panicked or channel closed unexpectedly.".into());
                        }
                    }
                },

                else => {
                    event!(Level::INFO, "ALL CHANNELS CLOSED FOR PEER");
                    break 'session Err("ALL CHANNELS CLOSED".into());
                }
            }
        };

        // Cleanly shutdown the peers write stream to reclaim file descriptors.
        writer_handle.abort();

        Ok(())
    }
}
