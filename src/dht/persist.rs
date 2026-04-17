// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::routing::RoutingSnapshot;
use super::types::{AddressFamily, Bep42State, NodeId, NodeRecord, NodeTrust};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PERSISTENCE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistenceConfig {
    pub path: PathBuf,
    pub max_age: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedRoutingNode {
    pub addr: SocketAddr,
    pub node_id: Option<NodeId>,
    pub trust: NodeTrust,
    pub bep42_state: Bep42State,
    pub consecutive_failures: u16,
    pub dead_referral_count: u16,
    pub live_referral_count: u16,
    pub id_churn_count: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedRoutingTable {
    pub family: AddressFamily,
    pub nodes: Vec<PersistedRoutingNode>,
    #[serde(default)]
    pub replacements: Vec<PersistedRoutingNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedStateEnvelope {
    pub version: u32,
    pub created_at_unix_secs: u64,
    pub node_id: NodeId,
    pub ipv4_routes: PersistedRoutingTable,
    pub ipv6_routes: PersistedRoutingTable,
}

#[derive(Debug, Clone)]
pub struct PersistenceManager {
    config: PersistenceConfig,
}

impl PersistenceManager {
    pub fn new(config: PersistenceConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &PersistenceConfig {
        &self.config
    }

    pub fn build_snapshot(
        &self,
        node_id: NodeId,
        ipv4_routes: &RoutingSnapshot,
        ipv6_routes: &RoutingSnapshot,
        now: SystemTime,
    ) -> PersistedStateEnvelope {
        PersistedStateEnvelope {
            version: PERSISTENCE_VERSION,
            created_at_unix_secs: now
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            node_id,
            ipv4_routes: PersistedRoutingTable {
                family: AddressFamily::Ipv4,
                nodes: ipv4_routes
                    .nodes
                    .iter()
                    .map(PersistedRoutingNode::from_record)
                    .collect(),
                replacements: Vec::new(),
            },
            ipv6_routes: PersistedRoutingTable {
                family: AddressFamily::Ipv6,
                nodes: ipv6_routes
                    .nodes
                    .iter()
                    .map(PersistedRoutingNode::from_record)
                    .collect(),
                replacements: Vec::new(),
            },
        }
    }

    pub fn save_snapshot(&self, snapshot: &PersistedStateEnvelope) -> io::Result<()> {
        let new_total = snapshot.ipv4_routes.nodes.len() + snapshot.ipv6_routes.nodes.len();
        if new_total == 0 {
            return Ok(());
        }

        if let Some(existing) = self.load_snapshot(SystemTime::now())? {
            let existing_total =
                existing.ipv4_routes.nodes.len() + existing.ipv6_routes.nodes.len();
            if existing.node_id == snapshot.node_id
                && existing_total > new_total
                && new_total < 16
            {
                return Ok(());
            }
        }

        if let Some(parent) = self.config.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(snapshot)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        fs::write(&self.config.path, bytes)
    }

    pub fn load_snapshot(&self, now: SystemTime) -> io::Result<Option<PersistedStateEnvelope>> {
        let bytes = match fs::read(&self.config.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };

        let snapshot = match serde_json::from_slice::<PersistedStateEnvelope>(&bytes) {
            Ok(snapshot) => snapshot,
            Err(_) => return Ok(None),
        };

        if snapshot.version != PERSISTENCE_VERSION {
            return Ok(None);
        }

        let created_at = UNIX_EPOCH + Duration::from_secs(snapshot.created_at_unix_secs);
        if now.duration_since(created_at).unwrap_or_default() > self.config.max_age {
            return Ok(None);
        }

        Ok(Some(snapshot))
    }

    pub fn restore_nodes(
        &self,
        routes: &PersistedRoutingTable,
        now: Instant,
    ) -> Vec<NodeRecord> {
        routes
            .nodes
            .iter()
            .map(|node| node.to_record(now))
            .collect()
    }
}

impl PersistedRoutingNode {
    fn from_record(record: &NodeRecord) -> Self {
        Self {
            addr: record.addr,
            node_id: record.node_id,
            trust: record.trust,
            bep42_state: record.bep42_state,
            consecutive_failures: record.consecutive_failures,
            dead_referral_count: record.dead_referral_count,
            live_referral_count: record.live_referral_count,
            id_churn_count: record.id_churn_count,
        }
    }

    fn to_record(&self, now: Instant) -> NodeRecord {
        let mut record = NodeRecord::new(self.addr, self.node_id, now);
        record.trust = self.trust;
        record.bep42_state = self.bep42_state;
        record.consecutive_failures = self.consecutive_failures;
        record.dead_referral_count = self.dead_referral_count;
        record.live_referral_count = self.live_referral_count;
        record.id_churn_count = self.id_churn_count;
        record
    }
}
