// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::{
    decode_network_history_state, encode_network_history_state, enforce_retention_caps,
    sparse_state_for_persistence, NetworkHistoryPersistedState, NETWORK_HISTORY_FILE_NAME,
};
use crate::config::runtime_persistence_dir;
use crate::persistence::atomic::write_bytes_atomically;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tracing::{event as tracing_event, Level};

#[allow(dead_code)]
pub fn network_history_state_file_path() -> io::Result<PathBuf> {
    let data_dir = runtime_persistence_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Could not resolve app data directory for network history persistence",
        )
    })?;

    Ok(data_dir.join(NETWORK_HISTORY_FILE_NAME))
}

#[allow(dead_code)]
pub fn load_network_history_state() -> NetworkHistoryPersistedState {
    match network_history_state_file_path() {
        Ok(path) => load_network_history_state_from_path(&path),
        Err(e) => {
            tracing_event!(
                Level::WARN,
                "Failed to get network history persistence path. Using empty state: {}",
                e
            );
            NetworkHistoryPersistedState::default()
        }
    }
}

#[allow(dead_code)]
pub fn save_network_history_state(state: &NetworkHistoryPersistedState) -> io::Result<()> {
    let path = network_history_state_file_path()?;
    save_network_history_state_to_path(state, &path)
}

pub(super) fn load_network_history_state_from_path(path: &Path) -> NetworkHistoryPersistedState {
    if !path.exists() {
        return NetworkHistoryPersistedState::default();
    }

    match fs::read(path) {
        Ok(bytes) => match decode_network_history_state(&bytes) {
            Ok(mut state) => {
                enforce_retention_caps(&mut state);
                state
            }
            Err(e) => {
                tracing_event!(
                    Level::WARN,
                    "Failed to decode network history persistence file {:?}. Resetting state: {}",
                    path,
                    e
                );
                NetworkHistoryPersistedState::default()
            }
        },
        Err(e) => {
            tracing_event!(
                Level::WARN,
                "Failed to read network history persistence file {:?}. Using empty state: {}",
                path,
                e
            );
            NetworkHistoryPersistedState::default()
        }
    }
}

pub(super) fn save_network_history_state_to_path(
    state: &NetworkHistoryPersistedState,
    path: &Path,
) -> io::Result<()> {
    let sparse_state = sparse_state_for_persistence(state);
    let content = encode_network_history_state(&sparse_state);
    write_bytes_atomically(path, &content)
}
