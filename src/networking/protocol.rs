// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::token_bucket::consume_tokens;
use crate::token_bucket::TokenBucket;

use tokio::sync::Mutex;

use std::collections::HashMap;
use std::collections::HashSet;
use std::error::Error as StdError;
use std::io::{Error, ErrorKind};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::broadcast;
use tokio::sync::mpsc::Receiver;
use tokio::sync::oneshot;

use serde::{Deserialize, Serialize};

use std::fmt;
use tracing::{event, Level};

use strum::IntoEnumIterator;
use strum_macros::EnumIter;

const STANDARD_BLOCK_SIZE: u32 = 16384;

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
    #[serde(default)]
    pub added_f: Vec<u8>,
    #[serde(with = "serde_bytes", default)]
    pub dropped: Vec<u8>,
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
}

pub struct MessageSummary<'a>(pub &'a Message);
impl fmt::Debug for MessageSummary<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Message::Bitfield(bitfield) => {
                write!(f, "BITFIELD(len: {})", bitfield.len())
            }
            Message::Piece(index, begin, data) => {
                write!(
                    f,
                    "PIECE(index: {}, begin: {}, len: {})",
                    index,
                    begin,
                    data.len()
                )
            }

            Message::Handshake(_, _) => write!(f, "HANDSHAKE(...)"),
            other => write!(f, "{:?}", other),
        }
    }
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
    Bitfield(Vec<u8>),
    Request(u32, u32, u32),
    Piece(u32, u32, Vec<u8>),
    Cancel(u32, u32, u32),
    Port(u32),

    ExtendedHandshake(Option<i64>),
    Extended(u8, Vec<u8>),
}

#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct BlockInfo {
    pub piece_index: u32,
    pub offset: u32,
    pub length: u32,
}

pub fn calculate_blocks_for_piece(piece_index: u32, piece_size: u32) -> HashSet<BlockInfo> {
    let mut blocks = HashSet::new();

    let mut current_offset = 0;
    while current_offset < piece_size {
        let block_length = std::cmp::min(STANDARD_BLOCK_SIZE, piece_size - current_offset);

        blocks.insert(BlockInfo {
            piece_index,
            offset: current_offset,
            length: block_length,
        });

        current_offset += block_length;
    }

    blocks
}

