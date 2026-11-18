// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

mod codec;
#[cfg(test)]
mod codec_tests;
use bytes::BytesMut;
use codec::PeerCodec;
use tokio_util::codec::{Decoder, Encoder};

// A full block plus framing fits without growth; active batches still reach 256 KiB.
const BUFFER_FLOOR: usize = 17 * 1024;
const BUFFER_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

/// Keep two windows of peak demand, with headroom and hysteresis to avoid churn.
#[derive(Default)]
struct BufferRetention {
    current: usize,
    previous: usize,
}
impl BufferRetention {
    fn record(&mut self, demand: usize) {
        self.current = self.current.max(demand);
    }
    fn observe(&mut self, capacity: usize, pending: usize) -> Option<usize> {
        let peak = self.current.max(self.previous).max(pending);
        self.previous = self.current;
        self.current = 0;
        let target = BUFFER_FLOOR.max(peak.saturating_mul(2));
        (capacity > target.saturating_mul(2)).then_some(target)
    }
}

use crate::token_bucket::consume_tokens;
use crate::token_bucket::TokenBucket;

use std::collections::HashMap;
use std::error::Error as StdError;
use std::io::{Error, ErrorKind};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Receiver;
use tokio::sync::oneshot;

use serde::{Deserialize, Serialize};

use std::fmt;
use tracing::{event, Level};

use strum::IntoEnumIterator;
use strum_macros::EnumIter;

#[derive(Debug)]
pub enum MessageGenerationError {
    PayloadTooLarge(String),
    BencodeError(serde_bencode::Error),
}
impl std::error::Error for MessageGenerationError {}
impl fmt::Display for MessageGenerationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MessageGenerationError::PayloadTooLarge(s) => write!(f, "Payload too large: {}", s),
            MessageGenerationError::BencodeError(e) => write!(f, "Bencode error: {}", e),
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, EnumIter)]
pub enum ClientExtendedId {
    Handshake = 0,
    #[cfg(feature = "pex")]
    UtPex = 1,
    UtMetadata = 2,
}
impl ClientExtendedId {
    /// Returns the integer ID for the extension message.
    pub fn id(&self) -> u8 {
        *self as u8
    }

    /// Returns the string name for the extension message.
    pub fn as_str(&self) -> &'static str {
        match self {
            ClientExtendedId::Handshake => "handshake",
            #[cfg(feature = "pex")]
            ClientExtendedId::UtPex => "ut_pex",
            ClientExtendedId::UtMetadata => "ut_metadata",
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Default)]
#[cfg(feature = "pex")]
pub struct PexMessage {
    #[serde(with = "serde_bytes", default)]
    pub added: Vec<u8>,
    #[serde(rename = "added.f", with = "serde_bytes", default)]
    pub added_f: Vec<u8>,
    #[serde(rename = "added6", with = "serde_bytes", default)]
    pub added6: Vec<u8>,
    #[serde(rename = "added6.f", with = "serde_bytes", default)]
    pub added6_f: Vec<u8>,
    #[serde(with = "serde_bytes", default)]
    pub dropped: Vec<u8>,
    #[serde(rename = "dropped6", with = "serde_bytes", default)]
    pub dropped6: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct MetadataMessage {
    /// 0 for request, 1 for data, 2 for reject.
    pub msg_type: u8,

    /// The zero-indexed piece number.
    pub piece: usize,

    /// The total size of the metadata file.
    /// Only included in 'data' messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_size: Option<usize>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExtendedHandshakePayload {
    pub m: HashMap<String, u8>,

    #[serde(default)]
    pub metadata_size: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lt_v2: Option<u8>,
}

#[derive(Debug, PartialEq, Clone)]
pub enum Message {
    Handshake(Vec<u8>, Vec<u8>),
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    HaveBatch(Vec<u32>),
    Bitfield(Vec<u8>),
    Request(u32, u32, u32),
    Piece(u32, u32, Vec<u8>),
    Cancel(u32, u32, u32),
    Port(u32),

    ExtendedHandshake(Option<i64>),
    Extended(u8, Vec<u8>),

