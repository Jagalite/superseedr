// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::RssPersistedState;
use crate::config::runtime_persistence_dir;
use crate::persistence::atomic::{
    deserialize_versioned_toml, serialize_versioned_toml, write_string_atomically,
};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tracing::{event as tracing_event, Level};

#[allow(dead_code)]
pub fn rss_state_file_path() -> io::Result<PathBuf> {
    let data_dir = runtime_persistence_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Could not resolve app data directory for RSS persistence",
        )
    })?;

    Ok(data_dir.join("rss.toml"))
}

#[allow(dead_code)]
pub fn load_rss_state() -> RssPersistedState {
    match rss_state_file_path() {
        Ok(path) => load_rss_state_from_path(&path),
        Err(e) => {
            tracing_event!(
                Level::WARN,
                "Failed to get RSS persistence path. Using empty state: {}",
                e
            );
            RssPersistedState::default()
        }
    }
}

#[allow(dead_code)]
pub fn save_rss_state(state: &RssPersistedState) -> io::Result<()> {
    let path = rss_state_file_path()?;
    save_rss_state_to_path(state, &path)
}

pub(super) fn load_rss_state_from_path(path: &Path) -> RssPersistedState {
    if !path.exists() {
        return RssPersistedState::default();
    }

    match fs::read_to_string(path) {
        Ok(content) => match deserialize_versioned_toml::<RssPersistedState>(&content) {
            Ok(state) => state,
            Err(e) => {
                tracing_event!(
                    Level::WARN,
                    "Failed to parse RSS persistence file {:?}. Resetting RSS state: {}",
                    path,
                    e
                );
                RssPersistedState::default()
            }
        },
        Err(e) => {
            tracing_event!(
                Level::WARN,
                "Failed to read RSS persistence file {:?}. Using empty state: {}",
                path,
                e
            );
            RssPersistedState::default()
        }
    }
}

pub(super) fn save_rss_state_to_path(state: &RssPersistedState, path: &Path) -> io::Result<()> {
    let content = serialize_versioned_toml(state)?;
    write_string_atomically(path, &content)
}
