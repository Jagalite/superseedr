// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::types::{AddressFamily, CompactPeer, InfoHash};
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone)]
pub struct PeerStoreConfig {
    pub max_info_hashes: usize,
    pub max_peers_per_info_hash: usize,
    pub max_total_peers: usize,
    pub peer_ttl: Duration,
}

impl Default for PeerStoreConfig {
    fn default() -> Self {
        Self {
            max_info_hashes: 2048,
            max_peers_per_info_hash: 128,
            max_total_peers: 16_384,
            peer_ttl: Duration::from_secs(1800),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPeer {
    pub info_hash: InfoHash,
    pub peer: CompactPeer,
    pub announced_at: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PeerStoreKey {
    info_hash: InfoHash,
    family: AddressFamily,
}

#[derive(Debug, Clone)]
pub struct PeerStore {
    config: PeerStoreConfig,
    peers: HashMap<PeerStoreKey, Vec<StoredPeer>>,
}

impl PeerStore {
    pub fn new(config: PeerStoreConfig) -> Self {
        Self {
            config,
            peers: HashMap::new(),
        }
    }

    pub fn config(&self) -> &PeerStoreConfig {
        &self.config
    }

    pub fn total_peer_count(&self) -> usize {
        self.peers.values().map(Vec::len).sum()
    }

    pub fn insert(&mut self, info_hash: InfoHash, peer: CompactPeer, now: SystemTime) -> bool {
        self.prune_expired(now);

        let key = PeerStoreKey {
            info_hash,
            family: peer.family(),
        };

        if !self.peers.contains_key(&key) && self.peers.len() >= self.config.max_info_hashes {
            self.evict_oldest_bucket();
        }

        let bucket = self.peers.entry(key).or_default();
        bucket.retain(|existing| existing.peer.addr != peer.addr);
        bucket.push(StoredPeer {
            info_hash,
            peer,
            announced_at: now,
        });
        bucket.sort_by_key(|stored| stored.announced_at);
        if bucket.len() > self.config.max_peers_per_info_hash {
            let overflow = bucket.len() - self.config.max_peers_per_info_hash;
            bucket.drain(..overflow);
        }

        self.enforce_global_limit();
        true
    }

    pub fn peers_for(
        &mut self,
        info_hash: InfoHash,
        family: AddressFamily,
        now: SystemTime,
    ) -> Vec<CompactPeer> {
        self.prune_expired(now);
        self.peers
            .get(&PeerStoreKey { info_hash, family })
            .map(|bucket| bucket.iter().map(|stored| stored.peer).collect())
            .unwrap_or_default()
    }

    pub fn prune_expired(&mut self, now: SystemTime) {
        let ttl = self.config.peer_ttl;
        self.peers.retain(|_, bucket| {
            bucket.retain(|stored| {
                now.duration_since(stored.announced_at)
                    .unwrap_or_default()
                    <= ttl
            });
            !bucket.is_empty()
        });
    }

    fn enforce_global_limit(&mut self) {
        while self.total_peer_count() > self.config.max_total_peers {
            let Some((key, oldest_index)) = self.oldest_peer_entry() else {
                break;
            };
            let mut remove_bucket = false;
            if let Some(bucket) = self.peers.get_mut(&key) {
                bucket.remove(oldest_index);
                remove_bucket = bucket.is_empty();
            }
            if remove_bucket {
                self.peers.remove(&key);
            }
        }
    }

    fn evict_oldest_bucket(&mut self) {
        let oldest = self
            .peers
            .iter()
            .filter_map(|(key, bucket)| bucket.first().map(|stored| (*key, stored.announced_at)))
            .min_by_key(|(_, announced_at)| *announced_at)
            .map(|(key, _)| key);

        if let Some(key) = oldest {
            self.peers.remove(&key);
        }
    }

    fn oldest_peer_entry(&self) -> Option<(PeerStoreKey, usize)> {
        self.peers
            .iter()
            .flat_map(|(key, bucket)| {
                bucket
                    .iter()
                    .enumerate()
                    .map(move |(idx, stored)| (*key, idx, stored.announced_at))
            })
            .min_by_key(|(_, _, announced_at)| *announced_at)
            .map(|(key, idx, _)| (key, idx))
    }
}
