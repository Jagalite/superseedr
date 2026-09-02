// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Application configuration persistence selected by the platform composition root.

#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

use crate::config::{Settings, TorrentMetadataConfig, TorrentMetadataEntry};
use std::io;
use std::sync::Arc;
#[cfg(any(target_arch = "wasm32", test))]
use std::sync::Mutex;

trait ConfigStorage: Send + Sync {
    fn load_settings(&self) -> io::Result<Settings>;
    fn load_settings_for_cli(&self) -> io::Result<Settings>;
    fn save_settings(&self, settings: &Settings) -> io::Result<()>;
    fn load_torrent_metadata(&self) -> io::Result<TorrentMetadataConfig>;
    fn upsert_torrent_metadata(&self, entry: TorrentMetadataEntry) -> io::Result<()>;
}

/// Cloneable application-storage capability held by a runtime host.
///
/// Native construction routes through the existing normal/shared config backend.
/// Browser construction uses an ephemeral in-memory backend. Torrent managers do
/// not receive this capability.
#[derive(Clone)]
pub(crate) struct AppStorage {
    config: Arc<dyn ConfigStorage>,
}

impl AppStorage {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn native() -> Self {
        Self {
            config: Arc::new(NativeConfigStorage),
        }
    }

    #[cfg(any(target_arch = "wasm32", test))]
    pub(crate) fn memory(settings: Settings) -> Self {
        Self {
            config: Arc::new(MemoryConfigStorage {
                state: Mutex::new(MemoryConfigState {
                    settings,
                    metadata: TorrentMetadataConfig::default(),
                }),
            }),
        }
    }

    pub(crate) fn load_settings(&self) -> io::Result<Settings> {
        self.config.load_settings()
    }

    pub(crate) fn load_settings_for_cli(&self) -> io::Result<Settings> {
        self.config.load_settings_for_cli()
    }

    pub(crate) fn save_settings(&self, settings: &Settings) -> io::Result<()> {
        self.config.save_settings(settings)
    }

    pub(crate) fn load_torrent_metadata(&self) -> io::Result<TorrentMetadataConfig> {
        self.config.load_torrent_metadata()
    }

    pub(crate) fn upsert_torrent_metadata(&self, entry: TorrentMetadataEntry) -> io::Result<()> {
        self.config.upsert_torrent_metadata(entry)
    }
}

#[cfg(not(target_arch = "wasm32"))]
struct NativeConfigStorage;

#[cfg(not(target_arch = "wasm32"))]
impl ConfigStorage for NativeConfigStorage {
    fn load_settings(&self) -> io::Result<Settings> {
        crate::config::load_settings()
    }

    fn load_settings_for_cli(&self) -> io::Result<Settings> {
        crate::config::load_settings_for_cli()
    }

    fn save_settings(&self, settings: &Settings) -> io::Result<()> {
        crate::config::save_settings(settings)
    }

    fn load_torrent_metadata(&self) -> io::Result<TorrentMetadataConfig> {
        crate::config::load_torrent_metadata()
    }

    fn upsert_torrent_metadata(&self, entry: TorrentMetadataEntry) -> io::Result<()> {
        crate::config::upsert_torrent_metadata(entry)
    }
}

#[cfg(any(target_arch = "wasm32", test))]
struct MemoryConfigState {
    settings: Settings,
    metadata: TorrentMetadataConfig,
}

#[cfg(any(target_arch = "wasm32", test))]
struct MemoryConfigStorage {
    state: Mutex<MemoryConfigState>,
}

#[cfg(any(target_arch = "wasm32", test))]
impl MemoryConfigStorage {
    fn state(&self) -> std::sync::MutexGuard<'_, MemoryConfigState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(any(target_arch = "wasm32", test))]
impl ConfigStorage for MemoryConfigStorage {
    fn load_settings(&self) -> io::Result<Settings> {
        Ok(self.state().settings.clone())
    }

    fn load_settings_for_cli(&self) -> io::Result<Settings> {
        self.load_settings()
    }

    fn save_settings(&self, settings: &Settings) -> io::Result<()> {
        self.state().settings = settings.clone();
        Ok(())
    }

    fn load_torrent_metadata(&self) -> io::Result<TorrentMetadataConfig> {
        Ok(self.state().metadata.clone())
    }

    fn upsert_torrent_metadata(&self, entry: TorrentMetadataEntry) -> io::Result<()> {
        let mut state = self.state();
        if let Some(existing) = state
            .metadata
            .torrents
            .iter_mut()
            .find(|existing| existing.info_hash_hex == entry.info_hash_hex)
        {
            *existing = entry;
        } else {
            state.metadata.torrents.push(entry);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::AppStorage;
    use crate::config::{Settings, TorrentMetadataEntry};

    #[test]
    fn memory_storage_round_trips_settings_and_metadata() {
        let initial = Settings::default();
        let storage = AppStorage::memory(initial.clone());
        assert_eq!(storage.load_settings().unwrap(), initial);

        let mut updated = initial;
        updated.client_port = 42_424;
        storage.save_settings(&updated).unwrap();
        assert_eq!(storage.load_settings().unwrap().client_port, 42_424);

        storage
            .upsert_torrent_metadata(TorrentMetadataEntry {
                info_hash_hex: "11".repeat(20),
                torrent_name: "Example fixture".to_string(),
                ..TorrentMetadataEntry::default()
            })
            .unwrap();
        let metadata = storage.load_torrent_metadata().unwrap();
        assert_eq!(metadata.torrents.len(), 1);
        assert_eq!(metadata.torrents[0].torrent_name, "Example fixture");
    }
}
