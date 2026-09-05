// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

pub mod client;
mod error;

pub(crate) use error::TrackerError;

use crate::torrent_file::Torrent;
use std::collections::HashSet;
use std::fmt;
use std::net::SocketAddr;

use reqwest::Url;
use serde::Deserialize;
use serde_bytes::ByteBuf;

#[derive(Debug, Clone, Copy)]
pub enum TrackerEvent {
    Started,
    Completed,
    Stopped,
}
impl fmt::Display for TrackerEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrackerEvent::Started => write!(f, "started"),
            TrackerEvent::Completed => write!(f, "completed"),
            TrackerEvent::Stopped => write!(f, "stopped"),
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct TrackerResponse {
    pub failure_reason: Option<String>,
    pub warning_message: Option<String>,
    pub interval: i64,
    pub min_interval: Option<i64>,
    pub tracker_id: Option<String>,
    pub complete: i64,
    pub incomplete: i64,
    pub peers: Vec<SocketAddr>,
}

#[derive(Debug, Deserialize)]
struct PeerDictModel {
    ip: String,
    port: u16,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Peers {
    Compact(#[serde(with = "serde_bytes")] Vec<u8>),
    Dicts(Vec<PeerDictModel>),
}

#[derive(Debug, Deserialize)]
struct RawTrackerResponse {
    #[serde(rename = "failure reason", default)]
    failure_reason: Option<String>,
    #[serde(rename = "warning message", default)]
    warning_message: Option<String>,
    #[serde(default)]
    interval: i64,
    #[serde(rename = "min interval", default)]
    min_interval: Option<i64>,
    #[serde(rename = "tracker id", default)]
    tracker_id: Option<String>,
    #[serde(default)]
    complete: i64,
    #[serde(default)]
    incomplete: i64,
    #[serde(default)]
    peers: Option<Peers>,
    #[serde(rename = "peers6", default)]
    peers6: Option<ByteBuf>,
}

pub(crate) fn is_websocket_tracker_url(url: &str) -> bool {
    #[cfg(feature = "webtorrent")]
    {
        Url::parse(url).is_ok_and(|parsed| {
            matches!(parsed.scheme().to_ascii_lowercase().as_str(), "ws" | "wss")
        })
    }
    #[cfg(not(feature = "webtorrent"))]
    {
        let _ = url;
        false
    }
}

pub fn normalize_tracker_urls<I, S>(urls: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut seen = HashSet::new();
    let mut entries = Vec::new();

    for raw in urls {
        let raw = raw.as_ref().trim();
        if raw.is_empty() {
            continue;
        }

        let mut parsed = match Url::parse(raw) {
            Ok(url) => url,
            Err(_) => continue,
        };

        let scheme = parsed.scheme().to_ascii_lowercase();
        let is_websocket = is_websocket_tracker_url(raw);
        let supported = matches!(scheme.as_str(), "http" | "https" | "udp")
            || cfg!(feature = "webtorrent") && is_websocket;
        if !supported {
            continue;
        }

        let normalized = if is_websocket {
            // URL fragments are client-side labels and are never sent in a WebSocket request.
            // Strip them before the URL becomes a TrackerState key so tracker events use
            // the same canonical identity as the manager map.
            parsed.set_fragment(None);
            parsed.to_string()
        } else {
            raw.to_string()
        };
        if seen.insert(normalized.clone()) {
            entries.push(normalized);
        }
    }

    entries
}

pub fn torrent_tracker_urls(torrent: &Torrent) -> Vec<String> {
    let mut urls = Vec::new();
    if let Some(announce) = &torrent.announce {
        urls.push(announce.clone());
    }
    if let Some(announce_list) = &torrent.announce_list {
        for tier in announce_list {
            urls.extend(tier.iter().cloned());
        }
    }
    normalize_tracker_urls(urls)
}

#[cfg(test)]
mod tests {
    use super::{normalize_tracker_urls, torrent_tracker_urls};
    use crate::torrent_file::Torrent;

    #[test]
    fn torrent_tracker_urls_flattens_announce_list_and_keeps_http_fallback() {
        let torrent = Torrent {
            announce: Some("http://tracker.local:6969/announce".to_string()),
            announce_list: Some(vec![vec![
                "udp://tracker.local:6969/announce".to_string(),
                "https://tracker-alt.local/announce".to_string(),
            ]]),
            ..Torrent::default()
        };

        assert_eq!(
            torrent_tracker_urls(&torrent),
            vec![
                "http://tracker.local:6969/announce".to_string(),
                "udp://tracker.local:6969/announce".to_string(),
                "https://tracker-alt.local/announce".to_string(),
            ]
        );
    }
    #[cfg(feature = "webtorrent")]
    use super::is_websocket_tracker_url;

    #[cfg(feature = "webtorrent")]
    #[test]
    fn websocket_tracker_classification_is_parsed_and_case_insensitive() {
        assert!(is_websocket_tracker_url("WSS://tracker.local/announce"));
        assert!(is_websocket_tracker_url("Ws://127.0.0.1:9000/announce"));
        assert!(!is_websocket_tracker_url("https://tracker.local/announce"));
        assert!(!is_websocket_tracker_url("not a tracker URL"));
    }

    #[cfg(feature = "webtorrent")]
    #[test]
    fn websocket_tracker_normalization_strips_fragments_before_state_keying() {
        let urls = normalize_tracker_urls([
            "WSS://tracker.local/announce#primary",
            "wss://tracker.local/announce#duplicate",
        ]);

        assert_eq!(urls, vec!["wss://tracker.local/announce".to_string()]);
    }

    #[cfg(not(feature = "webtorrent"))]
    #[test]
    fn websocket_tracker_classification_is_disabled_without_feature() {
        assert!(!super::is_websocket_tracker_url(
            "WSS://tracker.local/announce"
        ));
    }

    #[test]
    fn normalize_tracker_urls_keeps_http_tracker_when_udp_matches() {
        let urls = normalize_tracker_urls([
            "http://tracker.local:6969/announce",
            "udp://tracker.local:6969/announce",
            "https://tracker-alt.local/announce",
        ]);

        assert_eq!(
            urls,
            vec![
                "http://tracker.local:6969/announce".to_string(),
                "udp://tracker.local:6969/announce".to_string(),
                "https://tracker-alt.local/announce".to_string(),
            ]
        );
    }

    #[test]
    fn normalize_tracker_urls_keeps_distinct_tracker_paths() {
        let urls = normalize_tracker_urls([
            "http://tracker.local:6969/announce",
            "udp://tracker.local:6969/other",
        ]);

        assert_eq!(
            urls,
            vec![
                "http://tracker.local:6969/announce".to_string(),
                "udp://tracker.local:6969/other".to_string(),
            ]
        );
    }

    #[test]
    fn normalize_tracker_urls_keeps_authenticated_http_tracker_alongside_udp() {
        let urls = normalize_tracker_urls([
            "https://tracker.local:6969/announce?token=abc123",
            "udp://tracker.local:6969/announce",
        ]);

        assert_eq!(
            urls,
            vec![
                "https://tracker.local:6969/announce?token=abc123".to_string(),
                "udp://tracker.local:6969/announce".to_string(),
            ]
        );
    }

    #[test]
    fn normalize_tracker_urls_keeps_credentialed_http_tracker_alongside_udp() {
        let urls = normalize_tracker_urls([
            "https://user:pass@tracker.local:6969/announce",
            "udp://tracker.local:6969/announce",
        ]);

        assert_eq!(
            urls,
            vec![
                "https://user:pass@tracker.local:6969/announce".to_string(),
                "udp://tracker.local:6969/announce".to_string(),
            ]
        );
    }

    #[cfg(not(feature = "webtorrent"))]
    #[test]
    fn normalize_tracker_urls_drops_websocket_trackers_without_feature() {
        let urls = normalize_tracker_urls([
            "WSS://tracker.local/announce",
            "https://tracker.local/announce",
        ]);

        assert_eq!(urls, vec!["https://tracker.local/announce".to_string()]);
    }

    #[cfg(feature = "webtorrent")]
    #[test]
    fn normalize_tracker_urls_keeps_websocket_trackers_with_feature() {
        let urls = normalize_tracker_urls([
            "WSS://tracker.local/announce",
            "Ws://127.0.0.1:9000/announce",
        ]);

        assert_eq!(
            urls,
            vec![
                "wss://tracker.local/announce".to_string(),
                "ws://127.0.0.1:9000/announce".to_string(),
            ]
        );
    }
}
