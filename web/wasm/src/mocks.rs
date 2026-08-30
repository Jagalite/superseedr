// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser-owned deterministic command fulfillment for the simulated demo.

use superseedr::web_integration::{
    BrowserCommand, BrowserSession, BrowserTorrentUpdate,
};

#[derive(Default)]
pub struct DemoCommandService {
    next_torrent_id: u8,
}

impl DemoCommandService {
    pub fn fulfill_pending(&mut self, session: &mut BrowserSession) -> Vec<BrowserCommand> {
        let commands = session.drain_commands();
        for command in &commands {
            match command {
                BrowserCommand::AddMagnet { magnet_link, .. } => {
                    self.next_torrent_id = self.next_torrent_id.wrapping_add(1).max(1);
                    let id = self.next_torrent_id;
                    session.upsert_mock_torrent(BrowserTorrentUpdate {
                        info_hash: vec![id; 20],
                        torrent_name: format!("Orbit Archive {id:02}"),
                        torrent_or_magnet: magnet_link.clone(),
                        pieces_total: 192,
                        pieces_completed: 0,
                        download_speed_bps: 0,
                        upload_speed_bps: 0,
                        activity_message: "Queued by simulated browser service".to_string(),
                    });
                }
                BrowserCommand::Pause { info_hash_hex } => {
                    let _ = session.set_torrent_paused_hex(info_hash_hex, true);
                }
                BrowserCommand::Resume { info_hash_hex } => {
                    let _ = session.set_torrent_paused_hex(info_hash_hex, false);
                }
                BrowserCommand::Delete { info_hash_hex, .. } => {
                    let _ = session.remove_torrent_hex(info_hash_hex);
                }
            }
        }
        commands
    }
}
