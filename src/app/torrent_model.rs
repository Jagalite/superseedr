// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared application torrent model definitions and transitions.

use super::*;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerInfo {
    pub address: String,
    pub peer_id: Vec<u8>,
    pub am_choking: bool,
    pub peer_choking: bool,
    pub am_interested: bool,
    pub peer_interested: bool,
    pub bitfield: Vec<bool>,
    pub download_speed_bps: u64,
    pub upload_speed_bps: u64,
    pub total_downloaded: u64,
    pub total_uploaded: u64,
    #[serde(default)]
    pub connection_count: u64,
    #[serde(default)]
    pub disconnect_count: u64,
    pub last_action: String,
}

pub fn swarm_availability_counts(peers: &[PeerInfo], total_pieces: u32) -> Vec<u32> {
    let total_pieces_usize = total_pieces as usize;
    let mut availability = vec![0; total_pieces_usize];

    for peer in peers {
        for (i, has_piece) in peer.bitfield.iter().enumerate().take(total_pieces_usize) {
            if *has_piece {
                availability[i] += 1;
            }
        }
    }

    availability
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TorrentMetrics {
    pub torrent_control_state: TorrentControlState,
    pub delete_files: bool,
    pub info_hash: Vec<u8>,
    pub torrent_or_magnet: String,
    pub torrent_name: String,
    pub download_path: Option<PathBuf>,
    pub container_name: Option<String>,
    #[serde(default)]
    pub is_multi_file: bool,
    pub file_count: Option<usize>,
    pub file_priorities: HashMap<usize, FilePriority>,
    /// Verified, committed bytes per manifest file; None means unavailable or rechecking.
    #[serde(skip)]
    pub file_verified_bytes: Vec<Option<u64>>,
    pub data_available: bool,
    pub is_complete: bool,
    pub number_of_successfully_connected_peers: usize,
    #[serde(default)]
    pub tcp_peer_count: usize,
    #[serde(default)]
    pub utp_peer_count: usize,
    #[serde(default)]
    pub beneficial_tcp_peer_count: usize,
    #[serde(default)]
    pub beneficial_utp_peer_count: usize,
    pub number_of_pieces_total: u32,
    pub number_of_pieces_completed: u32,
    pub download_speed_bps: u64,
    pub upload_speed_bps: u64,
    pub bytes_downloaded_this_tick: u64,
    pub bytes_uploaded_this_tick: u64,
    pub session_total_downloaded: u64,
    pub session_total_uploaded: u64,
    pub eta: Duration,

    #[serde(skip)]
    pub peers: Vec<PeerInfo>,
    /// Opaque in-process availability marker assigned at native publication.
    /// Other producers must leave this as None (or clear it when changing peers);
    /// unversioned snapshots use content comparison in the UI.
    #[serde(skip)]
    pub availability_revision: Option<u64>,
    /// Recently departed peers retained long enough for background consumers to observe
    /// their final cumulative transfer counters. UI telemetry does not display these rows.
    #[serde(skip)]
    pub departed_peers: Vec<PeerInfo>,
    /// Cumulative reconnects after an IP has no remaining active peer for this manager lifetime.
    #[serde(skip)]
    pub peer_reconnect_counts: HashMap<IpAddr, u64>,
    pub activity_message: String,
    pub next_announce_in: Duration,
    pub total_size: u64,
    pub bytes_written: u64,

    #[serde(skip)]
    pub blocks_in_history: Vec<u64>,

    #[serde(skip)]
    pub blocks_out_history: Vec<u64>,

    #[serde(skip)]
    pub file_activity_updates: Vec<FileActivityUpdate>,

    pub blocks_in_this_tick: u64,
    pub blocks_out_this_tick: u64,
}

impl Default for TorrentMetrics {
    fn default() -> Self {
        Self {
            torrent_control_state: TorrentControlState::default(),
            delete_files: false,
            info_hash: Vec::new(),
            torrent_or_magnet: String::new(),
            torrent_name: String::new(),
            download_path: None,
            container_name: None,
            is_multi_file: false,
            file_count: None,
            file_priorities: HashMap::new(),
            file_verified_bytes: Vec::new(),
            data_available: true,
            is_complete: false,
            number_of_successfully_connected_peers: 0,
            tcp_peer_count: 0,
            utp_peer_count: 0,
            beneficial_tcp_peer_count: 0,
            beneficial_utp_peer_count: 0,
            number_of_pieces_total: 0,
            number_of_pieces_completed: 0,
            download_speed_bps: 0,
            upload_speed_bps: 0,
            bytes_downloaded_this_tick: 0,
            bytes_uploaded_this_tick: 0,
            session_total_downloaded: 0,
            session_total_uploaded: 0,
            eta: Duration::default(),
            peers: Vec::new(),
            availability_revision: None,
            departed_peers: Vec::new(),
            peer_reconnect_counts: HashMap::new(),
            activity_message: String::new(),
            next_announce_in: Duration::default(),
            total_size: 0,
            bytes_written: 0,
            blocks_in_history: Vec::new(),
            blocks_out_history: Vec::new(),
            file_activity_updates: Vec::new(),
            blocks_in_this_tick: 0,
            blocks_out_this_tick: 0,
        }
    }
}

#[derive(Default, Debug)]
pub struct TorrentDisplayState {
    pub latest_state: TorrentMetrics,
    pub added_at_unix_secs: Option<u64>,
    pub file_preview_tree: Vec<RawNode<TorrentPreviewPayload>>,
    pub recent_file_activity: HashMap<String, RecentFileActivity>,
    pub latest_file_probe_status: Option<TorrentFileProbeStatus>,
    pub integrity_next_probe_in: Option<Duration>,
    pub download_history: Vec<u64>,
    pub upload_history: Vec<u64>,

    pub bytes_read_this_tick: u64,
    pub bytes_written_this_tick: u64,
    pub disk_read_speed_bps: u64,
    pub disk_write_speed_bps: u64,
    pub disk_read_history_log: VecDeque<DiskIoOperation>,
    pub disk_write_history_log: VecDeque<DiskIoOperation>,
    pub disk_read_thrash_score: u64,
    pub disk_write_thrash_score: u64,

    pub smoothed_download_speed_bps: u64,
    pub smoothed_upload_speed_bps: u64,

    pub swarm_availability_samples: usize,

    pub peers_discovered_this_tick: u64,
    pub peers_connected_this_tick: u64,
    pub peers_disconnected_this_tick: u64,
    pub peer_discovery_history: Vec<u64>,
    pub peer_connection_history: Vec<u64>,
    pub peer_disconnect_history: Vec<u64>,
    pub last_seen_session_total_downloaded: u64,
    pub last_seen_session_total_uploaded: u64,
}

#[derive(Debug, Clone, Default)]
pub struct RecentFileActivity {
    pub download_at: Option<Instant>,
    pub upload_at: Option<Instant>,
}

#[derive(Debug, Clone, Default)]
pub struct SwarmAvailabilityFlashState {
    pub info_hash: Vec<u8>,
    pub previous_availability: Vec<u32>,
    pub flash_start: Vec<Option<Instant>>,
    pub flash_until: Vec<Option<Instant>>,
    pub(super) active_flash_pieces: Vec<usize>,
    pub(super) previous_peer_bitfields: HashMap<String, Vec<bool>>,
    pub(super) previous_peer_keys: Vec<String>,
    pub(super) availability_revision: Option<u64>,
}

impl SwarmAvailabilityFlashState {
    /// Native snapshots carry a persistent marker, so skipped watch updates cannot
    /// hide changes. Other producers are checked by value, without shared storage.
    pub(crate) fn matches_peers(
        &self,
        info_hash: &[u8],
        peers: &[PeerInfo],
        total_pieces: u32,
        revision: Option<u64>,
    ) -> bool {
        if self.info_hash.as_slice() != info_hash
            || self.previous_availability.len() != total_pieces as usize
        {
            return false;
        }
        if let Some(revision) = revision {
            return self.availability_revision == Some(revision);
        }
        self.previous_peer_bitfields.len() == peers.len()
            && self.previous_peer_keys.len() == peers.len()
            && peers.iter().enumerate().all(|(index, peer)| {
                let key = swarm_availability_peer_key(peer, index);
                self.previous_peer_keys[index] == key
                    && self.previous_peer_bitfields.get(&key) == Some(&peer.bitfield)
            })
    }

    // Preserve input multiplicity as well as map identity. Duplicate display
    // keys overwrite map entries; they must never make a changed peer list
    // appear unchanged. Reordering conservatively recomputes availability.
    pub(super) fn remember_peer_keys(&mut self, peers: &[PeerInfo]) {
        self.previous_peer_keys = peers
            .iter()
            .enumerate()
            .map(|(index, peer)| swarm_availability_peer_key(peer, index))
            .collect();
    }

    #[cfg(test)]
    pub fn update(
        &mut self,
        info_hash: &[u8],
        current_availability: Vec<u32>,
        now: Instant,
        flash_duration: Duration,
    ) {
        self.previous_peer_bitfields.clear();
        self.previous_peer_keys.clear();
        self.availability_revision = None;
        self.update_from_availability(
            info_hash,
            current_availability.clone(),
            current_availability,
            now,
            flash_duration,
        );
    }

    #[cfg(test)]
    pub fn update_from_peers(
        &mut self,
        info_hash: &[u8],
        peers: &[PeerInfo],
        total_pieces: u32,
        now: Instant,
        flash_duration: Duration,
    ) {
        let current_availability = swarm_availability_counts(peers, total_pieces);
        let current_peer_bitfields =
            swarm_availability_peer_bitfields(peers, current_availability.len());
        self.update_from_peer_availability(
            info_hash,
            current_availability,
            current_peer_bitfields,
            now,
            flash_duration,
        );
        self.remember_peer_keys(peers);
        self.availability_revision = None;
    }

    pub(super) fn update_from_peer_availability(
        &mut self,
        info_hash: &[u8],
        current_availability: Vec<u32>,
        current_peer_bitfields: HashMap<String, Vec<bool>>,
        now: Instant,
        flash_duration: Duration,
    ) {
        if self.info_hash.as_slice() != info_hash
            || self.previous_availability.len() != current_availability.len()
        {
            self.info_hash = info_hash.to_vec();
            self.previous_availability = current_availability;
            self.flash_start = vec![None; self.previous_availability.len()];
            self.flash_until = vec![None; self.previous_availability.len()];
            self.active_flash_pieces.clear();
            self.previous_peer_bitfields = current_peer_bitfields;
            return;
        }

        let mut known_peer_availability = vec![0; current_availability.len()];
        for (peer_key, bitfield) in &current_peer_bitfields {
            if !self.previous_peer_bitfields.contains_key(peer_key) {
                continue;
            }

            for (idx, has_piece) in bitfield.iter().enumerate() {
                if *has_piece {
                    known_peer_availability[idx] += 1;
                }
            }
        }

        self.update_from_availability(
            info_hash,
            current_availability,
            known_peer_availability,
            now,
            flash_duration,
        );
        self.previous_peer_bitfields = current_peer_bitfields;
    }

    pub(super) fn update_from_availability(
        &mut self,
        info_hash: &[u8],
        current_availability: Vec<u32>,
        flashable_availability: Vec<u32>,
        now: Instant,
        flash_duration: Duration,
    ) {
        if self.info_hash.as_slice() != info_hash
            || self.previous_availability.len() != current_availability.len()
        {
            self.info_hash = info_hash.to_vec();
            self.previous_availability = current_availability;
            self.flash_start = vec![None; self.previous_availability.len()];
            self.flash_until = vec![None; self.previous_availability.len()];
            self.active_flash_pieces.clear();
            self.previous_peer_bitfields.clear();
            self.previous_peer_keys.clear();
            return;
        }

        if self.flash_start.len() != current_availability.len() {
            self.flash_start.resize(current_availability.len(), None);
        }
        if self.flash_until.len() != current_availability.len() {
            self.flash_until.resize(current_availability.len(), None);
        }

        let increased_count = self
            .previous_availability
            .iter()
            .zip(flashable_availability.iter())
            .filter(|&(&previous, &current)| current > previous)
            .count();
        let suppress_full_map_flash =
            !flashable_availability.is_empty() && increased_count == flashable_availability.len();

        let mut rank = 0usize;
        for (idx, (&previous, &current)) in self
            .previous_availability
            .iter()
            .zip(flashable_availability.iter())
            .enumerate()
        {
            if current > previous && !suppress_full_map_flash {
                let delay =
                    swarm_availability_flash_rollout_delay(rank, increased_count, flash_duration);
                let start = now + delay;
                self.flash_start[idx] = Some(start);
                self.flash_until[idx] = Some(start + flash_duration);
                if !self.active_flash_pieces.contains(&idx) {
                    self.active_flash_pieces.push(idx);
                }
                rank += 1;
            }
        }

        self.previous_availability = current_availability;
        self.clear_expired(now);
    }

    pub fn is_piece_flashing(&self, info_hash: &[u8], piece_index: usize, now: Instant) -> bool {
        self.info_hash.as_slice() == info_hash
            && self
                .flash_start
                .get(piece_index)
                .copied()
                .flatten()
                .is_some_and(|start| start <= now)
            && self
                .flash_until
                .get(piece_index)
                .copied()
                .flatten()
                .is_some_and(|deadline| deadline > now)
    }

    pub fn has_active_flash(&self, now: Instant) -> bool {
        self.active_flash_pieces.iter().any(|&piece_index| {
            self.flash_until
                .get(piece_index)
                .copied()
                .flatten()
                .is_some_and(|deadline| deadline > now)
        })
    }

    pub fn active_flash_piece_indices(&self, info_hash: &[u8], now: Instant) -> Vec<usize> {
        if self.info_hash.as_slice() != info_hash {
            return Vec::new();
        }

        self.active_flash_pieces
            .iter()
            .copied()
            .filter(|&piece_index| self.is_piece_flashing(info_hash, piece_index, now))
            .collect()
    }

    pub(super) fn clear_expired(&mut self, now: Instant) {
        self.active_flash_pieces.retain(|&idx| {
            if self.flash_until[idx].is_some_and(|deadline| deadline <= now) {
                self.flash_until[idx] = None;
                if let Some(start) = self.flash_start.get_mut(idx) {
                    *start = None;
                }
                false
            } else {
                true
            }
        });
    }
}

pub(super) fn swarm_availability_flash_rollout_delay(
    rank: usize,
    flash_count: usize,
    flash_duration: Duration,
) -> Duration {
    if rank == 0 || flash_count <= 1 || flash_duration.is_zero() {
        return Duration::ZERO;
    }

    let steps = flash_count.saturating_sub(1) as u128;
    let delay_nanos = flash_duration
        .as_nanos()
        .saturating_mul(rank as u128)
        .checked_div(steps)
        .unwrap_or(0);
    Duration::from_nanos(delay_nanos.min(u64::MAX as u128) as u64)
}

pub(super) fn swarm_availability_peer_bitfields(
    peers: &[PeerInfo],
    total_pieces: usize,
) -> HashMap<String, Vec<bool>> {
    let mut bitfields = HashMap::with_capacity(peers.len());
    for (idx, peer) in peers.iter().enumerate() {
        let bitfield = if peer.bitfield.len() == total_pieces {
            peer.bitfield.clone()
        } else {
            let mut normalized = vec![false; total_pieces];
            for (piece_idx, has_piece) in peer.bitfield.iter().enumerate().take(total_pieces) {
                normalized[piece_idx] = *has_piece;
            }
            normalized
        };
        bitfields.insert(swarm_availability_peer_key(peer, idx), bitfield);
    }
    bitfields
}

pub(super) fn swarm_availability_peer_key(peer: &PeerInfo, fallback_index: usize) -> String {
    if !peer.address.is_empty() {
        return format!("addr:{}", peer.address);
    }

    if !peer.peer_id.is_empty() {
        return format!("peer:{}", hex::encode(&peer.peer_id));
    }

    format!("slot:{fallback_index}")
}

#[cfg(test)]
mod telemetry_snapshot_tests {
    use super::*;

    #[test]
    fn owned_bitfield_preserves_old_snapshot_and_serialized_values() {
        let mut peer = PeerInfo {
            address: "192.0.2.10:6881".to_string(),
            bitfield: vec![false, true, false],
            ..Default::default()
        };
        let snapshot = peer.clone();
        assert_ne!(peer.bitfield.as_ptr(), snapshot.bitfield.as_ptr());
        peer.bitfield[0] = true;
        assert_eq!(snapshot.bitfield.as_slice(), &[false, true, false]);
        assert_eq!(peer.bitfield.as_slice(), &[true, true, false]);
        let encoded = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(encoded["bitfield"], serde_json::json!([false, true, false]));
        let decoded: PeerInfo = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn availability_cache_invalidates_when_duplicate_peer_keys_change() {
        let hash = vec![1; 20];
        let first = PeerInfo {
            address: "192.0.2.10:6881".to_string(),
            bitfield: vec![true, false],
            ..Default::default()
        };
        let second = PeerInfo {
            address: "192.0.2.20:6881".to_string(),
            bitfield: vec![false, true],
            ..Default::default()
        };
        let mut same_key = second.clone();
        same_key.address.clone_from(&first.address);
        // Cover both collapse of duplicate keys and replacement by duplicates
        // without changing the number of input peers.
        for (before, after) in [
            (vec![first.clone(), same_key.clone()], vec![same_key]),
            (vec![first.clone(), second], vec![first.clone(), first]),
        ] {
            let mut app = AppState::default();
            app.torrent_list_order.push(hash.clone());
            app.torrents.insert(
                hash.clone(),
                TorrentDisplayState {
                    latest_state: TorrentMetrics {
                        number_of_pieces_total: 2,
                        peers: before,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            );
            let now = Instant::now();
            update_swarm_availability_flash_state(&mut app, now);
            assert!(
                !app.ui
                    .swarm_availability_flash
                    .matches_peers(&hash, &after, 2, None),
                "changed peer multiplicity must invalidate cached availability"
            );
            let expected = swarm_availability_counts(&after, 2);
            app.torrents.get_mut(&hash).unwrap().latest_state.peers = after;
            update_swarm_availability_flash_state(&mut app, now);
            assert_eq!(
                app.ui.swarm_availability_flash.previous_availability,
                expected
            );
        }
    }

    #[test]
    fn availability_cache_matches_full_recomputation_through_churn_and_geometry_changes() {
        for versioned in [false, true] {
            let mut publishers = [
                crate::telemetry::manager_telemetry::ManagerTelemetry::default(),
                crate::telemetry::manager_telemetry::ManagerTelemetry::default(),
            ];
            let mut app = AppState::default();
            for tag in [1, 2] {
                let hash = vec![tag; 20];
                app.torrent_list_order.push(hash.clone());
                app.torrents.insert(
                    hash,
                    TorrentDisplayState {
                        latest_state: TorrentMetrics {
                            number_of_pieces_total: 4,
                            peers: vec![PeerInfo {
                                address: "192.0.2.10:6881".to_string(),
                                bitfield: vec![false, true, false, true],
                                ..Default::default()
                            }],
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                );
            }
            let mut reference = SwarmAvailabilityFlashState::default();
            let start = Instant::now();
            let mut cache_hits = 0;
            for step in 0..140 {
                if step % 17 == 0 {
                    app.ui.selected_torrent_index ^= 1;
                }
                let hash = app.torrent_list_order[app.ui.selected_torrent_index].clone();
                let state = &mut app.torrents.get_mut(&hash).unwrap().latest_state;
                match step % 10 {
                    1 => {
                        if let Some(peer) = state.peers.first_mut() {
                            if let Some(bit) = peer.bitfield.first_mut() {
                                *bit = !*bit;
                            }
                        }
                    }
                    2 => state.peers.reverse(),
                    3 => state.peers.push(PeerInfo {
                        address: format!("192.0.2.20:{}", 7000 + step),
                        bitfield: vec![true; (step % 7) as usize],
                        ..Default::default()
                    }),
                    4 => state.number_of_pieces_total = 2 + step % 5,
                    5 => {
                        state.peers.pop();
                    }
                    7 => state.peers.clear(),
                    _ => {}
                }
                if versioned {
                    state.info_hash.clone_from(&hash);
                    publishers[app.ui.selected_torrent_index].prepare_snapshot(state);
                }
                let now = start + Duration::from_millis(u64::from(step) * 80);
                if app.ui.swarm_availability_flash.matches_peers(
                    &hash,
                    &state.peers,
                    state.number_of_pieces_total,
                    state.availability_revision,
                ) {
                    cache_hits += 1;
                }
                reference.update_from_peers(
                    &hash,
                    &state.peers,
                    state.number_of_pieces_total,
                    now,
                    SWARM_AVAILABILITY_FLASH_DURATION,
                );
                update_swarm_availability_flash_state(&mut app, now);
                let cached = &app.ui.swarm_availability_flash;
                assert_eq!(cached.info_hash, reference.info_hash, "step {step}");
                assert_eq!(
                    cached.previous_availability, reference.previous_availability,
                    "step {step}"
                );
                assert_eq!(cached.flash_start, reference.flash_start, "step {step}");
                assert_eq!(cached.flash_until, reference.flash_until, "step {step}");
                assert_eq!(
                    cached.active_flash_pieces, reference.active_flash_pieces,
                    "step {step}"
                );
                assert_eq!(
                    cached.previous_peer_bitfields, reference.previous_peer_bitfields,
                    "step {step}"
                );
            }
            assert!(cache_hits > 0);
        }
    }

    #[test]
    fn ui_cache_handles_coalesced_snapshots_and_restarted_or_unversioned_producers() {
        use crate::telemetry::{manager_telemetry::ManagerTelemetry, ui_telemetry::UiTelemetry};

        let mut publisher = ManagerTelemetry::default();
        let mut metrics = TorrentMetrics {
            info_hash: vec![3; 20],
            number_of_pieces_total: 3,
            peers: vec![PeerInfo {
                address: "192.0.2.30:6881".into(),
                bitfield: vec![true, false, false],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(publisher.prepare_snapshot(&mut metrics));
        let initial_revision = metrics.availability_revision;
        let (tx, mut rx) = watch::channel(metrics.clone());
        let mut app = AppState::default();
        app.torrent_list_order.push(metrics.info_hash.clone());
        let now = Instant::now();
        UiTelemetry::on_metrics(&mut app, rx.borrow_and_update().clone());
        update_swarm_availability_flash_state(&mut app, now);
        assert_eq!(
            app.ui.swarm_availability_flash.availability_revision,
            initial_revision
        );

        // The UI misses the changed snapshot and receives a later speed-only
        // update. Its persistent revision must still invalidate the old counts.
        metrics.peers[0].bitfield[1] = true;
        assert!(publisher.prepare_snapshot(&mut metrics));
        tx.send(metrics.clone()).unwrap();
        let changed_revision = metrics.availability_revision;
        assert_ne!(changed_revision, initial_revision);
        metrics.download_speed_bps = 123;
        assert!(publisher.prepare_snapshot(&mut metrics));
        assert_eq!(metrics.availability_revision, changed_revision);
        tx.send(metrics.clone()).unwrap();
        UiTelemetry::on_metrics(&mut app, rx.borrow_and_update().clone());
        update_swarm_availability_flash_state(&mut app, now);
        assert_eq!(
            app.ui.swarm_availability_flash.previous_availability,
            vec![1, 1, 0]
        );
        assert!(app.ui.swarm_availability_flash.matches_peers(
            &metrics.info_hash,
            &metrics.peers,
            3,
            changed_revision,
        ));

        // A new manager for the same torrent cannot accidentally reuse a token.
        metrics.peers[0].bitfield = vec![false, false, true];
        assert!(ManagerTelemetry::default().prepare_snapshot(&mut metrics));
        assert_ne!(metrics.availability_revision, changed_revision);
        UiTelemetry::on_metrics(&mut app, metrics.clone());
        update_swarm_availability_flash_state(&mut app, now);
        assert_eq!(
            app.ui.swarm_availability_flash.previous_availability,
            vec![0, 0, 1]
        );

        // Revision metadata never crosses serialized boundaries. Unversioned
        // input uses exact contents, including a direct follower-style replacement.
        let serialized = serde_json::to_value(&metrics).unwrap();
        assert!(serialized.get("availability_revision").is_none());
        let mut unversioned: TorrentMetrics = serde_json::from_value(serialized).unwrap();
        assert_eq!(unversioned.availability_revision, None);
        unversioned.peers = metrics.peers.clone();
        unversioned.peers[0].bitfield = vec![true, false, true];
        app.torrents
            .get_mut(&metrics.info_hash)
            .unwrap()
            .latest_state = unversioned;
        update_swarm_availability_flash_state(&mut app, now);
        assert_eq!(
            app.ui.swarm_availability_flash.previous_availability,
            vec![1, 0, 1]
        );
    }
}
