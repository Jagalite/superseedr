// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::types::{AddressFamily, NodeId, NodeRecord};
use rand::Rng;
use std::cmp::Ordering;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

pub const GOOD_NODE_WINDOW: Duration = Duration::from_secs(15 * 60);
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);
pub const BAD_NODE_FAILURE_THRESHOLD: u16 = 2;

#[derive(Debug, Clone)]
pub struct RoutingConfig {
    pub family: AddressFamily,
    pub bucket_size: usize,
    pub replacement_limit: usize,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            family: AddressFamily::Ipv4,
            bucket_size: 8,
            replacement_limit: 8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStatus {
    Good,
    Questionable,
    Bad,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketRange {
    pub min: NodeId,
    pub max: NodeId,
    pub prefix_len: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketSummary {
    pub range: BucketRange,
    pub node_count: usize,
    pub replacement_count: usize,
    pub last_changed_at: Instant,
}

#[derive(Debug, Clone)]
pub struct RoutingSnapshot {
    pub family: AddressFamily,
    pub buckets: Vec<BucketSummary>,
    pub nodes: Vec<NodeRecord>,
    pub replacement_count: usize,
    pub refresh_due_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshPlan {
    pub bucket_index: usize,
    pub range: BucketRange,
    pub target: NodeId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted,
    Updated,
    ReplacedBad { evicted: SocketAddr },
    QueuedReplacement,
    NeedsProbe { targets: Vec<SocketAddr> },
    Discarded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BucketPrefix {
    bits: [u8; 20],
    bit_len: u8,
}

impl BucketPrefix {
    fn root() -> Self {
        Self {
            bits: [0u8; 20],
            bit_len: 0,
        }
    }

    fn contains(&self, node_id: &NodeId) -> bool {
        prefix_matches(&self.bits, self.bit_len, node_id.as_array())
    }

    fn split(&self) -> Option<(Self, Self)> {
        if self.bit_len >= 160 {
            return None;
        }

        let left = Self {
            bits: self.bits,
            bit_len: self.bit_len + 1,
        };

        let mut right_bits = self.bits;
        set_bit(&mut right_bits, self.bit_len, true);
        let right = Self {
            bits: right_bits,
            bit_len: self.bit_len + 1,
        };

        Some((left, right))
    }

    fn range(&self) -> BucketRange {
        let mut min = self.bits;
        let mut max = self.bits;
        for bit_idx in self.bit_len..160 {
            set_bit(&mut min, bit_idx, false);
            set_bit(&mut max, bit_idx, true);
        }

        BucketRange {
            min: NodeId::from(min),
            max: NodeId::from(max),
            prefix_len: self.bit_len,
        }
    }

    fn random_target(&self) -> NodeId {
        let mut bytes = [0u8; 20];
        rand::rng().fill(&mut bytes);
        for bit_idx in 0..self.bit_len {
            set_bit(&mut bytes, bit_idx, bit_at(&self.bits, bit_idx));
        }
        NodeId::from(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Bucket {
    prefix: BucketPrefix,
    nodes: Vec<NodeRecord>,
    replacements: Vec<NodeRecord>,
    last_changed_at: Instant,
}

impl Bucket {
    fn new(prefix: BucketPrefix, now: Instant) -> Self {
        Self {
            prefix,
            nodes: Vec::new(),
            replacements: Vec::new(),
            last_changed_at: now,
        }
    }

    fn contains_local_id(&self, local_node_id: &NodeId) -> bool {
        self.prefix.contains(local_node_id)
    }

    fn summary(&self) -> BucketSummary {
        BucketSummary {
            range: self.prefix.range(),
            node_count: self.nodes.len(),
            replacement_count: self.replacements.len(),
            last_changed_at: self.last_changed_at,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RoutingTable {
    local_node_id: NodeId,
    config: RoutingConfig,
    buckets: Vec<Bucket>,
}

impl RoutingTable {
    pub fn new(local_node_id: NodeId, config: RoutingConfig, now: Instant) -> Self {
        Self {
            local_node_id,
            config,
            buckets: vec![Bucket::new(BucketPrefix::root(), now)],
        }
    }

    pub fn family(&self) -> AddressFamily {
        self.config.family
    }

    pub fn local_node_id(&self) -> NodeId {
        self.local_node_id
    }

    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    pub fn all_nodes(&self) -> Vec<NodeRecord> {
        self.buckets
            .iter()
            .flat_map(|bucket| bucket.nodes.iter().cloned())
            .collect()
    }

    pub fn snapshot(&self, now: Instant) -> RoutingSnapshot {
        let refresh_due_count = self
            .buckets
            .iter()
            .filter(|bucket| bucket.last_changed_at + REFRESH_INTERVAL <= now)
            .count();

        RoutingSnapshot {
            family: self.config.family,
            buckets: self.buckets.iter().map(Bucket::summary).collect(),
            nodes: self.all_nodes(),
            replacement_count: self.buckets.iter().map(|bucket| bucket.replacements.len()).sum(),
            refresh_due_count,
        }
    }

    pub fn insert(&mut self, mut candidate: NodeRecord, now: Instant) -> InsertOutcome {
        let Some(node_id) = candidate.node_id else {
            return InsertOutcome::Discarded;
        };
        if candidate.family() != self.config.family {
            return InsertOutcome::Discarded;
        }

        candidate.last_changed_at = now;

        loop {
            let bucket_index = self.bucket_index_for(&node_id);
            if let Some(outcome) = self.update_existing(bucket_index, &candidate, now) {
                return outcome;
            }

            if self.buckets[bucket_index].nodes.len() < self.config.bucket_size {
                self.buckets[bucket_index].nodes.push(candidate.clone());
                self.buckets[bucket_index].last_changed_at = now;
                return InsertOutcome::Inserted;
            }

            if self.buckets[bucket_index].contains_local_id(&self.local_node_id)
                && self.split_bucket(bucket_index, now)
            {
                continue;
            }

            if let Some(bad_index) = self.buckets[bucket_index]
                .nodes
                .iter()
                .position(|record| node_status(record, now) == NodeStatus::Bad)
            {
                let evicted = self.buckets[bucket_index].nodes[bad_index].addr;
                self.buckets[bucket_index].nodes[bad_index] = candidate.clone();
                self.buckets[bucket_index].last_changed_at = now;
                return InsertOutcome::ReplacedBad { evicted };
            }

            let questionable_targets = questionable_probe_targets(&self.buckets[bucket_index], now);
            self.queue_replacement(bucket_index, candidate.clone(), now);
            if !questionable_targets.is_empty() {
                return InsertOutcome::NeedsProbe {
                    targets: questionable_targets,
                };
            }
            return InsertOutcome::QueuedReplacement;
        }
    }

    pub fn record_query_sent(&mut self, addr: SocketAddr, now: Instant) -> bool {
        self.with_record_mut(addr, |bucket, record| {
            record.note_query_sent(now);
            bucket.last_changed_at = now;
        })
    }

    pub fn record_response(
        &mut self,
        addr: SocketAddr,
        node_id: Option<NodeId>,
        now: Instant,
    ) -> bool {
        self.with_record_mut(addr, |bucket, record| {
            record.note_query_response(node_id, now);
            bucket.last_changed_at = now;
        })
    }

    pub fn record_inbound_query(
        &mut self,
        addr: SocketAddr,
        node_id: Option<NodeId>,
        now: Instant,
    ) -> bool {
        self.with_record_mut(addr, |bucket, record| {
            if record.node_id.is_none() {
                record.node_id = node_id;
            }
            record.note_inbound_query(now);
            bucket.last_changed_at = now;
        })
    }

    pub fn record_failure(&mut self, addr: SocketAddr, now: Instant) -> bool {
        self.with_record_mut(addr, |bucket, record| {
            record.note_failure(now);
            bucket.last_changed_at = now;
        })
    }

    pub fn closest_nodes(&self, target: NodeId, limit: usize) -> Vec<NodeRecord> {
        let mut nodes = self.all_nodes();
        nodes.sort_by(|left, right| compare_distance(left.node_id, right.node_id, &target));
        nodes.truncate(limit);
        nodes
    }

    pub fn closest_good_nodes(&self, target: NodeId, limit: usize, now: Instant) -> Vec<NodeRecord> {
        let mut nodes = self
            .all_nodes()
            .into_iter()
            .filter(|record| node_status(record, now) == NodeStatus::Good)
            .collect::<Vec<_>>();
        nodes.sort_by(|left, right| compare_distance(left.node_id, right.node_id, &target));
        nodes.truncate(limit);
        nodes
    }

    pub fn refresh_plans(&self, now: Instant) -> Vec<RefreshPlan> {
        self.buckets
            .iter()
            .enumerate()
            .filter(|(_, bucket)| bucket.last_changed_at + REFRESH_INTERVAL <= now)
            .map(|(bucket_index, bucket)| RefreshPlan {
                bucket_index,
                range: bucket.prefix.range(),
                target: bucket.prefix.random_target(),
            })
            .collect()
    }

    fn bucket_index_for(&self, node_id: &NodeId) -> usize {
        self.buckets
            .iter()
            .position(|bucket| bucket.prefix.contains(node_id))
            .expect("routing bucket for node id")
    }

    fn split_bucket(&mut self, bucket_index: usize, now: Instant) -> bool {
        let bucket = self.buckets.remove(bucket_index);
        let Some((left_prefix, right_prefix)) = bucket.prefix.split() else {
            self.buckets.insert(bucket_index, bucket);
            return false;
        };

        let mut left = Bucket::new(left_prefix, now);
        let mut right = Bucket::new(right_prefix, now);

        for record in bucket.nodes {
            let Some(node_id) = record.node_id else {
                continue;
            };
            if left.prefix.contains(&node_id) {
                left.nodes.push(record);
            } else {
                right.nodes.push(record);
            }
        }

        for record in bucket.replacements {
            let Some(node_id) = record.node_id else {
                continue;
            };
            if left.prefix.contains(&node_id) {
                left.replacements.push(record);
            } else {
                right.replacements.push(record);
            }
        }

        left.last_changed_at = now;
        right.last_changed_at = now;
        self.buckets.insert(bucket_index, right);
        self.buckets.insert(bucket_index, left);
        true
    }

    fn update_existing(
        &mut self,
        bucket_index: usize,
        candidate: &NodeRecord,
        now: Instant,
    ) -> Option<InsertOutcome> {
        if let Some(existing) = self.buckets[bucket_index]
            .nodes
            .iter_mut()
            .find(|existing| existing.addr == candidate.addr)
        {
            merge_record(existing, candidate, now);
            self.buckets[bucket_index].last_changed_at = now;
            return Some(InsertOutcome::Updated);
        }

        if let Some(existing) = self.buckets[bucket_index]
            .replacements
            .iter_mut()
            .find(|existing| existing.addr == candidate.addr)
        {
            merge_record(existing, candidate, now);
            self.buckets[bucket_index].last_changed_at = now;
            return Some(InsertOutcome::QueuedReplacement);
        }

        None
    }

    fn queue_replacement(&mut self, bucket_index: usize, candidate: NodeRecord, now: Instant) {
        let replacements = &mut self.buckets[bucket_index].replacements;
        replacements.retain(|existing| existing.addr != candidate.addr);
        replacements.push(candidate);
        replacements.sort_by(|left, right| compare_replacement_priority(left, right, now));
        if replacements.len() > self.config.replacement_limit {
            replacements.truncate(self.config.replacement_limit);
        }
        self.buckets[bucket_index].last_changed_at = now;
    }

    fn with_record_mut<F>(&mut self, addr: SocketAddr, mut apply: F) -> bool
    where
        F: FnMut(&mut Bucket, &mut NodeRecord),
    {
        for bucket in &mut self.buckets {
            if let Some(index) = bucket.nodes.iter().position(|record| record.addr == addr) {
                let mut record = bucket.nodes.remove(index);
                apply(bucket, &mut record);
                bucket.nodes.insert(index, record);
                return true;
            }
            if let Some(index) = bucket
                .replacements
                .iter()
                .position(|record| record.addr == addr)
            {
                let mut record = bucket.replacements.remove(index);
                apply(bucket, &mut record);
                bucket.replacements.insert(index, record);
                return true;
            }
        }
        false
    }
}

#[derive(Debug, Clone)]
pub struct RoutingActor {
    table: RoutingTable,
}

impl RoutingActor {
    pub fn new(local_node_id: NodeId, config: RoutingConfig, now: Instant) -> Self {
        Self {
            table: RoutingTable::new(local_node_id, config, now),
        }
    }

    pub fn family(&self) -> AddressFamily {
        self.table.family()
    }

    pub fn table(&self) -> &RoutingTable {
        &self.table
    }

    pub fn table_mut(&mut self) -> &mut RoutingTable {
        &mut self.table
    }
}

pub fn node_status(record: &NodeRecord, now: Instant) -> NodeStatus {
    if record.consecutive_failures >= BAD_NODE_FAILURE_THRESHOLD {
        return NodeStatus::Bad;
    }

    if record
        .last_query_response_at
        .is_some_and(|at| now.duration_since(at) <= GOOD_NODE_WINDOW)
    {
        return NodeStatus::Good;
    }

    if record.last_query_response_at.is_some()
        && record
            .last_inbound_query_at
            .is_some_and(|at| now.duration_since(at) <= GOOD_NODE_WINDOW)
    {
        return NodeStatus::Good;
    }

    NodeStatus::Questionable
}

pub fn xor_distance(left: &NodeId, right: &NodeId) -> [u8; 20] {
    let mut distance = [0u8; 20];
    for (idx, (left_byte, right_byte)) in left
        .as_array()
        .iter()
        .zip(right.as_array().iter())
        .enumerate()
    {
        distance[idx] = left_byte ^ right_byte;
    }
    distance
}

fn compare_distance(
    left: Option<NodeId>,
    right: Option<NodeId>,
    target: &NodeId,
) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => xor_distance(&left, target).cmp(&xor_distance(&right, target)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_replacement_priority(left: &NodeRecord, right: &NodeRecord, now: Instant) -> Ordering {
    match (node_status(left, now), node_status(right, now)) {
        (NodeStatus::Good, NodeStatus::Good)
        | (NodeStatus::Questionable, NodeStatus::Questionable)
        | (NodeStatus::Bad, NodeStatus::Bad) => {}
        (NodeStatus::Good, _) => return Ordering::Less,
        (_, NodeStatus::Good) => return Ordering::Greater,
        (NodeStatus::Questionable, _) => return Ordering::Less,
        (_, NodeStatus::Questionable) => return Ordering::Greater,
    }

    left.last_changed_at.cmp(&right.last_changed_at).reverse()
}

fn questionable_probe_targets(bucket: &Bucket, now: Instant) -> Vec<SocketAddr> {
    let mut records = bucket
        .nodes
        .iter()
        .filter(|record| node_status(record, now) == NodeStatus::Questionable)
        .cloned()
        .collect::<Vec<_>>();
    records.sort_by_key(least_recently_seen_at);
    records.into_iter().map(|record| record.addr).collect()
}

fn least_recently_seen_at(record: &NodeRecord) -> Option<Instant> {
    match (record.last_query_response_at, record.last_inbound_query_at) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn merge_record(target: &mut NodeRecord, candidate: &NodeRecord, now: Instant) {
    if let (Some(existing), Some(updated)) = (target.node_id, candidate.node_id) {
        if existing != updated {
            target.id_churn_count = target.id_churn_count.saturating_add(1);
        }
    }

    if target.node_id.is_none() {
        target.node_id = candidate.node_id;
    }
    target.last_query_sent_at = candidate.last_query_sent_at.or(target.last_query_sent_at);
    target.last_query_response_at =
        candidate.last_query_response_at.or(target.last_query_response_at);
    target.last_inbound_query_at = candidate.last_inbound_query_at.or(target.last_inbound_query_at);
    target.consecutive_failures = target.consecutive_failures.min(candidate.consecutive_failures);
    target.dead_referral_count = target
        .dead_referral_count
        .saturating_add(candidate.dead_referral_count);
    target.live_referral_count = target
        .live_referral_count
        .saturating_add(candidate.live_referral_count);
    target.id_churn_count = target.id_churn_count.saturating_add(candidate.id_churn_count);
    target.last_changed_at = now;
}

fn prefix_matches(prefix: &[u8; 20], prefix_len: u8, candidate: &[u8; 20]) -> bool {
    for bit_idx in 0..prefix_len {
        if bit_at(prefix, bit_idx) != bit_at(candidate, bit_idx) {
            return false;
        }
    }
    true
}

fn bit_at(bytes: &[u8; 20], bit_idx: u8) -> bool {
    let byte_idx = (bit_idx / 8) as usize;
    let bit_offset = 7 - (bit_idx % 8);
    ((bytes[byte_idx] >> bit_offset) & 1) == 1
}

fn set_bit(bytes: &mut [u8; 20], bit_idx: u8, value: bool) {
    let byte_idx = (bit_idx / 8) as usize;
    let bit_offset = 7 - (bit_idx % 8);
    let mask = 1u8 << bit_offset;
    if value {
        bytes[byte_idx] |= mask;
    } else {
        bytes[byte_idx] &= !mask;
    }
}
