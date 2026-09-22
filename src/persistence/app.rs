// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Application-state persistence selected by the platform composition root.

use crate::config::{Settings, TorrentMetadataConfig, TorrentMetadataEntry};
use crate::persistence::activity_history::ActivityHistoryPersistedState;
use crate::persistence::event_journal::EventJournalState;
use crate::persistence::network_history::NetworkHistoryPersistedState;
use crate::persistence::rss::RssPersistedState;
use std::io;
use std::sync::Arc;
#[cfg(any(target_arch = "wasm32", test))]
use std::sync::Mutex;

trait AppPersistenceBackend: Send + Sync {
    fn load_settings(&self) -> io::Result<Settings>;
    fn load_settings_for_cli(&self) -> io::Result<Settings>;
    fn save_settings(&self, settings: &Settings) -> io::Result<()>;
    fn load_torrent_metadata(&self) -> io::Result<TorrentMetadataConfig>;
    fn upsert_torrent_metadata(&self, entry: TorrentMetadataEntry) -> io::Result<()>;
    fn load_rss_state(&self) -> RssPersistedState;
    fn save_rss_state(&self, state: &RssPersistedState) -> io::Result<()>;
    fn load_network_history_state(&self) -> NetworkHistoryPersistedState;
    fn save_network_history_state(&self, state: &NetworkHistoryPersistedState) -> io::Result<()>;
    fn load_activity_history_state(&self) -> ActivityHistoryPersistedState;
    fn save_activity_history_state(&self, state: &ActivityHistoryPersistedState) -> io::Result<()>;
    fn load_event_journal_state(&self) -> EventJournalState;
    fn save_event_journal_state(
        &self,
        state: &EventJournalState,
        can_write_shared_state: bool,
    ) -> io::Result<()>;
}

/// Cloneable application-persistence capability held by a runtime host.
///
/// Native construction routes through the existing normal/shared config backend.
/// Browser construction uses an ephemeral in-memory backend. Torrent managers do
/// not receive this capability.
#[derive(Clone)]
pub(crate) struct AppPersistence {
    backend: Arc<dyn AppPersistenceBackend>,
}

impl AppPersistence {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn native() -> Self {
        Self {
            backend: Arc::new(NativeAppPersistence),
        }
    }

    #[cfg(any(target_arch = "wasm32", test))]
    pub(crate) fn memory(settings: Settings) -> Self {
        Self {
            backend: Arc::new(MemoryAppPersistence {
                state: Mutex::new(MemoryConfigState {
                    settings,
                    metadata: TorrentMetadataConfig::default(),
                    rss: RssPersistedState::default(),
                    network_history: NetworkHistoryPersistedState::default(),
                    activity_history: ActivityHistoryPersistedState::default(),
                    event_journal: EventJournalState::default(),
                }),
            }),
        }
    }

    pub(crate) fn load_settings(&self) -> io::Result<Settings> {
        self.backend.load_settings()
    }

    pub(crate) fn load_settings_for_cli(&self) -> io::Result<Settings> {
        self.backend.load_settings_for_cli()
    }

    pub(crate) fn save_settings(&self, settings: &Settings) -> io::Result<()> {
        self.backend.save_settings(settings)
    }

    pub(crate) fn load_torrent_metadata(&self) -> io::Result<TorrentMetadataConfig> {
        self.backend.load_torrent_metadata()
    }

    pub(crate) fn upsert_torrent_metadata(&self, entry: TorrentMetadataEntry) -> io::Result<()> {
        self.backend.upsert_torrent_metadata(entry)
    }

    pub(crate) fn load_rss_state(&self) -> RssPersistedState {
        self.backend.load_rss_state()
    }

    pub(crate) fn save_rss_state(&self, state: &RssPersistedState) -> io::Result<()> {
        self.backend.save_rss_state(state)
    }

    pub(crate) fn load_network_history_state(&self) -> NetworkHistoryPersistedState {
        self.backend.load_network_history_state()
    }

    pub(crate) fn save_network_history_state(
        &self,
        state: &NetworkHistoryPersistedState,
    ) -> io::Result<()> {
        self.backend.save_network_history_state(state)
    }

    pub(crate) fn load_activity_history_state(&self) -> ActivityHistoryPersistedState {
        self.backend.load_activity_history_state()
    }

    pub(crate) fn save_activity_history_state(
        &self,
        state: &ActivityHistoryPersistedState,
    ) -> io::Result<()> {
        self.backend.save_activity_history_state(state)
    }

    pub(crate) fn load_event_journal_state(&self) -> EventJournalState {
        self.backend.load_event_journal_state()
    }

    pub(crate) fn save_event_journal_state(
        &self,
        state: &EventJournalState,
        can_write_shared_state: bool,
    ) -> io::Result<()> {
        self.backend
            .save_event_journal_state(state, can_write_shared_state)
    }
}

#[cfg(not(target_arch = "wasm32"))]
struct NativeAppPersistence;

#[cfg(not(target_arch = "wasm32"))]
impl AppPersistenceBackend for NativeAppPersistence {
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

    fn load_rss_state(&self) -> RssPersistedState {
        crate::persistence::rss::load_rss_state()
    }

    fn save_rss_state(&self, state: &RssPersistedState) -> io::Result<()> {
        crate::persistence::rss::save_rss_state(state)
    }

