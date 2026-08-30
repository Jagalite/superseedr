// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser-owned deterministic command fulfillment for the simulated demo.

use std::path::Path;
use superseedr::web_integration::{
    BrowserCommand, BrowserFileTreeEntry, BrowserFileUpdate, BrowserJournalUpdate,
    BrowserPeerUpdate, BrowserRssUpdate, BrowserSession, BrowserTelemetryUpdate,
    BrowserTorrentControlState, BrowserTorrentUpdate,
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
                        data_available: true,
                        ..BrowserTorrentUpdate::default()
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
                BrowserCommand::FetchFileTree {
                    browser_generation,
                    path,
                    highlight_path,
                } => {
                    let _ = session.apply_mock_file_tree(
                        *browser_generation,
                        path.clone(),
                        mock_file_tree(path),
                        highlight_path.clone(),
                    );
                }
                BrowserCommand::AddTorrentFromFile { path } => {
                    self.next_torrent_id = self.next_torrent_id.wrapping_add(1).max(1);
                    let id = self.next_torrent_id;
                    let name = path
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .unwrap_or("Simulated File")
                        .to_string();
                    session.upsert_mock_torrent(BrowserTorrentUpdate {
                        info_hash: vec![id; 20],
                        torrent_name: name,
                        torrent_or_magnet: path.to_string_lossy().into_owned(),
                        pieces_total: 128,
                        activity_message: "Queued from mocked torrent metadata".to_string(),
                        data_available: true,
                        ..BrowserTorrentUpdate::default()
                    });
                }
                BrowserCommand::SetTorrentConfig {
                    info_hash_hex,
                    download_path,
                    container_name,
                    file_priorities,
                } => {
                    let _ = session.apply_mock_torrent_config(
                        info_hash_hex,
                        download_path.clone(),
                        container_name.clone(),
                        file_priorities,
                    );
                }
            }
        }
        commands
    }
}

fn mock_file_tree(path: &Path) -> Vec<BrowserFileTreeEntry> {
    match path.to_string_lossy().as_ref() {
        "/" => vec![BrowserFileTreeEntry {
            name: "simulated".to_string(),
            is_dir: true,
            ..BrowserFileTreeEntry::default()
        }],
        "/simulated/incoming" => vec![BrowserFileTreeEntry {
            name: "nested-fixture.torrent".to_string(),
            size: 16_384,
            is_dir: false,
        }],
        _ => vec![
            BrowserFileTreeEntry {
                name: "fixture-input.torrent".to_string(),
                size: 18_432,
                is_dir: false,
            },
            BrowserFileTreeEntry {
                name: "incoming".to_string(),
                is_dir: true,
                ..BrowserFileTreeEntry::default()
            },
        ],
    }
}