    HashRequest(Vec<u8>, u32, u32, u32, u32), // root, base, offset, length, proof_layers
    HashReject(Vec<u8>, u32, u32, u32, u32),
    HashPiece(Vec<u8>, u32, u32, Vec<u8>), // root, base, offset, proof_data
}

#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct BlockInfo {
    pub piece_index: u32,
    pub offset: u32,
    pub length: u32,
}

pub async fn writer_task<W>(
    mut stream_write_half: W,
    mut write_rx: Receiver<Message>,
    error_tx: oneshot::Sender<Box<dyn StdError + Send + Sync>>,
    global_ul_bucket: Arc<TokenBucket>,
    mut shutdown_rx: broadcast::Receiver<()>,
    diagnostics: Option<crate::telemetry::download_diagnostics::Handle>,
) where
    W: AsyncWriteExt + Unpin + Send + 'static,
{
    // A reusable buffer to aggregate messages before writing to TCP.
    // 16KB initial capacity covers a standard block + headers.
    let mut batch_buffer = BytesMut::with_capacity(BUFFER_FLOOR);
    let mut codec = PeerCodec;
    let mut retention = BufferRetention::default();
    let mut timer = crate::execution::time::interval(BUFFER_WINDOW);
    timer.set_missed_tick_behavior(crate::execution::time::MissedTickBehavior::Skip);
    timer.tick().await;

    loop {
        // Clear buffer for the new batch (retains capacity)
        batch_buffer.clear();

        tokio::select! {
            _ = timer.tick() => {
                if let Some(target) = retention.observe(batch_buffer.capacity(), 0) {
                    batch_buffer = BytesMut::with_capacity(target);
                }
            }
            // Priority: Check for shutdown signal
            _ = shutdown_rx.recv() => {
                event!(Level::TRACE, "Writer task shutting down.");
                break;
            }

            // Wait for at least one message
            res = write_rx.recv() => {
                match res {
                    Some(first_msg) => {
                        let trace_requests = diagnostics.as_ref().is_some_and(|handle| handle.enabled(crate::telemetry::download_diagnostics::Detail::Trace));
                        let mut request_ids = Vec::new();
                        let mut omitted_trace_requests = 0u64;
                        if trace_requests {
                            if let Message::Request(piece, offset, length) = &first_msg {
                                request_ids.push((*piece, *offset, *length));
                            }
                        }

                        match codec.encode(first_msg, &mut batch_buffer) {
                            Ok(()) => {},
                            Err(e) => {
                                event!(Level::ERROR, "Failed to generate message: {}", e);
                                break;
                            }
                        }

                        // Check if more messages are immediately available in the channel.
                        // This reduces syscalls by writing multiple messages in one go.
                        // We cap the batch size (e.g., ~256KB) to ensure we don't hog memory
                        // or introduce too much latency for the first message.
                        while batch_buffer.len() < 262_144 {
                            match write_rx.try_recv() {
                                Ok(next_msg) => {
                                    let omitted_request = trace_requests
                                        && request_ids.len() >= 100
                                        && matches!(&next_msg, Message::Request(..));
                                    let request_id = if trace_requests && request_ids.len() < 100 {
                                        if let Message::Request(piece, offset, length) = &next_msg {
                                            Some((*piece, *offset, *length))
                                        } else { None }
                                    } else { None };
                                    match codec.encode(next_msg, &mut batch_buffer) {
                                        Ok(()) => {
                                            if let Some(id) = request_id { request_ids.push(id); }
                                            if omitted_request { omitted_trace_requests += 1; }
                                        },
                                        Err(e) => {
                                            event!(Level::ERROR, "Failed to generate batched message: {}", e);
                                            // We don't break here, we try to send what we have so far
                                        }
                                    }
                                }
                                Err(_) => break, // Channel empty for now
                            }
                        }

                        if !batch_buffer.is_empty() {

                            let len = batch_buffer.len();
                            retention.record(len);
                            consume_tokens(&global_ul_bucket, len as f64).await;

                            if let Err(e) = stream_write_half.write_all(&batch_buffer).await {
                                let _ = error_tx.send(e.into());
                                break;
                            }
                            if let Some(diagnostics) = &diagnostics {
                                diagnostics.omitted_trace_requests(omitted_trace_requests);
                                for (piece, offset, length) in request_ids {
                                    diagnostics.trace_request("request_written_to_transport", piece, offset, length);
                                }
                            }
                        }
                    }
                    None => {
                        event!(Level::TRACE, "Writer channel closed.");
                        break;
                    }
                }
            }
        }
    }
}

