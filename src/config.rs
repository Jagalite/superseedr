// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use figment::providers::{Env, Serialized};
use figment::Figment;
use sha1::{Digest, Sha1};
use tracing::{event as tracing_event, Level};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
#[cfg(test)]
use std::sync::{Mutex, OnceLock};

use crate::app::FilePriority;
use crate::app::TorrentControlState;
use crate::fs_atomic::{
    deserialize_versioned_toml, serialize_versioned_toml, write_string_atomically,
    write_toml_atomically,
};
use crate::theme::ThemeName;

use strum_macros::EnumCount;
use strum_macros::EnumIter;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default, EnumIter, EnumCount)]
pub enum TorrentSortColumn {
    Name,
    #[default]
    Up,
    Down,
    Progress,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default, EnumIter, EnumCount)]
pub enum PeerSortColumn {
    Flags,
    Completed,
    Address,
    Client,
    Action,
    #[default]
    #[serde(alias = "TotalUL")]
    UL,
    #[serde(alias = "TotalDL")]
    DL,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
pub enum SortDirection {
    #[default]
    Ascending,
    Descending,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RssAddedVia {
    Auto,
    #[default]
    Manual,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct RssFeed {
    pub url: String,
    pub enabled: bool,
}

impl Default for RssFeed {
    fn default() -> Self {
        Self {
            url: String::new(),
            enabled: true,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct RssFilter {
    #[serde(alias = "regex")]
    pub query: String,
    pub mode: RssFilterMode,
    pub enabled: bool,
}

impl Default for RssFilter {
    fn default() -> Self {
        Self {
            query: String::new(),
            mode: RssFilterMode::Fuzzy,
            enabled: true,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RssFilterMode {
    #[default]
    Fuzzy,
    Regex,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct RssSettings {
    pub enabled: bool,
    pub poll_interval_secs: u64,
    pub max_preview_items: usize,
    pub feeds: Vec<RssFeed>,
    pub filters: Vec<RssFilter>,
}

impl Default for RssSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval_secs: 900,
            max_preview_items: 500,
            feeds: Vec::new(),
            filters: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct RssHistoryEntry {
    pub dedupe_key: String,
    pub info_hash: Option<String>,
    pub guid: Option<String>,
    pub link: Option<String>,
    pub title: String,
    pub source: Option<String>,
    pub date_iso: String,
    pub added_via: RssAddedVia,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct FeedSyncError {
    pub message: String,
    pub occurred_at_iso: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub client_id: String,
    pub client_port: u16,
    pub torrents: Vec<TorrentSettings>,
    pub lifetime_downloaded: u64,
    pub lifetime_uploaded: u64,
    pub private_client: bool,
    pub torrent_sort_column: TorrentSortColumn,
    pub torrent_sort_direction: SortDirection,
    pub peer_sort_column: PeerSortColumn,
    pub peer_sort_direction: SortDirection,
    pub ui_theme: ThemeName,
    pub watch_folder: Option<PathBuf>,
    pub default_download_folder: Option<PathBuf>,
    pub max_connected_peers: usize,
    pub bootstrap_nodes: Vec<String>,
    pub global_download_limit_bps: u64,
    pub global_upload_limit_bps: u64,
    pub max_concurrent_validations: usize,
    pub connection_attempt_permits: usize,
    pub resource_limit_override: Option<usize>,
    pub upload_slots: usize,
    pub peer_upload_in_flight_limit: usize,
    pub tracker_fallback_interval_secs: u64,
    pub client_leeching_fallback_interval_secs: u64,
    pub output_status_interval: u64,
    pub rss: RssSettings,
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
            ui_theme: ThemeName::default(),
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
            output_status_interval: 0,
            rss: RssSettings::default(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq)]
#[serde(default)]
pub struct TorrentSettings {
    pub torrent_or_magnet: String,
    pub name: String,
    pub validation_status: bool,
    pub download_path: Option<PathBuf>,
    pub container_name: Option<String>,
    pub torrent_control_state: TorrentControlState,
    pub delete_files: bool,
    #[serde(with = "string_usize_map")]
    pub file_priorities: HashMap<usize, FilePriority>,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct TorrentMetadataFileEntry {
    pub relative_path: String,
    pub length: u64,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct TorrentMetadataEntry {
    pub info_hash_hex: String,
    pub torrent_name: String,
    pub total_size: u64,
    pub is_multi_file: bool,
    pub files: Vec<TorrentMetadataFileEntry>,
    #[serde(with = "string_usize_map")]
    pub file_priorities: HashMap<usize, FilePriority>,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct TorrentMetadataConfig {
    pub torrents: Vec<TorrentMetadataEntry>,
}

mod string_usize_map {
    use crate::app::FilePriority;
    use serde::{self, Deserialize, Deserializer, Serializer};
    use std::collections::HashMap;
    use std::str::FromStr;

    pub fn serialize<S>(
        map: &HashMap<usize, FilePriority>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let string_map: HashMap<String, FilePriority> =
            map.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        serde::Serialize::serialize(&string_map, serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<HashMap<usize, FilePriority>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let string_map: HashMap<String, FilePriority> = HashMap::deserialize(deserializer)?;
        let mut result = HashMap::new();
        for (k, v) in string_map {
            let k_usize = usize::from_str(&k).map_err(serde::de::Error::custom)?;
            result.insert(k_usize, v);
        }
        Ok(result)
    }
}

const SHARED_CONFIG_DIR_ENV: &str = "SUPERSEEDR_SHARED_CONFIG_DIR";
const SHARED_HOST_ID_ENV: &str = "SUPERSEEDR_SHARED_HOST_ID";
const LEGACY_SHARED_HOST_ID_ENV: &str = "SUPERSEEDR_HOST_ID";
const SHARED_TORRENT_SOURCE_PREFIX: &str = "shared:";
const SHARED_CONFIG_SUBDIR: &str = "superseedr-config";
const LAUNCHER_SHARED_CONFIG_FILE: &str = "launcher_shared_config.toml";
const LAUNCHER_HOST_ID_FILE: &str = "launcher_host_id.toml";

#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(default)]
struct LauncherSharedConfig {
    shared_config_dir: Option<PathBuf>,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(default)]
struct LauncherHostId {
    host_id: Option<String>,
}

#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SharedConfigSource {
    Env,
    Launcher,
}

#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostIdSource {
    Env,
    Launcher,
    Hostname,
    System,
    Default,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct HostIdSelection {
    pub source: HostIdSource,
    pub host_id: String,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct SharedConfigSelection {
    pub source: SharedConfigSource,
    pub mount_root: PathBuf,
    pub config_root: PathBuf,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq)]
#[serde(default)]
struct CatalogTorrentSettings {
    pub torrent_or_magnet: String,
    pub name: String,
    pub validation_status: bool,
    pub download_path: Option<PathBuf>,
    pub container_name: Option<String>,
    pub torrent_control_state: TorrentControlState,
    pub delete_files: bool,
    #[serde(with = "string_usize_map")]
    pub file_priorities: HashMap<usize, FilePriority>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
struct SharedSettingsConfig {
    pub client_id: String,
    pub lifetime_downloaded: u64,
    pub lifetime_uploaded: u64,
    pub private_client: bool,
    pub torrent_sort_column: TorrentSortColumn,
    pub torrent_sort_direction: SortDirection,
    pub peer_sort_column: PeerSortColumn,
    pub peer_sort_direction: SortDirection,
    pub ui_theme: ThemeName,
    pub default_download_folder: Option<PathBuf>,
    pub max_connected_peers: usize,
    pub bootstrap_nodes: Vec<String>,
    pub global_download_limit_bps: u64,
    pub global_upload_limit_bps: u64,
    pub max_concurrent_validations: usize,
    pub connection_attempt_permits: usize,
    pub resource_limit_override: Option<usize>,
    pub upload_slots: usize,
    pub peer_upload_in_flight_limit: usize,
    pub tracker_fallback_interval_secs: u64,
    pub client_leeching_fallback_interval_secs: u64,
    pub output_status_interval: u64,
    pub rss: RssSettings,
}

impl Default for SharedSettingsConfig {
    fn default() -> Self {
        let settings = Settings::default();
        Self {
            client_id: settings.client_id,
            lifetime_downloaded: settings.lifetime_downloaded,
            lifetime_uploaded: settings.lifetime_uploaded,
            private_client: settings.private_client,
            torrent_sort_column: settings.torrent_sort_column,
            torrent_sort_direction: settings.torrent_sort_direction,
            peer_sort_column: settings.peer_sort_column,
            peer_sort_direction: settings.peer_sort_direction,
            ui_theme: settings.ui_theme,
            default_download_folder: None,
            max_connected_peers: settings.max_connected_peers,
            bootstrap_nodes: settings.bootstrap_nodes,
            global_download_limit_bps: settings.global_download_limit_bps,
            global_upload_limit_bps: settings.global_upload_limit_bps,
            max_concurrent_validations: settings.max_concurrent_validations,
            connection_attempt_permits: settings.connection_attempt_permits,
            resource_limit_override: settings.resource_limit_override,
            upload_slots: settings.upload_slots,
            peer_upload_in_flight_limit: settings.peer_upload_in_flight_limit,
            tracker_fallback_interval_secs: settings.tracker_fallback_interval_secs,
            client_leeching_fallback_interval_secs: settings.client_leeching_fallback_interval_secs,
            output_status_interval: settings.output_status_interval,
            rss: settings.rss,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(default)]
struct CatalogConfig {
    pub torrents: Vec<CatalogTorrentSettings>,
}

#[derive(Clone, Debug, PartialEq)]
struct LayeredConfig {
    settings: SharedSettingsConfig,
    catalog: CatalogConfig,
    host: HostConfig,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
struct HostConfig {
    pub client_id: Option<String>,
    pub client_port: u16,
    pub watch_folder: Option<PathBuf>,
}

impl Default for HostConfig {
    fn default() -> Self {
        let settings = Settings::default();
        Self {
            client_id: None,
            client_port: settings.client_port,
            watch_folder: settings.watch_folder,
        }
    }
}
#[derive(Clone, Debug)]
struct NormalConfigPaths {
    settings_path: PathBuf,
    metadata_path: PathBuf,
    backup_dir: PathBuf,
}

#[derive(Clone, Debug)]
struct SharedConfigPaths {
    mount_dir: PathBuf,
    root_dir: PathBuf,
    settings_path: PathBuf,
    catalog_path: PathBuf,
    metadata_path: PathBuf,
    host_dir: PathBuf,
    host_path: PathBuf,
    host_id: String,
}

#[derive(Clone, Debug)]
struct NormalConfigBackend {
    paths: NormalConfigPaths,
}

#[derive(Clone, Debug)]
struct SharedConfigBackend {
    paths: SharedConfigPaths,
}

#[derive(Clone, Debug)]
enum ConfigBackend {
    Normal(NormalConfigBackend),
    Shared(SharedConfigBackend),
}

#[cfg(test)]
static APP_PATHS_OVERRIDE: OnceLock<Mutex<Option<(PathBuf, PathBuf)>>> = OnceLock::new();

#[cfg(test)]
static SHARED_ENV_TEST_GUARD: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(test)]
fn app_paths_override() -> &'static Mutex<Option<(PathBuf, PathBuf)>> {
    APP_PATHS_OVERRIDE.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
pub(crate) fn shared_env_guard_for_tests() -> &'static Mutex<()> {
    SHARED_ENV_TEST_GUARD.get_or_init(|| Mutex::new(()))
}

impl LayeredConfig {
    fn from_flat_settings(settings: &Settings) -> Self {
        Self {
            settings: SharedSettingsConfig::from_settings(settings, None)
                .expect("flat settings should always be encodable"),
            catalog: CatalogConfig::from_settings(settings, None, None)
                .expect("flat catalog should always be encodable"),
            host: HostConfig::from_flat_settings(settings),
        }
    }

    fn from_shared_settings(
        settings: &Settings,
        shared_mount_root: &Path,
        shared_config_root: &Path,
        preserved_shared_client_id: Option<&str>,
    ) -> io::Result<Self> {
        let mut settings_config =
            SharedSettingsConfig::from_settings(settings, Some(shared_mount_root))?;
        let shared_client_id = preserved_shared_client_id.unwrap_or(&settings_config.client_id);
        let host = HostConfig::from_settings(settings, shared_client_id);
        if let Some(shared_client_id) =
            preserved_shared_client_id.filter(|_| host.client_id.is_some())
        {
            settings_config.client_id = shared_client_id.to_string();
        }

        Ok(Self {
            settings: settings_config,
            catalog: CatalogConfig::from_settings(
                settings,
                Some(shared_config_root),
                Some(shared_mount_root),
            )?,
            host,
        })
    }

    fn resolve_flat_settings(&self) -> io::Result<Settings> {
        self.resolve_settings(None, None)
    }

    fn resolve_shared_settings(
        &self,
        shared_mount_root: &Path,
        shared_config_root: &Path,
    ) -> io::Result<Settings> {
        self.resolve_settings(Some(shared_mount_root), Some(shared_config_root))
    }

    fn resolve_settings(
        &self,
        shared_mount_root: Option<&Path>,
        shared_config_root: Option<&Path>,
    ) -> io::Result<Settings> {
        let mut settings = Settings::default();
        self.settings
            .apply_to_settings(&mut settings, shared_mount_root)?;
        self.catalog
            .apply_to_settings(&mut settings, shared_config_root, shared_mount_root)?;
        self.host.apply_to_settings(&mut settings);
        Ok(settings)
    }
}

impl CatalogTorrentSettings {
    fn from_settings(
        settings: &TorrentSettings,
        shared_config_root: Option<&Path>,
        shared_mount_root: Option<&Path>,
    ) -> io::Result<Self> {
        Ok(Self {
            torrent_or_magnet: encode_catalog_torrent_source(
                &settings.torrent_or_magnet,
                shared_config_root,
            ),
            name: settings.name.clone(),
            validation_status: settings.validation_status,
            download_path: settings
                .download_path
                .as_deref()
                .map(|path| {
                    encode_shared_data_path(
                        path,
                        shared_mount_root,
                        &format!("torrent '{}'", settings.name),
                    )
                })
                .transpose()?,
            container_name: settings.container_name.clone(),
            torrent_control_state: settings.torrent_control_state.clone(),
            delete_files: settings.delete_files,
            file_priorities: settings.file_priorities.clone(),
        })
    }

    fn to_settings(
        &self,
        shared_config_root: Option<&Path>,
        shared_mount_root: Option<&Path>,
    ) -> io::Result<TorrentSettings> {
        Ok(TorrentSettings {
            torrent_or_magnet: decode_catalog_torrent_source(
                &self.torrent_or_magnet,
                shared_config_root,
            ),
            name: self.name.clone(),
            validation_status: self.validation_status,
            download_path: self
                .download_path
                .as_ref()
                .map(|path| {
                    resolve_shared_data_path(
                        path,
                        shared_mount_root,
                        &format!("torrent '{}'", self.name),
                    )
                })
                .transpose()?,
            container_name: self.container_name.clone(),
            torrent_control_state: self.torrent_control_state.clone(),
            delete_files: self.delete_files,
            file_priorities: self.file_priorities.clone(),
        })
    }
}

impl TorrentMetadataEntry {
    fn placeholder_from_settings(settings: &TorrentSettings) -> Option<Self> {
        let info_hash =
            crate::torrent_identity::info_hash_from_torrent_source(&settings.torrent_or_magnet)?;
        Some(Self {
            info_hash_hex: hex::encode(info_hash),
            torrent_name: settings.name.clone(),
            total_size: 0,
            is_multi_file: false,
            files: Vec::new(),
            file_priorities: settings.file_priorities.clone(),
        })
    }

    fn apply_settings_overrides(&mut self, settings: &TorrentSettings) {
        if !settings.name.is_empty() {
            self.torrent_name = settings.name.clone();
        }
        self.file_priorities = settings.file_priorities.clone();
    }
}

fn sync_torrent_metadata_with_settings(
    existing: TorrentMetadataConfig,
    settings: &Settings,
) -> TorrentMetadataConfig {
    let mut existing_by_hash: HashMap<String, TorrentMetadataEntry> = existing
        .torrents
        .into_iter()
        .map(|entry| (entry.info_hash_hex.clone(), entry))
        .collect();

    let torrents = settings
        .torrents
        .iter()
        .filter_map(|torrent| {
            let mut entry =
                TorrentMetadataEntry::placeholder_from_settings(torrent).or_else(|| {
                    crate::torrent_identity::info_hash_from_torrent_source(
                        &torrent.torrent_or_magnet,
                    )
                    .map(|info_hash| TorrentMetadataEntry {
                        info_hash_hex: hex::encode(info_hash),
                        ..Default::default()
                    })
                })?;

            if let Some(existing_entry) = existing_by_hash.remove(&entry.info_hash_hex) {
                entry = existing_entry;
            }

            entry.apply_settings_overrides(torrent);
            Some(entry)
        })
        .collect();

    TorrentMetadataConfig { torrents }
}

fn apply_metadata_to_settings(settings: &mut Settings, metadata: &TorrentMetadataConfig) {
    let metadata_by_hash: HashMap<&str, &TorrentMetadataEntry> = metadata
        .torrents
        .iter()
        .map(|entry| (entry.info_hash_hex.as_str(), entry))
        .collect();

    for torrent in &mut settings.torrents {
        let Some(info_hash) =
            crate::torrent_identity::info_hash_from_torrent_source(&torrent.torrent_or_magnet)
        else {
            continue;
        };
        let info_hash_hex = hex::encode(info_hash);
        let Some(entry) = metadata_by_hash.get(info_hash_hex.as_str()) else {
            continue;
        };
        torrent.file_priorities = entry.file_priorities.clone();
        if torrent.name.is_empty() && !entry.torrent_name.is_empty() {
            torrent.name = entry.torrent_name.clone();
        }
    }
}

impl SharedSettingsConfig {
    fn from_settings(settings: &Settings, shared_root: Option<&Path>) -> io::Result<Self> {
        Ok(Self {
            client_id: settings.client_id.clone(),
            lifetime_downloaded: settings.lifetime_downloaded,
            lifetime_uploaded: settings.lifetime_uploaded,
            private_client: settings.private_client,
            torrent_sort_column: settings.torrent_sort_column,
            torrent_sort_direction: settings.torrent_sort_direction,
            peer_sort_column: settings.peer_sort_column,
            peer_sort_direction: settings.peer_sort_direction,
            ui_theme: settings.ui_theme,
            default_download_folder: settings
                .default_download_folder
                .as_deref()
                .map(|path| encode_shared_data_path(path, shared_root, "default_download_folder"))
                .transpose()?,
            max_connected_peers: settings.max_connected_peers,
            bootstrap_nodes: settings.bootstrap_nodes.clone(),
            global_download_limit_bps: settings.global_download_limit_bps,
            global_upload_limit_bps: settings.global_upload_limit_bps,
            max_concurrent_validations: settings.max_concurrent_validations,
            connection_attempt_permits: settings.connection_attempt_permits,
            resource_limit_override: settings.resource_limit_override,
            upload_slots: settings.upload_slots,
            peer_upload_in_flight_limit: settings.peer_upload_in_flight_limit,
            tracker_fallback_interval_secs: settings.tracker_fallback_interval_secs,
            client_leeching_fallback_interval_secs: settings.client_leeching_fallback_interval_secs,
            output_status_interval: settings.output_status_interval,
            rss: settings.rss.clone(),
        })
    }

    fn apply_to_settings(
        &self,
        settings: &mut Settings,
        shared_root: Option<&Path>,
    ) -> io::Result<()> {
        settings.client_id = self.client_id.clone();
        settings.lifetime_downloaded = self.lifetime_downloaded;
        settings.lifetime_uploaded = self.lifetime_uploaded;
        settings.private_client = self.private_client;
        settings.torrent_sort_column = self.torrent_sort_column;
        settings.torrent_sort_direction = self.torrent_sort_direction;
        settings.peer_sort_column = self.peer_sort_column;
        settings.peer_sort_direction = self.peer_sort_direction;
        settings.ui_theme = self.ui_theme;
        settings.default_download_folder = self
            .default_download_folder
            .as_ref()
            .map(|path| resolve_shared_data_path(path, shared_root, "default_download_folder"))
            .transpose()?;
        if settings.default_download_folder.is_none() {
            if let Some(shared_root) = shared_root {
                settings.default_download_folder = Some(shared_root.to_path_buf());
            }
        }
        settings.max_connected_peers = self.max_connected_peers;
        settings.bootstrap_nodes = self.bootstrap_nodes.clone();
        settings.global_download_limit_bps = self.global_download_limit_bps;
        settings.global_upload_limit_bps = self.global_upload_limit_bps;
        settings.max_concurrent_validations = self.max_concurrent_validations;
        settings.connection_attempt_permits = self.connection_attempt_permits;
        settings.resource_limit_override = self.resource_limit_override;
        settings.upload_slots = self.upload_slots;
        settings.peer_upload_in_flight_limit = self.peer_upload_in_flight_limit;
        settings.tracker_fallback_interval_secs = self.tracker_fallback_interval_secs;
        settings.client_leeching_fallback_interval_secs =
            self.client_leeching_fallback_interval_secs;
        settings.output_status_interval = self.output_status_interval;
        settings.rss = self.rss.clone();
        Ok(())
    }
}

impl CatalogConfig {
    fn from_settings(
        settings: &Settings,
        shared_config_root: Option<&Path>,
        shared_mount_root: Option<&Path>,
    ) -> io::Result<Self> {
        Ok(Self {
            torrents: settings
                .torrents
                .iter()
                .map(|torrent| {
                    CatalogTorrentSettings::from_settings(
                        torrent,
                        shared_config_root,
                        shared_mount_root,
                    )
                })
                .collect::<io::Result<Vec<_>>>()?,
        })
    }

    fn apply_to_settings(
        &self,
        settings: &mut Settings,
        shared_config_root: Option<&Path>,
        shared_mount_root: Option<&Path>,
    ) -> io::Result<()> {
        settings.torrents = self
            .torrents
            .iter()
            .map(|torrent| torrent.to_settings(shared_config_root, shared_mount_root))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(())
    }
}

impl HostConfig {
    fn from_flat_settings(settings: &Settings) -> Self {
        Self {
            client_id: None,
            client_port: settings.client_port,
            watch_folder: settings.watch_folder.clone(),
        }
    }

    fn from_settings(settings: &Settings, shared_client_id: &str) -> Self {
        Self {
            client_id: (settings.client_id != shared_client_id).then(|| settings.client_id.clone()),
            client_port: settings.client_port,
            watch_folder: settings.watch_folder.clone(),
        }
    }

    fn apply_to_settings(&self, settings: &mut Settings) {
        if let Some(client_id) = &self.client_id {
            settings.client_id = client_id.clone();
        }
        settings.client_port = self.client_port;
        settings.watch_folder = self.watch_folder.clone();
    }
}
fn sanitize_host_id(raw: &str) -> String {
    let mut sanitized = String::new();
    let mut last_was_separator = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            sanitized.push(ch.to_ascii_lowercase());
            last_was_separator = false;
        } else if !last_was_separator {
            sanitized.push('-');
            last_was_separator = true;
        }
    }

    sanitized.trim_matches('-').to_string()
}

fn resolve_shared_mount_and_config_root(path: PathBuf) -> (PathBuf, PathBuf) {
    let already_points_to_subdir = path
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(SHARED_CONFIG_SUBDIR));

    if already_points_to_subdir {
        let mount_root = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.clone());
        (mount_root, path)
    } else {
        let mount_root = path;
        let config_root = mount_root.join(SHARED_CONFIG_SUBDIR);
        (mount_root, config_root)
    }
}

fn launcher_shared_config_path() -> io::Result<PathBuf> {
    let (config_dir, _) = get_app_paths().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Could not resolve application config directory",
        )
    })?;
    Ok(config_dir.join(LAUNCHER_SHARED_CONFIG_FILE))
}

fn launcher_host_id_path() -> io::Result<PathBuf> {
    let (config_dir, _) = get_app_paths().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Could not resolve application config directory",
        )
    })?;
    Ok(config_dir.join(LAUNCHER_HOST_ID_FILE))
}

fn load_launcher_shared_config() -> io::Result<Option<PathBuf>> {
    let path = launcher_shared_config_path()?;
    if !path.exists() {
        return Ok(None);
    }

    let sidecar: LauncherSharedConfig = read_toml_or_default(&path)?;
    Ok(sidecar
        .shared_config_dir
        .filter(|value| !value.as_os_str().is_empty()))
}

fn load_launcher_host_id() -> io::Result<Option<String>> {
    let path = launcher_host_id_path()?;
    if !path.exists() {
        return Ok(None);
    }

    let sidecar: LauncherHostId = read_toml_or_default(&path)?;
    Ok(sidecar
        .host_id
        .and_then(|value| sanitized_host_id_candidate(&value)))
}

fn resolve_shared_config_selection() -> io::Result<Option<SharedConfigSelection>> {
    if let Some(path) = env::var_os(SHARED_CONFIG_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        let (mount_root, config_root) = resolve_shared_mount_and_config_root(path);
        return Ok(Some(SharedConfigSelection {
            source: SharedConfigSource::Env,
            mount_root,
            config_root,
        }));
    }

    let Some(path) = load_launcher_shared_config().ok().flatten() else {
        return Ok(None);
    };
    let (mount_root, config_root) = resolve_shared_mount_and_config_root(path);
    Ok(Some(SharedConfigSelection {
        source: SharedConfigSource::Launcher,
        mount_root,
        config_root,
    }))
}

pub fn shared_mount_root() -> Option<PathBuf> {
    resolve_shared_config_selection()
        .ok()
        .flatten()
        .map(|selection| selection.mount_root)
}

fn shared_config_root() -> Option<PathBuf> {
    resolve_shared_config_selection()
        .ok()
        .flatten()
        .map(|selection| selection.config_root)
}

fn sanitized_host_id_candidate(raw: &str) -> Option<String> {
    let sanitized = sanitize_host_id(raw);
    (!sanitized.is_empty()).then_some(sanitized)
}

fn resolve_host_id_selection_from_sources(
    explicit_host_id: Option<String>,
    launcher_host_id: Option<String>,
    env_hostnames: Vec<String>,
    system_hostname: Option<String>,
) -> HostIdSelection {
    if let Some(host_id) = explicit_host_id
        .as_deref()
        .and_then(sanitized_host_id_candidate)
    {
        return HostIdSelection {
            source: HostIdSource::Env,
            host_id,
        };
    }

    if let Some(host_id) = launcher_host_id
        .as_deref()
        .and_then(sanitized_host_id_candidate)
    {
        return HostIdSelection {
            source: HostIdSource::Launcher,
            host_id,
        };
    }

    for hostname in env_hostnames {
        if let Some(host_id) = sanitized_host_id_candidate(&hostname) {
            return HostIdSelection {
                source: HostIdSource::Hostname,
                host_id,
            };
        }
    }

    if let Some(host_id) = system_hostname
        .as_deref()
        .and_then(sanitized_host_id_candidate)
    {
        return HostIdSelection {
            source: HostIdSource::System,
            host_id,
        };
    }

    HostIdSelection {
        source: HostIdSource::Default,
        host_id: "default-host".to_string(),
    }
}

fn resolve_host_id() -> String {
    resolve_host_id_selection().host_id
}

fn resolve_host_id_selection() -> HostIdSelection {
    let explicit_host_id = env::var(SHARED_HOST_ID_ENV)
        .ok()
        .or_else(|| env::var(LEGACY_SHARED_HOST_ID_ENV).ok());
    let launcher_host_id = load_launcher_host_id().ok().flatten();
    let env_hostnames = ["HOSTNAME", "COMPUTERNAME"]
        .into_iter()
        .filter_map(|key| env::var(key).ok())
        .collect();
    let system_hostname = sysinfo::System::host_name();

    resolve_host_id_selection_from_sources(
        explicit_host_id,
        launcher_host_id,
        env_hostnames,
        system_hostname,
    )
}

fn resolve_shared_config_paths() -> io::Result<Option<SharedConfigPaths>> {
    let Some(selection) = resolve_shared_config_selection()? else {
        return Ok(None);
    };
    let mount_dir = selection.mount_root;
    let root_dir = selection.config_root;
    let host_id = resolve_host_id();
    let host_dir = root_dir.join("hosts").join(&host_id);
    Ok(Some(SharedConfigPaths {
        mount_dir,
        settings_path: root_dir.join("settings.toml"),
        catalog_path: root_dir.join("catalog.toml"),
        metadata_path: root_dir.join("torrent_metadata.toml"),
        host_dir: host_dir.clone(),
        host_path: host_dir.join("config.toml"),
        root_dir,
        host_id,
    }))
}

fn resolve_config_backend() -> io::Result<ConfigBackend> {
    if let Some(paths) = resolve_shared_config_paths()? {
        return Ok(ConfigBackend::Shared(SharedConfigBackend { paths }));
    }

    let (config_dir, _) = get_app_paths().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Could not resolve application config directory",
        )
    })?;
    Ok(ConfigBackend::Normal(NormalConfigBackend {
        paths: NormalConfigPaths {
            settings_path: config_dir.join("settings.toml"),
            metadata_path: config_dir.join("torrent_metadata.toml"),
            backup_dir: config_dir.join("backups_settings_files"),
        },
    }))
}
fn portable_relative_path_string(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/")
}

fn shared_relative_path_to_pathbuf(relative: &str) -> PathBuf {
    let mut path = PathBuf::new();
    for segment in relative.split(['/', '\\']) {
        if !segment.is_empty() {
            path.push(segment);
        }
    }
    path
}

fn normalize_shared_relative_path(
    path: &Path,
    context: &str,
    allow_empty: bool,
) -> io::Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => normalized.push(segment),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{} must be a relative path inside the shared root, got {:?}",
                        context, path
                    ),
                ));
            }
        }
    }

    if normalized.as_os_str().is_empty() && !allow_empty {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} must not be empty", context),
        ));
    }

    Ok(normalized)
}

