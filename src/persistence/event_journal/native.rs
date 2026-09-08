// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::{
    enforce_event_journal_retention, EventJournalState, EventScope, EVENT_JOURNAL_FILE_NAME,
    SHARED_EVENT_JOURNAL_FILE_NAME,
};
use crate::config::runtime_persistence_dir;
use crate::persistence::atomic::{
    deserialize_versioned_toml, serialize_versioned_toml, write_string_atomically,
};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tracing::{event as tracing_event, Level};

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

pub(super) fn load_event_journal_state_from_path(path: &Path) -> EventJournalState {
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

pub(super) fn save_event_journal_state_to_path(
    state: &EventJournalState,
    path: &Path,
) -> io::Result<()> {
    let mut journal_state = state.clone();
    enforce_event_journal_retention(&mut journal_state);

    let content = serialize_versioned_toml(&journal_state)?;
    write_string_atomically(path, &content)
}
