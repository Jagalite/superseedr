// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use figment::providers::{Env, Format};
use figment::{providers::Toml, Figment};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;

use crate::app::TorrentControlState;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
pub enum TorrentSortColumn {
    Name,
    Down,
    #[default]
    Up,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
pub enum PeerSortColumn {
    Flags,
    Address,
    Client,
    Action,
    Completed,
    DL,
    #[default]
    UL,
    TotalDL,
    TotalUL,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
pub enum SortDirection {
    #[default]
    Ascending,
    Descending,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default)]
pub struct Settings {
    pub client_id: String,
    pub client_port: u16,
    pub torrents: Vec<TorrentSettings>,
    pub lifetime_downloaded: u64,
    pub lifetime_uploaded: u64,

    pub private_client: bool,

    // UI
    pub torrent_sort_column: TorrentSortColumn,
    pub torrent_sort_direction: SortDirection,
    pub peer_sort_column: PeerSortColumn,
    pub peer_sort_direction: SortDirection,

    // Disk
    pub watch_folder: Option<PathBuf>,
    pub default_download_folder: Option<PathBuf>,

    // Networking
    pub max_connected_peers: usize,
    pub bootstrap_nodes: Vec<String>,
    pub global_download_limit_bps: u64,
    pub global_upload_limit_bps: u64,

    // Performance
    pub max_concurrent_validations: usize,
    pub connection_attempt_permits: usize,
    pub resource_limit_override: Option<usize>,

    // Throttling / Choking
    pub upload_slots: usize,
    pub peer_upload_in_flight_limit: usize,

