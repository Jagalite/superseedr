// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fmt;
use std::net::SocketAddr;

use crate::torrent_file::Torrent;

use crate::tracker::TrackerResponse;

use crate::networking::transport::PeerTransportKind;
use crate::networking::{BlockInfo, NetworkScopeId, Scoped};
use tokio::sync::mpsc::Sender;
use tokio::sync::watch;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum TorrentCommand {
    RegisterPeer {
        peer_id: String,
        peer_addr: SocketAddr,
        tx: Sender<TorrentCommand>,
        registration_result_tx: Sender<Option<watch::Receiver<bool>>>,
    },
    RegisterPeerTransport {
        peer_id: String,
        peer_addr: SocketAddr,
        tx: Sender<TorrentCommand>,
        transport: PeerTransportKind,
        scope_id: NetworkScopeId,
        registration_result_tx: Sender<Option<watch::Receiver<bool>>>,
    },
    SuccessfullyConnected(String),
    PeerId(String, Vec<u8>),
    PeerTransportSelected {
        peer_id: String,
        transport: PeerTransportKind,
    },

    Choke(String),
    Unchoke(String),
    PeerUnchoke,
    PeerChoke,

    Block(String, u32, u32, Vec<u8>),
    Have(String, u32),

    NotInterested,

    ClientInterested,
    PeerInterested(String),

    PeerBitfield(String, Vec<u8>),

    BulkRequest(Vec<(u32, u32, u32)>),
    BulkCancel(Vec<(u32, u32, u32)>),

    RequestUpload(String, u32, u32, u32),
    Upload(u32, u32, Vec<u8>),

    CancelUpload(String, u32, u32, u32),

    Disconnect(String),

    DisconnectGeneration {
        peer_id: String,
        scope_id: NetworkScopeId,
    },

    WebSeedDisconnected {
        peer_id: String,
        scope_id: NetworkScopeId,
    },

    #[cfg(feature = "pex")]
    AddPexPeers(String, Vec<SocketAddr>),

    #[cfg(feature = "pex")]
    SendPexPeers(Vec<String>),

    MetadataTorrent(Box<Torrent>, i64),

    Network(Scoped<NetworkResult>),

    MerkleHashData {
        peer_id: String,
        root: Vec<u8>,
        piece_index: u32,
        base_layer: u32,
        length: u32,
        proof: Vec<u8>,
    },

    #[allow(dead_code)]
    RequestHash {
        piece_index: u32,
        base_layer: u32,
        length: u32,
        proof_layers: u32,
    },

    GetHashes {
        peer_id: String,
        file_root: Vec<u8>,
        base_layer: u32,
        index: u32,
        length: u32,
        proof_layers: u32,
    },

    SendHashPiece {
        peer_id: String,
        root: Vec<u8>,
        base_layer: u32,
        index: u32,
        proof: Vec<u8>,
    },

    SendHashReject {
        peer_id: String,
        root: Vec<u8>,
        base_layer: u32,
        index: u32,
        length: u32,
    },

    PieceVerified {
        piece_index: u32,
        peer_id: String,
        verification_result: Result<Vec<u8>, ()>,
    },

    UploadTaskCompleted {
        peer_id: String,
        block_info: BlockInfo,
    },

    PieceWrittenToDisk {
        peer_id: String,
        piece_index: u32,
    },
    PieceWriteFailed {
        piece_index: u32,
    },

    ValidationComplete(Vec<u32>),

    BlockSent {
        peer_id: String,
        bytes: u64,
    },

    SetDataAvailability(bool),

    ValidationProgress(u32),

    FatalStorageError(String),
}

#[derive(Debug, Clone)]
pub enum NetworkResult {
    AnnounceResponse {
        url: String,
        response: TrackerResponse,
    },
    AnnounceFailed {
        url: String,
        error: String,
    },
    CompletionAnnounceFinished {
        url: String,
        error: Option<String>,
    },
    UnresponsivePeer(String),
    RecoveryReady,
}

pub struct TorrentCommandSummary<'a>(pub &'a TorrentCommand);
impl fmt::Debug for TorrentCommandSummary<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            TorrentCommand::Block(_peer_id, index, begin, data) => {
                write!(
                    f,
                    "PIECE(index: {}, begin: {}, len: {})",
                    index,
                    begin,
                    data.len()
                )
            }
            TorrentCommand::PeerBitfield(peer_id, bitfield) => {
                write!(
                    f,
                    "PEER_BITFIELD(peer: {}, len: {})",
                    peer_id,
                    bitfield.len()
                )
            }

            TorrentCommand::Upload(index, begin, data) => {
                write!(
                    f,
                    "PIECE(index: {}, begin: {}, len: {})",
                    index,
                    begin,
                    data.len()
                )
            }

            other => write!(f, "{:?}", other), // Fallback to default Debug for the rest
        }
    }
}
