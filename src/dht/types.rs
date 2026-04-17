// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;
use std::net::SocketAddr;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum AddressFamily {
    #[default]
    Ipv4,
    Ipv6,
}

impl AddressFamily {
    pub const fn for_addr(addr: SocketAddr) -> Self {
        if addr.is_ipv4() {
            Self::Ipv4
        } else {
            Self::Ipv6
        }
    }

    pub const fn is_ipv6(self) -> bool {
        matches!(self, Self::Ipv6)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FixedLengthError {
    pub expected: usize,
    pub actual: usize,
}

impl fmt::Display for FixedLengthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "expected {} bytes but received {}",
            self.expected, self.actual
        )
    }
}

impl Error for FixedLengthError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId([u8; 20]);

impl NodeId {
    pub const LEN: usize = 20;

    pub const fn new(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    pub const fn into_bytes(self) -> [u8; 20] {
        self.0
    }

    pub const fn as_array(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn first_21_bits(&self) -> [u8; 3] {
        [self.0[0], self.0[1], self.0[2] & 0xf8]
    }
}

impl From<[u8; 20]> for NodeId {
    fn from(value: [u8; 20]) -> Self {
        Self::new(value)
    }
}

impl From<InfoHash> for NodeId {
    fn from(value: InfoHash) -> Self {
        Self::new(value.into_bytes())
    }
}

impl AsRef<[u8]> for NodeId {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl TryFrom<&[u8]> for NodeId {
    type Error = FixedLengthError;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        if value.len() != Self::LEN {
            return Err(FixedLengthError {
                expected: Self::LEN,
                actual: value.len(),
            });
        }

        let mut bytes = [0u8; Self::LEN];
        bytes.copy_from_slice(value);
        Ok(Self::new(bytes))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InfoHash([u8; 20]);

impl InfoHash {
    pub const LEN: usize = 20;

    pub const fn new(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    pub const fn into_bytes(self) -> [u8; 20] {
        self.0
    }

    pub const fn as_array(&self) -> &[u8; 20] {
        &self.0
    }
}

impl From<[u8; 20]> for InfoHash {
    fn from(value: [u8; 20]) -> Self {
        Self::new(value)
    }
}

impl From<NodeId> for InfoHash {
    fn from(value: NodeId) -> Self {
        Self::new(value.into_bytes())
    }
}

impl AsRef<[u8]> for InfoHash {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl TryFrom<&[u8]> for InfoHash {
    type Error = FixedLengthError;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        if value.len() != Self::LEN {
            return Err(FixedLengthError {
                expected: Self::LEN,
                actual: value.len(),
            });
        }

        let mut bytes = [0u8; Self::LEN];
        bytes.copy_from_slice(value);
        Ok(Self::new(bytes))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TransactionId([u8; 4]);

impl TransactionId {
    pub const LEN: usize = 4;

    pub const fn new(bytes: [u8; 4]) -> Self {
        Self(bytes)
    }

    pub const fn into_bytes(self) -> [u8; 4] {
        self.0
    }

    pub const fn as_array(&self) -> &[u8; 4] {
        &self.0
    }
}

impl From<[u8; 4]> for TransactionId {
    fn from(value: [u8; 4]) -> Self {
        Self::new(value)
    }
}

impl AsRef<[u8]> for TransactionId {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl TryFrom<&[u8]> for TransactionId {
    type Error = FixedLengthError;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        if value.len() != Self::LEN {
            return Err(FixedLengthError {
                expected: Self::LEN,
                actual: value.len(),
            });
        }

        let mut bytes = [0u8; Self::LEN];
        bytes.copy_from_slice(value);
        Ok(Self::new(bytes))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct LookupId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactNode {
    pub id: NodeId,
    pub addr: SocketAddr,
}

impl CompactNode {
    pub const fn family(&self) -> AddressFamily {
        AddressFamily::for_addr(self.addr)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactPeer {
    pub addr: SocketAddr,
}

impl CompactPeer {
    pub const fn family(&self) -> AddressFamily {
        AddressFamily::for_addr(self.addr)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum NodeTrust {
    Trusted,
    #[default]
    Neutral,
    Suspicious,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Bep42State {
    #[default]
    Unknown,
    Compliant,
    NonCompliant,
    ExemptLocal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRecord {
    pub addr: SocketAddr,
    pub node_id: Option<NodeId>,
    pub last_query_sent_at: Option<Instant>,
    pub last_query_response_at: Option<Instant>,
    pub last_inbound_query_at: Option<Instant>,
    pub consecutive_failures: u16,
    pub last_changed_at: Instant,
    pub trust: NodeTrust,
    pub bep42_state: Bep42State,
    pub dead_referral_count: u16,
    pub live_referral_count: u16,
    pub id_churn_count: u16,
}

impl NodeRecord {
    pub const fn family(&self) -> AddressFamily {
        AddressFamily::for_addr(self.addr)
    }

    pub fn new(addr: SocketAddr, node_id: Option<NodeId>, now: Instant) -> Self {
        Self {
            addr,
            node_id,
            last_query_sent_at: None,
            last_query_response_at: None,
            last_inbound_query_at: None,
            consecutive_failures: 0,
            last_changed_at: now,
            trust: NodeTrust::Neutral,
            bep42_state: Bep42State::Unknown,
            dead_referral_count: 0,
            live_referral_count: 0,
            id_churn_count: 0,
        }
    }

    pub fn note_query_sent(&mut self, now: Instant) {
        self.last_query_sent_at = Some(now);
        self.last_changed_at = now;
    }

    pub fn note_query_response(&mut self, node_id: Option<NodeId>, now: Instant) {
        if let (Some(existing), Some(candidate)) = (self.node_id, node_id) {
            if existing != candidate {
                self.id_churn_count = self.id_churn_count.saturating_add(1);
            }
        }

        if let Some(node_id) = node_id {
            self.node_id = Some(node_id);
        }
        self.last_query_response_at = Some(now);
        self.consecutive_failures = 0;
        self.last_changed_at = now;
    }

    pub fn note_inbound_query(&mut self, now: Instant) {
        self.last_inbound_query_at = Some(now);
        self.last_changed_at = now;
    }

    pub fn note_failure(&mut self, now: Instant) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.last_changed_at = now;
    }

    pub fn note_live_referral(&mut self, now: Instant) {
        self.live_referral_count = self.live_referral_count.saturating_add(1);
        self.last_changed_at = now;
    }

    pub fn note_dead_referral(&mut self, now: Instant) {
        self.dead_referral_count = self.dead_referral_count.saturating_add(1);
        self.last_changed_at = now;
    }
}
