// SPDX-License-Identifier: GPL-3.0-or-later
//! BEP 9 handling for RTC sessions; the manager alone publishes verified availability.
use super::*;
const PIECE: usize = 16 * 1024;
const MAX_METADATA: usize = 16 * 1024 * 1024;
#[derive(serde::Deserialize)]
struct Header {
    msg_type: i64,
    piece: Option<usize>,
    total_size: Option<usize>,
}
impl PeerSession {
    pub(super) async fn rtc_metadata(
        &mut self,
        payload: Vec<u8>,
    ) -> Result<(), Box<dyn StdError + Send + Sync>> {
        let mut cursor = std::io::Cursor::new(&payload[..payload.len().min(1024)]);
        let header: Header =
            serde::Deserialize::deserialize(&mut serde_bencode::Deserializer::new(&mut cursor))?;
        if !(0..=2).contains(&header.msg_type) {
            return Ok(());
        }
        let piece = header.piece.ok_or("metadata message has no piece index")?;
        let consumed = cursor.position() as usize;
        match header.msg_type {
            0 => {
                if consumed != payload.len() {
                    return Err("metadata request has trailing bytes".into());
                }
                if self.rtc_metadata_pending >= 16 {
                    return Err("metadata request pipeline exceeded".into());
                }
                self.rtc_metadata_pending += 1;
                self.torrent_manager_tx
                    .send(TorrentCommand::MetadataRequest {
                        peer_id: self.peer_ip_port.clone(),
                        piece,
                    })
                    .await?;
            }
            2 => {
                return Err("metadata request rejected by peer".into());
            }
            1 => {
                if self.peer_session_established {
                    return Ok(());
                }
                let total = self
                    .peer_extended_handshake_payload
                    .as_ref()
                    .and_then(|handshake| handshake.metadata_size)
                    .and_then(|size| usize::try_from(size).ok())
                    .filter(|&size| size > 0 && size <= MAX_METADATA)
                    .ok_or("metadata size unavailable or outside bounds")?;
                let start = piece.checked_mul(PIECE).ok_or("metadata offset overflow")?;
                let expected = total.saturating_sub(start).min(PIECE);
                if header.total_size != Some(total)
                    || piece != self.peer_torrent_metadata_piece_count
                    || expected == 0
                    || payload.len() - consumed != expected
                    || start != self.peer_torrent_metadata_pieces.len()
                {
                    return Err("metadata fragment does not match the outstanding request".into());
                }
                self.peer_torrent_metadata_pieces
                    .extend_from_slice(&payload[consumed..]);
                if self.peer_torrent_metadata_pieces.len() == total {
                    let torrent = crate::torrent_file::parser::from_info_bytes(
                        &self.peer_torrent_metadata_pieces,
                    )?;
                    self.torrent_manager_tx
                        .send(TorrentCommand::MetadataTorrent(
                            Box::new(torrent),
                            total as i64,
                        ))
                        .await?;
                } else {
                    self.peer_torrent_metadata_piece_count += 1;
                    let request = MetadataMessage {
                        msg_type: 0,
                        piece: self.peer_torrent_metadata_piece_count,
                        total_size: None,
                    };
                    self.rtc_send_metadata(serde_bencode::to_bytes(&request)?)?;
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    }
    pub(super) fn rtc_send_metadata(
        &self,
        bytes: Vec<u8>,
    ) -> Result<(), Box<dyn StdError + Send + Sync>> {
        let id = self
            .peer_advertised_extension_id(ClientExtendedId::UtMetadata)
            .ok_or("peer disabled metadata extension")?;
        self.writer_tx.try_send(Message::Extended(id, bytes))?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_types_remain_parseable_without_data_fields() {
        let header: Header = serde_bencode::from_bytes(b"d8:msg_typei987ee").unwrap();
        assert!(!(0..=2).contains(&header.msg_type));
        assert_eq!(header.piece, None);
    }
}