pub async fn writer_task(
    mut stream_write_half: OwnedWriteHalf,
    mut write_rx: Receiver<Message>,
    error_tx: oneshot::Sender<Box<dyn StdError + Send + Sync>>,
    global_ul_bucket: Arc<Mutex<TokenBucket>>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    loop {
        tokio::select! {
            Some(message) = write_rx.recv() => {
                if let Message::Piece(_, _, data) = &message {
                    if !data.is_empty() {
                        tokio::select! {
                            _ = consume_tokens(&global_ul_bucket, data.len() as f64) => {},
                            _ = shutdown_rx.recv() => {
                                event!(Level::TRACE, "Writer task shutting down during token wait.");
                                break;
                            }
                        }
                    }
                }

                match generate_message(message) {
                    Ok(message_bytes) => {
                        tokio::select! {
                            write_result = stream_write_half.write_all(&message_bytes) => {
                                if let Err(e) = write_result {
                                    let _ = error_tx.send(e.into());
                                    break;
                                }
                            }
                            _ = shutdown_rx.recv() => {
                                event!(Level::TRACE, "Writer task shutting down during TCP write.");
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        event!(
                            Level::ERROR,
                            "Failed to generate message for writer task: {}",
                            e
                        );
                        break;
                    }
                }
            }

            _ = shutdown_rx.recv() => {
                event!(Level::TRACE, "Writer task shutting down while idle.");
                break;
            }

            else => {
                event!(Level::TRACE, "Writer task shutting down, channel closed.");
                break;
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
            let payload = ExtendedHandshakePayload { m, metadata_size };
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
    }
}

pub async fn parse_message(
    socket: &mut (impl AsyncReadExt + Unpin),
) -> Result<Message, std::io::Error> {
    let mut buffer_message_len = [0u8; 4];
    let _ = socket.read_exact(&mut buffer_message_len).await?;
    let message_len = u32::from_be_bytes(buffer_message_len);

    if message_len == 0 {
        return Ok(Message::KeepAlive);
    }

    let mut buffer_message_id = [0u8; 1];
    let _ = socket.read_exact(&mut buffer_message_id).await?;
    let message_id: usize = u8::from_be_bytes(buffer_message_id).into();

    match message_id {
        0 => Ok(Message::Choke),
        1 => Ok(Message::Unchoke),
        2 => Ok(Message::Interested),
        3 => Ok(Message::NotInterested),
        4 => {
            let mut message_payload: Vec<u8> = vec![0; (message_len - 1) as usize];
            let _ = socket.read_exact(&mut message_payload).await?;
            let byte_array: [u8; 4] = message_payload.try_into().map_err(|_| {
                Error::new(
                    ErrorKind::InvalidData,
                    "Incorrect payload size for HAVE message",
                )
            })?;
            Ok(Message::Have(u32::from_be_bytes(byte_array)))
        }
        5 => {
            let mut message_payload: Vec<u8> = vec![0; (message_len - 1) as usize];
            let _ = socket.read_exact(&mut message_payload).await?;
            Ok(Message::Bitfield(message_payload))
        }
        6 => {
            let mut buffer_message_index = [0u8; 4];
            let mut buffer_message_begin = [0u8; 4];
            let mut buffer_message_len = [0u8; 4];

            let _ = socket.read_exact(&mut buffer_message_index).await?;
            let _ = socket.read_exact(&mut buffer_message_begin).await?;
            let _ = socket.read_exact(&mut buffer_message_len).await?;

            Ok(Message::Request(
                u32::from_be_bytes(buffer_message_index),
                u32::from_be_bytes(buffer_message_begin),
                u32::from_be_bytes(buffer_message_len),
            ))
        }
        7 => {
            let mut buffer_message_index = [0u8; 4];
            let mut buffer_message_begin = [0u8; 4];

            let _ = socket.read_exact(&mut buffer_message_index).await?;
            let _ = socket.read_exact(&mut buffer_message_begin).await?;

            let mut message_payload: Vec<u8> = vec![0; (message_len - 9) as usize];
            let _ = socket.read_exact(&mut message_payload).await?;

            Ok(Message::Piece(
                u32::from_be_bytes(buffer_message_index),
                u32::from_be_bytes(buffer_message_begin),
                message_payload,
            ))
        }
        8 => {
            let mut buffer_message_index = [0u8; 4];
            let mut buffer_message_begin = [0u8; 4];
            let mut buffer_message_len = [0u8; 4];

            let _ = socket.read_exact(&mut buffer_message_index).await?;
            let _ = socket.read_exact(&mut buffer_message_begin).await?;
            let _ = socket.read_exact(&mut buffer_message_len).await?;

            Ok(Message::Cancel(
                u32::from_be_bytes(buffer_message_index),
                u32::from_be_bytes(buffer_message_begin),
                u32::from_be_bytes(buffer_message_len),
            ))
        }
        9 => {
            let mut buffer_message_port = [0u8; 4];
            let _ = socket.read_exact(&mut buffer_message_port).await?;
            Ok(Message::Port(u32::from_be_bytes(buffer_message_port)))
        }
        20 => {
            let mut extended_id_buf = [0u8; 1];
            socket.read_exact(&mut extended_id_buf).await?;
            let extended_id = extended_id_buf[0];

            let payload_len = message_len - 2;
            let mut payload = vec![0u8; payload_len as usize];
            socket.read_exact(&mut payload).await?;

            Ok(Message::Extended(extended_id, payload))
        }
        _ => {
            let error_message = format!("Invalid message ID received from peer: {}", message_id);
            Err(Error::new(ErrorKind::InvalidData, error_message))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt}; // Import traits for read_exact/write_all
    use tokio::net::{TcpListener, TcpStream}; // Import networking components

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
        // 1. Generate the message into bytes
        let bytes = generate_message(msg.clone()).unwrap();

        // 2. Create an in-memory "reader" from those bytes
        let mut reader = &bytes[..];

        // 3. Parse the message back (this works because of Step 1)
        let parsed_msg = parse_message(&mut reader).await.unwrap();

        // 4. Assert they are identical
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
        // 1. Generate the ExtendedHandshake message
        let metadata_size = 12345;
        let msg = Message::ExtendedHandshake(Some(metadata_size));
        let generated_bytes = generate_message(msg).unwrap();

        // 2. Parse it back using our generic parser
        let mut reader = &generated_bytes[..];
        let parsed = parse_message(&mut reader).await.unwrap();

        // 3. It should parse as a Message::Extended with ID 0 (Handshake ID)
        if let Message::Extended(id, payload_bytes) = parsed {
            assert_eq!(id, ClientExtendedId::Handshake.id()); // ID is 0

            // 4. Check the bencoded payload
            let payload: ExtendedHandshakePayload =
                serde_bencode::from_bytes(&payload_bytes).unwrap();

            assert_eq!(payload.metadata_size, Some(metadata_size as i64));
            assert!(payload.m.contains_key("ut_pex"));
            assert!(payload.m.contains_key("ut_metadata"));
        } else {
            panic!("ExtendedHandshake did not parse back as Message::Extended");
        }
    }
}