fn encode_shared_data_path(
    path: &Path,
    shared_mount_root: Option<&Path>,
    context: &str,
) -> io::Result<PathBuf> {
    let Some(shared_mount_root) = shared_mount_root else {
        return Ok(path.to_path_buf());
    };

    if !path.is_absolute() {
        return normalize_shared_relative_path(path, context, true);
    }

    let relative = strip_shared_mount_prefix(path, shared_mount_root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} must live under the shared root {:?}, got {:?}",
                context, shared_mount_root, path
            ),
        )
    })?;

    normalize_shared_relative_path(&relative, context, true)
}

fn strip_shared_mount_prefix(path: &Path, shared_mount_root: &Path) -> Result<PathBuf, ()> {
    if let Ok(relative) = path.strip_prefix(shared_mount_root) {
        return Ok(relative.to_path_buf());
    }

    #[cfg(windows)]
    {
        let normalized_path = path_without_verbatim_prefix(path);
        let normalized_root = path_without_verbatim_prefix(shared_mount_root);
        if let Ok(relative) = normalized_path.strip_prefix(&normalized_root) {
            return Ok(relative.to_path_buf());
        }
    }

    Err(())
}

#[cfg(windows)]
fn path_without_verbatim_prefix(path: &Path) -> PathBuf {
    let raw = path.as_os_str().to_string_lossy();
    if let Some(stripped) = raw.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        path.to_path_buf()
    }
}

fn resolve_shared_data_path(
    path: &Path,
    shared_mount_root: Option<&Path>,
    context: &str,
) -> io::Result<PathBuf> {
    let Some(shared_mount_root) = shared_mount_root else {
        return Ok(path.to_path_buf());
    };

    let relative = normalize_shared_relative_path(path, context, true)?;
    if relative.as_os_str().is_empty() {
        Ok(shared_mount_root.to_path_buf())
    } else {
        Ok(shared_mount_root.join(relative))
    }
}

