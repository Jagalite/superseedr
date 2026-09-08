// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Platform-neutral peer policy and presentation data.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use web_time::SystemTime;

pub(crate) const SUPERSEEDR_CLIENT_CODE: &[u8; 2] = b"SS";
pub(crate) const SUPERSEEDR_CLIENT_NAME: &str = "Superseedr";

type InfoHash = Vec<u8>;

fn format_superseedr_peer_version(version: &[u8]) -> String {
    if version.len() == 4 && version.iter().all(u8::is_ascii_digit) {
        let major = version[0] - b'0';
        let minor = version[1] - b'0';
        let patch = (version[2] - b'0') * 10 + (version[3] - b'0');
        format!("{major}.{minor}.{patch}")
    } else {
        String::from_utf8_lossy(version).into_owned()
    }
}

pub(crate) fn parse_peer_client(peer_id: &[u8]) -> String {
    if peer_id.len() < 8 {
        return "Unknown".to_string();
    }

    if peer_id[0] == b'-' && peer_id[7] == b'-' {
        let client_code = &peer_id[1..3];
        let version = &peer_id[3..7];
        if client_code == SUPERSEEDR_CLIENT_CODE {
            return format!(
                "{SUPERSEEDR_CLIENT_NAME} {}",
                format_superseedr_peer_version(version)
            );
        }
        let client_name = match client_code {
            b"BC" => "BitComet",
            b"TR" => "Transmission",
            b"UT" => "µTorrent",
            b"qB" => "qBittorrent",
            b"AZ" => "Vuze/Azureus",
            b"LT" => "libtorrent",
            b"DE" => "Deluge",
            b"S" | b"SD" => "Shadow",
            _ => {
                return format!(
                    "Unknown ({}{})",
                    String::from_utf8_lossy(client_code),
                    String::from_utf8_lossy(version)
                );
            }
        };
        return format!("{} {}", client_name, String::from_utf8_lossy(version));
    }

    if peer_id.starts_with(b"M")
        && peer_id[1..8]
            .iter()
            .all(|c| c.is_ascii_digit() || *c == b'-')
    {
        let version = String::from_utf8_lossy(&peer_id[1..8])
            .trim_matches('-')
            .replace('-', ".");
        return format!("Mainline {version}");
    }

    if peer_id.starts_with(b"exbc") && peer_id.len() >= 6 {
        return format!("BitComet {}.{:02}", peer_id[4], peer_id[5]);
    }

    "Unknown".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum PeerRestrictionReason {
    ExcessiveUpload {
        uploaded_bytes: u64,
        threshold_bytes: u64,
    },
    ExcessiveDownload {
        downloaded_bytes: u64,
        threshold_bytes: u64,
    },
    ReconnectChurn {
        reconnects: u32,
        threshold: u32,
        window_secs: u64,
    },
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PeerRestriction {
    pub detected_at: SystemTime,
    pub blocked_until: SystemTime,
    pub torrent_info_hash: Option<InfoHash>,
    pub reason: PeerRestrictionReason,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct PeerPolicy {
    pub restrictions: HashMap<IpAddr, PeerRestriction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeerManagerEndpointView {
    pub address: String,
    pub total_downloaded: u64,
    pub total_uploaded: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeerManagerTrackedPeer {
    pub torrent_info_hash: Vec<u8>,
    pub torrent_name: String,
    pub ip: IpAddr,
    pub is_active: bool,
    pub endpoints: Vec<PeerManagerEndpointView>,
    pub downloaded_evidence_bytes: u64,
    pub uploaded_evidence_bytes: u64,
    pub total_downloaded_bytes: u64,
    pub total_uploaded_bytes: u64,
    pub connection_count: u64,
    pub disconnect_count: u64,
    pub transfer_threshold_bytes: u64,
    pub reconnect_count: u32,
    pub reconnect_limit: u32,
    pub reconnect_window_secs: u64,
    pub last_seen: Option<SystemTime>,
    pub clients: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PeerManagerView {
    pub registered_torrents: usize,
    pub metrics_updates: u64,
    pub tracked_peers: Vec<PeerManagerTrackedPeer>,
}