pub async fn reader_task<R>(
    mut stream_read_half: R,
    session_tx: mpsc::Sender<Message>,
    global_dl_bucket: Arc<TokenBucket>,
    mut shutdown_rx: broadcast::Receiver<()>,
) where
    R: AsyncReadExt + Unpin + Send + 'static,
{
    // Decode directly from transport storage. Message payloads own their Vecs, so
    // consuming a frame never pins a large backing allocation in the session.
    let mut processing_buf = BytesMut::with_capacity(BUFFER_FLOOR);
    let mut codec = PeerCodec;
    let mut retention = BufferRetention::default();
    // advance() can hide the consumed prefix of a still-retained allocation.
    let mut retained_capacity = BUFFER_FLOOR;
    let mut timer = crate::execution::time::interval(BUFFER_WINDOW);
    timer.set_missed_tick_behavior(crate::execution::time::MissedTickBehavior::Skip);
    timer.tick().await;
    loop {
        tokio::select! {
            _ = timer.tick() => {
                if let Some(target) = retention.observe(retained_capacity, processing_buf.len()) {
                    let mut smaller = BytesMut::with_capacity(target);
                    smaller.extend_from_slice(&processing_buf);
                    processing_buf = smaller;
                    retained_capacity = target;
                }
            }
            _ = shutdown_rx.recv() => break,
            read_result = async {
                // Preserve the former socket-read quantum and its token charging granularity.
                (&mut stream_read_half).take(BUFFER_FLOOR as u64)
                    .read_buf(&mut processing_buf).await
            } => {
                match read_result {
                    // Preserve the existing silent-close behavior for truncated EOF.
                    Ok(0) => break,
                    Ok(n) => {
                        retained_capacity = retained_capacity.max(processing_buf.capacity());
                        retention.record(processing_buf.len());
                        consume_tokens(&global_dl_bucket, n as f64).await;
                        loop {
                            match codec.decode(&mut processing_buf) {
                                Ok(Some(message)) => {
                                    if session_tx.send(message).await.is_err() { return; }
                                }
                                Ok(None) => break,
                                Err(e) => {
                                    event!(Level::ERROR, "Protocol error: {}", e);
                                    return;
                                }
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

pub fn generate_message(message: Message) -> Result<Vec<u8>, MessageGenerationError> {
    match message {
        Message::Handshake(info_hash, client_id) => {
            let mut handshake: Vec<u8> = Vec::new();

            let protocol_str = "BitTorrent protocol";
            let pstrlen = [19u8];
            let mut reserved = [0u8; 8];
            reserved[5] |= 0x10;

            handshake.extend_from_slice(&pstrlen);
            handshake.extend_from_slice(protocol_str.as_bytes());
            handshake.extend_from_slice(&reserved);
            handshake.extend_from_slice(&info_hash);
            handshake.extend_from_slice(&client_id);

            Ok(handshake)
        }
        Message::KeepAlive => Ok([0, 0, 0, 0].to_vec()),
        Message::Choke => Ok([0, 0, 0, 1, 0].to_vec()),
        Message::Unchoke => Ok([0, 0, 0, 1, 1].to_vec()),
        Message::Interested => Ok([0, 0, 0, 1, 2].to_vec()),
        Message::NotInterested => Ok([0, 0, 0, 1, 3].to_vec()),
        Message::Have(index) => {
            let mut message_bytes = Vec::new();
            message_bytes.extend([0, 0, 0, 5]);
            message_bytes.extend([4]);
            message_bytes.extend(index.to_be_bytes());
            Ok(message_bytes)
        }
        Message::HaveBatch(indices) => {
            let mut bytes = Vec::with_capacity(indices.len() * 9);
            for index in indices {
                bytes.extend_from_slice(&[0, 0, 0, 5, 4]);
                bytes.extend_from_slice(&index.to_be_bytes());
            }
            Ok(bytes)
        }
        Message::Bitfield(bitfield) => {
            let mut message_bytes: Vec<u8> = Vec::new();
            let message_len: u32 = (1 + bitfield.len())
                .try_into()
                .map_err(|_| MessageGenerationError::PayloadTooLarge("Bitfield".to_string()))?;
            message_bytes.extend(message_len.to_be_bytes());
            message_bytes.extend([5]);
            message_bytes.extend(bitfield);
            Ok(message_bytes)
        }
        Message::Request(index, begin, length) => {
            let mut message_bytes = Vec::new();
            message_bytes.extend([0, 0, 0, 13]);
            message_bytes.extend([6]);
            message_bytes.extend(index.to_be_bytes());
            message_bytes.extend(begin.to_be_bytes());
            message_bytes.extend(length.to_be_bytes());
            Ok(message_bytes)
        }
        Message::Piece(index, begin, block) => {
            let mut message_bytes: Vec<u8> = Vec::new();
            let message_len: u32 = (9 + block.len())
                .try_into()
                .map_err(|_| MessageGenerationError::PayloadTooLarge("Piece".to_string()))?;
            message_bytes.extend(message_len.to_be_bytes());
            message_bytes.extend([7]);
            message_bytes.extend(index.to_be_bytes());
            message_bytes.extend(begin.to_be_bytes());
            message_bytes.extend(block);
            Ok(message_bytes)
        }
        Message::Cancel(index, begin, length) => {
            let mut message_bytes = Vec::new();
            message_bytes.extend([0, 0, 0, 13]);
            message_bytes.extend([8]);
            message_bytes.extend(index.to_be_bytes());
            message_bytes.extend(begin.to_be_bytes());
            message_bytes.extend(length.to_be_bytes());
            Ok(message_bytes)
        }
        Message::Port(port) => {
            let mut message_bytes = Vec::new();
            message_bytes.extend([0, 0, 0, 5]);
            message_bytes.extend([9]);
            message_bytes.extend(port.to_be_bytes());
            Ok(message_bytes)
        }
        Message::ExtendedHandshake(metadata_size) => {
            let m: HashMap<String, u8> = ClientExtendedId::iter()
                .filter(|&variant| variant != ClientExtendedId::Handshake) // Exclude the special handshake ID
                .map(|variant| (variant.as_str().to_string(), variant.id()))
                .collect();
            let payload = ExtendedHandshakePayload {
                m,
                metadata_size,
                lt_v2: Some(1),
            };
            let bencoded_payload =
                serde_bencode::to_bytes(&payload).map_err(MessageGenerationError::BencodeError)?;

            let mut message_bytes: Vec<u8> = Vec::new();
            let message_len: u32 = (2 + bencoded_payload.len()) as u32;
            message_bytes.extend(message_len.to_be_bytes());
            message_bytes.push(20);
            message_bytes.push(ClientExtendedId::Handshake.id());
            message_bytes.extend(bencoded_payload);
            Ok(message_bytes)
        }
        Message::Extended(extended_id, payload) => {
            let mut message_bytes: Vec<u8> = Vec::new();
            let message_len: u32 = (2 + payload.len()) as u32;
            message_bytes.extend(message_len.to_be_bytes());
            message_bytes.push(20);
            message_bytes.push(extended_id);
            message_bytes.extend(payload);
            Ok(message_bytes)
        }

        Message::HashRequest(root, base, offset, length, proof_layers) => {
            let mut buffer = Vec::with_capacity(53); // 4 (len) + 1 (id) + 32 (root) + 16 (4*u32)

            // 49 bytes: ID + root (32) + base + offset + length + proof_layers
            let payload_len: u32 = 49;
            buffer.extend_from_slice(&payload_len.to_be_bytes());

            buffer.push(21); // HashRequest ID
            buffer.extend_from_slice(&root); // 32 bytes
            buffer.extend_from_slice(&base.to_be_bytes());
            buffer.extend_from_slice(&offset.to_be_bytes());
            buffer.extend_from_slice(&length.to_be_bytes());
            buffer.extend_from_slice(&proof_layers.to_be_bytes());

            Ok(buffer)
        }

        Message::HashPiece(root, base, offset, data) => {
            let mut buffer = Vec::new();
            // Length: 1 (ID) + 32 (Root) + 8 (2 * u32) + Data
            let len = 1 + 32 + 4 + 4 + data.len();
            buffer.extend_from_slice(&(len as u32).to_be_bytes());
            buffer.push(22);
            buffer.extend_from_slice(&root); // Write 32-byte Root
            buffer.extend_from_slice(&base.to_be_bytes());
            buffer.extend_from_slice(&offset.to_be_bytes());
            buffer.extend_from_slice(&data);
            Ok(buffer)
        }
        Message::HashReject(root, base, offset, length, proof_layers) => {
            let mut buffer = Vec::new();
            // Length: 1 (ID) + 32 (Root) + 16 (4 * u32) = 49 bytes
            let len = 1 + 32 + 4 + 4 + 4 + 4;
            buffer.extend_from_slice(&(len as u32).to_be_bytes());
            buffer.push(23);
            buffer.extend_from_slice(&root); // Write 32-byte Root
            buffer.extend_from_slice(&base.to_be_bytes());
            buffer.extend_from_slice(&offset.to_be_bytes());
            buffer.extend_from_slice(&length.to_be_bytes());
            buffer.extend_from_slice(&proof_layers.to_be_bytes());
            Ok(buffer)
        }
    }
}

pub fn parse_message_from_bytes(
    cursor: &mut std::io::Cursor<&Vec<u8>>,
) -> Result<Message, std::io::Error> {
    let mut len_buf = [0u8; 4];

    if std::io::Read::read_exact(cursor, &mut len_buf).is_err() {
        // Not enough bytes for length
        return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
    }
    let message_len = u32::from_be_bytes(len_buf);

    // KeepAlive (Len 0)
    if message_len == 0 {
        return Ok(Message::KeepAlive);
    }

    let current_pos = cursor.position();
    let available_bytes = cursor.get_ref().len() as u64 - current_pos;

    if available_bytes < message_len as u64 {
        // Not enough bytes for the payload yet.
        // Rewind to the start of the length prefix so we can retry later.
        cursor.set_position(current_pos - 4);
        return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
    }

    let mut id_buf = [0u8; 1];

    std::io::Read::read_exact(cursor, &mut id_buf)?;

    let message_id = id_buf[0];

    let payload_len = message_len as usize - 1;
    let mut payload = vec![0u8; payload_len];

    std::io::Read::read_exact(cursor, &mut payload)?;

    match message_id {
        // ... (rest of the function remains the same)
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
            Ok(Message::Bitfield(payload))
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
            let base = read_be_u32(&payload, 32)?;
            let offset = read_be_u32(&payload, 36)?;
            let length = read_be_u32(&payload, 40)?;
            let proof_layers = read_be_u32(&payload, 44)?;
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
            let base = read_be_u32(&payload, 32)?;
            let offset = read_be_u32(&payload, 36)?;

            let mut data = payload[40..].to_vec();

            if !data.is_empty() && !data.len().is_multiple_of(32) {
                let remainder = data.len() % 32;
                if remainder == 4 {
                    // Likely [Count: 4] [Hashes...]
                    data = data[4..].to_vec();
                    tracing::debug!("Trimmed 4-byte prefix from HashPiece proof");
                } else if remainder == 8 {
                    // Likely [Length: 4] [Count: 4] [Hashes...]
                    data = data[8..].to_vec();
                    tracing::debug!("Trimmed 8-byte prefix from HashPiece proof");
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
            let base = read_be_u32(&payload, 32)?;
            let offset = read_be_u32(&payload, 36)?;
            let length = read_be_u32(&payload, 40)?;
            let proof_layers = read_be_u32(&payload, 44)?; // Read extra field

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

// Helper to read a u32 from a byte slice at a specific offset
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt}; // Import traits for read_exact/write_all
    use tokio::net::{TcpListener, TcpStream}; // Import networking components

    #[tokio::test]
    async fn small_buffers_preserve_fragmented_large_frames() {
        tokio::time::timeout(Duration::from_secs(10), async {
            for size in [1, 16384, 65536, 131072] {
                let (mut tx, rx) = tokio::io::duplex(997);
                let (events, mut received) = mpsc::channel(1);
                let (shutdown, _) = broadcast::channel(1);
                let bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
                let reader = tokio::spawn(reader_task(rx, events, bucket, shutdown.subscribe()));
                let expected = Message::Bitfield(vec![0x55; size]);
                let wire = generate_message(expected.clone()).unwrap();
                let sender = tokio::spawn(async move {
                    for part in wire.chunks(317) {
                        tx.write_all(part).await.unwrap();
                    }
                });
                assert_eq!(received.recv().await.unwrap(), expected);
                sender.await.unwrap();
                reader.await.unwrap();
                assert!(received.recv().await.is_none());
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn smaller_writer_batches_preserve_order_under_backpressure() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (tx, mut rx) = tokio::io::duplex(997);
            let (commands, queue) = mpsc::channel(128);
            let (errors, _error_rx) = oneshot::channel();
            let (shutdown, _) = broadcast::channel(1);
            for index in 0..64 {
                commands
                    .send(Message::Piece(index, 0, vec![index as u8; 16384]))
                    .await
                    .unwrap();
            }
            commands
                .send(Message::Bitfield(vec![0x55; 131072]))
                .await
                .unwrap();
            commands.send(Message::Choke).await.unwrap();
            drop(commands);
            let bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
            let writer = tokio::spawn(writer_task(
                tx,
                queue,
                errors,
                bucket,
                shutdown.subscribe(),
                None,
            ));
            for index in 0..64 {
                assert_eq!(
                    parse_message(&mut rx).await.unwrap(),
                    Message::Piece(index, 0, vec![index as u8; 16384])
                );
                tokio::task::yield_now().await;
            }
            assert_eq!(
                parse_message(&mut rx).await.unwrap(),
                Message::Bitfield(vec![0x55; 131072])
            );
            assert_eq!(parse_message(&mut rx).await.unwrap(), Message::Choke);
            writer.await.unwrap();
            assert_eq!(rx.read(&mut [0]).await.unwrap(), 0);
        })
        .await
        .unwrap();
    }

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
        parse_message_from_bytes(&mut cursor)
    }

    #[test]
    fn test_generate_handshake() {
        let my_peer_id = b"-SS1000-69fG2wk6wWLc";
        let info_hash = [0u8; 20].to_vec();
        let peer_id_vec = my_peer_id.to_vec();

        let actual_result =
            generate_message(Message::Handshake(info_hash.clone(), peer_id_vec.clone())).unwrap();

        let mut expected_reserved = [0u8; 8];
        expected_reserved[5] |= 0x10; // This matches your implementation

        assert_eq!(actual_result.len(), 68);
        assert_eq!(actual_result[0], 19); // Pstrlen should be 19
        assert_eq!(&actual_result[1..20], b"BitTorrent protocol"); // Protocol string
        assert_eq!(&actual_result[20..28], &expected_reserved); // Reserved bytes
        assert_eq!(&actual_result[28..48], &info_hash[..]); // Info_hash
        assert_eq!(&actual_result[48..68], &peer_id_vec[..]); // Peer ID
    }

    #[tokio::test]
    async fn test_tcp_handshake() -> Result<(), Box<dyn Error>> {
        let ip_port = "127.0.0.1:8080";
        let listener = TcpListener::bind(&ip_port).await?;

        let info_hash = b"infohashinfohashinfo".to_vec(); // 20 bytes
        let my_peer_id = b"-SS1000-69fG2wk6wWLc".to_vec(); // 20 bytes

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = vec![0; 68];
                // Use read_exact to ensure all 68 bytes are read
                if socket.read_exact(&mut buffer).await.is_ok() {
                    // Echo the received handshake back
                    let _ = socket.write_all(&buffer).await;
                }
            }
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut client = TcpStream::connect(ip_port).await?;

        let handshake_msg =
            generate_message(Message::Handshake(info_hash.clone(), my_peer_id.clone())).unwrap();

        client.write_all(&handshake_msg).await?;

        let mut buffer = [0; 68];
        client.read_exact(&mut buffer).await?;

        let mut expected_reserved = [0u8; 8];
        expected_reserved[5] |= 0x10;

        assert_eq!(buffer[0], 19);
        assert_eq!(&buffer[1..20], b"BitTorrent protocol");
        assert_eq!(&buffer[20..28], &expected_reserved);
        assert_eq!(&buffer[28..48], &info_hash[..]);
        assert_eq!(&buffer[48..68], &my_peer_id[..]);

        return Ok(());
    }

    // --- Template for all other TCP tests ---
    // This helper function reduces boilerplate for all message types
    async fn run_message_test(
        ip_port: &str,
        message_to_send: Message,
        expected_message: Message,
    ) -> Result<(), Box<dyn Error>> {
        let listener = TcpListener::bind(ip_port).await?;

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let msg_bytes = generate_message(message_to_send).unwrap();
                let _ = socket.write_all(&msg_bytes).await;
            }
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        let client = TcpStream::connect(ip_port).await?;

        let (mut read_half, _) = client.into_split();

        assert_eq!(expected_message, parse_message(&mut read_half).await?);

        Ok(())
    }

    #[tokio::test]
    async fn test_tcp_keep_alive() -> Result<(), Box<dyn Error>> {
        run_message_test("127.0.0.1:8081", Message::KeepAlive, Message::KeepAlive).await
    }

    #[tokio::test]
    async fn test_tcp_choke() -> Result<(), Box<dyn Error>> {
        run_message_test("127.0.0.1:8082", Message::Choke, Message::Choke).await
    }

    #[tokio::test]
    async fn test_tcp_unchoke() -> Result<(), Box<dyn Error>> {
        run_message_test("127.0.0.1:8083", Message::Unchoke, Message::Unchoke).await
    }

    #[tokio::test]
    async fn test_tcp_interested() -> Result<(), Box<dyn Error>> {
        run_message_test("127.0.0.1:8084", Message::Interested, Message::Interested).await
    }

    #[tokio::test]
    async fn test_tcp_have() -> Result<(), Box<dyn Error>> {
        run_message_test("127.0.0.1:8085", Message::Have(123), Message::Have(123)).await
    }

    #[tokio::test]
    async fn test_tcp_bitfield() -> Result<(), Box<dyn Error>> {
        let bitfield = vec![0b10101010, 0b01010101];
        run_message_test(
            "127.0.0.1:8086",
            Message::Bitfield(bitfield.clone()),
            Message::Bitfield(bitfield),
        )
        .await
    }

    #[tokio::test]
    async fn test_tcp_request() -> Result<(), Box<dyn Error>> {
        run_message_test(
            "127.0.0.1:8087",
            Message::Request(1, 2, 3),
            Message::Request(1, 2, 3),
        )
        .await
    }

    #[tokio::test]
    async fn test_tcp_piece() -> Result<(), Box<dyn Error>> {
        let piece_data = vec![1, 2, 3, 4, 5];
        run_message_test(
            "127.0.0.1:8088",
            Message::Piece(1, 2, piece_data.clone()),
            Message::Piece(1, 2, piece_data),
        )
        .await
    }

    #[tokio::test]
    async fn test_tcp_cancel() -> Result<(), Box<dyn Error>> {
        run_message_test(
            "127.0.0.1:8089",
            Message::Cancel(1, 2, 3),
            Message::Cancel(1, 2, 3),
        )
        .await
    }

    #[tokio::test]
    async fn test_tcp_port() -> Result<(), Box<dyn Error>> {
        run_message_test("127.0.0.1:8090", Message::Port(9999), Message::Port(9999)).await
    }

    /// This one helper function replaces all your TCP tests.
    /// It checks that a message can be serialized and then parsed back.
    async fn assert_message_roundtrip(msg: Message) {
        let bytes = generate_message(msg.clone()).unwrap();

        let mut reader = &bytes[..];

        let parsed_msg = parse_message(&mut reader).await.unwrap();

        assert_eq!(msg, parsed_msg);
    }

    /// This single test runs instantly and checks all your message types.
    #[tokio::test]
    async fn test_all_message_roundtrips() {
        assert_message_roundtrip(Message::KeepAlive).await;
        assert_message_roundtrip(Message::Choke).await;
        assert_message_roundtrip(Message::Unchoke).await;
        assert_message_roundtrip(Message::Interested).await;
        assert_message_roundtrip(Message::NotInterested).await;
        assert_message_roundtrip(Message::Have(123)).await;
        assert_message_roundtrip(Message::Bitfield(vec![0b10101010, 0b01010101])).await;
        assert_message_roundtrip(Message::Request(1, 16384, 16384)).await;
        assert_message_roundtrip(Message::Piece(1, 16384, vec![1, 2, 3, 4, 5])).await;
        assert_message_roundtrip(Message::Cancel(1, 16384, 16384)).await;
        assert_message_roundtrip(Message::Port(6881)).await;
        assert_message_roundtrip(Message::Extended(1, vec![10, 20, 30])).await;
    }

    /// Special test for the ExtendedHandshake
    #[tokio::test]
    async fn test_extended_handshake_parsing() {
        let metadata_size = 12345;
        let msg = Message::ExtendedHandshake(Some(metadata_size));
        let generated_bytes = generate_message(msg).unwrap();

        let mut reader = &generated_bytes[..];
        let parsed = parse_message(&mut reader).await.unwrap();

        if let Message::Extended(id, payload_bytes) = parsed {
            assert_eq!(id, ClientExtendedId::Handshake.id()); // ID is 0

            let payload: ExtendedHandshakePayload =
                serde_bencode::from_bytes(&payload_bytes).unwrap();

            assert_eq!(payload.metadata_size, Some(metadata_size));
            #[cfg(feature = "pex")]
            assert!(payload.m.contains_key("ut_pex"));
            assert!(payload.m.contains_key("ut_metadata"));
        } else {
            panic!("ExtendedHandshake did not parse back as Message::Extended");
        }
    }

    #[cfg(feature = "pex")]
    #[test]
    fn test_pex_message_roundtrip_supports_ipv6_keys() {
        let message = PexMessage {
            added: vec![127, 0, 0, 1, 0x1A, 0xE1],
            added_f: vec![0],
            added6: {
                let mut bytes = vec![0u8; 16];
                bytes[15] = 1;
                bytes.extend_from_slice(&6881u16.to_be_bytes());
                bytes
            },
            added6_f: vec![0],
            dropped: vec![127, 0, 0, 2, 0x1A, 0xE2],
            dropped6: {
                let mut bytes = vec![0u8; 16];
                bytes[15] = 2;
                bytes.extend_from_slice(&6882u16.to_be_bytes());
                bytes
            },
        };

        let encoded = serde_bencode::to_bytes(&message).expect("serialize pex");
        assert!(
            encoded.windows(b"added6".len()).any(|w| w == b"added6"),
            "serialized payload should include added6 key"
        );
        assert!(
            encoded.windows(b"added6.f".len()).any(|w| w == b"added6.f"),
            "serialized payload should include added6.f key"
        );
        assert!(
            encoded.windows(b"dropped6".len()).any(|w| w == b"dropped6"),
            "serialized payload should include dropped6 key"
        );

        let decoded: PexMessage = serde_bencode::from_bytes(&encoded).expect("deserialize pex");
        assert_eq!(decoded.added, message.added);
        assert_eq!(decoded.added_f, message.added_f);
        assert_eq!(decoded.added6, message.added6);
        assert_eq!(decoded.added6_f, message.added6_f);
        assert_eq!(decoded.dropped, message.dropped);
        assert_eq!(decoded.dropped6, message.dropped6);
    }

    #[cfg(feature = "pex")]
    #[test]
    fn test_pex_message_serializes_dropped_as_compact_bytes() {
        let message = PexMessage {
            dropped: vec![127, 0, 0, 2, 0x1A, 0xE2],
            ..Default::default()
        };

        let encoded = serde_bencode::to_bytes(&message).expect("serialize pex");

        assert!(
            encoded
                .windows(b"7:dropped6:".len())
                .any(|w| w == b"7:dropped6:"),
            "dropped peers should serialize as a compact byte string"
        );
    }

    #[cfg(feature = "pex")]
    #[test]
    fn test_pex_message_serializes_flag_vectors_as_compact_bytes() {
        let message = PexMessage {
            added_f: vec![0x01],
            added6_f: vec![0x02],
            ..Default::default()
        };

        let encoded = serde_bencode::to_bytes(&message).expect("serialize pex");

        assert!(
            encoded
                .windows(b"7:added.f1:".len())
                .any(|w| w == b"7:added.f1:"),
            "added.f flags should serialize as a compact byte string"
        );
        assert!(
            encoded
                .windows(b"8:added6.f1:".len())
                .any(|w| w == b"8:added6.f1:"),
            "added6.f flags should serialize as a compact byte string"
        );
    }
}
