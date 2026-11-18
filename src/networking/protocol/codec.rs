// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Transport codec with owned message payloads. The public generate/parse helpers
//! remain independently implemented for existing callers and migration differential
//! tests. Wire-format changes must update both paths and their compatibility tests.

use super::*;
use bytes::{Buf, BufMut, BytesMut};
use std::{
    collections::HashMap,
    io::{self, Error, ErrorKind},
};
use strum::IntoEnumIterator;
use tokio_util::codec::{Decoder, Encoder};
#[derive(Default)]
pub struct PeerCodec;
impl Encoder<Message> for PeerCodec {
    type Error = io::Error;
    fn encode(&mut self, message: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        encode_message(message, dst)
            .map_err(|e| io::Error::new(ErrorKind::InvalidData, e.to_string()))
    }
}
impl Decoder for PeerCodec {
    type Item = Message;
    type Error = io::Error;
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Message>, io::Error> {
        if src.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes(src[..4].try_into().unwrap()) as usize;
        let total = len
            .checked_add(4)
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "Frame length overflow"))?;
        if src.len() < total {
            return Ok(None);
        }
        let message = if len == 0 {
            Message::KeepAlive
        } else {
            decode_payload(src[4], &src[5..total])?
        };
        src.advance(total);
        Ok(Some(message))
    }
}
fn decode_payload(message_id: u8, payload: &[u8]) -> Result<Message, io::Error> {
    match message_id {
        0 => Ok(Message::Choke),
        1 => Ok(Message::Unchoke),
        2 => Ok(Message::Interested),
        3 => Ok(Message::NotInterested),
        4 => {
            // Have
            if payload.len() != 4 {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "Invalid payload size for Have",
                ));
            }
            let idx_bytes: [u8; 4] = payload.try_into().unwrap();
            Ok(Message::Have(u32::from_be_bytes(idx_bytes)))
        }
        5 => {
            // Bitfield
            Ok(Message::Bitfield(payload.to_vec()))
        }
        6 => {
            // Request
            if payload.len() != 12 {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "Invalid payload size for Request",
                ));
            }
            let (i, rest) = payload.split_at(4);
            let (b, l) = rest.split_at(4);
            Ok(Message::Request(
                u32::from_be_bytes(i.try_into().unwrap()),
                u32::from_be_bytes(b.try_into().unwrap()),
                u32::from_be_bytes(l.try_into().unwrap()),
            ))
        }
        7 => {
            // Piece
            if payload.len() < 8 {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "Invalid payload size for Piece",
                ));
            }
            let (i, rest) = payload.split_at(4);
            let (b, data) = rest.split_at(4);
            Ok(Message::Piece(
                u32::from_be_bytes(i.try_into().unwrap()),
                u32::from_be_bytes(b.try_into().unwrap()),
                data.to_vec(),
            ))
        }
        8 => {
            // Cancel
            if payload.len() != 12 {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "Invalid payload size for Cancel",
                ));
            }
            let (i, rest) = payload.split_at(4);
            let (b, l) = rest.split_at(4);
            Ok(Message::Cancel(
                u32::from_be_bytes(i.try_into().unwrap()),
                u32::from_be_bytes(b.try_into().unwrap()),
                u32::from_be_bytes(l.try_into().unwrap()),
            ))
        }
        9 => {
            // Port
            if payload.len() != 4 {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "Invalid payload size for Port",
                ));
            }
            let port_bytes: [u8; 4] = payload.try_into().unwrap();
            Ok(Message::Port(u32::from_be_bytes(port_bytes)))
        }
        20 => {
            // Extended
            if payload.is_empty() {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "Empty payload for Extended message",
                ));
            }
            let extended_id = payload[0];
            let extended_payload = payload[1..].to_vec();
            Ok(Message::Extended(extended_id, extended_payload))
        }
        21 => {
            if payload.len() != 48 {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    format!("Invalid HashRequest length: {}", payload.len()),
                ));
            }
            let root = payload[0..32].to_vec(); // Read Root
            let base = read_be_u32(payload, 32)?;
            let offset = read_be_u32(payload, 36)?;
            let length = read_be_u32(payload, 40)?;
            let proof_layers = read_be_u32(payload, 44)?;
            Ok(Message::HashRequest(
                root,
                base,
                offset,
                length,
                proof_layers,
            ))
        }
        22 => {
            if payload.len() < 40 {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "Invalid HashPiece length",
                ));
            }
            let root = payload[0..32].to_vec();
            let base = read_be_u32(payload, 32)?;
            let offset = read_be_u32(payload, 36)?;

            let mut data = payload[40..].to_vec();

            if !data.is_empty() && !data.len().is_multiple_of(32) {
                let remainder = data.len() % 32;
                if remainder == 4 {
                    // Likely [Count: 4] [Hashes...]
                    data = data[4..].to_vec();
                } else if remainder == 8 {
                    // Likely [Length: 4] [Count: 4] [Hashes...]
                    data = data[8..].to_vec();
                }
            }

            Ok(Message::HashPiece(root, base, offset, data))
        }
        23 => {
            if payload.len() != 48 {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    format!("Invalid HashReject length: {}", payload.len()),
                ));
            }
            let root = payload[0..32].to_vec();
            let base = read_be_u32(payload, 32)?;
            let offset = read_be_u32(payload, 36)?;
            let length = read_be_u32(payload, 40)?;
            let proof_layers = read_be_u32(payload, 44)?; // Read extra field

            Ok(Message::HashReject(
                root,
                base,
                offset,
                length,
                proof_layers,
            ))
        }
        _ => {
            // Unknown ID
            let msg = format!("Unknown message ID: {}", message_id);
            Err(Error::new(ErrorKind::InvalidData, msg))
        }
    }
}