pub fn install_simulated_state(session: &mut BrowserSession) {
    let lifecycle = [
        (
            0x5a,
            "Nebula Field Sample",
            0,
            0,
            "Discovering simulated metadata",
            false,
            false,
        ),
        (
            0x6b,
            "Orbit Archive 02",
            256,
            91,
            "Downloading simulated pieces",
            true,
            false,
        ),
        (
            0x7c,
            "Lattice Study",
            144,
            52,
            "Simulated peer stall",
            true,
            false,
        ),
        (
            0x8d,
            "Prism Notes",
            320,
            320,
            "Checking simulated pieces",
            true,
            false,
        ),
        (
            0x9e,
            "Signal Garden",
            192,
            192,
            "Seeding simulated data",
            true,
            true,
        ),
        (
            0xaf,
            "Vector Almanac",
            96,
            96,
            "Simulated deletion pending",
            true,
            true,
        ),
    ];

    for (index, (byte, name, total, complete, activity, available, is_complete)) in
        lifecycle.into_iter().enumerate()
    {
        let files = vec![
            BrowserFileUpdate {
                relative_path: format!("set-{index}/segment-a.bin"),
                size: 4 * 1024 * 1024,
            },
            BrowserFileUpdate {
                relative_path: format!("set-{index}/segment-b.bin"),
                size: 6 * 1024 * 1024,
            },
        ];
        session.upsert_mock_torrent(BrowserTorrentUpdate {
            info_hash: vec![byte; 20],
            torrent_name: name.to_string(),
            torrent_or_magnet: format!("magnet:?xt=urn:btih:{}", hex_byte(byte)),
            pieces_total: total,
            pieces_completed: complete,
            download_speed_bps: if index == 1 { 3_200_000 } else { 0 },
            upload_speed_bps: if index == 4 { 640_000 } else { 48_000 },
            activity_message: activity.to_string(),
            download_path: Some(format!("/simulated/downloads/set-{index}").into()),
            container_name: Some(format!("set-{index}")),
            control_state: if index == 5 {
                BrowserTorrentControlState::Deleting
            } else {
                BrowserTorrentControlState::Running
            },
            data_available: available,
            is_complete,
            total_size: 10 * 1024 * 1024,
            bytes_written: if total == 0 {
                0
            } else {
                10 * 1024 * 1024 * u64::from(complete) / u64::from(total)
            },
            session_downloaded: (index as u64 + 1) * 12 * 1024 * 1024,
            session_uploaded: (index as u64 + 1) * 3 * 1024 * 1024,
            peers: vec![
                BrowserPeerUpdate {
                    address: format!("192.0.2.{}:6881", 10 + index),
                    client: format!("sim-peer-{index}-a"),
                    download_speed_bps: if index == 1 { 2_100_000 } else { 0 },
                    upload_speed_bps: 32_000,
                    total_downloaded: 8 * 1024 * 1024,
                    total_uploaded: 2 * 1024 * 1024,
                    bitfield: alternating_bits(total as usize, index),
                    active: index != 2,
                },
                BrowserPeerUpdate {
                    address: format!("198.51.100.{}:51413", 20 + index),
                    client: format!("sim-peer-{index}-b"),
                    download_speed_bps: if index == 1 { 1_100_000 } else { 0 },
                    upload_speed_bps: 16_000,
                    total_downloaded: 4 * 1024 * 1024,
                    total_uploaded: 1024 * 1024,
                    bitfield: alternating_bits(total as usize, index + 1),
                    active: index != 2,
                },
            ],
            files,
            download_history: vec![0, 400_000, 1_200_000, 2_400_000, 3_200_000],
            upload_history: vec![8_000, 16_000, 24_000, 40_000, 48_000],
            blocks_in_history: vec![0, 2, 5, 9, 12],
            blocks_out_history: vec![1, 1, 2, 3, 4],
            disk_read_bps: 1_400_000,
            disk_write_bps: 2_800_000,
            peer_discovery_history: vec![1, 3, 6, 8, 12],
            peer_connection_history: vec![0, 1, 2, 3, 4],
            peer_disconnect_history: vec![0, 0, 1, 0, 1],
        });
    }

    session.apply_mock_telemetry(BrowserTelemetryUpdate {
        cpu_usage: 17.5,
        ram_usage_percent: 42.0,
        app_ram_usage: 96 * 1024 * 1024,
        run_time: 7_321,
        total_download_history: vec![300_000, 900_000, 1_800_000, 2_700_000, 3_200_000],
        total_upload_history: vec![40_000, 64_000, 88_000, 104_000, 128_000],
        disk_read_history: vec![400_000, 700_000, 1_000_000, 1_400_000],
        disk_write_history: vec![900_000, 1_600_000, 2_100_000, 2_800_000],
        disk_read_bps: 1_400_000,
        disk_write_bps: 2_800_000,
        disk_backoff_history_ms: vec![0, 3, 0, 7, 1],
        dht_nodes: 1_248,
        dht_active_lookups: 3,
        dht_peers_found: 11,
        filesystem: vec![
            BrowserFileUpdate {
                relative_path: "incoming-demo.torrent".to_string(),
                size: 18_432,
            },
            BrowserFileUpdate {
                relative_path: "queued-example.torrent".to_string(),
                size: 22_016,
            },
        ],
        journal: vec![
            BrowserJournalUpdate {
                timestamp: "2026-08-30T12:00:00Z".to_string(),
                torrent_name: Some("Signal Garden".to_string()),
                message: "Simulated metadata resolved".to_string(),
            },
            BrowserJournalUpdate {
                timestamp: "2026-08-30T12:03:00Z".to_string(),
                torrent_name: Some("Prism Notes".to_string()),
                message: "Simulated piece check completed".to_string(),
            },
        ],
        rss: vec![BrowserRssUpdate {
            feed_url: "https://feed.invalid/simulated.xml".to_string(),
            filter_query: "signal garden".to_string(),
            item_title: "Signal Garden Dispatch".to_string(),
            item_link: "magnet:?xt=urn:btih:b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0".to_string(),
            timestamp: "2026-08-30T12:04:00Z".to_string(),
        }],
    });
}

fn alternating_bits(len: usize, offset: usize) -> Vec<bool> {
    (0..len)
        .map(|piece| !(piece + offset).is_multiple_of(3))
        .collect()
}

fn hex_byte(byte: u8) -> String {
    format!("{byte:02x}").repeat(20)
}
