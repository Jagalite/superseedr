// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fmt;

use crate::torrent_file::Torrent;

use crate::tracker::TrackerResponse;

use crate::networking::BlockInfo;

#[derive(Debug, PartialEq, Clone)]
pub enum TorrentCommand {
    SuccessfullyConnected(String),
    PeerId(String, Vec<u8>),

    Choke(String),
    Unchoke(String),
    PeerUnchoke,
    PeerChoke,

    Block(String, u32, u32, Vec<u8>),
    PieceAcquired(u32),
    Have(String, u32),

    NotInterested,

    ClientInterested,
    PeerInterested(String),

    ClientBitfield(Vec<u8>, Option<i64>),
    PeerBitfield(String, Vec<u8>),

    RequestDownload(u32, i64, i64),

    RequestUpload(String, u32, u32, u32),
    Upload(u32, u32, Vec<u8>),

    CancelUpload(String, u32, u32, u32),
    Cancel(u32),

    Disconnect(String),

    AddPexPeers(String, Vec<(String, u16)>),
    SendPexPeers(Vec<String>),

    DhtTorrent(Torrent, i64),

    AnnounceResponse(String, TrackerResponse),
    AnnounceFailed(String, String),

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

    UnresponsivePeer(String),
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
            TorrentCommand::ClientBitfield(bitfield, torrent_metadata_length) => {
                write!(
                    f,
                    "CLIENT_BITFIELD(bitfield: {}, len: {:?})",
                    bitfield.len(),
                    torrent_metadata_length
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
