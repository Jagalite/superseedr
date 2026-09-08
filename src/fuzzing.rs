// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Narrow fuzzing facade for protocol parsers.
//!
//! These helpers intentionally return no structured value. Fuzz targets only
//! need to drive parser code and catch panics, aborts, or sanitizer findings.

use std::io::Cursor;
pub fn decode_utp_packet(bytes: &[u8]) {
    let _ = std::hint::black_box(crate::networking::utp::decode_packet_for_fuzzing(bytes));
}

pub fn roundtrip_utp_packet(bytes: &[u8]) {
    crate::networking::utp::roundtrip_packet_for_fuzzing(bytes);
}

pub fn parse_tcp_message(bytes: &[u8]) {
    let input = bytes.to_vec();
    let mut cursor = Cursor::new(&input);
    let _ = std::hint::black_box(crate::networking::protocol::parse_message_from_bytes(
        &mut cursor,
    ));
}

pub fn roundtrip_tcp_message(bytes: &[u8]) {
    use crate::networking::protocol::{generate_message, parse_message_from_bytes, Message};

    let mut input = FuzzBytes::new(bytes);
    let message = match input.next_u8() % 12 {
        0 => Message::KeepAlive,
        1 => Message::Choke,
        2 => Message::Unchoke,
        3 => Message::Interested,
        4 => Message::NotInterested,
        5 => Message::Have(input.next_u32()),
        6 => Message::Bitfield(input.take_vec(64)),
        7 => Message::Request(input.next_u32(), input.next_u32(), input.next_u32()),
        8 => Message::Piece(input.next_u32(), input.next_u32(), input.take_vec(512)),
        9 => Message::Cancel(input.next_u32(), input.next_u32(), input.next_u32()),
        10 => Message::Port(input.next_u32()),
        _ => Message::Extended(input.next_u8(), input.take_vec(512)),
    };

    let encoded = generate_message(message.clone()).expect("structured fuzz message encodes");
    let mut cursor = Cursor::new(&encoded);
    let decoded = parse_message_from_bytes(&mut cursor).expect("structured fuzz message decodes");
    assert_eq!(decoded, message);
    assert_eq!(cursor.position() as usize, encoded.len());
}

pub fn parse_torrent_file(bytes: &[u8]) {
    let _ = std::hint::black_box(crate::torrent_file::parser::from_bytes(bytes));
}

pub fn parse_torrent_info(bytes: &[u8]) {
    let _ = std::hint::black_box(crate::torrent_file::parser::from_info_bytes(bytes));
}

#[cfg(feature = "dht")]
pub fn decode_krpc_message(bytes: &[u8]) {
    let _ = std::hint::black_box(crate::dht::krpc::decode_message(bytes));
}

#[cfg(not(feature = "dht"))]
pub fn decode_krpc_message(_bytes: &[u8]) {}

#[cfg(feature = "dht")]
pub fn decode_krpc_compact(bytes: &[u8]) {
    use crate::dht::types::AddressFamily;

    let family = if bytes.first().is_some_and(|byte| byte & 1 == 1) {
        AddressFamily::Ipv6
    } else {
        AddressFamily::Ipv4
    };
    let payload = bytes.get(1..).unwrap_or_default();
    let _ = std::hint::black_box(crate::dht::krpc::decode_compact_peers(payload, family));
    let _ = std::hint::black_box(crate::dht::krpc::decode_compact_nodes(payload, family));
}

#[cfg(not(feature = "dht"))]
pub fn decode_krpc_compact(_bytes: &[u8]) {}

