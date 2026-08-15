// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::config::runtime_persistence_dir;
use crate::fs_atomic::{
    deserialize_versioned_toml, serialize_versioned_toml, write_string_atomically,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tracing::{event as tracing_event, Level};

const EVENT_JOURNAL_FILE_NAME: &str = "event_journal.toml";
const SHARED_EVENT_JOURNAL_FILE_NAME: &str = "shared_event_journal.toml";
pub const EVENT_JOURNAL_CAP: usize = 5_000;
pub const EVENT_JOURNAL_HEALTH_CAP: usize = 1_500;
pub const EVENT_JOURNAL_OPERATOR_CAP: usize = EVENT_JOURNAL_CAP - EVENT_JOURNAL_HEALTH_CAP;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum EventScope {
    #[default]
    Host,
    Shared,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum EventCategory {
    #[default]
    Ingest,
    TorrentLifecycle,
    DataHealth,
    Control,
    Network,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    #[default]
    IngestQueued,
    IngestAdded,
    IngestDuplicate,
    IngestInvalid,
    IngestFailed,
    TorrentCompleted,
    DataUnavailable,
    DataRecovered,
    ControlQueued,
    ControlApplied,
    ControlFailed,
    NetworkRebinding,
    NetworkBlocked,
    NetworkRestored,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum IngestOrigin {
    #[default]
    WatchFolder,
    RssAuto,
    RssManual,
}

#[allow(clippy::enum_variant_names)]
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum IngestKind {
    #[default]
    TorrentFile,
    MagnetFile,
    PathFile,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ControlOrigin {
    #[default]
    CliOnline,
    CliOffline,
    WatchFolder,
    RssAuto,
    RssManual,
    SharedRelay,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventDetails {
    #[default]
    None,
    Ingest {
        origin: IngestOrigin,
        ingest_kind: IngestKind,
        #[serde(default)]
        download_path: Option<PathBuf>,
        #[serde(default)]
        container_name: Option<String>,
        #[serde(default)]
        payload_path: Option<PathBuf>,
    },
    DataHealth {
        issue_count: usize,
        issue_files: Vec<String>,
    },
    Control {
        origin: ControlOrigin,
        action: String,
        target_info_hash_hex: Option<String>,
        file_index: Option<usize>,
        file_path: Option<String>,
        priority: Option<String>,
    },
    Network {
        interface: Option<String>,
        generation_id: Option<u64>,
        listen_port: Option<u16>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct EventJournalEntry {
    pub id: u64,
    pub scope: EventScope,
    pub host_id: Option<String>,
    pub ts_iso: String,
    pub category: EventCategory,
    pub event_type: EventType,
    pub torrent_name: Option<String>,
    pub info_hash_hex: Option<String>,
    pub source_watch_folder: Option<PathBuf>,
    pub source_path: Option<PathBuf>,
    pub correlation_id: Option<String>,
    pub message: Option<String>,
    pub details: EventDetails,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct EventJournalState {
    pub next_id: u64,
    pub entries: Vec<EventJournalEntry>,
}

pub fn event_journal_state_file_path() -> io::Result<PathBuf> {
    let data_dir = runtime_persistence_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Could not resolve app data directory for event journal persistence",
        )
    })?;

    Ok(data_dir.join(EVENT_JOURNAL_FILE_NAME))
}

pub fn shared_event_journal_state_file_path() -> io::Result<PathBuf> {
    let root_dir = crate::config::shared_root_path().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Could not resolve shared config root for shared event journal persistence",
        )
    })?;

    Ok(root_dir
        .join("journal")
        .join(SHARED_EVENT_JOURNAL_FILE_NAME))
}

pub fn load_event_journal_state() -> EventJournalState {
    let mut merged = match event_journal_state_file_path() {
        Ok(path) => load_event_journal_state_from_path(&path),
        Err(e) => {
            tracing_event!(
                Level::WARN,
                "Failed to get event journal persistence path. Using empty state: {}",
                e
            );
            EventJournalState::default()
        }
    };

    if crate::config::is_shared_config_mode() {
        match shared_event_journal_state_file_path() {
            Ok(path) => {
                let shared = load_event_journal_state_from_path(&path);
                merged.entries.extend(shared.entries);
                merged
                    .entries
                    .sort_by(|a, b| a.ts_iso.cmp(&b.ts_iso).then_with(|| a.id.cmp(&b.id)));
                enforce_event_journal_retention(&mut merged);
                merged.next_id = merged
                    .entries
                    .iter()
                    .map(|entry| entry.id)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
            }
            Err(e) => {
                tracing_event!(
                    Level::WARN,
                    "Failed to get shared event journal persistence path. Continuing with host journal only: {}",
                    e
                );
            }
        }
    }

    merged
}

pub fn save_event_journal_state(state: &EventJournalState) -> io::Result<()> {
    if crate::config::is_shared_config_mode() {
        let shared_path = shared_event_journal_state_file_path()?;
        let shared_state = EventJournalState {
            next_id: state.next_id,
            entries: state
                .entries
                .iter()
                .filter(|entry| entry.scope == EventScope::Shared)
                .cloned()
                .collect(),
        };
        save_host_event_journal_state(state)?;
        save_event_journal_state_to_path(&shared_state, &shared_path)
    } else {
        let path = event_journal_state_file_path()?;
        save_event_journal_state_to_path(state, &path)
    }
}

/// Persists only host-scoped entries to the host-local journal.
///
/// Shared followers use this path so recording local runtime health can never
/// rewrite the shared journal owned by the leader.
pub fn save_host_event_journal_state(state: &EventJournalState) -> io::Result<()> {
    let path = event_journal_state_file_path()?;
    let host_state = EventJournalState {
        next_id: state.next_id,
        entries: state
            .entries
            .iter()
            .filter(|entry| entry.scope == EventScope::Host)
            .cloned()
            .collect(),
    };
    save_event_journal_state_to_path(&host_state, &path)
}

pub fn event_journal_json() -> io::Result<String> {
    serde_json::to_string_pretty(&load_event_journal_state()).map_err(io::Error::other)
}

pub fn enforce_event_journal_retention(state: &mut EventJournalState) {
    let mut retained = state
        .entries
        .iter()
        .rev()
        .scan((0usize, 0usize), |(operator_count, health_count), entry| {
            let keep = match entry.category {
                EventCategory::DataHealth => {
                    if *health_count < EVENT_JOURNAL_HEALTH_CAP {
                        *health_count += 1;
                        true
                    } else {
                        false
                    }
                }
                EventCategory::Ingest
                | EventCategory::Control
                | EventCategory::TorrentLifecycle
                | EventCategory::Network => {
                    if *operator_count < EVENT_JOURNAL_OPERATOR_CAP {
                        *operator_count += 1;
                        true
                    } else {
                        false
                    }
                }
            };
            Some((keep, entry.clone()))
        })
        .filter_map(|(keep, entry)| keep.then_some(entry))
        .collect::<Vec<_>>();

    retained.reverse();
    state.entries = retained;
}

pub fn append_event_journal_entry(state: &mut EventJournalState, mut entry: EventJournalEntry) {
    entry.id = state.next_id;
    state.next_id = state.next_id.saturating_add(1);
    state.entries.push(entry);
    enforce_event_journal_retention(state);
}

fn load_event_journal_state_from_path(path: &Path) -> EventJournalState {
    if !path.exists() {
        return EventJournalState::default();
    }

    match fs::read_to_string(path) {
        Ok(content) => match deserialize_versioned_toml::<EventJournalState>(&content) {
            Ok(mut state) => {
                enforce_event_journal_retention(&mut state);
                state
            }
            Err(e) => {
                tracing_event!(
                    Level::WARN,
                    "Failed to parse event journal file {:?}. Resetting event journal state: {}",
                    path,
                    e
                );
                EventJournalState::default()
            }
        },
        Err(e) => {
            tracing_event!(
                Level::WARN,
                "Failed to read event journal file {:?}. Using empty state: {}",
                path,
                e
            );
            EventJournalState::default()
        }
    }
}

fn save_event_journal_state_to_path(state: &EventJournalState, path: &Path) -> io::Result<()> {
    let mut journal_state = state.clone();
    enforce_event_journal_retention(&mut journal_state);

    let content = serialize_versioned_toml(&journal_state)?;
    write_string_atomically(path, &content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        clear_shared_config_state_for_tests, set_app_paths_override_for_tests,
        shared_env_guard_for_tests,
    };
    use tempfile::tempdir;

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("event_journal.toml");

        let state = load_event_journal_state_from_path(&path);
        assert_eq!(state, EventJournalState::default());
    }

    #[test]
    fn load_invalid_file_returns_default() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("event_journal.toml");
        fs::write(&path, "not = [valid").expect("write malformed toml");

        let state = load_event_journal_state_from_path(&path);
        assert_eq!(state, EventJournalState::default());
    }

    #[test]
    fn save_then_load_round_trip() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("event_journal.toml");

        let state = EventJournalState {
            next_id: 2,
            entries: vec![EventJournalEntry {
                id: 1,
                scope: EventScope::Host,
                host_id: Some("node-a".to_string()),
                ts_iso: "2026-03-15T12:00:00Z".to_string(),
                category: EventCategory::Ingest,
                event_type: EventType::IngestAdded,
                torrent_name: Some("Sample Alpha Episode 1".to_string()),
                info_hash_hex: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
                source_watch_folder: Some(PathBuf::from("/watch")),
                source_path: Some(PathBuf::from("/watch/alpha.magnet")),
                correlation_id: Some("corr-1".to_string()),
                message: Some("Added torrent from watched magnet file".to_string()),
                details: EventDetails::Ingest {
                    origin: IngestOrigin::WatchFolder,
                    ingest_kind: IngestKind::MagnetFile,
                    download_path: Some(PathBuf::from("/downloads")),
                    container_name: Some("Sample Alpha".to_string()),
                    payload_path: Some(PathBuf::from("/downloads/Sample Alpha")),
                },
            }],
        };

        save_event_journal_state_to_path(&state, &path).expect("save event journal state");
        let loaded = load_event_journal_state_from_path(&path);

        assert_eq!(loaded, state);
    }

    #[test]
    fn network_event_round_trips_with_diagnostic_details() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("event_journal.toml");
        let state = EventJournalState {
            next_id: 2,
            entries: vec![EventJournalEntry {
                id: 1,
                scope: EventScope::Host,
                host_id: Some("node-test".to_string()),
                ts_iso: "2026-03-15T12:00:00Z".to_string(),
                category: EventCategory::Network,
                event_type: EventType::NetworkBlocked,
                message: Some(
                    "interface interface-test0 was not found: Device not configured (os error 6)"
                        .to_string(),
                ),
                details: EventDetails::Network {
                    interface: Some("interface-test0".to_string()),
                    generation_id: Some(8),
                    listen_port: None,
                },
                ..Default::default()
            }],
        };

        save_event_journal_state_to_path(&state, &path).expect("save network journal event");
        let loaded = load_event_journal_state_from_path(&path);

        assert_eq!(loaded, state);
    }

    #[test]
    fn retention_prunes_oldest_entries() {
        let mut state = EventJournalState {
            next_id: (EVENT_JOURNAL_CAP + 3) as u64,
            entries: (0..(EVENT_JOURNAL_OPERATOR_CAP + 2))
                .map(|idx| EventJournalEntry {
                    id: idx as u64,
                    ts_iso: format!("2026-03-15T12:00:{idx:02}Z"),
                    category: EventCategory::Control,
                    ..Default::default()
                })
                .chain(
                    (0..(EVENT_JOURNAL_HEALTH_CAP + 2)).map(|idx| EventJournalEntry {
                        id: (EVENT_JOURNAL_OPERATOR_CAP + 2 + idx) as u64,
                        ts_iso: format!("2026-03-15T13:00:{idx:02}Z"),
                        category: EventCategory::DataHealth,
                        ..Default::default()
                    }),
                )
                .collect(),
        };

        enforce_event_journal_retention(&mut state);

        assert_eq!(state.entries.len(), EVENT_JOURNAL_CAP);
        let retained_controls = state
            .entries
            .iter()
            .filter(|entry| entry.category == EventCategory::Control)
            .count();
        let retained_health = state
            .entries
            .iter()
            .filter(|entry| entry.category == EventCategory::DataHealth)
            .count();
        assert_eq!(retained_controls, EVENT_JOURNAL_OPERATOR_CAP);
        assert_eq!(retained_health, EVENT_JOURNAL_HEALTH_CAP);
    }

    #[test]
    fn append_entry_assigns_next_id_and_prunes() {
        let mut state = EventJournalState {
            next_id: 7,
            entries: Vec::new(),
        };

        append_event_journal_entry(
            &mut state,
            EventJournalEntry {
                ts_iso: "2026-03-17T12:00:00Z".to_string(),
                category: EventCategory::Control,
                event_type: EventType::ControlApplied,
                details: EventDetails::Control {
                    origin: ControlOrigin::CliOffline,
                    action: "pause".to_string(),
                    target_info_hash_hex: Some(
                        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
                    ),
                    file_index: None,
                    file_path: None,
                    priority: None,
                },
                ..Default::default()
            },
        );

        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].id, 7);
        assert_eq!(state.next_id, 8);
    }

    #[test]
    fn event_journal_json_serializes_current_state() {
        let json = serde_json::to_string_pretty(&EventJournalState::default())
            .expect("serialize journal state");
        assert!(json.contains("\"next_id\""));
        assert!(json.contains("\"entries\""));
    }

    #[test]
    fn shared_mode_saves_host_and_shared_entries_to_separate_files() {
        let _guard = shared_env_guard_for_tests()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let shared_root = tempdir().expect("create shared root");
        let local_paths = tempdir().expect("create local app paths");
        let config_dir = local_paths.path().join("config");
        let data_dir = local_paths.path().join("data");
        set_app_paths_override_for_tests(Some((config_dir, data_dir)));

        let original_shared_dir = std::env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
        let original_host_id = std::env::var_os("SUPERSEEDR_SHARED_HOST_ID");

        std::env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", shared_root.path());
        std::env::set_var("SUPERSEEDR_SHARED_HOST_ID", "node-a");
        clear_shared_config_state_for_tests();

        let host_entry = EventJournalEntry {
            id: 1,
            scope: EventScope::Host,
            host_id: Some("node-a".to_string()),
            ts_iso: "2026-03-26T10:00:00Z".to_string(),
            category: EventCategory::DataHealth,
            event_type: EventType::DataUnavailable,
            torrent_name: Some("Sample Fault".to_string()),
            info_hash_hex: Some("1111111111111111111111111111111111111111".to_string()),
            details: EventDetails::DataHealth {
                issue_count: 1,
                issue_files: vec!["missing.bin".to_string()],
            },
            ..Default::default()
        };
        let shared_entry = EventJournalEntry {
            id: 2,
            scope: EventScope::Shared,
            host_id: Some("node-a".to_string()),
            ts_iso: "2026-03-26T10:01:00Z".to_string(),
            category: EventCategory::Control,
            event_type: EventType::ControlApplied,
            details: EventDetails::Control {
                origin: ControlOrigin::CliOffline,
                action: "pause".to_string(),
                target_info_hash_hex: Some("2222222222222222222222222222222222222222".to_string()),
                file_index: None,
                file_path: None,
                priority: None,
            },
            ..Default::default()
        };
        let state = EventJournalState {
            next_id: 3,
            entries: vec![host_entry.clone(), shared_entry.clone()],
        };

        save_event_journal_state(&state).expect("save split event journal");

        let host_path = event_journal_state_file_path().expect("host journal path");
        let shared_path = shared_event_journal_state_file_path().expect("shared journal path");
        let host_state = load_event_journal_state_from_path(&host_path);
        let shared_state = load_event_journal_state_from_path(&shared_path);
        let merged_state = load_event_journal_state();

        assert_eq!(host_state.entries, vec![host_entry]);
        assert_eq!(shared_state.entries, vec![shared_entry]);
        assert_eq!(merged_state.entries.len(), 2);
        assert!(merged_state
            .entries
            .iter()
            .any(|entry| entry.category == EventCategory::DataHealth));
        assert!(merged_state
            .entries
            .iter()
            .any(|entry| entry.category == EventCategory::Control));
        assert_eq!(merged_state.next_id, 3);

        if let Some(value) = original_shared_dir {
            std::env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", value);
        } else {
            std::env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
        }
        if let Some(value) = original_host_id {
            std::env::set_var("SUPERSEEDR_SHARED_HOST_ID", value);
        } else {
            std::env::remove_var("SUPERSEEDR_SHARED_HOST_ID");
        }
        clear_shared_config_state_for_tests();
        set_app_paths_override_for_tests(None);
    }

    #[test]
    fn host_only_save_does_not_modify_the_shared_journal() {
        let _guard = shared_env_guard_for_tests()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let shared_root = tempdir().expect("create shared root");
        let local_paths = tempdir().expect("create local app paths");
        set_app_paths_override_for_tests(Some((
            local_paths.path().join("config"),
            local_paths.path().join("data"),
        )));

        let original_shared_dir = std::env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
        let original_host_id = std::env::var_os("SUPERSEEDR_SHARED_HOST_ID");
        std::env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", shared_root.path());
        std::env::set_var("SUPERSEEDR_SHARED_HOST_ID", "node-follower");
        clear_shared_config_state_for_tests();

        let shared_entry = EventJournalEntry {
            id: 1,
            scope: EventScope::Shared,
            category: EventCategory::Control,
            event_type: EventType::ControlApplied,
            ..Default::default()
        };
        let shared_path = shared_event_journal_state_file_path().expect("shared journal path");
        save_event_journal_state_to_path(
            &EventJournalState {
                next_id: 2,
                entries: vec![shared_entry.clone()],
            },
            &shared_path,
        )
        .expect("seed shared journal");

        let host_entry = EventJournalEntry {
            id: 2,
            scope: EventScope::Host,
            host_id: Some("node-follower".to_string()),
            category: EventCategory::Network,
            event_type: EventType::NetworkBlocked,
            message: Some("Interface unavailable".to_string()),
            ..Default::default()
        };
        save_host_event_journal_state(&EventJournalState {
            next_id: 3,
            entries: vec![shared_entry.clone(), host_entry.clone()],
        })
        .expect("save host-only journal");

        let host_path = event_journal_state_file_path().expect("host journal path");
        assert_eq!(
            load_event_journal_state_from_path(&host_path).entries,
            vec![host_entry]
        );
        assert_eq!(
            load_event_journal_state_from_path(&shared_path).entries,
            vec![shared_entry]
        );

        if let Some(value) = original_shared_dir {
            std::env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", value);
        } else {
            std::env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
        }
        if let Some(value) = original_host_id {
            std::env::set_var("SUPERSEEDR_SHARED_HOST_ID", value);
        } else {
            std::env::remove_var("SUPERSEEDR_SHARED_HOST_ID");
        }
        clear_shared_config_state_for_tests();
        set_app_paths_override_for_tests(None);
    }
}
