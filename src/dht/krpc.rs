// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::types::{
    AddressFamily, CompactNode, CompactPeer, FixedLengthError, InfoHash, NodeId, TransactionId,
};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use thiserror::Error;

pub const DEFAULT_KRPC_VERSION: &[u8; 4] = b"SSR0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KrpcQueryKind {
    Ping,
    FindNode,
    GetPeers,
    AnnouncePeer,
}

impl KrpcQueryKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::FindNode => "find_node",
            Self::GetPeers => "get_peers",
            Self::AnnouncePeer => "announce_peer",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KrpcQueryEnvelope<A> {
    pub t: ByteBuf,
    pub y: &'static str,
    pub q: &'static str,
    pub a: A,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub v: Option<ByteBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct KrpcDecodedQueryEnvelope<A> {
    t: ByteBuf,
    #[allow(dead_code)]
    y: String,
    #[allow(dead_code)]
    q: String,
    a: A,
    #[serde(default)]
    v: Option<ByteBuf>,
}

impl<A> KrpcQueryEnvelope<A> {
    pub fn new(transaction_id: TransactionId, query: KrpcQueryKind, args: A) -> Self {
        Self::with_version(transaction_id, query, args, Some(DEFAULT_KRPC_VERSION))
    }

    pub fn with_version(
        transaction_id: TransactionId,
        query: KrpcQueryKind,
        args: A,
        version: Option<&[u8]>,
    ) -> Self {
        Self {
            t: ByteBuf::from(transaction_id.as_ref().to_vec()),
            y: "q",
            q: query.as_str(),
            a: args,
            v: version.map(|bytes| ByteBuf::from(bytes.to_vec())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KrpcPingArgs {
    pub id: ByteBuf,
}

impl KrpcPingArgs {
    pub fn new(id: NodeId) -> Self {
        Self {
            id: ByteBuf::from(id.as_ref().to_vec()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KrpcFindNodeArgs {
    pub id: ByteBuf,
    pub target: ByteBuf,
}

impl KrpcFindNodeArgs {
    pub fn new(id: NodeId, target: NodeId) -> Self {
        Self {
            id: ByteBuf::from(id.as_ref().to_vec()),
            target: ByteBuf::from(target.as_ref().to_vec()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KrpcGetPeersArgs {
    pub id: ByteBuf,
    pub info_hash: ByteBuf,
}

impl KrpcGetPeersArgs {
    pub fn new(id: NodeId, info_hash: InfoHash) -> Self {
        Self {
            id: ByteBuf::from(id.as_ref().to_vec()),
            info_hash: ByteBuf::from(info_hash.as_ref().to_vec()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KrpcAnnouncePeerArgs {
    pub id: ByteBuf,
    pub info_hash: ByteBuf,
    pub port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implied_port: Option<u8>,
    pub token: ByteBuf,
}

impl KrpcAnnouncePeerArgs {
    pub fn new(
        id: NodeId,
        info_hash: InfoHash,
        port: u16,
        implied_port: Option<u8>,
        token: &[u8],
    ) -> Self {
        Self {
            id: ByteBuf::from(id.as_ref().to_vec()),
            info_hash: ByteBuf::from(info_hash.as_ref().to_vec()),
            port,
            implied_port,
            token: ByteBuf::from(token.to_vec()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KrpcIncomingQuery {
    Ping {
        transaction_id: ByteBuf,
        version: Option<ByteBuf>,
        args: KrpcPingArgs,
    },
    FindNode {
        transaction_id: ByteBuf,
        version: Option<ByteBuf>,
        args: KrpcFindNodeArgs,
    },
    GetPeers {
        transaction_id: ByteBuf,
        version: Option<ByteBuf>,
        args: KrpcGetPeersArgs,
    },
    AnnouncePeer {
        transaction_id: ByteBuf,
        version: Option<ByteBuf>,
        args: KrpcAnnouncePeerArgs,
    },
}

impl KrpcIncomingQuery {
    pub fn kind(&self) -> KrpcQueryKind {
        match self {
            Self::Ping { .. } => KrpcQueryKind::Ping,
            Self::FindNode { .. } => KrpcQueryKind::FindNode,
            Self::GetPeers { .. } => KrpcQueryKind::GetPeers,
            Self::AnnouncePeer { .. } => KrpcQueryKind::AnnouncePeer,
        }
    }

    pub fn transaction_id(&self) -> &[u8] {
        match self {
            Self::Ping { transaction_id, .. }
            | Self::FindNode { transaction_id, .. }
            | Self::GetPeers { transaction_id, .. }
            | Self::AnnouncePeer { transaction_id, .. } => transaction_id.as_ref(),
        }
    }

    pub fn version(&self) -> Option<&[u8]> {
        match self {
            Self::Ping { version, .. }
            | Self::FindNode { version, .. }
            | Self::GetPeers { version, .. }
            | Self::AnnouncePeer { version, .. } => version.as_ref().map(ByteBuf::as_ref),
        }
    }

    pub fn requester_id(&self) -> Option<NodeId> {
        match self {
            Self::Ping { args, .. } => NodeId::try_from(args.id.as_ref()).ok(),
            Self::FindNode { args, .. } => NodeId::try_from(args.id.as_ref()).ok(),
            Self::GetPeers { args, .. } => NodeId::try_from(args.id.as_ref()).ok(),
            Self::AnnouncePeer { args, .. } => NodeId::try_from(args.id.as_ref()).ok(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KrpcInboundMessage {
    Query(KrpcIncomingQuery),
    Response(KrpcResponseEnvelope),
    Error(KrpcErrorEnvelope),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KrpcResponseEnvelope {
    pub t: ByteBuf,
    pub y: ByteBuf,
    #[serde(default)]
    pub r: Option<KrpcResponseBody>,
    #[serde(default)]
    pub v: Option<ByteBuf>,
    #[serde(default)]
    pub ip: Option<ByteBuf>,
}

impl KrpcResponseEnvelope {
    pub fn new(transaction_id: &[u8], body: KrpcResponseBody) -> Self {
        Self {
            t: ByteBuf::from(transaction_id.to_vec()),
            y: ByteBuf::from(b"r".to_vec()),
            r: Some(body),
            v: Some(ByteBuf::from(DEFAULT_KRPC_VERSION.to_vec())),
            ip: None,
        }
    }

    pub fn transaction_id(&self) -> Result<TransactionId, FixedLengthError> {
        TransactionId::try_from(self.t.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KrpcErrorEnvelope {
    pub t: ByteBuf,
    pub y: ByteBuf,
    pub e: KrpcErrorBody,
    #[serde(default)]
    pub v: Option<ByteBuf>,
}

impl KrpcErrorEnvelope {
    pub fn new(transaction_id: &[u8], code: i64, message: impl Into<String>) -> Self {
        Self {
            t: ByteBuf::from(transaction_id.to_vec()),
            y: ByteBuf::from(b"e".to_vec()),
            e: KrpcErrorBody(code, message.into()),
            v: Some(ByteBuf::from(DEFAULT_KRPC_VERSION.to_vec())),
        }
    }

    pub fn transaction_id(&self) -> Result<TransactionId, FixedLengthError> {
        TransactionId::try_from(self.t.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KrpcErrorBody(pub i64, pub String);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KrpcResponseBody {
    #[serde(default)]
    pub id: ByteBuf,
    #[serde(default)]
    pub token: ByteBuf,
    #[serde(default)]
    pub values: Vec<ByteBuf>,
    #[serde(default)]
    pub nodes: ByteBuf,
    #[serde(default)]
    pub nodes6: ByteBuf,
}

impl KrpcResponseBody {
    pub fn pong(node_id: NodeId) -> Self {
        Self {
            id: ByteBuf::from(node_id.as_ref().to_vec()),
            ..Self::default()
        }
    }

    pub fn with_nodes(node_id: NodeId, nodes: &[CompactNode], family: AddressFamily) -> Self {
        let mut body = Self::pong(node_id);
        match family {
            AddressFamily::Ipv4 => body.nodes = encode_compact_nodes(nodes, family),
            AddressFamily::Ipv6 => body.nodes6 = encode_compact_nodes(nodes, family),
        }
        body
    }

    pub fn with_peers(
        node_id: NodeId,
        peers: &[CompactPeer],
        token: &[u8],
    ) -> Self {
        Self {
            id: ByteBuf::from(node_id.as_ref().to_vec()),
            token: ByteBuf::from(token.to_vec()),
            values: peers
                .iter()
                .copied()
                .map(encode_compact_peer)
                .collect(),
            nodes: ByteBuf::new(),
            nodes6: ByteBuf::new(),
        }
    }

    pub fn with_closest_nodes(
        node_id: NodeId,
        nodes: &[CompactNode],
        family: AddressFamily,
        token: &[u8],
    ) -> Self {
        let mut body = Self::with_nodes(node_id, nodes, family);
        body.token = ByteBuf::from(token.to_vec());
        body
    }

    pub fn node_id(&self) -> Option<NodeId> {
        NodeId::try_from(self.id.as_ref()).ok()
    }

    pub fn peers(&self, family: AddressFamily) -> Vec<CompactPeer> {
        self.values
            .iter()
            .flat_map(|entry| decode_compact_peers(entry.as_ref(), family))
            .collect()
    }

    pub fn closest_nodes(&self, family: AddressFamily) -> Vec<CompactNode> {
        match family {
            AddressFamily::Ipv4 => decode_compact_nodes(self.nodes.as_ref(), family),
            AddressFamily::Ipv6 => decode_compact_nodes(self.nodes6.as_ref(), family),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct KrpcEnvelopeProbe {
    t: ByteBuf,
    y: ByteBuf,
    #[serde(default)]
    q: Option<String>,
}

#[derive(Debug, Error)]
pub enum KrpcDecodeError {
    #[error("failed to decode KRPC message")]
    InvalidEnvelope(#[from] serde_bencode::Error),
    #[error("unsupported KRPC query '{0}'")]
    UnsupportedQuery(String),
    #[error("missing KRPC query name")]
    MissingQueryName,
    #[error("unsupported KRPC message type")]
    UnsupportedMessageType,
}

pub fn decode_message(bytes: &[u8]) -> Result<KrpcInboundMessage, KrpcDecodeError> {
    let probe = serde_bencode::from_bytes::<KrpcEnvelopeProbe>(bytes)?;
    match probe.y.as_ref() {
        b"q" => decode_query(bytes, probe.q.as_deref()).map(KrpcInboundMessage::Query),
        b"r" => Ok(KrpcInboundMessage::Response(serde_bencode::from_bytes(bytes)?)),
        b"e" => Ok(KrpcInboundMessage::Error(serde_bencode::from_bytes(bytes)?)),
        _ => Err(KrpcDecodeError::UnsupportedMessageType),
    }
}

fn decode_query(bytes: &[u8], query_name: Option<&str>) -> Result<KrpcIncomingQuery, KrpcDecodeError> {
    match query_name.ok_or(KrpcDecodeError::MissingQueryName)? {
        "ping" => {
            let query = serde_bencode::from_bytes::<KrpcDecodedQueryEnvelope<KrpcPingArgs>>(bytes)?;
            Ok(KrpcIncomingQuery::Ping {
                transaction_id: query.t,
                version: query.v,
                args: query.a,
            })
        }
        "find_node" => {
            let query =
                serde_bencode::from_bytes::<KrpcDecodedQueryEnvelope<KrpcFindNodeArgs>>(bytes)?;
            Ok(KrpcIncomingQuery::FindNode {
                transaction_id: query.t,
                version: query.v,
                args: query.a,
            })
        }
        "get_peers" => {
            let query =
                serde_bencode::from_bytes::<KrpcDecodedQueryEnvelope<KrpcGetPeersArgs>>(bytes)?;
            Ok(KrpcIncomingQuery::GetPeers {
                transaction_id: query.t,
                version: query.v,
                args: query.a,
            })
        }
        "announce_peer" => {
            let query =
                serde_bencode::from_bytes::<KrpcDecodedQueryEnvelope<KrpcAnnouncePeerArgs>>(bytes)?;
            Ok(KrpcIncomingQuery::AnnouncePeer {
                transaction_id: query.t,
                version: query.v,
                args: query.a,
            })
        }
        other => Err(KrpcDecodeError::UnsupportedQuery(other.to_string())),
    }
}

pub fn decode_compact_peers(bytes: &[u8], family: AddressFamily) -> Vec<CompactPeer> {
    match family {
        AddressFamily::Ipv4 if !bytes.is_empty() && bytes.len() % 6 == 0 => bytes
            .chunks_exact(6)
            .map(|chunk| CompactPeer {
                addr: SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3])),
                    u16::from_be_bytes([chunk[4], chunk[5]]),
                ),
            })
            .collect(),
        AddressFamily::Ipv6 if !bytes.is_empty() && bytes.len() % 18 == 0 => bytes
            .chunks_exact(18)
            .map(|chunk| {
                let mut ip = [0u8; 16];
                ip.copy_from_slice(&chunk[..16]);
                CompactPeer {
                    addr: SocketAddr::new(
                        IpAddr::V6(Ipv6Addr::from(ip)),
                        u16::from_be_bytes([chunk[16], chunk[17]]),
                    ),
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub fn encode_compact_peer(peer: CompactPeer) -> ByteBuf {
    match peer.addr {
        SocketAddr::V4(addr) => {
            let mut bytes = Vec::with_capacity(6);
            bytes.extend_from_slice(&addr.ip().octets());
            bytes.extend_from_slice(&addr.port().to_be_bytes());
            ByteBuf::from(bytes)
        }
        SocketAddr::V6(addr) => {
            let mut bytes = Vec::with_capacity(18);
            bytes.extend_from_slice(&addr.ip().octets());
            bytes.extend_from_slice(&addr.port().to_be_bytes());
            ByteBuf::from(bytes)
        }
    }
}

pub fn decode_compact_nodes(bytes: &[u8], family: AddressFamily) -> Vec<CompactNode> {
    match family {
        AddressFamily::Ipv4 if bytes.len() % 26 == 0 => bytes
            .chunks_exact(26)
            .filter_map(|chunk| {
                let id = NodeId::try_from(&chunk[..20]).ok()?;
                Some(CompactNode {
                    id,
                    addr: SocketAddr::new(
                        IpAddr::V4(Ipv4Addr::new(chunk[20], chunk[21], chunk[22], chunk[23])),
                        u16::from_be_bytes([chunk[24], chunk[25]]),
                    ),
                })
            })
            .collect(),
        AddressFamily::Ipv6 if bytes.len() % 38 == 0 => bytes
            .chunks_exact(38)
            .filter_map(|chunk| {
                let id = NodeId::try_from(&chunk[..20]).ok()?;
                let mut ip = [0u8; 16];
                ip.copy_from_slice(&chunk[20..36]);
                Some(CompactNode {
                    id,
                    addr: SocketAddr::new(
                        IpAddr::V6(Ipv6Addr::from(ip)),
                        u16::from_be_bytes([chunk[36], chunk[37]]),
                    ),
                })
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub fn encode_compact_nodes(nodes: &[CompactNode], family: AddressFamily) -> ByteBuf {
    let mut bytes = Vec::new();

    match family {
        AddressFamily::Ipv4 => {
            for node in nodes.iter().filter(|node| node.addr.is_ipv4()) {
                let SocketAddr::V4(addr) = node.addr else {
                    continue;
                };
                bytes.extend_from_slice(node.id.as_ref());
                bytes.extend_from_slice(&addr.ip().octets());
                bytes.extend_from_slice(&addr.port().to_be_bytes());
            }
        }
        AddressFamily::Ipv6 => {
            for node in nodes.iter().filter(|node| node.addr.is_ipv6()) {
                let SocketAddr::V6(addr) = node.addr else {
                    continue;
                };
                bytes.extend_from_slice(node.id.as_ref());
                bytes.extend_from_slice(&addr.ip().octets());
                bytes.extend_from_slice(&addr.port().to_be_bytes());
            }
        }
    }

    ByteBuf::from(bytes)
}