fn validate_shared_runtime_settings(
    settings: &Settings,
    shared_mount_root: &Path,
) -> io::Result<()> {
    if let Some(path) = settings.default_download_folder.as_deref() {
        encode_shared_data_path(path, Some(shared_mount_root), "default_download_folder")?;
    }

    for torrent in &settings.torrents {
        if let Some(path) = torrent.download_path.as_deref() {
            encode_shared_data_path(
                path,
                Some(shared_mount_root),
                &format!("torrent '{}'", torrent.name),
            )?;
        }
    }

    Ok(())
}

fn encode_catalog_torrent_source(source: &str, shared_root: Option<&Path>) -> String {
    if source.starts_with("magnet:") {
        return source.to_string();
    }

    let Some(shared_root) = shared_root else {
        return source.to_string();
    };

    let path = Path::new(source);
    if let Ok(relative) = path.strip_prefix(shared_root) {
        return format!(
            "{}{}",
            SHARED_TORRENT_SOURCE_PREFIX,
            portable_relative_path_string(relative)
        );
    }

    source.to_string()
}

fn decode_catalog_torrent_source(source: &str, shared_root: Option<&Path>) -> String {
    let Some(relative) = source.strip_prefix(SHARED_TORRENT_SOURCE_PREFIX) else {
        return source.to_string();
    };

    let Some(shared_root) = shared_root else {
        return source.to_string();
    };

    shared_root
        .join(shared_relative_path_to_pathbuf(relative))
        .to_string_lossy()
        .to_string()
}

fn apply_env_overrides(settings: &Settings) -> io::Result<Settings> {
    Figment::from(Serialized::defaults(settings.clone()))
        .merge(Env::prefixed("SUPERSEEDR_"))
        .extract::<Settings>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn read_toml_or_default<T>(path: &Path) -> io::Result<T>
where
    T: for<'de> Deserialize<'de> + Default,
{
    if !path.exists() {
        return Ok(T::default());
    }

    let content = fs::read_to_string(path)?;
    deserialize_versioned_toml(&content)
}

fn read_torrent_metadata_or_default(path: &Path) -> io::Result<TorrentMetadataConfig> {
    match read_toml_or_default(path) {
        Ok(metadata) => Ok(metadata),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            tracing_event!(
                Level::WARN,
                "Ignoring invalid torrent metadata at {:?}; treating it as empty: {}",
                path,
                error
            );
            Ok(TorrentMetadataConfig::default())
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
fn fingerprint_for_path(path: &Path) -> io::Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }

    let bytes = fs::read(path)?;
    Ok(Some(hex::encode(Sha1::digest(bytes))))
}

#[cfg(test)]
fn ensure_fingerprint_matches(
    path: &Path,
    expected: &Option<String>,
    label: &str,
) -> io::Result<()> {
    let current = fingerprint_for_path(path)?;
    if &current != expected {
        return Err(io::Error::other(format!(
            "{} changed on disk at {:?}; reload required before saving",
            label, path
        )));
    }
    Ok(())
}

fn write_toml_atomically_with_fingerprint<T: Serialize>(
    path: &Path,
    value: &T,
) -> io::Result<Option<String>> {
    let content = serialize_versioned_toml(value)?;
    write_string_atomically(path, &content)?;
    Ok(Some(hex::encode(Sha1::digest(content.as_bytes()))))
}

fn write_shared_cluster_revision_marker(root_dir: &Path) -> io::Result<()> {
    let revision_path = root_dir.join("cluster.revision");
    let revision = format!(
        "{}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    write_string_atomically(&revision_path, &revision)
}

fn validate_shared_runtime_root(paths: &SharedConfigPaths) -> io::Result<()> {
    if !paths.mount_dir.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "Shared root '{}' does not exist. If this is a network share, make sure it is mounted.",
                paths.mount_dir.display()
            ),
        ));
    }

    let mount_metadata = fs::metadata(&paths.mount_dir).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "Could not access shared root '{}': {}",
                paths.mount_dir.display(),
                error
            ),
        )
    })?;
    if !mount_metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Shared root '{}' is not a directory.",
                paths.mount_dir.display()
            ),
        ));
    }

    fs::read_dir(&paths.mount_dir).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "Shared root '{}' is not readable: {}",
                paths.mount_dir.display(),
                error
            ),
        )
    })?;

    Ok(())
}

fn bootstrap_shared_host_config(paths: &SharedConfigPaths) -> io::Result<HostConfig> {
    let host = HostConfig::default();
    fs::create_dir_all(&paths.host_dir).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "Shared root '{}' is not writable for host '{}'; could not create '{}': {}",
                paths.mount_dir.display(),
                paths.host_id,
                paths.host_dir.display(),
                error
            ),
        )
    })?;
    write_toml_atomically(&paths.host_path, &host).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "Shared root '{}' is not writable for host '{}'; could not write '{}': {}",
                paths.mount_dir.display(),
                paths.host_id,
                paths.host_path.display(),
                error
            ),
        )
    })?;
    Ok(host)
}

fn clear_shared_config_state() {}

#[cfg(test)]
pub(crate) fn clear_shared_config_state_for_tests() {
    clear_shared_config_state();
}

#[cfg(test)]
pub(crate) fn set_app_paths_override_for_tests(paths: Option<(PathBuf, PathBuf)>) {
    let mut guard = app_paths_override()
        .lock()
        .expect("app paths override lock poisoned");
    *guard = paths;
}

fn first_run_settings() -> Settings {
    let mut settings = Settings::default();
    if let Some(user_dirs) = directories::UserDirs::new() {
        if let Some(dl_dir) = user_dirs.download_dir() {
            settings.default_download_folder = Some(dl_dir.to_path_buf());
        }
    }
    settings
}

fn client_never_started_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "superseedr client has never started yet; start the client once before using CLI commands",
    )
}

fn runtime_lock_is_held(lock_path: Option<&Path>) -> bool {
    let Some(lock_path) = lock_path else {
        return false;
    };

    let file = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return false,
        Err(error) => {
            tracing_event!(
                Level::WARN,
                "Failed to inspect runtime lock at {:?}: {}",
                lock_path,
                error
            );
            return false;
        }
    };

    match file.try_lock() {
        Ok(()) => false,
        Err(_) => true,
    }
}

fn load_current_shared_layered(
    paths: &SharedConfigPaths,
    bootstrap_missing_host: bool,
) -> io::Result<(LayeredConfig, TorrentMetadataConfig)> {
    let settings: SharedSettingsConfig = read_toml_or_default(&paths.settings_path)?;
    let catalog: CatalogConfig = read_toml_or_default(&paths.catalog_path)?;
    let metadata = read_torrent_metadata_or_default(&paths.metadata_path)?;
    let host = if paths.host_path.exists() {
        read_toml_or_default(&paths.host_path)?
    } else if bootstrap_missing_host {
        tracing_event!(
            Level::INFO,
            "Bootstrapping missing shared host config at {:?}",
            paths.host_path
        );
        bootstrap_shared_host_config(paths)?
    } else {
        return Err(client_never_started_error());
    };

    Ok((
        LayeredConfig {
            settings,
            catalog,
            host,
        },
        metadata,
    ))
}

impl NormalConfigBackend {
    fn load_settings(&self) -> io::Result<Settings> {
        if !self.paths.settings_path.exists() {
            tracing_event!(
                Level::INFO,
                "No settings found. Performing first-run setup."
            );
            let settings = first_run_settings();
            self.save_settings(&settings)?;
            return Ok(settings);
        }

        tracing_event!(
            Level::INFO,
            "Found existing settings at: {:?}",
            self.paths.settings_path
        );

        let flat_settings: Settings = read_toml_or_default(&self.paths.settings_path)?;
        let metadata = read_torrent_metadata_or_default(&self.paths.metadata_path)?;
        let layered = LayeredConfig::from_flat_settings(&flat_settings);
        let mut resolved_settings = layered.resolve_flat_settings()?;
        apply_metadata_to_settings(&mut resolved_settings, &metadata);
        apply_env_overrides(&resolved_settings)
    }

    fn load_settings_for_cli(&self) -> io::Result<Settings> {
        if !self.paths.settings_path.exists() {
            tracing_event!(Level::INFO, "No standalone settings found during CLI load.");
            let settings = first_run_settings();
            if runtime_lock_is_held(local_lock_path().as_deref()) {
                tracing_event!(
                    Level::INFO,
                    "Local runtime lock is held; returning first-run settings without bootstrapping."
                );
                return Ok(settings);
            }
            self.save_settings(&settings)?;
            return Ok(settings);
        }

        tracing_event!(
            Level::INFO,
            "Found existing settings at: {:?}",
            self.paths.settings_path
        );

        let flat_settings: Settings = read_toml_or_default(&self.paths.settings_path)?;
        let metadata = read_torrent_metadata_or_default(&self.paths.metadata_path)?;
        let layered = LayeredConfig::from_flat_settings(&flat_settings);
        let mut resolved_settings = layered.resolve_flat_settings()?;
        apply_metadata_to_settings(&mut resolved_settings, &metadata);
        apply_env_overrides(&resolved_settings)
    }

    fn save_settings(&self, settings: &Settings) -> io::Result<()> {
        fs::create_dir_all(&self.paths.backup_dir)?;

        let now = chrono::Local::now();
        let timestamp = now.format("%Y%m%d_%H%M%S").to_string();
        let backup_path = self
            .paths
            .backup_dir
            .join(format!("settings_{}.toml", timestamp));

        let layered = LayeredConfig::from_flat_settings(settings);
        let flat_settings = layered.resolve_flat_settings()?;
        let content = serialize_versioned_toml(&flat_settings)?;
        write_string_atomically(&self.paths.settings_path, &content)?;
        fs::write(backup_path, content)?;
        cleanup_old_backups(&self.paths.backup_dir, 64)?;

        let existing_metadata = read_torrent_metadata_or_default(&self.paths.metadata_path)?;
        let next_metadata = sync_torrent_metadata_with_settings(existing_metadata, &flat_settings);
        let _ = write_toml_atomically_with_fingerprint(&self.paths.metadata_path, &next_metadata)?;

        Ok(())
    }
}

impl SharedConfigBackend {
    fn load_settings(&self) -> io::Result<Settings> {
        validate_shared_runtime_root(&self.paths)?;
        let (layered, metadata) = load_current_shared_layered(&self.paths, true)?;
        let mut resolved_settings =
            layered.resolve_shared_settings(&self.paths.mount_dir, &self.paths.root_dir)?;
        apply_metadata_to_settings(&mut resolved_settings, &metadata);
        let resolved_settings = apply_env_overrides(&resolved_settings)?;
        validate_shared_runtime_settings(&resolved_settings, &self.paths.mount_dir)?;
        Ok(resolved_settings)
    }

    fn load_settings_for_cli(&self) -> io::Result<Settings> {
        validate_shared_runtime_root(&self.paths)?;
        if !self.paths.settings_path.exists() {
            return Err(client_never_started_error());
        }

        let (layered, metadata) = load_current_shared_layered(&self.paths, true)?;
        let mut resolved_settings =
            layered.resolve_shared_settings(&self.paths.mount_dir, &self.paths.root_dir)?;
        apply_metadata_to_settings(&mut resolved_settings, &metadata);
        let resolved_settings = apply_env_overrides(&resolved_settings)?;
        validate_shared_runtime_settings(&resolved_settings, &self.paths.mount_dir)?;
        Ok(resolved_settings)
    }

    fn save_settings(&self, settings: &Settings) -> io::Result<()> {
        validate_shared_runtime_settings(settings, &self.paths.mount_dir)?;
        // Shared writes currently rely on the shared leader lock to preserve a
        // single-writer model. If future features introduce concurrent shared
        // writers, this reload-on-save path will need explicit conflict
        // detection or merge handling before writing.
        let (current_layered, existing_metadata) = load_current_shared_layered(&self.paths, true)?;

        let next_layered = LayeredConfig::from_shared_settings(
            settings,
            &self.paths.mount_dir,
            &self.paths.root_dir,
            current_layered
                .host
                .client_id
                .as_ref()
                .map(|_| current_layered.settings.client_id.as_str()),
        )?;

        let shared_settings_changed = next_layered.settings != current_layered.settings;
        if shared_settings_changed {
            let _ = write_toml_atomically_with_fingerprint(
                &self.paths.settings_path,
                &next_layered.settings,
            )?;
        }

        let shared_catalog_changed = next_layered.catalog != current_layered.catalog;
        if shared_catalog_changed {
            let _ = write_toml_atomically_with_fingerprint(
                &self.paths.catalog_path,
                &next_layered.catalog,
            )?;
        }

        let next_metadata =
            sync_torrent_metadata_with_settings(existing_metadata.clone(), settings);
        let shared_metadata_changed = next_metadata != existing_metadata;
        if shared_metadata_changed {
            let _ =
                write_toml_atomically_with_fingerprint(&self.paths.metadata_path, &next_metadata)?;
        }

        if next_layered.host != current_layered.host {
            let _ =
                write_toml_atomically_with_fingerprint(&self.paths.host_path, &next_layered.host)?;
        }

        if shared_settings_changed || shared_catalog_changed || shared_metadata_changed {
            write_shared_cluster_revision_marker(&self.paths.root_dir)?;
        }
        Ok(())
    }
}

impl ConfigBackend {
    fn load_settings(&self) -> io::Result<Settings> {
        match self {
            ConfigBackend::Normal(backend) => {
                clear_shared_config_state();
                backend.load_settings()
            }
            ConfigBackend::Shared(backend) => {
                tracing_event!(
                    Level::INFO,
                    "Using shared config root {:?} with host id {}",
                    backend.paths.root_dir,
                    backend.paths.host_id
                );
                backend.load_settings()
            }
        }
    }

    fn load_settings_for_cli(&self) -> io::Result<Settings> {
        match self {
            ConfigBackend::Normal(backend) => {
                clear_shared_config_state();
                backend.load_settings_for_cli()
            }
            ConfigBackend::Shared(backend) => {
                tracing_event!(
                    Level::INFO,
                    "Using shared config root {:?} with host id {}",
                    backend.paths.root_dir,
                    backend.paths.host_id
                );
                backend.load_settings_for_cli()
            }
        }
    }

    fn save_settings(&self, settings: &Settings) -> io::Result<()> {
        match self {
            ConfigBackend::Normal(backend) => backend.save_settings(settings),
            ConfigBackend::Shared(backend) => backend.save_settings(settings),
        }
    }

    fn load_torrent_metadata(&self) -> io::Result<TorrentMetadataConfig> {
        match self {
            ConfigBackend::Normal(backend) => {
                read_torrent_metadata_or_default(&backend.paths.metadata_path)
            }
            ConfigBackend::Shared(backend) => {
                read_torrent_metadata_or_default(&backend.paths.metadata_path)
            }
        }
    }

    fn upsert_torrent_metadata(&self, entry: TorrentMetadataEntry) -> io::Result<()> {
        match self {
            ConfigBackend::Normal(backend) => {
                let mut metadata = read_torrent_metadata_or_default(&backend.paths.metadata_path)?;
                upsert_torrent_metadata_entry(&mut metadata, entry);
                let _ = write_toml_atomically_with_fingerprint(
                    &backend.paths.metadata_path,
                    &metadata,
                )?;
                Ok(())
            }
            ConfigBackend::Shared(backend) => {
                // This shared metadata update is safe under today's lock-based
                // single-writer model. If concurrent shared writers are added
                // later, restore conflict detection here before writing.
                let mut metadata = read_torrent_metadata_or_default(&backend.paths.metadata_path)?;
                upsert_torrent_metadata_entry(&mut metadata, entry);
                let _ = write_toml_atomically_with_fingerprint(
                    &backend.paths.metadata_path,
                    &metadata,
                )?;
                Ok(())
            }
        }
    }
}

fn upsert_torrent_metadata_entry(
    metadata: &mut TorrentMetadataConfig,
    entry: TorrentMetadataEntry,
) {
    if let Some(existing) = metadata
        .torrents
        .iter_mut()
        .find(|existing| existing.info_hash_hex == entry.info_hash_hex)
    {
        *existing = entry;
    } else {
        metadata.torrents.push(entry);
    }
}