#[cfg(feature = "dht")]
pub fn roundtrip_krpc_query(bytes: &[u8]) {
    use crate::dht::krpc::{
        decode_message, KrpcAnnouncePeerArgs, KrpcFindNodeArgs, KrpcGetPeersArgs,
        KrpcInboundMessage, KrpcPingArgs, KrpcQueryEnvelope, KrpcQueryKind,
    };
    use crate::dht::types::{AddressFamily, InfoHash, NodeId, TransactionId};

    let mut input = FuzzBytes::new(bytes);
    let transaction_id = TransactionId::from(input.next_array_4());
    let local_node = NodeId::from(input.next_array_20());
    let target_node = NodeId::from(input.next_array_20());
    let info_hash = InfoHash::from(input.next_array_20());
    let want = match input.next_u8() % 4 {
        0 => Vec::new(),
        1 => vec![AddressFamily::Ipv4],
        2 => vec![AddressFamily::Ipv6],
        _ => vec![AddressFamily::Ipv4, AddressFamily::Ipv6],
    };

    let encoded = match input.next_u8() % 4 {
        0 => serde_bencode::to_bytes(&KrpcQueryEnvelope::new(
            transaction_id,
            KrpcQueryKind::Ping,
            KrpcPingArgs::new(local_node),
        )),
        1 => serde_bencode::to_bytes(&KrpcQueryEnvelope::new(
            transaction_id,
            KrpcQueryKind::FindNode,
            KrpcFindNodeArgs::new(local_node, target_node).with_want(&want),
        )),
        2 => serde_bencode::to_bytes(&KrpcQueryEnvelope::new(
            transaction_id,
            KrpcQueryKind::GetPeers,
            KrpcGetPeersArgs::new(local_node, info_hash).with_want(&want),
        )),
        _ => serde_bencode::to_bytes(&KrpcQueryEnvelope::new(
            transaction_id,
            KrpcQueryKind::AnnouncePeer,
            KrpcAnnouncePeerArgs::new(
                local_node,
                info_hash,
                input.next_u16(),
                Some(input.next_u8()),
                &input.take_vec(32),
            ),
        )),
    }
    .expect("structured KRPC query encodes");

    let decoded = decode_message(&encoded).expect("structured KRPC query decodes");
    assert!(matches!(decoded, KrpcInboundMessage::Query(_)));
}

#[cfg(not(feature = "dht"))]
pub fn roundtrip_krpc_query(_bytes: &[u8]) {}

#[cfg(feature = "dht")]
pub fn reduce_dht_lifecycle(bytes: &[u8]) {
    crate::dht::service::fuzzing::reduce_lifecycle_for_fuzzing(bytes);
}

#[cfg(not(feature = "dht"))]
pub fn reduce_dht_lifecycle(_bytes: &[u8]) {}

struct FuzzBytes<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> FuzzBytes<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn next_u8(&mut self) -> u8 {
        let byte = self.bytes.get(self.offset).copied().unwrap_or_default();
        self.offset = self.offset.saturating_add(1);
        byte
    }

    #[cfg(feature = "dht")]
    fn next_u16(&mut self) -> u16 {
        u16::from_be_bytes([self.next_u8(), self.next_u8()])
    }

    fn next_u32(&mut self) -> u32 {
        u32::from_be_bytes([
            self.next_u8(),
            self.next_u8(),
            self.next_u8(),
            self.next_u8(),
        ])
    }

    fn take_vec(&mut self, max_len: usize) -> Vec<u8> {
        let requested_len = usize::from(self.next_u8()).min(max_len);
        let start = self.offset.min(self.bytes.len());
        let remaining = self.bytes.len().saturating_sub(start);
        let len = requested_len.min(remaining);
        let end = start + len;
        let data = self.bytes[start..end].to_vec();
        self.offset = end;
        data
    }

    #[cfg(feature = "dht")]
    fn next_array_4(&mut self) -> [u8; 4] {
        [
            self.next_u8(),
            self.next_u8(),
            self.next_u8(),
            self.next_u8(),
        ]
    }

    #[cfg(feature = "dht")]
    fn next_array_20(&mut self) -> [u8; 20] {
        std::array::from_fn(|_| self.next_u8())
    }
}
