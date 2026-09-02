// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::{
    decode_activity_history_state, encode_activity_history_state, enforce_retention_caps,
    sparse_state_for_persistence, ActivityHistoryPersistedState, ACTIVITY_HISTORY_FILE_NAME,
};
use crate::config::runtime_persistence_dir;
use crate::fs_atomic::write_bytes_atomically;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tracing::{event as tracing_event, Level};

pub fn activity_history_state_file_path() -> io::Result<PathBuf> {
    let data_dir = runtime_persistence_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Could not resolve app data directory for activity history persistence",
        )
    })?;
    Ok(data_dir.join(ACTIVITY_HISTORY_FILE_NAME))
}

pub fn load_activity_history_state() -> ActivityHistoryPersistedState {
    match activity_history_state_file_path() {
        Ok(path) => load_activity_history_state_from_path(&path),
        Err(e) => {
            tracing_event!(
                Level::WARN,
                "Failed to resolve activity history persistence path. Using default state: {}",
                e
            );
            ActivityHistoryPersistedState::default()
        }
    }
}

pub fn save_activity_history_state(state: &ActivityHistoryPersistedState) -> io::Result<()> {
    let path = activity_history_state_file_path()?;
    save_activity_history_state_to_path(state, &path)
}

pub(super) fn load_activity_history_state_from_path(path: &Path) -> ActivityHistoryPersistedState {
    if !path.exists() {
        return ActivityHistoryPersistedState::default();
    }

    match fs::read(path) {
        Ok(bytes) => match decode_activity_history_state(&bytes) {
            Ok(mut state) => {
                enforce_retention_caps(&mut state);
                state
            }
            Err(e) => {
                tracing_event!(
                    Level::WARN,
                    "Failed to decode activity history persistence file {:?}. Resetting state: {}",
                    path,
                    e
                );
                ActivityHistoryPersistedState::default()
            }
        },
        Err(e) => {
            tracing_event!(
                Level::WARN,
                "Failed to read activity history persistence file {:?}. Using empty state: {}",
                path,
                e
            );
            ActivityHistoryPersistedState::default()
        }
    }
}

pub(super) fn save_activity_history_state_to_path(
    state: &ActivityHistoryPersistedState,
    path: &Path,
) -> io::Result<()> {
    let sparse_state = sparse_state_for_persistence(state);
    let content = encode_activity_history_state(&sparse_state);
    write_bytes_atomically(path, &content)
}