pub fn get_app_paths() -> Option<(PathBuf, PathBuf)> {
    #[cfg(test)]
    if let Some(paths) = app_paths_override()
        .lock()
        .expect("app paths override lock poisoned")
        .clone()
    {
        fs::create_dir_all(&paths.0).ok()?;
        fs::create_dir_all(&paths.1).ok()?;
        return Some(paths);
    }

    if let Some(proj_dirs) = ProjectDirs::from("com", "github", "jagalite.superseedr") {
        let config_dir = proj_dirs.config_dir().to_path_buf();
        let data_dir = proj_dirs.data_local_dir().to_path_buf();

        if fs::create_dir_all(&config_dir).is_ok() && fs::create_dir_all(&data_dir).is_ok() {
            return Some((config_dir, data_dir));
        }
    }

    fallback_app_paths()
}

fn fallback_app_paths() -> Option<(PathBuf, PathBuf)> {
    #[cfg(windows)]
    {
        let config_base = env::var_os("APPDATA").map(PathBuf::from)?;
        let data_base = env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| env::var_os("APPDATA").map(PathBuf::from))?;
        let config_dir = config_base
            .join("Jagalite")
            .join("superseedr")
            .join("config");
        let data_dir = data_base.join("Jagalite").join("superseedr").join("data");
        fs::create_dir_all(&config_dir).ok()?;
        fs::create_dir_all(&data_dir).ok()?;
        Some((config_dir, data_dir))
    }

    #[cfg(not(windows))]
    {
        None
    }
}

pub fn app_config_dir() -> Option<PathBuf> {
    get_app_paths().map(|(config_dir, _)| config_dir)
}

pub fn local_runtime_data_dir() -> Option<PathBuf> {
    get_app_paths().map(|(_, data_dir)| data_dir)
}

pub fn local_settings_path() -> Option<PathBuf> {
    app_config_dir().map(|config_dir| config_dir.join("settings.toml"))
}

pub fn effective_shared_config_selection() -> io::Result<Option<SharedConfigSelection>> {
    resolve_shared_config_selection()
}

pub fn persisted_shared_config_path() -> io::Result<PathBuf> {
    launcher_shared_config_path()
}

pub fn effective_host_id_selection() -> io::Result<HostIdSelection> {
    Ok(resolve_host_id_selection())
}

pub fn persisted_host_id_path() -> io::Result<PathBuf> {
    launcher_host_id_path()
}

pub fn set_persisted_shared_config(path: &Path) -> io::Result<SharedConfigSelection> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Shared config path must be absolute",
        ));
    }

    let (mount_root, config_root) = resolve_shared_mount_and_config_root(path.to_path_buf());
    let sidecar_path = launcher_shared_config_path()?;
    if let Some(parent) = sidecar_path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_toml_atomically(
        &sidecar_path,
        &LauncherSharedConfig {
            shared_config_dir: Some(mount_root.clone()),
        },
    )?;
    clear_shared_config_state();

    Ok(SharedConfigSelection {
        source: SharedConfigSource::Launcher,
        mount_root,
        config_root,
    })
}

pub fn clear_persisted_shared_config() -> io::Result<bool> {
    let sidecar_path = launcher_shared_config_path()?;
    let existed = sidecar_path.exists();
    if existed {
        fs::remove_file(&sidecar_path)?;
    }
    clear_shared_config_state();
    Ok(existed)
}

pub fn set_persisted_host_id(host_id: &str) -> io::Result<String> {
    let host_id = sanitized_host_id_candidate(host_id).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Host id must contain at least one letter or number",
        )
    })?;

    let sidecar_path = launcher_host_id_path()?;
    if let Some(parent) = sidecar_path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_toml_atomically(
        &sidecar_path,
        &LauncherHostId {
            host_id: Some(host_id.clone()),
        },
    )?;
    clear_shared_config_state();
    Ok(host_id)
}

pub fn clear_persisted_host_id() -> io::Result<bool> {
    let sidecar_path = launcher_host_id_path()?;
    let existed = sidecar_path.exists();
    if existed {
        fs::remove_file(&sidecar_path)?;
    }
    clear_shared_config_state();
    Ok(existed)
}

fn local_normal_backend() -> io::Result<NormalConfigBackend> {
    let (config_dir, _) = get_app_paths().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Could not resolve application config directory",
        )
    })?;
    Ok(NormalConfigBackend {
        paths: NormalConfigPaths {
            settings_path: config_dir.join("settings.toml"),
            metadata_path: config_dir.join("torrent_metadata.toml"),
            backup_dir: config_dir.join("backups_settings_files"),
        },
    })
}

fn shared_backend_for_mount_root(path: &Path) -> io::Result<SharedConfigBackend> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Shared config path must be absolute",
        ));
    }

    let (mount_dir, root_dir) = resolve_shared_mount_and_config_root(path.to_path_buf());
    let host_id = resolve_host_id();
    let host_dir = root_dir.join("hosts").join(&host_id);
    Ok(SharedConfigBackend {
        paths: SharedConfigPaths {
            mount_dir,
            root_dir: root_dir.clone(),
            settings_path: root_dir.join("settings.toml"),
            catalog_path: root_dir.join("catalog.toml"),
            metadata_path: root_dir.join("torrent_metadata.toml"),
            host_dir: host_dir.clone(),
            host_path: host_dir.join("config.toml"),
            host_id,
        },
    })
}

pub fn convert_standalone_to_shared(path: &Path) -> io::Result<SharedConfigSelection> {
    let normal_backend = local_normal_backend()?;
    let shared_backend = shared_backend_for_mount_root(path)?;
    let settings = normal_backend.load_settings()?;
    let metadata = read_torrent_metadata_or_default(&normal_backend.paths.metadata_path)?;
    validate_shared_runtime_settings(&settings, &shared_backend.paths.mount_dir)?;
    fs::create_dir_all(&shared_backend.paths.host_dir)?;
    let next_layered = LayeredConfig::from_shared_settings(
        &settings,
        &shared_backend.paths.mount_dir,
        &shared_backend.paths.root_dir,
        None,
    )?;
    let _ = write_toml_atomically_with_fingerprint(
        &shared_backend.paths.settings_path,
        &next_layered.settings,
    )?;
    let _ = write_toml_atomically_with_fingerprint(
        &shared_backend.paths.catalog_path,
        &next_layered.catalog,
    )?;
    let _ = write_toml_atomically_with_fingerprint(
        &shared_backend.paths.host_path,
        &next_layered.host,
    )?;
    let next_metadata = sync_torrent_metadata_with_settings(metadata, &settings);
    let _ = write_toml_atomically_with_fingerprint(
        &shared_backend.paths.metadata_path,
        &next_metadata,
    )?;
    write_shared_cluster_revision_marker(&shared_backend.paths.root_dir)?;

    clear_shared_config_state();
    Ok(SharedConfigSelection {
        source: SharedConfigSource::Launcher,
        mount_root: shared_backend.paths.mount_dir,
        config_root: shared_backend.paths.root_dir,
    })
}

pub fn convert_shared_to_standalone() -> io::Result<()> {
    let shared_selection = resolve_shared_config_selection()?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Shared config is not enabled. Set shared config first or use SUPERSEEDR_SHARED_CONFIG_DIR.",
        )
    })?;
    let normal_backend = local_normal_backend()?;
    let shared_backend = shared_backend_for_mount_root(&shared_selection.mount_root)?;
    let settings = shared_backend.load_settings()?;
    let metadata = read_torrent_metadata_or_default(&shared_backend.paths.metadata_path)?;

    normal_backend.save_settings(&settings)?;
    let next_metadata = sync_torrent_metadata_with_settings(metadata, &settings);
    let _ = write_toml_atomically_with_fingerprint(
        &normal_backend.paths.metadata_path,
        &next_metadata,
    )?;
    clear_shared_config_state();
    Ok(())
}

pub fn is_shared_config_mode() -> bool {
    shared_config_root().is_some()
}

pub fn shared_settings_path() -> Option<PathBuf> {
    resolve_shared_config_paths()
        .ok()
        .flatten()
        .map(|paths| paths.settings_path)
}

pub fn shared_host_dir() -> Option<PathBuf> {
    resolve_shared_config_paths()
        .ok()
        .flatten()
        .map(|paths| paths.host_dir)
}

pub fn shared_torrents_path() -> Option<PathBuf> {
    shared_config_root().map(|root| root.join("torrents"))
}

pub fn shared_root_path() -> Option<PathBuf> {
    shared_config_root()
}

pub fn shared_data_path() -> Option<PathBuf> {
    shared_mount_root()
}

pub fn shared_torrent_file_path(info_hash: &[u8]) -> Option<PathBuf> {
    shared_torrents_path().map(|path| path.join(format!("{}.torrent", hex::encode(info_hash))))
}

pub fn shared_inbox_path() -> Option<PathBuf> {
    shared_config_root().map(|root| root.join("inbox"))
}

pub fn shared_processed_path() -> Option<PathBuf> {
    shared_config_root().map(|root| root.join("processed"))
}

pub fn shared_status_path() -> Option<PathBuf> {
    shared_host_dir().map(|root| root.join("status.json"))
}

pub fn shared_leader_status_path() -> Option<PathBuf> {
    shared_config_root().map(|root| root.join("status").join("leader.json"))
}

pub fn runtime_data_dir() -> Option<PathBuf> {
    if let Some(host_dir) = shared_host_dir() {
        return Some(host_dir);
    }

    local_runtime_data_dir()
}

pub fn runtime_log_dir() -> Option<PathBuf> {
    runtime_data_dir().map(|data_dir| data_dir.join("logs"))
}

pub fn local_runtime_log_dir() -> Option<PathBuf> {
    local_runtime_data_dir().map(|data_dir| data_dir.join("logs"))
}

pub fn local_cli_log_dir() -> Option<PathBuf> {
    local_runtime_data_dir().map(|data_dir| data_dir.join("logs").join("cli"))
}

pub fn runtime_persistence_dir() -> Option<PathBuf> {
    runtime_data_dir().map(|data_dir| data_dir.join("persistence"))
}

pub fn local_lock_path() -> Option<PathBuf> {
    local_runtime_data_dir().map(|data_dir| data_dir.join("superseedr.lock"))
}

pub fn encode_shared_cli_torrent_path(path: &Path) -> io::Result<Option<String>> {
    let Some(shared_root) = shared_mount_root() else {
        return Ok(None);
    };

    let relative = encode_shared_data_path(path, Some(&shared_root), "torrent path")?;
    Ok(Some(portable_relative_path_string(&relative)))
}

pub fn resolve_shared_cli_torrent_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }

    let Some(shared_root) = shared_mount_root() else {
        return Ok(path.to_path_buf());
    };

    resolve_shared_data_path(path, Some(&shared_root), "torrent path")
}

pub fn shared_cluster_revision_path() -> Option<PathBuf> {
    shared_config_root().map(|root| root.join("cluster.revision"))
}

pub fn shared_lock_path() -> Option<PathBuf> {
    shared_config_root().map(|root| root.join("superseedr.lock"))
}

