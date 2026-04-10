// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

pub mod client;

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

pub fn normalize_tracker_urls<I, S>(urls: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut seen = HashSet::new();
    let mut udp_keys = HashSet::new();
    let mut entries = Vec::new();

    for raw in urls {
        let raw = raw.as_ref().trim();
        if raw.is_empty() || !seen.insert(raw.to_string()) {
            continue;
        }

        let parsed = match Url::parse(raw) {
            Ok(url) => url,
            Err(_) => continue,
        };

        let scheme = parsed.scheme().to_ascii_lowercase();
        if !matches!(scheme.as_str(), "http" | "https" | "udp") {
            continue;
        }

        let preference_key = tracker_preference_key(&parsed);
        if scheme == "udp" {
            if let Some(key) = &preference_key {
                udp_keys.insert(key.clone());
            }
        }

        entries.push(NormalizedTrackerUrl {
            raw: raw.to_string(),
            scheme,
            preference_key,
        });
    }

    entries
        .into_iter()
        .filter(|entry| {
            !(matches!(entry.scheme.as_str(), "http" | "https")
                && entry
                    .preference_key
                    .as_ref()
                    .is_some_and(|key| udp_keys.contains(key)))
        })
        .map(|entry| entry.raw)
        .collect()
}

#[derive(Debug)]
struct NormalizedTrackerUrl {
    raw: String,
    scheme: String,
    preference_key: Option<TrackerPreferenceKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TrackerPreferenceKey {
    host: String,
    port: u16,
    path: String,
    query: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

fn tracker_preference_key(url: &Url) -> Option<TrackerPreferenceKey> {
    Some(TrackerPreferenceKey {
        host: url.host_str()?.to_ascii_lowercase(),
        port: url.port_or_known_default()?,
        path: normalize_tracker_path(url.path()),
        query: url.query().map(str::to_string),
        username: (!url.username().is_empty()).then(|| url.username().to_string()),
        password: url.password().map(str::to_string),
    })
}

fn normalize_tracker_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_tracker_urls;

    #[test]
    fn normalize_tracker_urls_prefers_udp_tracker_when_http_matches() {
        let urls = normalize_tracker_urls([
            "http://tracker.local:6969/announce",
            "udp://tracker.local:6969/announce",
            "https://tracker-alt.local/announce",
        ]);

        assert_eq!(
            urls,
            vec![
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
}
