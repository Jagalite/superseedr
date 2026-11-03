// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::types::{
    AddressFamily, CompactNode, CompactPeer, FixedLengthError, InfoHash, NodeId, TransactionId,
};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use thiserror::Error;

pub const DEFAULT_KRPC_VERSION: &[u8; 4] = b"RS\0\x05";
const MAX_KRPC_MESSAGE_BYTES: usize = 8 * 1024;
const MAX_BENCODE_DEPTH: usize = 16;
const MAX_BENCODE_TOKENS: usize = 512;
const WANT_IPV4_NODES: &[u8; 2] = b"n4";
const WANT_IPV6_NODES: &[u8; 2] = b"n6";

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KrpcQueryEnvelope<A> {
    pub t: ByteBuf,
    pub y: &'static str,
    pub q: &'static str,
    pub a: A,
    pub ro: Option<u8>,
    pub v: Option<ByteBuf>,
}

impl<A> Serialize for KrpcQueryEnvelope<A>
where
    A: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut entries = 4usize;
        if self.ro.is_some() {
            entries += 1;
        }
        if self.v.is_some() {
            entries += 1;
        }
        let mut map = serializer.serialize_map(Some(entries))?;
        map.serialize_entry("a", &self.a)?;
        map.serialize_entry("q", self.q)?;
        if let Some(read_only) = self.ro {
            map.serialize_entry("ro", &read_only)?;
        }
        map.serialize_entry("t", &self.t)?;
        if let Some(version) = &self.v {
            map.serialize_entry("v", version)?;
        }
        map.serialize_entry("y", self.y)?;
        map.end()
    }
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
    ro: Option<u8>,
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
            ro: None,
            v: version.map(|bytes| ByteBuf::from(bytes.to_vec())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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

impl Serialize for KrpcPingArgs {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("id", &self.id)?;
        map.end()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct KrpcFindNodeArgs {
    pub id: ByteBuf,
    pub target: ByteBuf,
    #[serde(default)]
    pub want: Vec<ByteBuf>,
}

impl KrpcFindNodeArgs {
    pub fn new(id: NodeId, target: NodeId) -> Self {
        Self {
            id: ByteBuf::from(id.as_ref().to_vec()),
            target: ByteBuf::from(target.as_ref().to_vec()),
            want: Vec::new(),
        }
    }

    pub fn with_want(mut self, families: &[AddressFamily]) -> Self {
        self.want = encode_want_entries(families);
        self
    }

    pub fn wants_family(&self, family: AddressFamily) -> bool {
        wants_family(&self.want, family)
    }
}

impl Serialize for KrpcFindNodeArgs {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(2 + usize::from(!self.want.is_empty())))?;
        map.serialize_entry("id", &self.id)?;
        map.serialize_entry("target", &self.target)?;
        if !self.want.is_empty() {
            map.serialize_entry("want", &self.want)?;
        }
        map.end()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct KrpcGetPeersArgs {
    pub id: ByteBuf,
    pub info_hash: ByteBuf,
    #[serde(default)]
    pub want: Vec<ByteBuf>,
}

impl KrpcGetPeersArgs {
    pub fn new(id: NodeId, info_hash: InfoHash) -> Self {
        Self {
            id: ByteBuf::from(id.as_ref().to_vec()),
            info_hash: ByteBuf::from(info_hash.as_ref().to_vec()),
            want: Vec::new(),
        }
    }

    pub fn with_want(mut self, families: &[AddressFamily]) -> Self {
        self.want = encode_want_entries(families);
        self
    }

    pub fn wants_family(&self, family: AddressFamily) -> bool {
        wants_family(&self.want, family)
    }
}

impl Serialize for KrpcGetPeersArgs {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(2 + usize::from(!self.want.is_empty())))?;
        map.serialize_entry("id", &self.id)?;
        map.serialize_entry("info_hash", &self.info_hash)?;
        if !self.want.is_empty() {
            map.serialize_entry("want", &self.want)?;
        }
        map.end()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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

impl Serialize for KrpcAnnouncePeerArgs {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map =
            serializer.serialize_map(Some(if self.implied_port.is_some() { 5 } else { 4 }))?;
        map.serialize_entry("id", &self.id)?;
        if let Some(implied_port) = self.implied_port {
            map.serialize_entry("implied_port", &implied_port)?;
        }
        map.serialize_entry("info_hash", &self.info_hash)?;
        map.serialize_entry("port", &self.port)?;
        map.serialize_entry("token", &self.token)?;
        map.end()
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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

impl Serialize for KrpcResponseEnvelope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut entries = 2usize;
        if self.ip.is_some() {
            entries += 1;
        }
        if self.r.is_some() {
            entries += 1;
        }
        if self.v.is_some() {
            entries += 1;
        }
        let mut map = serializer.serialize_map(Some(entries))?;
        if let Some(ip) = &self.ip {
            map.serialize_entry("ip", ip)?;
        }
        if let Some(body) = &self.r {
            map.serialize_entry("r", body)?;
        }
        map.serialize_entry("t", &self.t)?;
        if let Some(version) = &self.v {
            map.serialize_entry("v", version)?;
        }
        map.serialize_entry("y", &self.y)?;
        map.end()
    }
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

    pub fn with_observed_addr(mut self, addr: SocketAddr) -> Self {
        self.ip = Some(encode_compact_socket_addr(addr));
        self
    }

    pub fn observed_addr(&self) -> Option<SocketAddr> {
        self.ip
            .as_ref()
            .and_then(|bytes| decode_compact_socket_addr(bytes.as_ref()))
    }

    pub fn transaction_id(&self) -> Result<TransactionId, FixedLengthError> {
        TransactionId::try_from(self.t.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct KrpcErrorEnvelope {
    pub t: ByteBuf,
    pub y: ByteBuf,
    pub e: KrpcErrorBody,
    #[serde(default)]
    pub v: Option<ByteBuf>,
}

impl Serialize for KrpcErrorEnvelope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(if self.v.is_some() { 4 } else { 3 }))?;
        map.serialize_entry("e", &self.e)?;
        map.serialize_entry("t", &self.t)?;
        if let Some(version) = &self.v {
            map.serialize_entry("v", version)?;
        }
        map.serialize_entry("y", &self.y)?;
        map.end()
    }
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
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

impl Serialize for KrpcResponseBody {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut entries = 0usize;
        if !self.id.is_empty() {
            entries += 1;
        }
        if !self.nodes.is_empty() {
            entries += 1;
        }
        if !self.nodes6.is_empty() {
            entries += 1;
        }
        if !self.token.is_empty() {
            entries += 1;
        }
        if !self.values.is_empty() {
            entries += 1;
        }

        let mut map = serializer.serialize_map(Some(entries))?;
        if !self.id.is_empty() {
            map.serialize_entry("id", &self.id)?;
        }
        if !self.nodes.is_empty() {
            map.serialize_entry("nodes", &self.nodes)?;
        }
        if !self.nodes6.is_empty() {
            map.serialize_entry("nodes6", &self.nodes6)?;
        }
        if !self.token.is_empty() {
            map.serialize_entry("token", &self.token)?;
        }
        if !self.values.is_empty() {
            map.serialize_entry("values", &self.values)?;
        }
        map.end()
    }
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

    pub fn with_peers(node_id: NodeId, peers: &[CompactPeer], token: &[u8]) -> Self {
        Self {
            id: ByteBuf::from(node_id.as_ref().to_vec()),
            token: ByteBuf::from(token.to_vec()),
            values: peers.iter().copied().map(encode_compact_peer).collect(),
            nodes: ByteBuf::new(),
            nodes6: ByteBuf::new(),
        }
    }

    pub fn with_peers_and_nodes(
        node_id: NodeId,
        peers: &[CompactPeer],
        nodes: &[CompactNode],
        family: AddressFamily,
        token: &[u8],
    ) -> Self {
        let mut body = Self::with_peers(node_id, peers, token);
        match family {
            AddressFamily::Ipv4 => body.nodes = encode_compact_nodes(nodes, family),
            AddressFamily::Ipv6 => body.nodes6 = encode_compact_nodes(nodes, family),
        }
        body
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

    pub fn set_closest_nodes(&mut self, family: AddressFamily, nodes: &[CompactNode]) {
        match family {
            AddressFamily::Ipv4 => self.nodes = encode_compact_nodes(nodes, family),
            AddressFamily::Ipv6 => self.nodes6 = encode_compact_nodes(nodes, family),
        }
    }
}

fn encode_want_entries(families: &[AddressFamily]) -> Vec<ByteBuf> {
    families
        .iter()
        .copied()
        .map(|family| match family {
            AddressFamily::Ipv4 => ByteBuf::from(WANT_IPV4_NODES.to_vec()),
            AddressFamily::Ipv6 => ByteBuf::from(WANT_IPV6_NODES.to_vec()),
        })
        .collect()
}

fn wants_family(entries: &[ByteBuf], family: AddressFamily) -> bool {
    let needle = match family {
        AddressFamily::Ipv4 => WANT_IPV4_NODES.as_slice(),
        AddressFamily::Ipv6 => WANT_IPV6_NODES.as_slice(),
    };
    entries.iter().any(|entry| entry.as_ref() == needle)
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
    #[error("KRPC message exceeds size limit")]
    MessageTooLarge,
    #[error("KRPC message exceeds bencode depth limit")]
    BencodeDepthExceeded,
    #[error("KRPC message exceeds bencode token limit")]
    BencodeTokenLimitExceeded,
    #[error("invalid KRPC bencode structure")]
    InvalidBencodeStructure,
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
    validate_bencode_limits(bytes)?;
    let probe = serde_bencode::from_bytes::<KrpcEnvelopeProbe>(bytes)?;
    match probe.y.as_ref() {
        b"q" => decode_query(bytes, probe.q.as_deref()).map(KrpcInboundMessage::Query),
        b"r" => Ok(KrpcInboundMessage::Response(serde_bencode::from_bytes(
            bytes,
        )?)),
        b"e" => Ok(KrpcInboundMessage::Error(serde_bencode::from_bytes(bytes)?)),
        _ => Err(KrpcDecodeError::UnsupportedMessageType),
    }
}

fn validate_bencode_limits(bytes: &[u8]) -> Result<(), KrpcDecodeError> {
    if bytes.len() > MAX_KRPC_MESSAGE_BYTES {
        return Err(KrpcDecodeError::MessageTooLarge);
    }

    let mut tokens = 0usize;
    let pos = validate_bencode_value(bytes, 0, 0, &mut tokens)?;
    if pos != bytes.len() {
        return Err(KrpcDecodeError::InvalidBencodeStructure);
    }
    Ok(())
}

fn validate_bencode_value(
    bytes: &[u8],
    mut pos: usize,
    depth: usize,
    tokens: &mut usize,
) -> Result<usize, KrpcDecodeError> {
    if depth > MAX_BENCODE_DEPTH {
        return Err(KrpcDecodeError::BencodeDepthExceeded);
    }
    if pos >= bytes.len() {
        return Err(KrpcDecodeError::InvalidBencodeStructure);
    }

    *tokens = tokens.saturating_add(1);
    if *tokens > MAX_BENCODE_TOKENS {
        return Err(KrpcDecodeError::BencodeTokenLimitExceeded);
    }

    match bytes[pos] {
        b'i' => validate_bencode_integer(bytes, pos + 1),
        b'l' | b'd' => {
            pos += 1;
            while pos < bytes.len() && bytes[pos] != b'e' {
                pos = validate_bencode_value(bytes, pos, depth + 1, tokens)?;
            }
            if pos >= bytes.len() || bytes[pos] != b'e' {
                return Err(KrpcDecodeError::InvalidBencodeStructure);
            }
            Ok(pos + 1)
        }
        b'0'..=b'9' => validate_bencode_bytes(bytes, pos),
        _ => Err(KrpcDecodeError::InvalidBencodeStructure),
    }
}

fn validate_bencode_integer(bytes: &[u8], mut pos: usize) -> Result<usize, KrpcDecodeError> {
    if pos >= bytes.len() {
        return Err(KrpcDecodeError::InvalidBencodeStructure);
    }
    if bytes[pos] == b'-' {
        pos += 1;
    }
    let start = pos;
    while pos < bytes.len() && bytes[pos].is_ascii_digit() {
        pos += 1;
    }
    if pos == start || pos >= bytes.len() || bytes[pos] != b'e' {
        return Err(KrpcDecodeError::InvalidBencodeStructure);
    }
    Ok(pos + 1)
}

fn validate_bencode_bytes(bytes: &[u8], mut pos: usize) -> Result<usize, KrpcDecodeError> {
    let mut len = 0usize;
    while pos < bytes.len() && bytes[pos].is_ascii_digit() {
        len = len
            .checked_mul(10)
            .and_then(|value| value.checked_add(usize::from(bytes[pos] - b'0')))
            .ok_or(KrpcDecodeError::InvalidBencodeStructure)?;
        pos += 1;
    }
    if pos >= bytes.len() || bytes[pos] != b':' {
        return Err(KrpcDecodeError::InvalidBencodeStructure);
    }
    let value_start = pos + 1;
    value_start
        .checked_add(len)
        .filter(|end| *end <= bytes.len())
        .ok_or(KrpcDecodeError::InvalidBencodeStructure)
}

fn decode_query(
    bytes: &[u8],
    query_name: Option<&str>,
) -> Result<KrpcIncomingQuery, KrpcDecodeError> {
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
        AddressFamily::Ipv4 if !bytes.is_empty() && bytes.len().is_multiple_of(6) => bytes
            .as_chunks::<6>()
            .0
            .iter()
            .map(|chunk| CompactPeer {
                addr: SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3])),
                    u16::from_be_bytes([chunk[4], chunk[5]]),
                ),
            })
            .collect(),
        AddressFamily::Ipv6 if !bytes.is_empty() && bytes.len().is_multiple_of(18) => bytes
            .as_chunks::<18>()
            .0
            .iter()
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
        AddressFamily::Ipv4 if bytes.len().is_multiple_of(26) => bytes
            .as_chunks::<26>()
            .0
            .iter()
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
        AddressFamily::Ipv6 if bytes.len().is_multiple_of(38) => bytes
            .as_chunks::<38>()
            .0
            .iter()
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