pub fn resolve_host_watch_path(settings: &Settings) -> Option<PathBuf> {
    settings
        .watch_folder
        .clone()
        .or_else(|| get_watch_path().map(|(watch_path, _)| watch_path))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsChangeScope {
    NoChange,
    HostOnly,
    SharedOrMixed,
}

pub fn classify_shared_mode_settings_change(
    current_settings: &Settings,
    new_settings: &Settings,
) -> SettingsChangeScope {
    if new_settings == current_settings {
        return SettingsChangeScope::NoChange;
    }

    let current_host = HostConfig::from_flat_settings(current_settings);
    let new_host = HostConfig::from_flat_settings(new_settings);

    let mut current_without_host = current_settings.clone();
    let mut new_without_host = new_settings.clone();
    HostConfig::default().apply_to_settings(&mut current_without_host);
    HostConfig::default().apply_to_settings(&mut new_without_host);

    if current_without_host == new_without_host && current_host != new_host {
        SettingsChangeScope::HostOnly
    } else {
        SettingsChangeScope::SharedOrMixed
    }
}

pub fn resolve_command_watch_path(settings: &Settings) -> Option<PathBuf> {
    if is_shared_config_mode() {
        return shared_inbox_path();
    }

    resolve_host_watch_path(settings)
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

pub fn additional_watch_paths() -> Vec<PathBuf> {
    Vec::new()
}

pub fn runtime_watch_paths(
    settings: &Settings,
    shared_mode_enabled: bool,
    watch_shared_inbox: bool,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Some(path) = resolve_host_watch_path(settings) {
        push_unique_path(&mut paths, path);
    }

    if shared_mode_enabled {
        if let Some(path) = shared_root_path() {
            push_unique_path(&mut paths, path);
        }
    }

    if watch_shared_inbox {
        if let Some(path) = shared_inbox_path() {
            push_unique_path(&mut paths, path);
        }
    } else if !shared_mode_enabled {
        if let Some(path) = resolve_command_watch_path(settings) {
            push_unique_path(&mut paths, path);
        }
    }

    for path in additional_watch_paths() {
        push_unique_path(&mut paths, path);
    }

    paths
}

pub fn configured_watch_paths(settings: &Settings) -> Vec<PathBuf> {
    runtime_watch_paths(settings, is_shared_config_mode(), is_shared_config_mode())
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

pub fn ensure_watch_directories(settings: &Settings) -> io::Result<()> {
    create_watch_directories()?;
    if let Some(path) = shared_inbox_path() {
        fs::create_dir_all(path)?;
    }
    if let Some(path) = shared_processed_path() {
        fs::create_dir_all(path)?;
    }
    if let Some(path) = shared_host_dir() {
        fs::create_dir_all(path)?;
    }
    if let Some(path) = shared_data_path() {
        fs::create_dir_all(path)?;
    }
    if let Some(path) = shared_status_path().and_then(|p| p.parent().map(Path::to_path_buf)) {
        fs::create_dir_all(path)?;
    }
    if let Some(path) = runtime_log_dir() {
        fs::create_dir_all(path)?;
    }
    if let Some(path) = runtime_persistence_dir() {
        fs::create_dir_all(path)?;
    }
    if let Some(path) =
        shared_cluster_revision_path().and_then(|p| p.parent().map(Path::to_path_buf))
    {
        fs::create_dir_all(path)?;
    }
    for watch_path in configured_watch_paths(settings) {
        fs::create_dir_all(&watch_path)?;
    }
    Ok(())
}

pub fn load_settings() -> io::Result<Settings> {
    resolve_config_backend()?.load_settings()
}

pub fn load_settings_for_cli() -> io::Result<Settings> {
    resolve_config_backend()?.load_settings_for_cli()
}

pub fn save_settings(settings: &Settings) -> io::Result<()> {
    resolve_config_backend()?.save_settings(settings)
}

pub fn load_torrent_metadata() -> io::Result<TorrentMetadataConfig> {
    resolve_config_backend()?.load_torrent_metadata()
}

pub fn upsert_torrent_metadata(entry: TorrentMetadataEntry) -> io::Result<()> {
    resolve_config_backend()?.upsert_torrent_metadata(entry)
}

pub fn shared_host_id() -> Option<String> {
    resolve_shared_config_paths()
        .ok()
        .flatten()
        .map(|paths| paths.host_id)
}
fn cleanup_old_backups(backup_dir: &PathBuf, limit: usize) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(backup_dir)?
        .filter_map(|res| res.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.starts_with("settings_") && s.ends_with(".toml"))
                .unwrap_or(false)
        })
        .collect();

    if entries.len() > limit {
        entries.sort();
        for path in entries.iter().take(entries.len() - limit) {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::providers::{Format, Toml};
    use figment::Figment;
    use std::path::PathBuf;
    use tempfile::tempdir;

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

            [[torrents]]
            torrent_or_magnet = "magnet:?xt=urn:btih:other"
            name = "Another Torrent"
            validation_status = false
            download_path = "/downloads/another"
            torrent_control_state = "Paused"
        "#;

        let settings: Settings = Figment::new()
            .merge(Toml::string(toml_str))
            .extract()
            .expect("Failed to parse full TOML string");

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
        assert_eq!(settings.torrents.len(), 2);
        assert_eq!(settings.torrents[0].name, "My Test Torrent");
        assert!(settings.torrents[0].validation_status);
        assert_eq!(
            settings.torrents[0].download_path,
            Some(PathBuf::from("/downloads/my_test_torrent"))
        );
        assert_eq!(settings.torrents[1].name, "Another Torrent");
        assert_eq!(
            settings.torrents[1].torrent_control_state,
            TorrentControlState::Paused
        );
    }

    #[test]
    fn test_partial_settings_override() {
        let toml_str = r#"
            client_port = 9999
            global_upload_limit_bps = 50000

            [[torrents]]
            name = "Partial Torrent"
            download_path = "/partial/path"
        "#;

        let settings: Settings = Figment::new()
            .merge(Toml::string(toml_str))
            .extract()
            .expect("Failed to parse partial TOML string");

        let default_settings = Settings::default();

        assert_eq!(settings.client_port, 9999);
        assert_eq!(settings.global_upload_limit_bps, 50000);
        assert_eq!(settings.client_id, default_settings.client_id);
        assert_eq!(
            settings.max_connected_peers,
            default_settings.max_connected_peers
        );
        assert_eq!(
            settings.torrent_sort_column,
            default_settings.torrent_sort_column
        );
        assert_eq!(settings.torrents.len(), 1);
        assert_eq!(settings.torrents[0].name, "Partial Torrent");
        assert_eq!(
            settings.torrents[0].download_path,
            Some(PathBuf::from("/partial/path"))
        );
        assert_eq!(settings.torrents[0].torrent_or_magnet, "");
        assert!(!settings.torrents[0].validation_status);
        assert_eq!(
            settings.torrents[0].torrent_control_state,
            TorrentControlState::default()
        );
    }

    #[test]
    fn test_default_settings() {
        let toml_str = "";

        let settings: Settings = Figment::new()
            .merge(Toml::string(toml_str))
            .extract()
            .expect("Failed to parse empty string");

        let default_settings = Settings::default();

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
    fn test_invalid_ui_theme_type_does_not_fail_settings_parse() {
        let toml_str = r#"
            client_id = "theme-type-regression"
            client_port = 7777
            ui_theme = 123
        "#;

        let settings: Settings = Figment::new()
            .merge(Toml::string(toml_str))
            .extract()
            .expect("Settings parsing should not fail for non-string ui_theme");

        assert_eq!(settings.client_id, "theme-type-regression");
        assert_eq!(settings.client_port, 7777);
        assert_eq!(
            settings.ui_theme,
            ThemeName::default(),
            "Invalid ui_theme type should safely fallback to default"
        );
    }

    #[test]
    fn test_rss_filter_legacy_regex_key_is_accepted() {
        let toml_str = r#"
            [rss]
            enabled = true
            poll_interval_secs = 300
            max_preview_items = 50

            [[rss.filters]]
            regex = "ubuntu"
            enabled = true
        "#;

        let settings: Settings = Figment::new()
            .merge(Toml::string(toml_str))
            .extract()
            .expect("Settings parsing should accept legacy rss.filters.regex key");

        assert_eq!(settings.rss.filters.len(), 1);
        assert_eq!(settings.rss.filters[0].query, "ubuntu");
        assert!(matches!(settings.rss.filters[0].mode, RssFilterMode::Fuzzy));
        assert!(settings.rss.filters[0].enabled);
    }

    #[test]
    fn test_rss_filter_mode_regex_is_parsed() {
        let toml_str = r#"
            [rss]
            enabled = true

            [[rss.filters]]
            query = "series\\s+alpha"
            mode = "regex"
            enabled = true
        "#;

        let settings: Settings = Figment::new()
            .merge(Toml::string(toml_str))
            .extract()
            .expect("Settings parsing should accept rss.filters.mode");

        assert_eq!(settings.rss.filters.len(), 1);
        assert!(matches!(settings.rss.filters[0].mode, RssFilterMode::Regex));
    }

    #[test]
    fn test_invalid_torrent_state_parsing() {
        let toml_str = r#"
            [[torrents]]
            name = "Invalid Torrent"
            download_path = "/invalid/path"
            torrent_control_state = "UNKNOWN"
        "#;

        let result: Result<Settings, figment::Error> =
            Figment::new().merge(Toml::string(toml_str)).extract();

        assert!(
            result.is_err(),
            "Parsing should fail with an invalid enum variant"
        );

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

    #[test]
    fn test_shared_data_path_round_trip_under_root() {
        let dir = tempdir().expect("create tempdir");
        let shared_mount_root = dir.path();
        let absolute = shared_mount_root.join("alpha");

        let encoded = encode_shared_data_path(
            &absolute,
            Some(shared_mount_root),
            "default_download_folder",
        )
        .expect("encode shared path");
        let resolved =
            resolve_shared_data_path(&encoded, Some(shared_mount_root), "default_download_folder")
                .expect("resolve shared path");

        assert_eq!(encoded, PathBuf::from("alpha"));
        assert_eq!(resolved, absolute);
    }

    #[test]
    fn test_shared_data_path_round_trip_allows_mount_root_itself() {
        let dir = tempdir().expect("create tempdir");
        let shared_mount_root = dir.path();

        let encoded = encode_shared_data_path(
            shared_mount_root,
            Some(shared_mount_root),
            "default_download_folder",
        )
        .expect("encode shared root path");
        let resolved =
            resolve_shared_data_path(&encoded, Some(shared_mount_root), "default_download_folder")
                .expect("resolve shared root path");

        assert!(encoded.as_os_str().is_empty());
        assert_eq!(resolved, shared_mount_root);
    }

    #[test]
    fn test_shared_data_path_rejects_path_outside_root() {
        let dir = tempdir().expect("create tempdir");
        let shared_mount_root = dir.path();
        let outside_root = dir
            .path()
            .parent()
            .unwrap_or_else(|| dir.path())
            .join("outside-root");
        let err = encode_shared_data_path(
            &outside_root.join("data").join("alpha"),
            Some(shared_mount_root),
            "default_download_folder",
        )
        .expect_err("path outside shared root should fail");

        assert!(err.to_string().contains("must live under the shared root"));
    }

    #[test]
    fn test_resolve_host_id_uses_system_hostname_fallback() {
        let resolved = resolve_host_id_selection_from_sources(
            None,
            None,
            Vec::new(),
            Some("MacBook Pro.local".to_string()),
        );

        assert_eq!(resolved.host_id, "macbook-pro.local");
        assert_eq!(resolved.source, HostIdSource::System);
    }

    #[test]
    fn test_resolve_host_id_prefers_explicit_override() {
        let resolved = resolve_host_id_selection_from_sources(
            Some("Custom Laptop".to_string()),
            None,
            vec!["IgnoredHost".to_string()],
            Some("IgnoredSystem".to_string()),
        );

        assert_eq!(resolved.host_id, "custom-laptop");
        assert_eq!(resolved.source, HostIdSource::Env);
    }

    #[test]
    fn test_shared_torrent_source_round_trip() {
        let shared_root = Path::new("/shared-root");
        let absolute = "/shared-root/torrents/0123456789abcdef0123456789abcdef01234567.torrent";
        let encoded = encode_catalog_torrent_source(absolute, Some(shared_root));
        assert_eq!(
            encoded,
            "shared:torrents/0123456789abcdef0123456789abcdef01234567.torrent"
        );
        let decoded = decode_catalog_torrent_source(&encoded, Some(shared_root));
        assert_eq!(PathBuf::from(decoded), PathBuf::from(absolute));
    }

    #[test]
    fn test_layered_config_round_trips_flat_settings() {
        let settings = Settings {
            client_id: "flat-node".to_string(),
            client_port: 7700,
            watch_folder: Some(PathBuf::from("/watch")),
            default_download_folder: Some(PathBuf::from("/downloads")),
            torrents: vec![TorrentSettings {
                torrent_or_magnet: "/library/example.torrent".to_string(),
                name: "Alpha Archive".to_string(),
                download_path: Some(PathBuf::from("/downloads/alpha")),
                ..TorrentSettings::default()
            }],
            ..Settings::default()
        };

        let layered = LayeredConfig::from_flat_settings(&settings);
        let resolved = layered
            .resolve_flat_settings()
            .expect("resolve flat settings");

        assert_eq!(resolved, settings);
        assert_eq!(
            layered.catalog.torrents[0].torrent_or_magnet,
            "/library/example.torrent"
        );
        assert_eq!(layered.host.watch_folder, Some(PathBuf::from("/watch")));
    }

    #[test]
    fn test_layered_config_round_trips_shared_settings() {
        let dir = tempdir().expect("create tempdir");
        let shared_mount_root = dir.path();
        let shared_config_root = shared_mount_root.join(SHARED_CONFIG_SUBDIR);

        let settings = Settings {
            client_id: "host-node".to_string(),
            client_port: 7711,
            watch_folder: Some(PathBuf::from("/watch")),
            default_download_folder: Some(shared_mount_root.join("downloads")),
            torrents: vec![TorrentSettings {
                torrent_or_magnet: shared_config_root
                    .join("torrents")
                    .join("abc123.torrent")
                    .to_string_lossy()
                    .to_string(),
                name: "Shared Archive".to_string(),
                download_path: Some(shared_mount_root.join("downloads").join("shared")),
                ..TorrentSettings::default()
            }],
            ..Settings::default()
        };

        let layered = LayeredConfig::from_shared_settings(
            &settings,
            shared_mount_root,
            &shared_config_root,
            Some("shared-node"),
        )
        .expect("build layered shared settings");
        let resolved = layered
            .resolve_shared_settings(shared_mount_root, &shared_config_root)
            .expect("resolve shared settings");

        assert_eq!(resolved.client_id, settings.client_id);
        assert_eq!(resolved.client_port, settings.client_port);
        assert_eq!(resolved.watch_folder, settings.watch_folder);
        assert_eq!(
            resolved.default_download_folder,
            settings.default_download_folder
        );
        assert_eq!(resolved.torrents[0].name, settings.torrents[0].name);
        assert_eq!(
            PathBuf::from(&resolved.torrents[0].torrent_or_magnet),
            PathBuf::from(&settings.torrents[0].torrent_or_magnet)
        );
        assert_eq!(
            resolved.torrents[0].download_path,
            settings.torrents[0].download_path
        );
        assert_eq!(layered.settings.client_id, "shared-node");
        assert_eq!(layered.host.client_id.as_deref(), Some("host-node"));
        assert_eq!(
            layered.settings.default_download_folder,
            Some(PathBuf::from("downloads"))
        );
        assert_eq!(
            layered.catalog.torrents[0].torrent_or_magnet,
            "shared:torrents/abc123.torrent"
        );
        assert_eq!(
            layered.catalog.torrents[0].download_path,
            Some(PathBuf::from("downloads").join("shared"))
        );
    }

    #[test]
    fn test_catalog_and_host_merge_into_runtime_settings() {
        let shared_mount_root = Path::new("/shared-root");
        let shared_config_root = Path::new("/shared-root/superseedr-config");

        let shared_settings = SharedSettingsConfig {
            client_id: "shared-id".to_string(),
            default_download_folder: Some(PathBuf::from("downloads")),
            global_download_limit_bps: 1234,
            ..SharedSettingsConfig::default()
        };
        let catalog = CatalogConfig {
            torrents: vec![CatalogTorrentSettings {
                torrent_or_magnet: "shared:torrents/shared-collection.torrent".to_string(),
                name: "Shared Collection".to_string(),
                download_path: Some(PathBuf::from("downloads").join("shared")),
                ..CatalogTorrentSettings::default()
            }],
        };
        let host = HostConfig {
            client_id: Some("host-a".to_string()),
            client_port: 7777,
            watch_folder: Some(PathBuf::from("/watch")),
        };

        let mut settings = Settings::default();
        shared_settings
            .apply_to_settings(&mut settings, Some(shared_mount_root))
            .expect("apply shared settings");
        catalog
            .apply_to_settings(
                &mut settings,
                Some(shared_config_root),
                Some(shared_mount_root),
            )
            .expect("apply catalog");
        host.apply_to_settings(&mut settings);

        assert_eq!(settings.client_id, "host-a");
        assert_eq!(settings.client_port, 7777);
        assert_eq!(settings.watch_folder, Some(PathBuf::from("/watch")));
        assert_eq!(
            settings.default_download_folder,
            Some(shared_mount_root.join("downloads"))
        );
        assert_eq!(settings.global_download_limit_bps, 1234);
        assert_eq!(
            settings.torrents[0].torrent_or_magnet,
            shared_config_root
                .join("torrents")
                .join("shared-collection.torrent")
                .to_string_lossy()
                .to_string()
        );
        assert_eq!(
            settings.torrents[0].download_path,
            Some(shared_mount_root.join("downloads").join("shared"))
        );
    }

    #[test]
    fn test_host_override_client_id_wins_over_shared_default() {
        let shared_settings = SharedSettingsConfig {
            client_id: "shared-id".to_string(),
            ..SharedSettingsConfig::default()
        };
        let host = HostConfig {
            client_id: Some("host-id".to_string()),
            ..HostConfig::default()
        };

        let mut settings = Settings::default();
        shared_settings
            .apply_to_settings(&mut settings, Some(Path::new("/shared-root")))
            .expect("apply shared settings");
        host.apply_to_settings(&mut settings);

        assert_eq!(settings.client_id, "host-id");
    }

    #[test]
    fn test_fingerprint_detection_catches_stale_write() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("catalog.toml");
        fs::write(&path, "value = 1\n").expect("write file");
        let fingerprint = fingerprint_for_path(&path).expect("fingerprint");
        fs::write(&path, "value = 2\n").expect("rewrite file");

        let err = ensure_fingerprint_matches(&path, &fingerprint, "Shared catalog")
            .expect_err("stale write should fail");
        assert!(err.to_string().contains("reload required"));
    }

    #[test]
    fn test_write_toml_atomically_writes_file() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("host.toml");
        let host = HostConfig {
            client_id: Some("host-a".to_string()),
            ..HostConfig::default()
        };

        let fingerprint = write_toml_atomically_with_fingerprint(&path, &host).expect("write toml");
        assert!(path.exists());
        assert!(fingerprint.is_some());
    }

    #[test]
    fn test_write_shared_cluster_revision_marker_writes_file_atomically() {
        let dir = tempdir().expect("create tempdir");
        let revision_path = dir.path().join("cluster.revision");

        write_shared_cluster_revision_marker(dir.path()).expect("write first revision");
        let first = fs::read_to_string(&revision_path).expect("read first revision");
        assert!(!first.trim().is_empty());

        std::thread::sleep(std::time::Duration::from_millis(2));

        write_shared_cluster_revision_marker(dir.path()).expect("write second revision");
        let second = fs::read_to_string(&revision_path).expect("read second revision");
        assert!(!second.trim().is_empty());
        assert_ne!(first, second);
        assert!(!revision_path.with_extension("revision.tmp").exists());
    }

    #[test]
    fn test_normal_backend_round_trips_settings() {
        let dir = tempdir().expect("create tempdir");
        let backend = NormalConfigBackend {
            paths: NormalConfigPaths {
                settings_path: dir.path().join("settings.toml"),
                metadata_path: dir.path().join("torrent_metadata.toml"),
                backup_dir: dir.path().join("backups_settings_files"),
            },
        };
        let settings = Settings {
            client_id: "unit-host".to_string(),
            client_port: 7777,
            global_download_limit_bps: 1234,
            ..Settings::default()
        };

        backend.save_settings(&settings).expect("save settings");
        let loaded = backend.load_settings().expect("load settings");

        assert_eq!(loaded.client_id, "unit-host");
        assert_eq!(loaded.client_port, 7777);
        assert_eq!(loaded.global_download_limit_bps, 1234);
        assert!(backend.paths.settings_path.exists());
        assert!(backend.paths.metadata_path.exists());
    }

    #[test]
    fn test_shared_backend_routes_shared_and_host_fields() {
        let _guard = shared_backend_guard().lock().unwrap();
        clear_shared_config_state();
        let dir = tempdir().expect("create tempdir");
        let config_root = dir.path().join(SHARED_CONFIG_SUBDIR);
        let host_dir = config_root.join("hosts").join("node-a");
        let backend = SharedConfigBackend {
            paths: SharedConfigPaths {
                mount_dir: dir.path().to_path_buf(),
                root_dir: config_root.clone(),
                settings_path: config_root.join("settings.toml"),
                catalog_path: config_root.join("catalog.toml"),
                metadata_path: config_root.join("torrent_metadata.toml"),
                host_dir: host_dir.clone(),
                host_path: host_dir.join("config.toml"),
                host_id: "node-a".to_string(),
            },
        };
        let shared_torrent_path = backend
            .paths
            .root_dir
            .join("torrents")
            .join("0123456789abcdef0123456789abcdef01234567.torrent");

        write_toml_atomically(&backend.paths.host_path, &HostConfig::default())
            .expect("seed host file");

        let mut loaded = backend.load_settings().expect("load shared settings");
        loaded.client_id = "shared-node".to_string();
        loaded.client_port = 9090;
        loaded.watch_folder = Some(PathBuf::from("/watch"));
        loaded.global_upload_limit_bps = 4321;
        loaded.default_download_folder = Some(dir.path().join("downloads"));
        loaded.torrents.push(TorrentSettings {
            torrent_or_magnet: shared_torrent_path.to_string_lossy().to_string(),
            name: "Library Item".to_string(),
            download_path: Some(dir.path().join("downloads").join("library-item")),
            ..TorrentSettings::default()
        });

        backend
            .save_settings(&loaded)
            .expect("save shared settings");
        let reloaded = backend.load_settings().expect("reload shared settings");

        let shared_settings: SharedSettingsConfig =
            read_toml_or_default(&backend.paths.settings_path).expect("read settings file");
        let host_config: HostConfig =
            read_toml_or_default(&backend.paths.host_path).expect("read host file");
        let catalog_config: CatalogConfig =
            read_toml_or_default(&backend.paths.catalog_path).expect("read catalog file");
        let metadata_contents =
            fs::read_to_string(&backend.paths.metadata_path).expect("read metadata file");
        let revision_path = backend.paths.root_dir.join("cluster.revision");

        assert_eq!(host_config.client_port, 9090);
        assert_eq!(host_config.client_id, None);
        assert_eq!(host_config.watch_folder, Some(PathBuf::from("/watch")));
        assert_eq!(shared_settings.client_id, "shared-node");
        assert_eq!(shared_settings.global_upload_limit_bps, 4321);
        assert_eq!(
            shared_settings.default_download_folder,
            Some(PathBuf::from("downloads"))
        );
        assert_eq!(catalog_config.torrents.len(), 1);
        assert_eq!(catalog_config.torrents[0].name, "Library Item");
        assert_eq!(
            catalog_config.torrents[0].torrent_or_magnet,
            "shared:torrents/0123456789abcdef0123456789abcdef01234567.torrent"
        );
        assert_eq!(
            catalog_config.torrents[0].download_path,
            Some(PathBuf::from("downloads").join("library-item"))
        );
        assert!(metadata_contents.contains("[[torrents]]"));
        assert!(metadata_contents.contains("torrent_name = \"Library Item\""));
        assert!(revision_path.exists());
        assert_eq!(
            reloaded.torrents[0].torrent_or_magnet,
            shared_torrent_path.to_string_lossy().to_string()
        );
        assert_eq!(
            reloaded.default_download_folder,
            Some(dir.path().join("downloads"))
        );
    }

    #[test]
    fn test_shared_backend_bootstraps_missing_host_file() {
        let _guard = shared_backend_guard().lock().unwrap();
        clear_shared_config_state();
        let dir = tempdir().expect("create tempdir");
        let shared_root = dir.path().join("superseedr-config");
        let host_dir = shared_root.join("hosts").join("windows-node");
        let backend = SharedConfigBackend {
            paths: SharedConfigPaths {
                mount_dir: dir.path().to_path_buf(),
                root_dir: shared_root.clone(),
                settings_path: shared_root.join("settings.toml"),
                catalog_path: shared_root.join("catalog.toml"),
                metadata_path: shared_root.join("torrent_metadata.toml"),
                host_dir: host_dir.clone(),
                host_path: host_dir.join("config.toml"),
                host_id: "windows-node".to_string(),
            },
        };

        fs::create_dir_all(&backend.paths.root_dir).expect("create shared root");
        let settings = backend
            .load_settings()
            .expect("missing host file should bootstrap");

        assert_eq!(settings.client_port, Settings::default().client_port);
        assert!(backend.paths.host_path.exists());
        let host: HostConfig =
            read_toml_or_default(&backend.paths.host_path).expect("read bootstrapped host file");
        assert_eq!(host, HostConfig::default());
    }

    #[test]
    fn test_shared_backend_reports_missing_mount_root_clearly() {
        let _guard = shared_backend_guard().lock().unwrap();
        clear_shared_config_state();
        let dir = tempdir().expect("create tempdir");
        let missing_mount = dir.path().join("missing-mount");
        let shared_root = missing_mount.join("superseedr-config");
        let host_dir = shared_root.join("hosts").join("node-a");
        let backend = SharedConfigBackend {
            paths: SharedConfigPaths {
                mount_dir: missing_mount.clone(),
                root_dir: shared_root.clone(),
                settings_path: shared_root.join("settings.toml"),
                catalog_path: shared_root.join("catalog.toml"),
                metadata_path: shared_root.join("torrent_metadata.toml"),
                host_dir: host_dir.clone(),
                host_path: host_dir.join("config.toml"),
                host_id: "node-a".to_string(),
            },
        };

        let error = backend
            .load_settings()
            .expect_err("missing mount root should fail");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(
            error.to_string().contains("does not exist"),
            "unexpected error: {error}"
        );
        assert!(
            error.to_string().contains("network share"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn test_bootstrap_shared_host_config_error_mentions_host_and_path() {
        let dir = tempdir().expect("create tempdir");
        let shared_root = dir.path().join("superseedr-config");
        let host_dir = shared_root.join("hosts").join("node-a");
        let paths = SharedConfigPaths {
            mount_dir: dir.path().to_path_buf(),
            root_dir: shared_root.clone(),
            settings_path: shared_root.join("settings.toml"),
            catalog_path: shared_root.join("catalog.toml"),
            metadata_path: shared_root.join("torrent_metadata.toml"),
            host_dir: host_dir.clone(),
            host_path: host_dir.join("config.toml"),
            host_id: "node-a".to_string(),
        };

        fs::write(&shared_root, "not a directory").expect("create blocking file");

        let error =
            bootstrap_shared_host_config(&paths).expect_err("bootstrap should fail on bad parent");

        assert!(
            error.to_string().contains("node-a"),
            "unexpected error: {error}"
        );
        assert!(
            error
                .to_string()
                .contains(&paths.host_dir.display().to_string()),
            "unexpected error: {error}"
        );
        assert!(
            error.to_string().contains("not writable"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn test_normal_backend_cli_load_bootstraps_missing_settings_when_local_client_is_not_running() {
        let _guard = shared_backend_guard().lock().unwrap();
        let temp = set_temp_app_paths();
        let backend = NormalConfigBackend {
            paths: NormalConfigPaths {
                settings_path: temp.path().join("settings.toml"),
                metadata_path: temp.path().join("torrent_metadata.toml"),
                backup_dir: temp.path().join("backups_settings_files"),
            },
        };

        let loaded = backend
            .load_settings_for_cli()
            .expect("missing standalone settings should bootstrap for cli");

        assert_eq!(loaded, first_run_settings());
        assert!(backend.paths.settings_path.exists());
        assert!(backend.paths.metadata_path.exists());
        assert!(backend.paths.backup_dir.exists());

        set_app_paths_override_for_tests(None);
    }

    #[test]
    fn test_normal_backend_cli_load_stays_read_only_when_local_client_is_running() {
        let _guard = shared_backend_guard().lock().unwrap();
        let temp = set_temp_app_paths();
        let lock_path = local_lock_path().expect("local lock path");
        fs::create_dir_all(lock_path.parent().expect("lock parent")).expect("create lock dir");
        let lock_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .expect("open lock file");
        lock_file.try_lock().expect("hold local runtime lock");

        let backend = NormalConfigBackend {
            paths: NormalConfigPaths {
                settings_path: temp.path().join("standalone-settings.toml"),
                metadata_path: temp.path().join("standalone-metadata.toml"),
                backup_dir: temp.path().join("standalone-backups"),
            },
        };

        let loaded = backend
            .load_settings_for_cli()
            .expect("locked runtime should still allow read-only cli load");

        assert_eq!(loaded, first_run_settings());
        assert!(!backend.paths.settings_path.exists());
        assert!(!backend.paths.metadata_path.exists());
        assert!(!backend.paths.backup_dir.exists());

        set_app_paths_override_for_tests(None);
    }

    #[test]
    fn test_shared_backend_cli_load_bootstraps_missing_host_file() {
        let _guard = shared_backend_guard().lock().unwrap();
        clear_shared_config_state();
        let dir = tempdir().expect("create tempdir");
        let shared_root = dir.path().join("superseedr-config");
        let host_dir = shared_root.join("hosts").join("windows-node");
        let backend = SharedConfigBackend {
            paths: SharedConfigPaths {
                mount_dir: dir.path().to_path_buf(),
                root_dir: shared_root.clone(),
                settings_path: shared_root.join("settings.toml"),
                catalog_path: shared_root.join("catalog.toml"),
                metadata_path: shared_root.join("torrent_metadata.toml"),
                host_dir: host_dir.clone(),
                host_path: host_dir.join("config.toml"),
                host_id: "windows-node".to_string(),
            },
        };

        fs::create_dir_all(&backend.paths.root_dir).expect("create shared root");
        write_toml_atomically(
            &backend.paths.settings_path,
            &SharedSettingsConfig::default(),
        )
        .expect("seed shared settings");

        let loaded = backend
            .load_settings_for_cli()
            .expect("missing host file should bootstrap for cli");

        assert_eq!(
            loaded.default_download_folder,
            Some(dir.path().to_path_buf())
        );
        assert!(backend.paths.host_path.exists());
    }

    #[test]
    fn test_shared_backend_defaults_download_folder_to_mount_dir_when_unset() {
        let _guard = shared_backend_guard().lock().unwrap();
        clear_shared_config_state();
        let dir = tempdir().expect("create tempdir");
        let shared_root = dir.path().join("superseedr-config");
        let host_dir = shared_root.join("hosts").join("node-a");
        let backend = SharedConfigBackend {
            paths: SharedConfigPaths {
                mount_dir: dir.path().to_path_buf(),
                root_dir: shared_root.clone(),
                settings_path: shared_root.join("settings.toml"),
                catalog_path: shared_root.join("catalog.toml"),
                metadata_path: shared_root.join("torrent_metadata.toml"),
                host_dir: host_dir.clone(),
                host_path: host_dir.join("config.toml"),
                host_id: "node-a".to_string(),
            },
        };

        fs::create_dir_all(&backend.paths.root_dir).expect("create shared root");
        write_toml_atomically(
            &backend.paths.settings_path,
            &SharedSettingsConfig::default(),
        )
        .expect("seed shared settings");
        write_toml_atomically(&backend.paths.host_path, &HostConfig::default())
            .expect("seed host config");

        let loaded = backend.load_settings().expect("load shared settings");

        assert_eq!(
            loaded.default_download_folder,
            Some(dir.path().to_path_buf())
        );
    }

    #[test]
    fn test_encode_shared_cli_torrent_path_returns_portable_relative_path() {
        let _guard = shared_backend_guard().lock().unwrap();
        let original_shared_dir = env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
        clear_shared_config_state();
        let dir = tempdir().expect("create tempdir");
        let nested = dir
            .path()
            .join("shared-fixtures")
            .join("sample-input.torrent");
        fs::create_dir_all(nested.parent().expect("parent")).expect("create nested dir");
        fs::write(&nested, "payload").expect("write fixture");
        env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", dir.path());

        let encoded = encode_shared_cli_torrent_path(&nested)
            .expect("encode shared cli torrent path")
            .expect("shared mode should encode");

        assert_eq!(encoded, "shared-fixtures/sample-input.torrent");

        if let Some(value) = original_shared_dir {
            env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
        }
        clear_shared_config_state();
    }

    #[test]
    fn test_resolve_shared_cli_torrent_path_expands_relative_path_against_mount_root() {
        let _guard = shared_backend_guard().lock().unwrap();
        let original_shared_dir = env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
        clear_shared_config_state();
        let dir = tempdir().expect("create tempdir");
        env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", dir.path());

        let resolved =
            resolve_shared_cli_torrent_path(Path::new("shared-fixtures/sample-input.torrent"))
                .expect("resolve shared cli torrent path");

        assert_eq!(
            resolved,
            dir.path()
                .join("shared-fixtures")
                .join("sample-input.torrent")
        );

        if let Some(value) = original_shared_dir {
            env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", value);
        } else {
            env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
        }
        clear_shared_config_state();
    }

    #[test]
    fn test_shared_backend_preserves_shared_client_id_when_host_override_exists() {
        let _guard = shared_backend_guard().lock().unwrap();
        clear_shared_config_state();
        let dir = tempdir().expect("create tempdir");
        let config_root = dir.path().join(SHARED_CONFIG_SUBDIR);
        let host_dir = config_root.join("hosts").join("node-a");
        let backend = SharedConfigBackend {
            paths: SharedConfigPaths {
                mount_dir: dir.path().to_path_buf(),
                root_dir: config_root.clone(),
                settings_path: config_root.join("settings.toml"),
                catalog_path: config_root.join("catalog.toml"),
                metadata_path: config_root.join("torrent_metadata.toml"),
                host_dir: host_dir.clone(),
                host_path: host_dir.join("config.toml"),
                host_id: "node-a".to_string(),
            },
        };

        write_toml_atomically(
            &backend.paths.settings_path,
            &SharedSettingsConfig {
                client_id: "shared-default".to_string(),
                ..SharedSettingsConfig::default()
            },
        )
        .expect("seed shared settings");
        write_toml_atomically(
            &backend.paths.host_path,
            &HostConfig {
                client_id: Some("host-override".to_string()),
                ..HostConfig::default()
            },
        )
        .expect("seed host config");

        let mut loaded = backend.load_settings().expect("load shared settings");
        assert_eq!(loaded.client_id, "host-override");

        loaded.global_download_limit_bps = 9876;
        backend
            .save_settings(&loaded)
            .expect("save shared settings");

        let settings_contents =
            fs::read_to_string(&backend.paths.settings_path).expect("read settings file");
        let host_contents = fs::read_to_string(&backend.paths.host_path).expect("read host file");

        assert!(settings_contents.contains("client_id = \"shared-default\""));
        assert!(settings_contents.contains("global_download_limit_bps = 9876"));
        assert!(host_contents.contains("client_id = \"host-override\""));
    }

    #[test]
    fn test_shared_backend_host_only_save_does_not_bump_cluster_revision() {
        let _guard = shared_backend_guard().lock().unwrap();
        clear_shared_config_state();
        let dir = tempdir().expect("create tempdir");
        let config_root = dir.path().join(SHARED_CONFIG_SUBDIR);
        let host_dir = config_root.join("hosts").join("node-a");
        let backend = SharedConfigBackend {
            paths: SharedConfigPaths {
                mount_dir: dir.path().to_path_buf(),
                root_dir: config_root.clone(),
                settings_path: config_root.join("settings.toml"),
                catalog_path: config_root.join("catalog.toml"),
                metadata_path: config_root.join("torrent_metadata.toml"),
                host_dir: host_dir.clone(),
                host_path: host_dir.join("config.toml"),
                host_id: "node-a".to_string(),
            },
        };

        write_toml_atomically(&backend.paths.host_path, &HostConfig::default())
            .expect("seed host file");

        let mut loaded = backend.load_settings().expect("load shared settings");
        loaded.global_download_limit_bps = 2048;
        backend
            .save_settings(&loaded)
            .expect("save initial shared settings");

        let revision_path = backend.paths.root_dir.join("cluster.revision");
        let first_revision = fs::read_to_string(&revision_path).expect("read first revision");

        std::thread::sleep(std::time::Duration::from_millis(10));

        loaded.client_port = 7777;
        loaded.watch_folder = Some(PathBuf::from("/host-watch"));
        backend
            .save_settings(&loaded)
            .expect("save host-only settings");

        let second_revision = fs::read_to_string(&revision_path).expect("read second revision");
        assert_eq!(first_revision, second_revision);
    }

    #[test]
    fn test_shared_backend_noop_save_does_not_rewrite_revision_or_metadata() {
        let _guard = shared_backend_guard().lock().unwrap();
        clear_shared_config_state();
        let dir = tempdir().expect("create tempdir");
        let config_root = dir.path().join(SHARED_CONFIG_SUBDIR);
        let host_dir = config_root.join("hosts").join("node-a");
        let backend = SharedConfigBackend {
            paths: SharedConfigPaths {
                mount_dir: dir.path().to_path_buf(),
                root_dir: config_root.clone(),
                settings_path: config_root.join("settings.toml"),
                catalog_path: config_root.join("catalog.toml"),
                metadata_path: config_root.join("torrent_metadata.toml"),
                host_dir: host_dir.clone(),
                host_path: host_dir.join("config.toml"),
                host_id: "node-a".to_string(),
            },
        };

        write_toml_atomically(&backend.paths.host_path, &HostConfig::default())
            .expect("seed host file");

        let mut loaded = backend.load_settings().expect("load shared settings");
        loaded.global_download_limit_bps = 4096;
        loaded.torrents.push(TorrentSettings {
            torrent_or_magnet: "magnet:?xt=urn:btih:1111111111111111111111111111111111111111"
                .to_string(),
            name: "Sample Node".to_string(),
            ..TorrentSettings::default()
        });
        backend
            .save_settings(&loaded)
            .expect("save shared settings");

        let revision_path = backend.paths.root_dir.join("cluster.revision");
        let first_revision = fs::read_to_string(&revision_path).expect("read first revision");
        let first_metadata =
            fs::read_to_string(&backend.paths.metadata_path).expect("read first metadata");

        std::thread::sleep(std::time::Duration::from_millis(10));

        backend.save_settings(&loaded).expect("save noop settings");

        let second_revision = fs::read_to_string(&revision_path).expect("read second revision");
        let second_metadata =
            fs::read_to_string(&backend.paths.metadata_path).expect("read second metadata");

        assert_eq!(first_revision, second_revision);
        assert_eq!(first_metadata, second_metadata);
    }

    #[test]
    fn test_metadata_syncs_file_priorities_from_settings() {
        let dir = tempdir().expect("create tempdir");
        let backend = NormalConfigBackend {
            paths: NormalConfigPaths {
                settings_path: dir.path().join("settings.toml"),
                metadata_path: dir.path().join("torrent_metadata.toml"),
                backup_dir: dir.path().join("backups_settings_files"),
            },
        };
        let settings = Settings {
            torrents: vec![TorrentSettings {
                torrent_or_magnet: "magnet:?xt=urn:btih:1111111111111111111111111111111111111111"
                    .to_string(),
                name: "Sample Alpha".to_string(),
                file_priorities: HashMap::from([(1, FilePriority::Skip)]),
                ..TorrentSettings::default()
            }],
            ..Settings::default()
        };

        backend.save_settings(&settings).expect("save settings");
        let metadata: TorrentMetadataConfig =
            read_toml_or_default(&backend.paths.metadata_path).expect("load metadata");

        assert_eq!(metadata.torrents.len(), 1);
        assert_eq!(
            metadata.torrents[0].info_hash_hex,
            "1111111111111111111111111111111111111111"
        );
        assert_eq!(
            metadata.torrents[0].file_priorities.get(&1),
            Some(&FilePriority::Skip)
        );
    }

    #[test]
    fn test_normal_load_settings_ignores_invalid_torrent_metadata() {
        let dir = tempdir().expect("create tempdir");
        let backend = NormalConfigBackend {
            paths: NormalConfigPaths {
                settings_path: dir.path().join("settings.toml"),
                metadata_path: dir.path().join("torrent_metadata.toml"),
                backup_dir: dir.path().join("backups_settings_files"),
            },
        };
        let settings = Settings {
            client_id: "normal-metadata-recovery".to_string(),
            torrents: vec![TorrentSettings {
                torrent_or_magnet: "magnet:?xt=urn:btih:1111111111111111111111111111111111111111"
                    .to_string(),
                ..TorrentSettings::default()
            }],
            ..Settings::default()
        };
        write_toml_atomically(&backend.paths.settings_path, &settings).expect("write settings");
        write_string_atomically(
            &backend.paths.metadata_path,
            "schema_version = 1\n[[torrents]]\ninfo_hash_hex = \"1111111111111111111111111111111111111111\"\n[torrents.file_priorities]\n[torrents.file_priorities]\n",
        )
        .expect("write invalid metadata");

        let loaded = backend.load_settings().expect("load settings");
        let metadata = ConfigBackend::Normal(backend.clone())
            .load_torrent_metadata()
            .expect("load metadata");

        assert_eq!(loaded.client_id, "normal-metadata-recovery");
        assert_eq!(loaded.torrents.len(), 1);
        assert!(metadata.torrents.is_empty());
    }

    #[test]
    fn test_shared_load_settings_ignores_invalid_torrent_metadata() {
        let dir = tempdir().expect("create tempdir");
        let shared_root = dir.path().join("shared-root");
        let backend = shared_backend_for_mount_root(&shared_root).expect("shared backend");
        fs::create_dir_all(&backend.paths.host_dir).expect("create host dir");
        write_toml_atomically(
            &backend.paths.settings_path,
            &SharedSettingsConfig {
                client_id: "shared-metadata-recovery".to_string(),
                ..SharedSettingsConfig::default()
            },
        )
        .expect("write shared settings");
        write_toml_atomically(&backend.paths.catalog_path, &CatalogConfig::default())
            .expect("write catalog");
        write_toml_atomically(&backend.paths.host_path, &HostConfig::default())
            .expect("write host config");
        write_string_atomically(
            &backend.paths.metadata_path,
            "schema_version = 1\n[[torrents]]\ninfo_hash_hex = \"1111111111111111111111111111111111111111\"\n[torrents.file_priorities]\n[torrents.file_priorities]\n",
        )
        .expect("write invalid metadata");

        let loaded = backend.load_settings().expect("load shared settings");
        let metadata = ConfigBackend::Shared(backend.clone())
            .load_torrent_metadata()
            .expect("load shared metadata");

        assert_eq!(loaded.client_id, "shared-metadata-recovery");
        assert!(loaded.torrents.is_empty());
        assert!(metadata.torrents.is_empty());
    }

    #[test]
    fn test_normal_save_settings_overwrites_invalid_torrent_metadata() {
        let dir = tempdir().expect("create tempdir");
        let backend = NormalConfigBackend {
            paths: NormalConfigPaths {
                settings_path: dir.path().join("settings.toml"),
                metadata_path: dir.path().join("torrent_metadata.toml"),
                backup_dir: dir.path().join("backups_settings_files"),
            },
        };
        let invalid_metadata = "schema_version = 1\n[[torrents]]\ninfo_hash_hex = \"1111111111111111111111111111111111111111\"\n[torrents.file_priorities]\n[torrents.file_priorities]\n";
        write_toml_atomically(&backend.paths.settings_path, &Settings::default())
            .expect("write initial settings");
        write_string_atomically(&backend.paths.metadata_path, invalid_metadata)
            .expect("write invalid metadata");

        let next_settings = Settings {
            client_id: "after-invalid-metadata".to_string(),
            torrents: vec![TorrentSettings {
                torrent_or_magnet: "magnet:?xt=urn:btih:1111111111111111111111111111111111111111"
                    .to_string(),
                name: "Sample Node".to_string(),
                file_priorities: HashMap::from([(1, FilePriority::Skip)]),
                ..TorrentSettings::default()
            }],
            ..Settings::default()
        };

        backend
            .save_settings(&next_settings)
            .expect("invalid metadata should be overwritten");

        let saved_settings: Settings =
            read_toml_or_default(&backend.paths.settings_path).expect("reload saved settings");
        let saved_metadata: TorrentMetadataConfig =
            read_toml_or_default(&backend.paths.metadata_path).expect("load rewritten metadata");

        assert_eq!(saved_settings.client_id, "after-invalid-metadata");
        assert_eq!(saved_metadata.torrents.len(), 1);
        assert_eq!(
            saved_metadata.torrents[0].info_hash_hex,
            "1111111111111111111111111111111111111111"
        );
        assert_eq!(saved_metadata.torrents[0].torrent_name, "Sample Node");
        assert_eq!(
            saved_metadata.torrents[0].file_priorities.get(&1),
            Some(&FilePriority::Skip)
        );
        assert!(saved_metadata.torrents[0].files.is_empty());
    }

    #[test]
    fn test_upsert_torrent_metadata_overwrites_invalid_metadata() {
        let dir = tempdir().expect("create tempdir");
        let backend = NormalConfigBackend {
            paths: NormalConfigPaths {
                settings_path: dir.path().join("settings.toml"),
                metadata_path: dir.path().join("torrent_metadata.toml"),
                backup_dir: dir.path().join("backups_settings_files"),
            },
        };
        let invalid_metadata = "schema_version = 1\n[[torrents]]\ninfo_hash_hex = \"1111111111111111111111111111111111111111\"\n[torrents.file_priorities]\n[torrents.file_priorities]\n";
        write_string_atomically(&backend.paths.metadata_path, invalid_metadata)
            .expect("write invalid metadata");

        ConfigBackend::Normal(backend.clone())
            .upsert_torrent_metadata(TorrentMetadataEntry {
                info_hash_hex: "2222222222222222222222222222222222222222".to_string(),
                torrent_name: "Queued Sample".to_string(),
                ..TorrentMetadataEntry::default()
            })
            .expect("invalid metadata should be overwritten on upsert");

        let saved_metadata: TorrentMetadataConfig =
            read_toml_or_default(&backend.paths.metadata_path).expect("load rewritten metadata");
        assert_eq!(saved_metadata.torrents.len(), 1);
        assert_eq!(
            saved_metadata.torrents[0].info_hash_hex,
            "2222222222222222222222222222222222222222"
        );
        assert_eq!(saved_metadata.torrents[0].torrent_name, "Queued Sample");
    }

    fn watch_env_guard() -> &'static std::sync::Mutex<()> {
        shared_env_guard_for_tests()
    }

    fn shared_backend_guard() -> &'static std::sync::Mutex<()> {
        shared_env_guard_for_tests()
    }

    fn set_temp_app_paths() -> tempfile::TempDir {
        let dir = tempdir().expect("create tempdir");
        let config_dir = dir.path().join("config");
        let data_dir = dir.path().join("data");
        set_app_paths_override_for_tests(Some((config_dir, data_dir)));
        dir
    }

    #[test]
    fn test_persisted_shared_config_normalizes_explicit_subdir_to_mount_root() {
        let _guard = shared_backend_guard().lock().unwrap();
        let temp = set_temp_app_paths();
        let explicit_root = temp.path().join("shared-root").join(SHARED_CONFIG_SUBDIR);

        let selection =
            set_persisted_shared_config(&explicit_root).expect("persist shared config path");

        assert_eq!(selection.source, SharedConfigSource::Launcher);
        assert_eq!(selection.mount_root, temp.path().join("shared-root"));
        assert_eq!(selection.config_root, explicit_root);

        let effective = effective_shared_config_selection()
            .expect("resolve effective shared config")
            .expect("shared config enabled");
        assert_eq!(effective, selection);

        set_app_paths_override_for_tests(None);
        clear_shared_config_state();
    }

    #[test]
    fn test_shared_config_env_takes_precedence_over_persisted_launcher_config() {
        let _guard = shared_backend_guard().lock().unwrap();
        let original_shared_dir = env::var_os(SHARED_CONFIG_DIR_ENV);
        let temp = set_temp_app_paths();
        let launcher_root = temp.path().join("launcher-root");
        let env_root = temp.path().join("env-root");

        set_persisted_shared_config(&launcher_root).expect("persist launcher config");
        env::set_var(SHARED_CONFIG_DIR_ENV, &env_root);
        clear_shared_config_state();

        let effective = effective_shared_config_selection()
            .expect("resolve effective shared config")
            .expect("shared config enabled");
        assert_eq!(effective.source, SharedConfigSource::Env);
        assert_eq!(effective.mount_root, env_root);
        assert_eq!(
            effective.config_root,
            temp.path().join("env-root").join(SHARED_CONFIG_SUBDIR)
        );

        if let Some(value) = original_shared_dir {
            env::set_var(SHARED_CONFIG_DIR_ENV, value);
        } else {
            env::remove_var(SHARED_CONFIG_DIR_ENV);
        }
        set_app_paths_override_for_tests(None);
        clear_shared_config_state();
    }

    #[test]
    fn test_clearing_persisted_shared_config_disables_shared_mode_without_env() {
        let _guard = shared_backend_guard().lock().unwrap();
        let temp = set_temp_app_paths();
        let launcher_root = temp.path().join("launcher-root");

        set_persisted_shared_config(&launcher_root).expect("persist launcher config");
        clear_shared_config_state();
        assert!(is_shared_config_mode());

        let cleared = clear_persisted_shared_config().expect("clear launcher config");
        assert!(cleared);
        assert_eq!(
            effective_shared_config_selection().expect("resolve effective shared config"),
            None
        );
        assert!(!is_shared_config_mode());

        set_app_paths_override_for_tests(None);
        clear_shared_config_state();
    }

    #[test]
    fn test_set_persisted_shared_config_rejects_relative_paths() {
        let _guard = shared_backend_guard().lock().unwrap();
        let _temp = set_temp_app_paths();

        let error = set_persisted_shared_config(Path::new("relative/shared-root"))
            .expect_err("relative path should fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("absolute"));

        set_app_paths_override_for_tests(None);
        clear_shared_config_state();
    }

    #[test]
    fn test_persisted_host_id_falls_back_after_env() {
        let _guard = shared_backend_guard().lock().unwrap();
        let temp = set_temp_app_paths();
        let original_host_id = env::var_os(SHARED_HOST_ID_ENV);
        let original_legacy_host_id = env::var_os(LEGACY_SHARED_HOST_ID_ENV);

        set_persisted_host_id("Desk Node").expect("persist host id");
        env::remove_var(SHARED_HOST_ID_ENV);
        env::remove_var(LEGACY_SHARED_HOST_ID_ENV);

        let selection = effective_host_id_selection().expect("resolve host id");
        assert_eq!(selection.host_id, "desk-node");
        assert_eq!(selection.source, HostIdSource::Launcher);
        assert_eq!(
            persisted_host_id_path().expect("persisted host id path"),
            temp.path().join("config").join(LAUNCHER_HOST_ID_FILE)
        );

        if let Some(value) = original_host_id {
            env::set_var(SHARED_HOST_ID_ENV, value);
        } else {
            env::remove_var(SHARED_HOST_ID_ENV);
        }
        if let Some(value) = original_legacy_host_id {
            env::set_var(LEGACY_SHARED_HOST_ID_ENV, value);
        } else {
            env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        }
        clear_persisted_host_id().expect("clear host id");
        set_app_paths_override_for_tests(None);
        clear_shared_config_state();
    }

    #[test]
    fn test_host_id_env_takes_precedence_over_persisted_host_id() {
        let _guard = shared_backend_guard().lock().unwrap();
        let _temp = set_temp_app_paths();
        let original_host_id = env::var_os(SHARED_HOST_ID_ENV);
        let original_legacy_host_id = env::var_os(LEGACY_SHARED_HOST_ID_ENV);

        set_persisted_host_id("desk-node").expect("persist host id");
        env::set_var(SHARED_HOST_ID_ENV, "travel-node");
        env::remove_var(LEGACY_SHARED_HOST_ID_ENV);

        let selection = effective_host_id_selection().expect("resolve host id");
        assert_eq!(selection.host_id, "travel-node");
        assert_eq!(selection.source, HostIdSource::Env);

        if let Some(value) = original_host_id {
            env::set_var(SHARED_HOST_ID_ENV, value);
        } else {
            env::remove_var(SHARED_HOST_ID_ENV);
        }
        if let Some(value) = original_legacy_host_id {
            env::set_var(LEGACY_SHARED_HOST_ID_ENV, value);
        } else {
            env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        }
        clear_persisted_host_id().expect("clear host id");
        set_app_paths_override_for_tests(None);
        clear_shared_config_state();
    }

    #[test]
    fn test_convert_standalone_to_shared_and_back_round_trips_settings() {
        let _guard = shared_backend_guard().lock().unwrap();
        let temp = set_temp_app_paths();
        let shared_root = temp.path().join("shared-root");
        let original_shared_dir = env::var_os(SHARED_CONFIG_DIR_ENV);
        let original_host_id = env::var_os(SHARED_HOST_ID_ENV);
        let original_legacy_host_id = env::var_os(LEGACY_SHARED_HOST_ID_ENV);

        env::remove_var(SHARED_CONFIG_DIR_ENV);
        env::set_var(SHARED_HOST_ID_ENV, "node-a");
        env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        clear_shared_config_state();

        let standalone_settings = Settings {
            client_id: "standalone-node".to_string(),
            client_port: 7788,
            watch_folder: Some(PathBuf::from("/watch-local")),
            default_download_folder: Some(shared_root.join("downloads")),
            torrents: vec![TorrentSettings {
                torrent_or_magnet: shared_root
                    .join(SHARED_CONFIG_SUBDIR)
                    .join("torrents")
                    .join("1111111111111111111111111111111111111111.torrent")
                    .to_string_lossy()
                    .to_string(),
                name: "Sample Convert".to_string(),
                download_path: Some(shared_root.join("downloads").join("alpha")),
                ..TorrentSettings::default()
            }],
            ..Settings::default()
        };
        let normal_backend = local_normal_backend().expect("local backend");
        normal_backend
            .save_settings(&standalone_settings)
            .expect("save standalone settings");
        let local_metadata = TorrentMetadataConfig {
            torrents: vec![TorrentMetadataEntry {
                info_hash_hex: "1111111111111111111111111111111111111111".to_string(),
                torrent_name: "Sample Convert".to_string(),
                total_size: 123,
                is_multi_file: true,
                files: vec![TorrentMetadataFileEntry {
                    relative_path: "alpha.bin".to_string(),
                    length: 123,
                }],
                file_priorities: HashMap::new(),
            }],
        };
        let _ = write_toml_atomically_with_fingerprint(
            &normal_backend.paths.metadata_path,
            &local_metadata,
        )
        .expect("write local metadata");

        let selection = convert_standalone_to_shared(&shared_root).expect("convert to shared");
        assert_eq!(selection.mount_root, shared_root);
        let shared_backend = shared_backend_for_mount_root(&shared_root).expect("shared backend");
        let shared_settings = shared_backend
            .load_settings()
            .expect("load shared settings");
        assert_eq!(shared_settings.client_id, "standalone-node");
        assert_eq!(shared_settings.client_port, 7788);
        assert_eq!(
            shared_settings.watch_folder,
            Some(PathBuf::from("/watch-local"))
        );
        assert_eq!(
            shared_settings.default_download_folder,
            Some(shared_root.join("downloads"))
        );
        assert!(shared_backend.paths.host_path.exists());
        assert!(shared_backend.paths.settings_path.exists());
        assert!(shared_backend.paths.catalog_path.exists());

        env::set_var(SHARED_CONFIG_DIR_ENV, &shared_root);
        clear_shared_config_state();
        convert_shared_to_standalone().expect("convert to standalone");
        let reloaded_local = normal_backend
            .load_settings()
            .expect("reload standalone settings");
        let reloaded_metadata: TorrentMetadataConfig =
            read_toml_or_default(&normal_backend.paths.metadata_path).expect("reload metadata");

        assert_eq!(reloaded_local.client_id, "standalone-node");
        assert_eq!(reloaded_local.client_port, 7788);
        assert_eq!(
            reloaded_local.watch_folder,
            Some(PathBuf::from("/watch-local"))
        );
        assert_eq!(
            reloaded_local.default_download_folder,
            Some(shared_root.join("downloads"))
        );
        assert_eq!(reloaded_local.torrents.len(), 1);
        assert_eq!(reloaded_metadata.torrents, local_metadata.torrents);

        if let Some(value) = original_shared_dir {
            env::set_var(SHARED_CONFIG_DIR_ENV, value);
        } else {
            env::remove_var(SHARED_CONFIG_DIR_ENV);
        }
        if let Some(value) = original_host_id {
            env::set_var(SHARED_HOST_ID_ENV, value);
        } else {
            env::remove_var(SHARED_HOST_ID_ENV);
        }
        if let Some(value) = original_legacy_host_id {
            env::set_var(LEGACY_SHARED_HOST_ID_ENV, value);
        } else {
            env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        }
        clear_persisted_host_id().ok();
        set_app_paths_override_for_tests(None);
        clear_shared_config_state();
    }

    #[test]
    fn test_configured_watch_paths_use_shared_inbox_in_shared_mode() {
        let _guard = watch_env_guard().lock().unwrap();
        let original_shared_dir = env::var_os(SHARED_CONFIG_DIR_ENV);
        let original_host_id = env::var_os(SHARED_HOST_ID_ENV);
        let original_legacy_host_id = env::var_os(LEGACY_SHARED_HOST_ID_ENV);
        let dir = tempdir().expect("create tempdir");

        env::set_var(SHARED_CONFIG_DIR_ENV, dir.path());
        env::set_var(SHARED_HOST_ID_ENV, "node-a");
        env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        clear_shared_config_state();

        let explicit_watch = PathBuf::from("/host-watch");
        let settings = Settings {
            watch_folder: Some(explicit_watch.clone()),
            ..Settings::default()
        };
        let configured = configured_watch_paths(&settings);
        let effective_root = dir.path().join(SHARED_CONFIG_SUBDIR);

        assert!(configured.contains(&effective_root.join("inbox")));
        assert!(configured.contains(&explicit_watch));
        assert_eq!(
            resolve_command_watch_path(&settings),
            Some(effective_root.join("inbox"))
        );

        if let Some(value) = original_shared_dir {
            env::set_var(SHARED_CONFIG_DIR_ENV, value);
        } else {
            env::remove_var(SHARED_CONFIG_DIR_ENV);
        }
        if let Some(value) = original_host_id {
            env::set_var(SHARED_HOST_ID_ENV, value);
        } else {
            env::remove_var(SHARED_HOST_ID_ENV);
        }
        if let Some(value) = original_legacy_host_id {
            env::set_var(LEGACY_SHARED_HOST_ID_ENV, value);
        } else {
            env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        }
        clear_shared_config_state();
    }

    #[test]
    fn test_shared_host_id_prefers_canonical_env_var() {
        let _guard = watch_env_guard().lock().unwrap();
        let original_shared_dir = env::var_os(SHARED_CONFIG_DIR_ENV);
        let original_host_id = env::var_os(SHARED_HOST_ID_ENV);
        let original_legacy_host_id = env::var_os(LEGACY_SHARED_HOST_ID_ENV);
        let dir = tempdir().expect("create tempdir");

        env::set_var(SHARED_CONFIG_DIR_ENV, dir.path());
        env::set_var(SHARED_HOST_ID_ENV, "canonical-node");
        env::set_var(LEGACY_SHARED_HOST_ID_ENV, "legacy-node");
        clear_shared_config_state();

        assert_eq!(shared_host_id().as_deref(), Some("canonical-node"));

        if let Some(value) = original_shared_dir {
            env::set_var(SHARED_CONFIG_DIR_ENV, value);
        } else {
            env::remove_var(SHARED_CONFIG_DIR_ENV);
        }
        if let Some(value) = original_host_id {
            env::set_var(SHARED_HOST_ID_ENV, value);
        } else {
            env::remove_var(SHARED_HOST_ID_ENV);
        }
        if let Some(value) = original_legacy_host_id {
            env::set_var(LEGACY_SHARED_HOST_ID_ENV, value);
        } else {
            env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        }
        clear_shared_config_state();
    }

    #[test]
    fn test_shared_config_dir_env_normalizes_to_superseedr_config_subdir() {
        let _guard = watch_env_guard().lock().unwrap();
        let original_shared_dir = env::var_os(SHARED_CONFIG_DIR_ENV);
        let original_host_id = env::var_os(SHARED_HOST_ID_ENV);
        let original_legacy_host_id = env::var_os(LEGACY_SHARED_HOST_ID_ENV);
        let dir = tempdir().expect("create tempdir");

        env::set_var(SHARED_CONFIG_DIR_ENV, dir.path());
        env::set_var(SHARED_HOST_ID_ENV, "node-a");
        env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        clear_shared_config_state();

        let expected_root = dir.path().join(SHARED_CONFIG_SUBDIR);
        assert_eq!(shared_root_path(), Some(expected_root.clone()));
        assert_eq!(shared_inbox_path(), Some(expected_root.join("inbox")));
        assert_eq!(
            shared_host_dir(),
            Some(expected_root.join("hosts").join("node-a"))
        );
        assert_eq!(
            shared_status_path(),
            Some(
                expected_root
                    .join("hosts")
                    .join("node-a")
                    .join("status.json")
            )
        );
        assert_eq!(
            runtime_data_dir(),
            Some(expected_root.join("hosts").join("node-a"))
        );

        if let Some(value) = original_shared_dir {
            env::set_var(SHARED_CONFIG_DIR_ENV, value);
        } else {
            env::remove_var(SHARED_CONFIG_DIR_ENV);
        }
        if let Some(value) = original_host_id {
            env::set_var(SHARED_HOST_ID_ENV, value);
        } else {
            env::remove_var(SHARED_HOST_ID_ENV);
        }
        if let Some(value) = original_legacy_host_id {
            env::set_var(LEGACY_SHARED_HOST_ID_ENV, value);
        } else {
            env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        }
        clear_shared_config_state();
    }

    #[test]
    fn test_shared_config_dir_env_accepts_explicit_superseedr_config_subdir() {
        let _guard = watch_env_guard().lock().unwrap();
        let original_shared_dir = env::var_os(SHARED_CONFIG_DIR_ENV);
        let original_host_id = env::var_os(SHARED_HOST_ID_ENV);
        let original_legacy_host_id = env::var_os(LEGACY_SHARED_HOST_ID_ENV);
        let dir = tempdir().expect("create tempdir");
        let explicit_root = dir.path().join(SHARED_CONFIG_SUBDIR);

        env::set_var(SHARED_CONFIG_DIR_ENV, &explicit_root);
        env::set_var(SHARED_HOST_ID_ENV, "node-a");
        env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        clear_shared_config_state();

        assert_eq!(shared_root_path(), Some(explicit_root.clone()));
        assert_eq!(shared_inbox_path(), Some(explicit_root.join("inbox")));

        if let Some(value) = original_shared_dir {
            env::set_var(SHARED_CONFIG_DIR_ENV, value);
        } else {
            env::remove_var(SHARED_CONFIG_DIR_ENV);
        }
        if let Some(value) = original_host_id {
            env::set_var(SHARED_HOST_ID_ENV, value);
        } else {
            env::remove_var(SHARED_HOST_ID_ENV);
        }
        if let Some(value) = original_legacy_host_id {
            env::set_var(LEGACY_SHARED_HOST_ID_ENV, value);
        } else {
            env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        }
        clear_shared_config_state();
    }

    #[test]
    fn test_classify_shared_mode_settings_change_scopes_host_only_changes() {
        let current = Settings {
            client_id: "node-a".to_string(),
            client_port: 4100,
            watch_folder: Some(PathBuf::from("/watch-a")),
            default_download_folder: Some(PathBuf::from("/shared-downloads")),
            ..Settings::default()
        };

        let mut host_only = current.clone();
        host_only.client_port = 4200;
        host_only.watch_folder = Some(PathBuf::from("/watch-b"));
        assert_eq!(
            classify_shared_mode_settings_change(&current, &host_only),
            SettingsChangeScope::HostOnly
        );

        let mut shared_change = current.clone();
        shared_change.default_download_folder = Some(PathBuf::from("/shared-next"));
        assert_eq!(
            classify_shared_mode_settings_change(&current, &shared_change),
            SettingsChangeScope::SharedOrMixed
        );

        assert_eq!(
            classify_shared_mode_settings_change(&current, &current),
            SettingsChangeScope::NoChange
        );
    }

    #[test]
    fn test_runtime_watch_paths_differ_by_shared_role() {
        let _guard = watch_env_guard().lock().unwrap();
        let original_shared_dir = env::var_os(SHARED_CONFIG_DIR_ENV);
        let original_host_id = env::var_os(SHARED_HOST_ID_ENV);
        let original_legacy_host_id = env::var_os(LEGACY_SHARED_HOST_ID_ENV);
        let dir = tempdir().expect("create tempdir");

        env::set_var(SHARED_CONFIG_DIR_ENV, dir.path());
        env::set_var(SHARED_HOST_ID_ENV, "node-a");
        env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        clear_shared_config_state();

        let settings = Settings {
            watch_folder: Some(PathBuf::from("/host-watch")),
            ..Settings::default()
        };
        let effective_root = dir.path().join(SHARED_CONFIG_SUBDIR);

        let follower_paths = runtime_watch_paths(&settings, true, false);
        assert!(follower_paths.contains(&PathBuf::from("/host-watch")));
        assert!(follower_paths.contains(&effective_root));
        assert!(!follower_paths.contains(&effective_root.join("inbox")));

        let leader_paths = runtime_watch_paths(&settings, true, true);
        assert!(leader_paths.contains(&effective_root.join("inbox")));

        if let Some(value) = original_shared_dir {
            env::set_var(SHARED_CONFIG_DIR_ENV, value);
        } else {
            env::remove_var(SHARED_CONFIG_DIR_ENV);
        }
        if let Some(value) = original_host_id {
            env::set_var(SHARED_HOST_ID_ENV, value);
        } else {
            env::remove_var(SHARED_HOST_ID_ENV);
        }
        if let Some(value) = original_legacy_host_id {
            env::set_var(LEGACY_SHARED_HOST_ID_ENV, value);
        } else {
            env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        }
        clear_shared_config_state();
    }

    #[test]
    fn test_resolve_host_watch_path_falls_back_to_local_app_watch_directory() {
        let _guard = watch_env_guard().lock().unwrap();
        let _temp = set_temp_app_paths();
        let settings = Settings::default();
        let expected_watch = get_watch_path().map(|(watch_path, _)| watch_path);

        assert_eq!(resolve_host_watch_path(&settings), expected_watch);
        set_app_paths_override_for_tests(None);
    }

    #[test]
    fn test_shared_runtime_watch_paths_include_local_app_watch_when_host_watch_unset() {
        let _guard = watch_env_guard().lock().unwrap();
        let original_shared_dir = env::var_os(SHARED_CONFIG_DIR_ENV);
        let original_host_id = env::var_os(SHARED_HOST_ID_ENV);
        let original_legacy_host_id = env::var_os(LEGACY_SHARED_HOST_ID_ENV);
        let dir = tempdir().expect("create tempdir");

        env::set_var(SHARED_CONFIG_DIR_ENV, dir.path());
        env::set_var(SHARED_HOST_ID_ENV, "node-a");
        env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        clear_shared_config_state();

        let settings = Settings::default();
        let effective_root = dir.path().join(SHARED_CONFIG_SUBDIR);
        let local_watch = get_watch_path().map(|(watch_path, _)| watch_path);

        let follower_paths = runtime_watch_paths(&settings, true, false);
        assert!(follower_paths.contains(&effective_root));
        assert!(!follower_paths.contains(&effective_root.join("inbox")));
        if let Some(local_watch) = &local_watch {
            assert!(follower_paths.contains(local_watch));
        }

        let leader_paths = runtime_watch_paths(&settings, true, true);
        assert!(leader_paths.contains(&effective_root.join("inbox")));
        if let Some(local_watch) = &local_watch {
            assert!(leader_paths.contains(local_watch));
        }

        if let Some(value) = original_shared_dir {
            env::set_var(SHARED_CONFIG_DIR_ENV, value);
        } else {
            env::remove_var(SHARED_CONFIG_DIR_ENV);
        }
        if let Some(value) = original_host_id {
            env::set_var(SHARED_HOST_ID_ENV, value);
        } else {
            env::remove_var(SHARED_HOST_ID_ENV);
        }
        if let Some(value) = original_legacy_host_id {
            env::set_var(LEGACY_SHARED_HOST_ID_ENV, value);
        } else {
            env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        }
        clear_shared_config_state();
    }
}