    // Timings
    pub tracker_fallback_interval_secs: u64,
    pub client_leeching_fallback_interval_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            client_port: 6681,
            torrents: Vec::new(),
            watch_folder: None,
            default_download_folder: None,
            lifetime_downloaded: 0,
            lifetime_uploaded: 0,
            private_client: false,
            global_download_limit_bps: 0,
            global_upload_limit_bps: 0,
            torrent_sort_column: TorrentSortColumn::default(),
            torrent_sort_direction: SortDirection::default(),
            peer_sort_column: PeerSortColumn::default(),
            peer_sort_direction: SortDirection::default(),
            max_connected_peers: 2000,
            bootstrap_nodes: vec![
                "router.utorrent.com:6881".to_string(),
                "router.bittorrent.com:6881".to_string(),
                "dht.transmissionbt.com:6881".to_string(),
                "dht.libtorrent.org:25401".to_string(),
                "router.cococorp.de:6881".to_string(),
            ],
            max_concurrent_validations: 64,
            resource_limit_override: None,
            connection_attempt_permits: 50,
            upload_slots: 8,
            peer_upload_in_flight_limit: 4,
            tracker_fallback_interval_secs: 1800,
            client_leeching_fallback_interval_secs: 60,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct TorrentSettings {
    pub torrent_or_magnet: String,
    pub name: String,
    pub validation_status: bool,
    pub download_path: PathBuf,
    pub torrent_control_state: TorrentControlState,
}

/// This is now the single source of truth for app directories.
pub fn get_app_paths() -> Option<(PathBuf, PathBuf)> {
    if let Some(proj_dirs) = ProjectDirs::from("com", "github", "jagalite.superseedr") {
        let config_dir = proj_dirs.config_dir().to_path_buf();
        let data_dir = proj_dirs.data_local_dir().to_path_buf();

        // Ensure directories exist
        fs::create_dir_all(&config_dir).ok()?;
        fs::create_dir_all(&data_dir).ok()?;

        Some((config_dir, data_dir))
    } else {
        None
    }
}

pub fn get_watch_path() -> Option<(PathBuf, PathBuf)> {
    if let Some((_, base_path)) = get_app_paths() {
        let watch_path = base_path.join("watch_files");
        let processed_path = base_path.join("processed_files");
        Some((watch_path, processed_path))
    } else {
        None
    }
}

pub fn create_watch_directories() -> io::Result<()> {
    if let Some((watch_path, processed_path)) = get_watch_path() {
        fs::create_dir_all(&watch_path)?;
        fs::create_dir_all(&processed_path)?;
    }

    Ok(())
}

pub fn load_settings() -> Settings {
    if let Some((config_dir, _)) = get_app_paths() {
        let config_file_path = config_dir.join("settings.toml");

        return Figment::new()
            .merge(Toml::file(config_file_path))
            .merge(Env::prefixed("SUPERSEEDR_"))
            .extract()
            .unwrap_or_default();
    }

    // Fallback if we can't even determine the application paths.
    Settings::default()
}

/// Saves the provided settings to the config file.
pub fn save_settings(settings: &Settings) -> io::Result<()> {
    if let Some((config_dir, _)) = get_app_paths() {
        let config_file_path = config_dir.join("settings.toml");
        let temp_file_path = config_dir.join("settings.toml.tmp");
        let content = toml::to_string_pretty(settings).map_err(io::Error::other)?;
        fs::write(&temp_file_path, content)?;
        fs::rename(&temp_file_path, &config_file_path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*; // Import everything from the parent module (your config.rs code)
    use figment::providers::{Format, Toml};
    use figment::Figment;
    use std::path::PathBuf;

    #[test]
    fn test_full_settings_parsing() {
        let toml_str = r#"
            client_id = "test-client-id-123"
            client_port = 12345
            lifetime_downloaded = 1000
            lifetime_uploaded = 2000

            torrent_sort_column = "Name"
            torrent_sort_direction = "Descending"
            peer_sort_column = "Address"
            peer_sort_direction = "Ascending"

            watch_folder = "/path/to/watch"
            default_download_folder = "/path/to/download"

            max_connected_peers = 500
            global_download_limit_bps = 102400
            global_upload_limit_bps = 51200

            max_concurrent_validations = 32
            connection_attempt_permits = 25
            resource_limit_override = 1024

            upload_slots = 10
            peer_upload_in_flight_limit = 2

            tracker_fallback_interval_secs = 3600
            client_leeching_fallback_interval_secs = 120

            bootstrap_nodes = [
                "node1.com:1234",
                "node2.com:5678"
            ]

            [[torrents]]
            torrent_or_magnet = "magnet:?xt=urn:btih:..."
            name = "My Test Torrent"
            validation_status = true
            download_path = "/downloads/my_test_torrent"
            # torrent_control_state is omitted, will use default (Stopped)

            [[torrents]]
            torrent_or_magnet = "magnet:?xt=urn:btih:other"
            name = "Another Torrent"
            validation_status = false
            download_path = "/downloads/another"
            torrent_control_state = "Paused"
        "#;

        // Parse the string using Figment, just like load_settings would
        let settings: Settings = Figment::new()
            .merge(Toml::string(toml_str))
            .extract()
            .expect("Failed to parse full TOML string");

        // Assert values
        assert_eq!(settings.client_id, "test-client-id-123");
        assert_eq!(settings.client_port, 12345);
        assert_eq!(settings.lifetime_downloaded, 1000);
        assert_eq!(settings.global_upload_limit_bps, 51200);
        assert_eq!(settings.torrent_sort_column, TorrentSortColumn::Name);
        assert_eq!(settings.torrent_sort_direction, SortDirection::Descending);
        assert_eq!(settings.peer_sort_column, PeerSortColumn::Address);
        assert_eq!(settings.watch_folder, Some(PathBuf::from("/path/to/watch")));
        assert_eq!(settings.resource_limit_override, Some(1024));
        assert_eq!(
            settings.bootstrap_nodes,
            vec!["node1.com:1234", "node2.com:5678"]
        );

        // Assert torrents
        assert_eq!(settings.torrents.len(), 2);
        assert_eq!(settings.torrents[0].name, "My Test Torrent");
        assert_eq!(settings.torrents[0].validation_status, true);
        assert_eq!(
            settings.torrents[0].download_path,
            PathBuf::from("/downloads/my_test_torrent")
        );
        // Check that omitting the field used the default
        assert_eq!(settings.torrents[1].name, "Another Torrent");
        assert_eq!(
            settings.torrents[1].torrent_control_state,
            TorrentControlState::Paused
        );
    }

    #[test]
    fn test_partial_settings_override() {
        let toml_str = r#"
            # Only override a few values
            client_port = 9999
            global_upload_limit_bps = 50000

            [[torrents]]
            name = "Partial Torrent"
            download_path = "/partial/path"
            # Other fields like torrent_or_magnet will be default (empty string)
        "#;

        let settings: Settings = Figment::new()
            .merge(Toml::string(toml_str))
            .extract()
            .expect("Failed to parse partial TOML string");

        let default_settings = Settings::default();

        // Assert changed values
        assert_eq!(settings.client_port, 9999);
        assert_eq!(settings.global_upload_limit_bps, 50000);

        // Assert unchanged (default) values
        assert_eq!(settings.client_id, default_settings.client_id); // ""
        assert_eq!(
            settings.max_connected_peers,
            default_settings.max_connected_peers
        ); // 2000
        assert_eq!(
            settings.torrent_sort_column,
            default_settings.torrent_sort_column
        ); // Up

        // Assert partial torrent
        assert_eq!(settings.torrents.len(), 1);
        assert_eq!(settings.torrents[0].name, "Partial Torrent");
        assert_eq!(
            settings.torrents[0].download_path,
            PathBuf::from("/partial/path")
        );
        assert_eq!(settings.torrents[0].torrent_or_magnet, ""); // Default for String
        assert_eq!(settings.torrents[0].validation_status, false); // Default for bool
        assert_eq!(
            settings.torrents[0].torrent_control_state,
            TorrentControlState::default()
        );
    }

    #[test]
    fn test_default_settings() {
        // An empty string should result in all default values
        let toml_str = "";

        let settings: Settings = Figment::new()
            .merge(Toml::string(toml_str))
            .extract()
            .expect("Failed to parse empty string");

        let default_settings = Settings::default();

        // Assert a few key default values
        assert_eq!(settings.client_id, default_settings.client_id);
        assert_eq!(settings.client_port, 6681);
        assert_eq!(settings.lifetime_downloaded, 0);
        assert_eq!(settings.global_upload_limit_bps, 0);
        assert_eq!(settings.torrent_sort_column, TorrentSortColumn::Up);
        assert_eq!(settings.peer_sort_direction, SortDirection::Ascending);
        assert!(settings.watch_folder.is_none());
        assert_eq!(settings.max_connected_peers, 2000);
        assert_eq!(settings.bootstrap_nodes, default_settings.bootstrap_nodes);
        assert!(settings.torrents.is_empty());
    }

    #[test]
    fn test_invalid_torrent_state_parsing() {
        let toml_str = r#"
            [[torrents]]
            name = "Invalid Torrent"
            download_path = "/invalid/path"
            torrent_control_state = "UNKNOWN" # This is not a valid variant
        "#;

        // Try to parse the string
        let result: Result<Settings, figment::Error> =
            Figment::new().merge(Toml::string(toml_str)).extract();

        // We expect this to fail
        assert!(
            result.is_err(),
            "Parsing should fail with an invalid enum variant"
        );

        // Optional: Check if the error message contains the problematic variant
        // This makes the test more robust.
        if let Err(e) = result {
            let error_string = e.to_string();
            assert!(
                error_string.contains("UNKNOWN"),
                "Error message should mention the invalid variant 'UNKNOWN'"
            );
            assert!(
                error_string.contains("torrent_control_state"),
                "Error message should mention the field 'torrent_control_state'"
            );
        }
    }
}