    fn load_network_history_state(&self) -> NetworkHistoryPersistedState {
        crate::persistence::network_history::load_network_history_state()
    }

    fn save_network_history_state(&self, state: &NetworkHistoryPersistedState) -> io::Result<()> {
        crate::persistence::network_history::save_network_history_state(state)
    }

    fn load_activity_history_state(&self) -> ActivityHistoryPersistedState {
        crate::persistence::activity_history::load_activity_history_state()
    }

    fn save_activity_history_state(&self, state: &ActivityHistoryPersistedState) -> io::Result<()> {
        crate::persistence::activity_history::save_activity_history_state(state)
    }

    fn load_event_journal_state(&self) -> EventJournalState {
        crate::persistence::event_journal::load_event_journal_state()
    }

    fn save_event_journal_state(
        &self,
        state: &EventJournalState,
        can_write_shared_state: bool,
    ) -> io::Result<()> {
        if can_write_shared_state {
            crate::persistence::event_journal::save_event_journal_state(state)
        } else {
            crate::persistence::event_journal::save_host_event_journal_state(state)
        }
    }
}

#[cfg(any(target_arch = "wasm32", test))]
struct MemoryConfigState {
    settings: Settings,
    metadata: TorrentMetadataConfig,
    rss: RssPersistedState,
    network_history: NetworkHistoryPersistedState,
    activity_history: ActivityHistoryPersistedState,
    event_journal: EventJournalState,
}

#[cfg(any(target_arch = "wasm32", test))]
struct MemoryAppPersistence {
    state: Mutex<MemoryConfigState>,
}

#[cfg(any(target_arch = "wasm32", test))]
impl MemoryAppPersistence {
    fn state(&self) -> std::sync::MutexGuard<'_, MemoryConfigState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(any(target_arch = "wasm32", test))]
impl AppPersistenceBackend for MemoryAppPersistence {
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

    fn load_rss_state(&self) -> RssPersistedState {
        self.state().rss.clone()
    }

    fn save_rss_state(&self, state: &RssPersistedState) -> io::Result<()> {
        self.state().rss = state.clone();
        Ok(())
    }

    fn load_network_history_state(&self) -> NetworkHistoryPersistedState {
        self.state().network_history.clone()
    }

    fn save_network_history_state(&self, state: &NetworkHistoryPersistedState) -> io::Result<()> {
        self.state().network_history = state.clone();
        Ok(())
    }

    fn load_activity_history_state(&self) -> ActivityHistoryPersistedState {
        self.state().activity_history.clone()
    }

    fn save_activity_history_state(&self, state: &ActivityHistoryPersistedState) -> io::Result<()> {
        self.state().activity_history = state.clone();
        Ok(())
    }

    fn load_event_journal_state(&self) -> EventJournalState {
        self.state().event_journal.clone()
    }

    fn save_event_journal_state(
        &self,
        state: &EventJournalState,
        _can_write_shared_state: bool,
    ) -> io::Result<()> {
        self.state().event_journal = state.clone();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::AppPersistence;
    use crate::config::{Settings, TorrentMetadataEntry};
    use crate::persistence::activity_history::ActivityHistoryPersistedState;
    use crate::persistence::event_journal::EventJournalState;
    use crate::persistence::network_history::NetworkHistoryPersistedState;
    use crate::persistence::rss::RssPersistedState;

    #[test]
    fn memory_persistence_round_trips_application_state() {
        let initial = Settings::default();
        let persistence = AppPersistence::memory(initial.clone());
        assert_eq!(persistence.load_settings().unwrap(), initial);

        let mut updated = initial;
        updated.client_port = 42_424;
        persistence.save_settings(&updated).unwrap();
        assert_eq!(persistence.load_settings().unwrap().client_port, 42_424);

        persistence
            .upsert_torrent_metadata(TorrentMetadataEntry {
                info_hash_hex: "11".repeat(20),
                torrent_name: "Example fixture".to_string(),
                ..TorrentMetadataEntry::default()
            })
            .unwrap();
        let metadata = persistence.load_torrent_metadata().unwrap();
        assert_eq!(metadata.torrents.len(), 1);
        assert_eq!(metadata.torrents[0].torrent_name, "Example fixture");

        let rss = RssPersistedState {
            last_sync_at: Some("2026-01-02T03:04:05Z".to_string()),
            ..RssPersistedState::default()
        };
        persistence.save_rss_state(&rss).unwrap();
        assert_eq!(persistence.load_rss_state(), rss);

        let network_history = NetworkHistoryPersistedState {
            updated_at_unix: 101,
            ..NetworkHistoryPersistedState::default()
        };
        persistence
            .save_network_history_state(&network_history)
            .unwrap();
        assert_eq!(persistence.load_network_history_state(), network_history);

        let activity_history = ActivityHistoryPersistedState {
            updated_at_unix: 202,
            ..ActivityHistoryPersistedState::default()
        };
        persistence
            .save_activity_history_state(&activity_history)
            .unwrap();
        assert_eq!(persistence.load_activity_history_state(), activity_history);

        let event_journal = EventJournalState {
            next_id: 303,
            ..EventJournalState::default()
        };
        persistence
            .save_event_journal_state(&event_journal, true)
            .unwrap();
        assert_eq!(persistence.load_event_journal_state(), event_journal);
    }
}
