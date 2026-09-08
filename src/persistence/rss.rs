// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::config::{FeedSyncError, RssHistoryEntry};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct RssPersistedState {
    pub history: Vec<RssHistoryEntry>,
    pub last_sync_at: Option<String>,
    pub feed_errors: HashMap<String, FeedSyncError>,
}

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
#[allow(unused_imports)]
pub use native::{load_rss_state, rss_state_file_path, save_rss_state};
#[cfg(test)]
use native::{load_rss_state_from_path, save_rss_state_to_path};

#[cfg(test)]
use std::fs;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RssAddedVia;
    use tempfile::tempdir;

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("rss.toml");

        let state = load_rss_state_from_path(&path);
        assert_eq!(state, RssPersistedState::default());
    }

    #[test]
    fn load_invalid_file_returns_default() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("rss.toml");
        fs::write(&path, "not = [valid").expect("write malformed toml");

        let state = load_rss_state_from_path(&path);
        assert_eq!(state, RssPersistedState::default());
    }

    #[test]
    fn save_then_load_round_trip() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("rss.toml");

        let mut feed_errors = HashMap::new();
        feed_errors.insert(
            "https://example.com/rss".to_string(),
            FeedSyncError {
                message: "timeout".to_string(),
                occurred_at_iso: "2026-02-17T12:00:00Z".to_string(),
            },
        );

        let state = RssPersistedState {
            history: vec![RssHistoryEntry {
                dedupe_key: "guid:123".to_string(),
                info_hash: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
                guid: Some("123".to_string()),
                link: Some("https://example.com/item.torrent".to_string()),
                title: "SampleAlpha ISO".to_string(),
                source: Some("Example Feed".to_string()),
                date_iso: "2026-02-17T10:00:00Z".to_string(),
                added_via: RssAddedVia::Manual,
            }],
            last_sync_at: Some("2026-02-17T12:00:00Z".to_string()),
            feed_errors,
        };

        save_rss_state_to_path(&state, &path).expect("save rss state");
        let loaded = load_rss_state_from_path(&path);

        assert_eq!(loaded, state);
    }
}
