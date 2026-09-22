// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native integrity execution. Runtime resources remain owned by `App`.

use super::*;

impl App {
    pub(super) fn torrent_saved_location(metrics: &TorrentMetrics) -> Option<PathBuf> {
        crate::app::reducer::health::torrent_saved_location(metrics)
    }

    pub(super) fn current_integrity_snapshots(&self) -> Vec<TorrentIntegritySnapshot> {
        self.app_state
            .torrents
            .iter()
            .filter_map(|(info_hash, torrent)| {
                if torrent.latest_state.torrent_control_state == TorrentControlState::Deleting {
                    return None;
                }

                Some(TorrentIntegritySnapshot {
                    info_hash: info_hash.clone(),
                    data_available: torrent.latest_state.data_available,
                    is_downloading: !torrent.latest_state.is_complete,
                    file_count: torrent.latest_state.file_count,
                    saved_location: Self::torrent_saved_location(&torrent.latest_state),
                    download_speed_bps: torrent.latest_state.download_speed_bps,
                    upload_speed_bps: torrent.latest_state.upload_speed_bps,
                })
            })
            .collect()
    }

    pub(super) fn dispatch_integrity_probe_batches(&mut self) {
        self.integrity_scheduler
            .sync_torrents(self.current_integrity_snapshots());

        for request in self.integrity_scheduler.drain_due_probe_requests() {
            let send_result = self
                .torrent_manager_command_txs
                .get(&request.info_hash)
                .map(|manager_tx| {
                    manager_tx.try_send(ManagerCommand::ProbeFileBatch {
                        epoch: request.epoch,
                        start_file_index: request.start_file_index,
                        max_files: request.max_files,
                    })
                });

            match send_result {
                Some(Ok(())) => {}
                _ => self
                    .integrity_scheduler
                    .on_dispatch_failed(&request.info_hash),
            }
        }

        self.sync_integrity_probe_deadlines();
    }

    pub(super) fn advance_integrity_scheduler(&mut self, dt: Duration) {
        self.integrity_scheduler.advance_time(dt);
        self.dispatch_integrity_probe_batches();
    }

    pub(super) fn sync_integrity_probe_deadlines(&mut self) {
        let probe_deadlines: Vec<(Vec<u8>, Option<Duration>)> = self
            .app_state
            .torrents
            .keys()
            .cloned()
            .map(|info_hash| {
                let next_probe_in = self.integrity_scheduler.next_probe_in(&info_hash);
                (info_hash, next_probe_in)
            })
            .collect();

        for (info_hash, next_probe_in) in probe_deadlines {
            if let Some(torrent) = self.app_state.torrents.get_mut(&info_hash) {
                torrent.integrity_next_probe_in = next_probe_in;
            }
        }
    }
}
