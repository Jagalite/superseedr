// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashSet;

/// Process-local receipt telemetry. Peaks and activity use the UI sampling cadence.
#[derive(Default)]
pub struct SessionStats {
    pub connection_attempts: u64,
    pub unique_peers: HashSet<String>,
    pub peak_peers: usize,
    pub active_seconds: u64,
    pub peak_upload_bps: u64,
    pub peak_download_bps: u64,
    pub peak_read_seek: u64,
    pub peak_write_seek: u64,
    pub peak_cpu: f32,
    pub peak_ram: u64,
    pub(crate) last_transfer: (u64, u64),
}

impl SessionStats {
    pub fn observe_peers<'a>(&mut self, peers: impl Iterator<Item = &'a crate::app::PeerInfo>) {
        for peer in peers {
            let key = if peer.peer_id.is_empty() {
                if peer.address.is_empty() {
                    continue;
                }
                format!("address:{}", peer.address)
            } else {
                format!("peer:{}", hex::encode(&peer.peer_id))
            };
            self.unique_peers.insert(key);
        }
    }

    pub fn observe_activity(&mut self, uploaded: u64, downloaded: u64) {
        if (uploaded, downloaded) != self.last_transfer {
            self.active_seconds = self.active_seconds.saturating_add(1);
        }
        self.last_transfer = (uploaded, downloaded);
    }
}

pub(crate) fn observe(state: &mut crate::app::AppState) {
    let stats = &mut state.session_stats;
    let mut peers = 0usize;
    let mut upload = 0u64;
    let mut download = 0u64;
    for torrent in state.torrents.values() {
        let metrics = &torrent.latest_state;
        peers = peers.saturating_add(metrics.number_of_successfully_connected_peers);
        upload = upload.saturating_add(metrics.upload_speed_bps);
        download = download.saturating_add(metrics.download_speed_bps);
    }
    stats.peak_peers = stats.peak_peers.max(peers);
    stats.peak_upload_bps = stats.peak_upload_bps.max(upload);
    stats.peak_download_bps = stats.peak_download_bps.max(download);
    stats.peak_read_seek = stats
        .peak_read_seek
        .max(state.global_disk_read_thrash_score);
    stats.peak_write_seek = stats
        .peak_write_seek
        .max(state.global_disk_write_thrash_score);
    if state.cpu_usage.is_finite() {
        stats.peak_cpu = stats.peak_cpu.max(state.cpu_usage);
    }
    stats.peak_ram = stats.peak_ram.max(state.app_ram_usage);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppState, PeerInfo, TorrentDisplayState};

    #[test]
    fn session_peaks_survive_idle_samples_and_peer_departure() {
        let mut state = AppState {
            cpu_usage: 42.5,
            app_ram_usage: 2_000_000,
            global_disk_read_thrash_score: 123_000,
            global_disk_write_thrash_score: 456_000,
            ..Default::default()
        };
        let mut torrent = TorrentDisplayState::default();
        torrent.latest_state.number_of_successfully_connected_peers = 2;
        torrent.latest_state.upload_speed_bps = 8_000_000;
        torrent.latest_state.download_speed_bps = 16_000_000;
        torrent.latest_state.peers = vec![PeerInfo {
            peer_id: vec![1],
            address: "192.0.2.1:6000".into(),
            ..Default::default()
        }];
        state
            .session_stats
            .observe_peers(torrent.latest_state.peers.iter());
        state
            .session_stats
            .observe_peers(torrent.latest_state.peers.iter());
        state.torrents.insert(vec![1], torrent);
        observe(&mut state);
        observe(&mut state);
        assert_eq!(state.session_stats.unique_peers.len(), 1);
        state.torrents.clear();
        state.cpu_usage = 0.0;
        state.app_ram_usage = 0;
        state.global_disk_read_thrash_score = 0;
        state.global_disk_write_thrash_score = 0;
        observe(&mut state);
        let stats = &state.session_stats;
        assert_eq!(stats.peak_peers, 2);
        assert_eq!(stats.peak_upload_bps, 8_000_000);
        assert_eq!(stats.peak_download_bps, 16_000_000);
        assert_eq!(stats.peak_cpu, 42.5);
        assert_eq!(stats.peak_ram, 2_000_000);
        assert_eq!(stats.peak_read_seek, 123_000);
        assert_eq!(stats.peak_write_seek, 456_000);
    }

    #[test]
    fn activity_counts_only_intervals_with_new_transfer() {
        let mut stats = SessionStats::default();
        for (upload, download) in [(0, 0), (1, 0), (1, 0), (1, 2), (1, 2)] {
            stats.observe_activity(upload, download);
        }
        assert_eq!(stats.active_seconds, 2);
    }
}
