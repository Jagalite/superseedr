// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared application torrent helpers definitions and transitions.

use super::*;

pub(super) fn activity_marks_torrent_complete(activity_message: &str) -> bool {
    activity_message.contains("Seeding") || activity_message.contains("Finished")
}

pub(super) fn torrent_has_skipped_files(metrics: &TorrentMetrics) -> bool {
    metrics
        .file_priorities
        .values()
        .any(|p| matches!(p, FilePriority::Skip))
}

pub fn torrent_is_effectively_incomplete(metrics: &TorrentMetrics) -> bool {
    if activity_marks_torrent_complete(&metrics.activity_message) {
        return false;
    }
    if torrent_has_skipped_files(metrics) {
        return false;
    }
    if metrics.number_of_pieces_total == 0 {
        return !metrics.is_complete;
    }
    metrics.number_of_pieces_total > 0
        && metrics.number_of_pieces_completed < metrics.number_of_pieces_total
}

pub fn torrent_completion_percent(metrics: &TorrentMetrics) -> f64 {
    if activity_marks_torrent_complete(&metrics.activity_message) {
        return 100.0;
    }
    if torrent_has_skipped_files(metrics) {
        return 100.0;
    }
    if metrics.number_of_pieces_total == 0 {
        return 0.0;
    }

    ((metrics.number_of_pieces_completed as f64 / metrics.number_of_pieces_total as f64) * 100.0)
        .min(100.0)
}

pub(super) fn compose_system_warning(
    base_warning: Option<&str>,
    dht_bootstrap_warning: Option<&str>,
) -> Option<String> {
    match (base_warning, dht_bootstrap_warning) {
        (Some(base), Some(dht)) => Some(format!("{} | {}", base, dht)),
        (Some(base), None) => Some(base.to_string()),
        (None, Some(dht)) => Some(dht.to_string()),
        (None, None) => None,
    }
}

pub(super) fn validate_runtime_control_request(request: &ControlRequest) -> Result<(), String> {
    if matches!(request, ControlRequest::MoveTorrent { .. }) {
        return Err(
            "The move command is CLI-only and requires the superseedr client to be stopped."
                .to_string(),
        );
    }
    Ok(())
}

pub fn parse_hybrid_hashes(magnet_link: &str) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    crate::torrent_identity::parse_hybrid_hashes(magnet_link)
}

pub fn info_hash_from_torrent_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
    crate::torrent_identity::info_hash_from_torrent_bytes(bytes)
}

pub(crate) fn resolve_magnet_torrent_name(
    requested_name: &str,
    magnet_link: &str,
    info_hash: &[u8],
) -> String {
    let is_placeholder = requested_name.trim().is_empty() || requested_name == "Fetching name...";
    if !is_placeholder {
        return requested_name.to_string();
    }

    extract_magnet_display_name(magnet_link)
        .unwrap_or_else(|| format!("Magnet {}", hex::encode(info_hash)))
}

pub(super) fn torrent_file_count(torrent: &crate::torrent_file::Torrent) -> usize {
    if torrent.info.files.is_empty() {
        1
    } else {
        torrent.info.files.len()
    }
}

pub(super) fn torrent_piece_count(torrent: &crate::torrent_file::Torrent) -> u32 {
    if !torrent.info.pieces.is_empty() {
        return (torrent.info.pieces.len() / 20) as u32;
    }

    let total_len = torrent.info.total_length();
    if torrent.info.piece_length > 0 {
        ((total_len as f64) / (torrent.info.piece_length as f64)).ceil() as u32
    } else {
        0
    }
}

pub(super) fn extract_magnet_display_name(magnet_link: &str) -> Option<String> {
    for raw_part in magnet_link.split('&') {
        let part = raw_part.strip_prefix("magnet:?").unwrap_or(raw_part);
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if key.eq_ignore_ascii_case("dn") {
            let value_for_decode = value.replace('+', "%20");
            if let Ok(decoded) = urlencoding::decode(&value_for_decode) {
                let name = decoded.trim();
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

pub(super) fn extract_magnet_exact_length(magnet_link: &str) -> Option<u64> {
    for raw_part in magnet_link.split('&') {
        let part = raw_part.strip_prefix("magnet:?").unwrap_or(raw_part);
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if key.eq_ignore_ascii_case("xl") {
            return value.parse::<u64>().ok();
        }
    }
    None
}

pub(super) fn normalize_magnet_metadata_path(name: &str) -> String {
    name.replace('\\', "/")
        .split('/')
        .filter(|segment| {
            let segment = segment.trim();
            !segment.is_empty() && segment != "." && segment != ".."
        })
        .collect::<Vec<_>>()
        .join("/")
}