fn read_be_u32(slice: &[u8], offset: usize) -> Result<u32, std::io::Error> {
    if offset + 4 > slice.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Payload too short",
        ));
    }
    // We strictly use try_into() to grab exactly 4 bytes
    let bytes: [u8; 4] = slice[offset..offset + 4].try_into().unwrap();
    Ok(u32::from_be_bytes(bytes))
}

fn encode_message(message: Message, dst: &mut BytesMut) -> Result<(), MessageGenerationError> {
    match message {
        Message::Handshake(info_hash, client_id) => {
            let protocol_str = "BitTorrent protocol";
            let pstrlen = [19u8];
            let mut reserved = [0u8; 8];
            reserved[5] |= 0x10;

            dst.extend_from_slice(&pstrlen);
            dst.extend_from_slice(protocol_str.as_bytes());
            dst.extend_from_slice(&reserved);
            dst.extend_from_slice(&info_hash);
            dst.extend_from_slice(&client_id);

            Ok(())
        }
        Message::KeepAlive => {
            dst.extend_from_slice(&[0, 0, 0, 0]);
            Ok(())
        }
        Message::Choke => {
            dst.extend_from_slice(&[0, 0, 0, 1, 0]);
            Ok(())
        }
        Message::Unchoke => {
            dst.extend_from_slice(&[0, 0, 0, 1, 1]);
            Ok(())
        }
        Message::Interested => {
            dst.extend_from_slice(&[0, 0, 0, 1, 2]);
            Ok(())
        }
        Message::NotInterested => {
            dst.extend_from_slice(&[0, 0, 0, 1, 3]);
            Ok(())
        }
        Message::Have(index) => {
            dst.extend_from_slice(&[0, 0, 0, 5]);
            dst.extend_from_slice(&[4]);
            dst.extend_from_slice(&index.to_be_bytes());
            Ok(())
        }
        Message::HaveBatch(indices) => {
            for index in indices {
                dst.extend_from_slice(&[0, 0, 0, 5, 4]);
                dst.extend_from_slice(&index.to_be_bytes());
            }
            Ok(())
        }
        Message::Bitfield(bitfield) => {
            let message_len: u32 = (1 + bitfield.len())
                .try_into()
                .map_err(|_| MessageGenerationError::PayloadTooLarge("Bitfield".to_string()))?;
            dst.extend_from_slice(&message_len.to_be_bytes());
            dst.extend_from_slice(&[5]);
            dst.extend_from_slice(&bitfield);
            Ok(())
        }
        Message::Request(index, begin, length) => {
            dst.extend_from_slice(&[0, 0, 0, 13]);
            dst.extend_from_slice(&[6]);
            dst.extend_from_slice(&index.to_be_bytes());
            dst.extend_from_slice(&begin.to_be_bytes());
            dst.extend_from_slice(&length.to_be_bytes());
            Ok(())
        }
        Message::Piece(index, begin, block) => {
            let message_len: u32 = (9 + block.len())
                .try_into()
                .map_err(|_| MessageGenerationError::PayloadTooLarge("Piece".to_string()))?;
            dst.extend_from_slice(&message_len.to_be_bytes());
            dst.extend_from_slice(&[7]);
            dst.extend_from_slice(&index.to_be_bytes());
            dst.extend_from_slice(&begin.to_be_bytes());
            dst.extend_from_slice(&block);
            Ok(())
        }
        Message::Cancel(index, begin, length) => {
            dst.extend_from_slice(&[0, 0, 0, 13]);
            dst.extend_from_slice(&[8]);
            dst.extend_from_slice(&index.to_be_bytes());
            dst.extend_from_slice(&begin.to_be_bytes());
            dst.extend_from_slice(&length.to_be_bytes());
            Ok(())
        }
        Message::Port(port) => {
            dst.extend_from_slice(&[0, 0, 0, 5]);
            dst.extend_from_slice(&[9]);
            dst.extend_from_slice(&port.to_be_bytes());
            Ok(())
        }
        Message::ExtendedHandshake(metadata_size) => {
            let m: HashMap<String, u8> = ClientExtendedId::iter()
                .filter(|&variant| variant != ClientExtendedId::Handshake) // Exclude the handshake ID
                .map(|variant| (variant.as_str().to_string(), variant.id()))
                .collect();
            let payload = ExtendedHandshakePayload {
                m,
                metadata_size,
                lt_v2: Some(1),
            };
            let bencoded_payload =
                serde_bencode::to_bytes(&payload).map_err(MessageGenerationError::BencodeError)?;

            let message_len: u32 = (2 + bencoded_payload.len()) as u32;
            dst.extend_from_slice(&message_len.to_be_bytes());
            dst.put_u8(20);
            dst.put_u8(ClientExtendedId::Handshake.id());
            dst.extend_from_slice(&bencoded_payload);
            Ok(())
        }
        Message::Extended(extended_id, payload) => {
            let message_len: u32 = (2 + payload.len()) as u32;
            dst.extend_from_slice(&message_len.to_be_bytes());
            dst.put_u8(20);
            dst.put_u8(extended_id);
            dst.extend_from_slice(&payload);
            Ok(())
        }

        Message::HashRequest(root, base, offset, length, proof_layers) => {
            // 49 bytes: ID + root (32) + base + offset + length + proof_layers
            let payload_len: u32 = 49;
            dst.extend_from_slice(&payload_len.to_be_bytes());

            dst.put_u8(21); // HashRequest ID
            dst.extend_from_slice(&root); // 32 bytes
            dst.extend_from_slice(&base.to_be_bytes());
            dst.extend_from_slice(&offset.to_be_bytes());
            dst.extend_from_slice(&length.to_be_bytes());
            dst.extend_from_slice(&proof_layers.to_be_bytes());

            Ok(())
        }

        Message::HashPiece(root, base, offset, data) => {
            // Length: 1 (ID) + 32 (Root) + 8 (2 * u32) + Data
            let len = 1 + 32 + 4 + 4 + data.len();
            dst.extend_from_slice(&(len as u32).to_be_bytes());
            dst.put_u8(22);
            dst.extend_from_slice(&root); // Write 32-byte Root
            dst.extend_from_slice(&base.to_be_bytes());
            dst.extend_from_slice(&offset.to_be_bytes());
            dst.extend_from_slice(&data);
            Ok(())
        }
        Message::HashReject(root, base, offset, length, proof_layers) => {
            // Length: 1 (ID) + 32 (Root) + 16 (4 * u32) = 49 bytes
            let len = 1 + 32 + 4 + 4 + 4 + 4;
            dst.extend_from_slice(&(len as u32).to_be_bytes());
            dst.put_u8(23);
            dst.extend_from_slice(&root); // Write 32-byte Root
            dst.extend_from_slice(&base.to_be_bytes());
            dst.extend_from_slice(&offset.to_be_bytes());
            dst.extend_from_slice(&length.to_be_bytes());
            dst.extend_from_slice(&proof_layers.to_be_bytes());
            Ok(())
        }
    }
}