pub fn decode_compact_socket_addr(bytes: &[u8]) -> Option<SocketAddr> {
    match bytes.len() {
        6 => {
            let ip = Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]);
            let port = u16::from_be_bytes([bytes[4], bytes[5]]);
            Some(SocketAddr::from((ip, port)))
        }
        18 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&bytes[..16]);
            let port = u16::from_be_bytes([bytes[16], bytes[17]]);
            Some(SocketAddr::from((Ipv6Addr::from(octets), port)))
        }
        _ => None,
    }
}

pub fn encode_compact_socket_addr(addr: SocketAddr) -> ByteBuf {
    match addr {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_queries_do_not_advertise_read_only_by_default() {
        let envelope = KrpcQueryEnvelope::new(
            TransactionId::from([1, 2, 3, 4]),
            KrpcQueryKind::Ping,
            KrpcPingArgs::new(NodeId::from([3; NodeId::LEN])),
        );

        assert_eq!(envelope.ro, None);
        let encoded = serde_bencode::to_bytes(&envelope).expect("encode query envelope");
        assert!(
            !encoded
                .windows(b"2:ro".len())
                .any(|window| window == b"2:ro"),
            "encoded query must omit BEP 43 read-only flag"
        );
    }

    #[test]
    fn get_peers_want_entries_round_trip() {
        let args = KrpcGetPeersArgs::new(
            NodeId::from([1; NodeId::LEN]),
            InfoHash::from([2; InfoHash::LEN]),
        )
        .with_want(&[AddressFamily::Ipv4, AddressFamily::Ipv6]);
        let encoded = serde_bencode::to_bytes(&args).expect("encode get_peers args");
        let decoded =
            serde_bencode::from_bytes::<KrpcGetPeersArgs>(&encoded).expect("decode get_peers args");

        assert!(decoded.wants_family(AddressFamily::Ipv4));
        assert!(decoded.wants_family(AddressFamily::Ipv6));
    }

    #[test]
    fn find_node_want_entries_round_trip() {
        let args = KrpcFindNodeArgs::new(
            NodeId::from([1; NodeId::LEN]),
            NodeId::from([2; NodeId::LEN]),
        )
        .with_want(&[AddressFamily::Ipv6]);
        let encoded = serde_bencode::to_bytes(&args).expect("encode find_node args");
        let decoded =
            serde_bencode::from_bytes::<KrpcFindNodeArgs>(&encoded).expect("decode find_node args");

        assert!(!decoded.wants_family(AddressFamily::Ipv4));
        assert!(decoded.wants_family(AddressFamily::Ipv6));
    }

    #[test]
    fn response_observed_addr_round_trips_compact_ip() {
        let observed = SocketAddr::from((Ipv4Addr::new(127, 0, 0, 1), 6881));
        let response = KrpcResponseEnvelope::new(
            &[1, 2, 3, 4],
            KrpcResponseBody::pong(NodeId::from([3; NodeId::LEN])),
        )
        .with_observed_addr(observed);
        let encoded = serde_bencode::to_bytes(&response).expect("encode response");
        let decoded =
            serde_bencode::from_bytes::<KrpcResponseEnvelope>(&encoded).expect("decode response");

        assert_eq!(decoded.observed_addr(), Some(observed));
    }

    #[test]
    fn decode_message_rejects_oversized_payload_before_deserialize() {
        let payload = vec![b'0'; MAX_KRPC_MESSAGE_BYTES + 1];

        assert!(matches!(
            decode_message(&payload),
            Err(KrpcDecodeError::MessageTooLarge)
        ));
    }

    #[test]
    fn decode_message_rejects_excessive_bencode_depth() {
        let mut payload = Vec::new();
        payload.extend(std::iter::repeat_n(b'l', MAX_BENCODE_DEPTH + 2));
        payload.extend_from_slice(b"0:");
        payload.extend(std::iter::repeat_n(b'e', MAX_BENCODE_DEPTH + 2));

        assert!(matches!(
            decode_message(&payload),
            Err(KrpcDecodeError::BencodeDepthExceeded)
        ));
    }

    #[test]
    fn decode_message_rejects_excessive_bencode_tokens() {
        let mut payload = Vec::from(*b"l");
        for _ in 0..MAX_BENCODE_TOKENS {
            payload.extend_from_slice(b"0:");
        }
        payload.push(b'e');

        assert!(matches!(
            decode_message(&payload),
            Err(KrpcDecodeError::BencodeTokenLimitExceeded)
        ));
    }
}
