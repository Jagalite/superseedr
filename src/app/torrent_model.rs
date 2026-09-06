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
    #[serde(default)]
    pub download_mode: crate::config::DownloadMode,
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
            download_mode: crate::config::DownloadMode::default(),
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

    pub swarm_availability_history: Vec<Vec<u32>>,

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
}

impl SwarmAvailabilityFlashState {
    #[cfg(test)]
    pub fn update(
        &mut self,
        info_hash: &[u8],
        current_availability: Vec<u32>,
        now: Instant,
        flash_duration: Duration,
    ) {
        self.previous_peer_bitfields.clear();
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
        let mut bitfield = vec![false; total_pieces];
        for (piece_idx, has_piece) in peer.bitfield.iter().enumerate().take(total_pieces) {
            bitfield[piece_idx] = *has_piece;
        }
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
