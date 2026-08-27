// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use chrono::{DateTime, Local, TimeZone};
use sha1::{Digest, Sha1};
use tracing::{event as tracing_event, Level};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::app::TorrentControlState;
use crate::app::{DataRate, FilePriority};
use crate::fs_atomic::{
    deserialize_versioned_toml, serialize_versioned_toml, write_string_atomically,
    write_toml_atomically,
};
use crate::networking::NetworkBindingConfig;
use crate::theme::ThemeName;

use strum_macros::EnumCount;
use strum_macros::EnumIter;

pub const UNLIMITED_RATE_LIMIT_BPS: u64 = i64::MAX as u64;

pub fn is_unlimited_rate_limit_bps(limit_bps: u64) -> bool {
    limit_bps == 0 || limit_bps >= UNLIMITED_RATE_LIMIT_BPS
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default, EnumIter, EnumCount)]
pub enum TorrentSortColumn {
    Name,
    #[default]
    Up,
    Down,
    Progress,
}

impl TorrentSortColumn {
    pub fn default_direction(self) -> SortDirection {
        match self {
            Self::Name => SortDirection::Ascending,
            Self::Up | Self::Down => SortDirection::Descending,
            Self::Progress => SortDirection::Ascending,
        }
    }
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

impl PeerSortColumn {
    pub fn default_direction(self) -> SortDirection {
        match self {
            Self::Address | Self::Client | Self::Action => SortDirection::Ascending,
            Self::Flags | Self::Completed | Self::UL | Self::DL => SortDirection::Descending,
        }
    }
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

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, EnumIter)]
#[serde(rename_all = "lowercase")]
pub enum UiLayoutMode {
    #[default]
    Auto,
    Horizontal,
    #[serde(alias = "veritical")]
    Vertical,
    Square,
}

impl UiLayoutMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Vertical => "vertical",
            Self::Square => "square",
            Self::Horizontal => "horizontal",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Horizontal,
            Self::Horizontal => Self::Vertical,
            Self::Vertical => Self::Square,
            Self::Square => Self::Auto,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::Auto => Self::Square,
            Self::Horizontal => Self::Auto,
            Self::Vertical => Self::Horizontal,
            Self::Square => Self::Vertical,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub client_id: String,
    #[serde(
        deserialize_with = "deserialize_client_port",
        serialize_with = "serialize_client_port"
    )]
    pub client_port: u16,
    #[serde(skip)]
    pub randomize_client_port: bool,
    pub network_binding: NetworkBindingConfig,
    pub torrents: Vec<TorrentSettings>,
    pub lifetime_downloaded: u64,
    pub lifetime_uploaded: u64,
    pub private_client: bool,
    pub torrent_sort_column: TorrentSortColumn,
    pub torrent_sort_direction: SortDirection,
    pub torrent_sort_pinned: bool,
    pub peer_sort_column: PeerSortColumn,
    pub peer_sort_direction: SortDirection,
    pub peer_sort_pinned: bool,
    pub ui_theme: ThemeName,
    #[serde(alias = "layout")]
    pub ui_layout_mode: UiLayoutMode,
    pub ui_refresh_rate: DataRate,
    pub watch_folder: Option<PathBuf>,
    pub default_download_folder: Option<PathBuf>,
    pub always_show_add_location_prompt: bool,
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
            randomize_client_port: false,
            network_binding: NetworkBindingConfig::default(),
            torrents: Vec::new(),
            watch_folder: None,
            default_download_folder: None,
            lifetime_downloaded: 0,
            lifetime_uploaded: 0,
            private_client: false,
            global_download_limit_bps: UNLIMITED_RATE_LIMIT_BPS,
            global_upload_limit_bps: UNLIMITED_RATE_LIMIT_BPS,
            torrent_sort_column: TorrentSortColumn::default(),
            torrent_sort_direction: TorrentSortColumn::default().default_direction(),
            torrent_sort_pinned: false,
            peer_sort_column: PeerSortColumn::default(),
            peer_sort_direction: PeerSortColumn::default().default_direction(),
            peer_sort_pinned: false,
            ui_theme: ThemeName::default(),
            ui_layout_mode: UiLayoutMode::default(),
            ui_refresh_rate: DataRate::default(),
            always_show_add_location_prompt: false,
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
    pub added_at_unix_secs: Option<u64>,
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
const CLIENT_PORT_ENV: &str = "SUPERSEEDR_CLIENT_PORT";
const PORT_ENV: &str = "PORT";
const DEFAULT_DOWNLOAD_FOLDER_ENV: &str = "SUPERSEEDR_DEFAULT_DOWNLOAD_FOLDER";
const OUTPUT_STATUS_INTERVAL_ENV: &str = "SUPERSEEDR_OUTPUT_STATUS_INTERVAL";
const EXTRA_WATCH_PATH_PREFIX: &str = "SUPERSEEDR_WATCH_PATH_";
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
    CurrentDirectory,
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
    pub added_at_unix_secs: Option<u64>,
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
    pub torrent_sort_pinned: bool,
    pub peer_sort_column: PeerSortColumn,
    pub peer_sort_direction: SortDirection,
    pub peer_sort_pinned: bool,
    pub ui_theme: ThemeName,
    #[serde(alias = "layout")]
    pub ui_layout_mode: UiLayoutMode,
    pub ui_refresh_rate: DataRate,
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
            torrent_sort_pinned: settings.torrent_sort_pinned,
            peer_sort_column: settings.peer_sort_column,
            peer_sort_direction: settings.peer_sort_direction,
            peer_sort_pinned: settings.peer_sort_pinned,
            ui_theme: settings.ui_theme,
            ui_layout_mode: settings.ui_layout_mode,
            ui_refresh_rate: settings.ui_refresh_rate,
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
    #[serde(
        deserialize_with = "deserialize_client_port",
        serialize_with = "serialize_client_port"
    )]
    pub client_port: u16,
    pub network_binding: NetworkBindingConfig,
    pub watch_folder: Option<PathBuf>,
    pub always_show_add_location_prompt: bool,
}

impl Default for HostConfig {
    fn default() -> Self {
        let settings = Settings::default();
        Self {
            client_id: None,
            client_port: settings.client_port,
            network_binding: settings.network_binding,
            watch_folder: settings.watch_folder,
            always_show_add_location_prompt: settings.always_show_add_location_prompt,
        }
    }
}
#[derive(Clone, Debug)]
struct NormalConfigPaths {
    settings_path: PathBuf,
    metadata_path: PathBuf,
    backup_dir: PathBuf,
    data_dir: PathBuf,
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct LoggedSharedConfigRevision {
    root_dir: PathBuf,
    host_id: String,
    revision: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SharedCatalogBackupPolicy {
    cadence_hours: i64,
    retained_backups: usize,
}

#[derive(Clone, Debug)]
enum ConfigBackend {
    Normal(NormalConfigBackend),
    Shared(SharedConfigBackend),
}

static LOGGED_SHARED_CONFIG_REVISION: OnceLock<Mutex<Option<LoggedSharedConfigRevision>>> =
    OnceLock::new();
#[cfg(not(test))]
static RETAINED_SHARED_CONFIG_SELECTION: OnceLock<Mutex<Option<Option<SharedConfigSelection>>>> =
    OnceLock::new();

#[cfg(test)]
thread_local! {
    static RETAINED_SHARED_CONFIG_SELECTION: std::cell::RefCell<
        Option<Option<SharedConfigSelection>>,
    > = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
static APP_PATHS_OVERRIDE: OnceLock<Mutex<Option<(PathBuf, PathBuf)>>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static HUMAN_BACKUP_ROOT_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
static SHARED_ENV_TEST_GUARD: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(test)]
fn app_paths_override() -> &'static Mutex<Option<(PathBuf, PathBuf)>> {
    APP_PATHS_OVERRIDE.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn human_backup_root_override() -> Option<PathBuf> {
    HUMAN_BACKUP_ROOT_OVERRIDE.with(|override_path| override_path.borrow().clone())
}

#[cfg(test)]
pub(crate) fn shared_env_guard_for_tests() -> &'static Mutex<()> {
    SHARED_ENV_TEST_GUARD.get_or_init(|| Mutex::new(()))
}

fn logged_shared_config_revision() -> &'static Mutex<Option<LoggedSharedConfigRevision>> {
    LOGGED_SHARED_CONFIG_REVISION.get_or_init(|| Mutex::new(None))
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
            added_at_unix_secs: settings.added_at_unix_secs,
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
            added_at_unix_secs: self.added_at_unix_secs,
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
        if settings.torrent_or_magnet.starts_with("magnet:") {
            return Some(magnet_metadata_placeholder_from_settings(
                settings, info_hash,
            ));
        }

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

fn magnet_metadata_placeholder_from_settings(
    settings: &TorrentSettings,
    info_hash: Vec<u8>,
) -> TorrentMetadataEntry {
    let display_name = extract_magnet_query_value(&settings.torrent_or_magnet, "dn")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| settings.name.clone());
    let length = extract_magnet_query_value(&settings.torrent_or_magnet, "xl")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_default();
    let files = if !display_name.is_empty() && length > 0 {
        vec![TorrentMetadataFileEntry {
            relative_path: normalize_magnet_file_name(&display_name),
            length,
        }]
    } else {
        Vec::new()
    };

    TorrentMetadataEntry {
        info_hash_hex: hex::encode(info_hash),
        torrent_name: display_name,
        total_size: length,
        is_multi_file: false,
        files,
        file_priorities: settings.file_priorities.clone(),
    }
}

fn extract_magnet_query_value(magnet_link: &str, target_key: &str) -> Option<String> {
    for raw_part in magnet_link.split('&') {
        let part = raw_part.strip_prefix("magnet:?").unwrap_or(raw_part);
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if key.eq_ignore_ascii_case(target_key) {
            let value_for_decode = value.replace('+', "%20");
            if let Ok(decoded) = urlencoding::decode(&value_for_decode) {
                let value = decoded.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

fn normalize_magnet_file_name(name: &str) -> String {
    name.replace('\\', "/")
        .split('/')
        .filter(|segment| {
            let segment = segment.trim();
            !segment.is_empty() && segment != "." && segment != ".."
        })
        .collect::<Vec<_>>()
        .join("/")
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
            torrent_sort_pinned: settings.torrent_sort_pinned,
            peer_sort_column: settings.peer_sort_column,
            peer_sort_direction: settings.peer_sort_direction,
            peer_sort_pinned: settings.peer_sort_pinned,
            ui_theme: settings.ui_theme,
            ui_layout_mode: settings.ui_layout_mode,
            ui_refresh_rate: settings.ui_refresh_rate,
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
        settings.torrent_sort_pinned = self.torrent_sort_pinned;
        settings.peer_sort_column = self.peer_sort_column;
        settings.peer_sort_direction = self.peer_sort_direction;
        settings.peer_sort_pinned = self.peer_sort_pinned;
        settings.ui_theme = self.ui_theme;
        settings.ui_layout_mode = self.ui_layout_mode;
        settings.ui_refresh_rate = self.ui_refresh_rate;
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
            client_port: configured_client_port(settings),
            network_binding: settings.network_binding.clone(),
            watch_folder: settings.watch_folder.clone(),
            always_show_add_location_prompt: settings.always_show_add_location_prompt,
        }
    }

    fn from_settings(settings: &Settings, shared_client_id: &str) -> Self {
        Self {
            client_id: (settings.client_id != shared_client_id).then(|| settings.client_id.clone()),
            client_port: configured_client_port(settings),
            network_binding: settings.network_binding.clone(),
            watch_folder: settings.watch_folder.clone(),
            always_show_add_location_prompt: settings.always_show_add_location_prompt,
        }
    }

    fn apply_to_settings(&self, settings: &mut Settings) {
        if let Some(client_id) = &self.client_id {
            settings.client_id = client_id.clone();
        }
        settings.client_port = self.client_port;
        settings.randomize_client_port = self.client_port == 0;
        settings.network_binding = self.network_binding.clone();
        settings.watch_folder = self.watch_folder.clone();
        settings.always_show_add_location_prompt = self.always_show_add_location_prompt;
    }
}

fn configured_client_port(settings: &Settings) -> u16 {
    if settings.randomize_client_port {
        0
    } else {
        settings.client_port
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

fn shared_config_selection(source: SharedConfigSource, path: PathBuf) -> SharedConfigSelection {
    let (mount_root, config_root) = resolve_shared_mount_and_config_root(path);
    SharedConfigSelection {
        source,
        mount_root,
        config_root,
    }
}

fn shared_config_from_current_dir(current_dir: &Path) -> Option<SharedConfigSelection> {
    let points_to_config_root = current_dir
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(SHARED_CONFIG_SUBDIR));
    let contains_config_root = current_dir.join(SHARED_CONFIG_SUBDIR).is_dir();

    (points_to_config_root || contains_config_root).then(|| {
        shared_config_selection(
            SharedConfigSource::CurrentDirectory,
            current_dir.to_path_buf(),
        )
    })
}

fn resolve_shared_config_selection_from_sources(
    env_path: Option<PathBuf>,
    current_dir: Option<&Path>,
    launcher_path: Option<PathBuf>,
) -> Option<SharedConfigSelection> {
    if let Some(path) = env_path {
        return Some(shared_config_selection(SharedConfigSource::Env, path));
    }

    if let Some(selection) = current_dir.and_then(shared_config_from_current_dir) {
        return Some(selection);
    }

    launcher_path.map(|path| shared_config_selection(SharedConfigSource::Launcher, path))
}

fn resolve_shared_config_selection_retaining_initial_decision(
    retained_selection: &mut Option<Option<SharedConfigSelection>>,
    env_path: Option<PathBuf>,
    current_dir: Option<&Path>,
    launcher_path: Option<PathBuf>,
) -> Option<SharedConfigSelection> {
    if let Some(selection) = retained_selection.as_ref() {
        return selection.clone();
    }

    let selection =
        resolve_shared_config_selection_from_sources(env_path, current_dir, launcher_path);
    *retained_selection = Some(selection.clone());
    selection
}

#[cfg(not(test))]
fn with_retained_shared_config_selection<T>(
    resolve: impl FnOnce(&mut Option<Option<SharedConfigSelection>>) -> T,
) -> io::Result<T> {
    let mut retained_selection = RETAINED_SHARED_CONFIG_SELECTION
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| io::Error::other("retained shared config selection lock poisoned"))?;
    Ok(resolve(&mut retained_selection))
}

#[cfg(test)]
fn with_retained_shared_config_selection<T>(
    resolve: impl FnOnce(&mut Option<Option<SharedConfigSelection>>) -> T,
) -> io::Result<T> {
    RETAINED_SHARED_CONFIG_SELECTION
        .with(|retained_selection| Ok(resolve(&mut retained_selection.borrow_mut())))
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
    let env_path = env_var_os_case_insensitive(SHARED_CONFIG_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map(expand_home_path)
        .map(absolutize_env_path);
    let current_dir = env::current_dir().ok();
    let launcher_path = load_launcher_shared_config().ok().flatten();
    with_retained_shared_config_selection(|retained_selection| {
        resolve_shared_config_selection_retaining_initial_decision(
            retained_selection,
            env_path,
            current_dir.as_deref(),
            launcher_path,
        )
    })
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
    let explicit_host_id = env_var_os_case_insensitive(SHARED_HOST_ID_ENV)
        .and_then(|value| value.into_string().ok())
        .or_else(|| {
            env_var_os_case_insensitive(LEGACY_SHARED_HOST_ID_ENV)
                .and_then(|value| value.into_string().ok())
        });
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

    let (config_dir, data_dir) = get_app_paths().ok_or_else(|| {
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
            data_dir,
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
        if let Some(relative) =
            strip_windows_prefix_case_insensitive(&normalized_path, &normalized_root)
        {
            return Ok(relative);
        }
    }

    Err(())
}

#[cfg(windows)]
fn path_without_verbatim_prefix(path: &Path) -> PathBuf {
    let raw = path.as_os_str().to_string_lossy();
    if let Some(stripped) = raw.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{}", stripped));
    }
    if let Some(stripped) = raw.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        path.to_path_buf()
    }
}

#[cfg(windows)]
fn strip_windows_prefix_case_insensitive(path: &Path, root: &Path) -> Option<PathBuf> {
    let mut path_components = path.components();
    for root_component in root.components() {
        let path_component = path_components.next()?;
        if !component_eq_ignore_ascii_case(path_component, root_component) {
            return None;
        }
    }

    let mut relative = PathBuf::new();
    for component in path_components {
        match component {
            Component::Normal(segment) => relative.push(segment),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(relative)
}

#[cfg(windows)]
fn component_eq_ignore_ascii_case(left: Component<'_>, right: Component<'_>) -> bool {
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
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
    let mut resolved = settings.clone();

    if resolved.client_port == 0 {
        resolved.randomize_client_port = true;
    }
    if let Some(client_port) = parse_client_port_env_override()? {
        match client_port {
            Some(client_port) => {
                resolved.client_port = client_port;
                resolved.randomize_client_port = false;
            }
            None => resolved.randomize_client_port = true,
        }
    }
    if let Some(default_download_folder) = parse_path_env_override(DEFAULT_DOWNLOAD_FOLDER_ENV)? {
        resolved.default_download_folder = Some(default_download_folder);
    }
    if let Some(output_status_interval) = parse_env_override(OUTPUT_STATUS_INTERVAL_ENV)? {
        resolved.output_status_interval = output_status_interval;
    }

    Ok(resolved)
}

fn parse_client_port_env_override() -> io::Result<Option<Option<u16>>> {
    if let Some(value) = env_var_case_insensitive(CLIENT_PORT_ENV)? {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{CLIENT_PORT_ENV} must not be empty"),
            ));
        }
        if trimmed.eq_ignore_ascii_case("random") {
            return Ok(Some(None));
        }
        return trimmed
            .parse::<u16>()
            .map(|port| Some(Some(port)))
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Invalid {CLIENT_PORT_ENV}={value:?}: expected a port number or RANDOM: {error}"
                    ),
                )
            });
    }

    if env_var_case_insensitive(PORT_ENV)?
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("random"))
    {
        return Ok(Some(None));
    }

    Ok(None)
}

fn deserialize_client_port<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ClientPortValue {
        Number(u16),
        Text(String),
    }

    match ClientPortValue::deserialize(deserializer)? {
        ClientPortValue::Number(port) => Ok(port),
        ClientPortValue::Text(value) if value.trim().eq_ignore_ascii_case("random") => Ok(0),
        ClientPortValue::Text(value) => value.trim().parse::<u16>().map_err(|error| {
            serde::de::Error::custom(format!(
                "invalid client_port {value:?}: expected a port number or RANDOM: {error}"
            ))
        }),
    }
}

fn serialize_client_port<S>(port: &u16, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if *port == 0 {
        serializer.serialize_str("RANDOM")
    } else {
        serializer.serialize_u16(*port)
    }
}

fn parse_env_override<T>(key: &str) -> io::Result<Option<T>>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match env_var_case_insensitive(key)? {
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{key} must not be empty"),
                ));
            }
            trimmed.parse::<T>().map(Some).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid {key}={value:?}: {error}"),
                )
            })
        }
        None => Ok(None),
    }
}

fn parse_path_env_override(key: &str) -> io::Result<Option<PathBuf>> {
    let Some(value) = env_var_os_case_insensitive(key) else {
        return Ok(None);
    };

    if value.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{key} must not be empty"),
        ));
    }

    Ok(Some(expand_home_path(value)))
}

fn env_var_case_insensitive(key: &str) -> io::Result<Option<String>> {
    match env_var_os_case_insensitive(key) {
        Some(value) => value.into_string().map(Some).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{key} must be valid Unicode"),
            )
        }),
        None => Ok(None),
    }
}

fn env_var_os_case_insensitive(key: &str) -> Option<OsString> {
    if let Some(value) = env::var_os(key) {
        return Some(value);
    }

    env::vars_os().find_map(|(env_key, value)| {
        env_key
            .to_string_lossy()
            .eq_ignore_ascii_case(key)
            .then_some(value)
    })
}

fn expand_home_path(value: OsString) -> PathBuf {
    let path = PathBuf::from(&value);
    let Some(raw) = value.to_str() else {
        return path;
    };

    let rest = match raw {
        "~" => Some(""),
        value if value.starts_with("~/") || value.starts_with(r"~\") => Some(&value[2..]),
        _ => None,
    };

    let Some(rest) = rest else {
        return path;
    };
    let Some(home) = home_dir_from_env() else {
        return path;
    };

    if rest.is_empty() {
        home
    } else {
        home.join(rest)
    }
}

fn home_dir_from_env() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(profile) = env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
            return Some(PathBuf::from(profile));
        }
        if let (Some(drive), Some(path)) = (env::var_os("HOMEDRIVE"), env::var_os("HOMEPATH")) {
            let mut home = PathBuf::from(drive);
            home.push(path);
            return Some(home);
        }
    }

    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn absolutize_env_path(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }

    env::current_dir()
        .map(|current_dir| current_dir.join(&path))
        .unwrap_or(path)
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

fn shared_catalog_backup_policy(torrent_count: usize) -> SharedCatalogBackupPolicy {
    match torrent_count {
        0..=999 => SharedCatalogBackupPolicy {
            cadence_hours: 1,
            retained_backups: 16_384,
        },
        1_000..=9_999 => SharedCatalogBackupPolicy {
            cadence_hours: 3,
            retained_backups: 4_096,
        },
        10_000..=99_999 => SharedCatalogBackupPolicy {
            cadence_hours: 6,
            retained_backups: 1_024,
        },
        100_000..=999_999 => SharedCatalogBackupPolicy {
            cadence_hours: 12,
            retained_backups: 256,
        },
        _ => SharedCatalogBackupPolicy {
            cadence_hours: 24,
            retained_backups: 64,
        },
    }
}

fn shared_catalog_backup_roll_start(
    now: DateTime<Local>,
    policy: SharedCatalogBackupPolicy,
) -> DateTime<Local> {
    let cadence_secs = policy.cadence_hours.saturating_mul(60 * 60).max(60 * 60);
    let bucket_start = now.timestamp().div_euclid(cadence_secs) * cadence_secs;
    Local.timestamp_opt(bucket_start, 0).single().unwrap_or(now)
}

fn cleanup_shared_catalog_backups(backup_dir: &Path, retained_backups: usize) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(backup_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("catalog_") && name.ends_with(".toml"))
                .unwrap_or(false)
        })
        .collect();

    if entries.len() > retained_backups {
        entries.sort();
        for path in entries.iter().take(entries.len() - retained_backups) {
            fs::remove_file(path)?;
        }
    }

    Ok(())
}

fn backup_shared_catalog_before_write(
    paths: &SharedConfigPaths,
    catalog: &CatalogConfig,
) -> io::Result<()> {
    if !paths.catalog_path.exists() {
        return Ok(());
    }

    let policy = shared_catalog_backup_policy(catalog.torrents.len());
    let backup_dir = paths.root_dir.join("backups").join("catalog");
    fs::create_dir_all(&backup_dir)?;

    let roll_start = shared_catalog_backup_roll_start(Local::now(), policy);
    let backup_path = backup_dir.join(format!("catalog_{}.toml", roll_start.format("%Y%m%d_%H")));

    if !backup_path.exists() {
        fs::copy(&paths.catalog_path, &backup_path)?;
    }

    cleanup_shared_catalog_backups(&backup_dir, policy.retained_backups)
}

fn human_backup_root_dir() -> io::Result<PathBuf> {
    #[cfg(test)]
    {
        human_backup_root_override().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Test backup root override is not configured",
            )
        })
    }

    #[cfg(not(test))]
    {
        home_dir_from_env()
            .map(|home| home.join(".superseedr").join("backups"))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Could not resolve home directory")
            })
    }
}

fn fully_refresh_backup_tree<F>(target: &Path, populate: F) -> io::Result<()>
where
    F: FnOnce(&Path) -> io::Result<()>,
{
    let parent = target.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Backup target has no parent: {:?}", target),
        )
    })?;
    fs::create_dir_all(parent)?;

    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("latest");
    let suffix = Local::now()
        .timestamp_nanos_opt()
        .unwrap_or_else(|| Local::now().timestamp_millis());
    let tmp_path = parent.join(format!("{name}.tmp.{}.{}", std::process::id(), suffix));
    let old_path = parent.join(format!("{name}.old.{}.{}", std::process::id(), suffix));

    remove_path_if_exists(&tmp_path)?;
    remove_path_if_exists(&old_path)?;

    fs::create_dir_all(&tmp_path)?;
    if let Err(error) = populate(&tmp_path) {
        let _ = remove_path_if_exists(&tmp_path);
        return Err(error);
    }

    match fs::symlink_metadata(target) {
        Ok(_) => fs::rename(target, &old_path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    match fs::rename(&tmp_path, target) {
        Ok(()) => {
            let _ = remove_path_if_exists(&old_path);
            Ok(())
        }
        Err(error) => {
            if fs::symlink_metadata(&old_path).is_ok() {
                let _ = fs::rename(&old_path, target);
            }
            let _ = remove_path_if_exists(&tmp_path);
            Err(error)
        }
    }
}

fn remove_path_if_exists(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn copy_dir_if_exists(source: &Path, destination: &Path) -> io::Result<()> {
    if !source.exists() {
        return Ok(());
    }
    copy_dir_recursive(source, destination)
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &destination_path)?;
        }
    }
    Ok(())
}

fn refresh_normal_config_recovery_backup(
    paths: &NormalConfigPaths,
    settings: &Settings,
) -> io::Result<()> {
    let target = human_backup_root_dir()?.join("local-config").join("latest");
    fully_refresh_backup_tree(&target, |backup_dir| {
        write_toml_atomically(&backup_dir.join("settings.toml"), settings)?;
        copy_dir_if_exists(
            &paths.data_dir.join("torrents"),
            &backup_dir.join("torrents"),
        )?;
        Ok(())
    })
}

fn refresh_shared_config_recovery_backup_tree(
    paths: &SharedConfigPaths,
    layered: &LayeredConfig,
) -> io::Result<()> {
    let target = human_backup_root_dir()?
        .join("shared-config")
        .join("latest");
    fully_refresh_backup_tree(&target, |backup_dir| {
        write_toml_atomically(&backup_dir.join("settings.toml"), &layered.settings)?;
        write_toml_atomically(&backup_dir.join("catalog.toml"), &layered.catalog)?;
        write_toml_atomically(
            &backup_dir
                .join("hosts")
                .join(&paths.host_id)
                .join("config.toml"),
            &layered.host,
        )?;
        copy_dir_if_exists(
            &paths.root_dir.join("torrents"),
            &backup_dir.join("torrents"),
        )?;
        copy_dir_if_exists(
            &paths.root_dir.join("backups").join("catalog"),
            &backup_dir.join("backups").join("catalog"),
        )?;
        Ok(())
    })
}

pub(crate) fn refresh_shared_config_recovery_backup_now() -> io::Result<bool> {
    let Some(paths) = resolve_shared_config_paths()? else {
        return Ok(false);
    };
    let (layered, _metadata) = load_current_shared_layered(&paths, true)?;
    refresh_shared_config_recovery_backup_tree(&paths, &layered)?;
    Ok(true)
}

fn log_recovery_backup_error(kind: &str, error: &io::Error) {
    tracing_event!(
        Level::WARN,
        backup_kind = kind,
        error = %error,
        "Failed to refresh recovery backup"
    );
}

fn write_shared_cluster_revision_marker(root_dir: &Path) -> io::Result<String> {
    let revision_path = root_dir.join("cluster.revision");
    let revision = format!(
        "{}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    write_string_atomically(&revision_path, &revision)?;
    Ok(revision.trim().to_string())
}

fn shared_config_revision_snapshot(
    paths: &SharedConfigPaths,
    revision: String,
) -> Option<LoggedSharedConfigRevision> {
    let revision = revision.trim().to_string();
    if revision.is_empty() {
        return None;
    }

    Some(LoggedSharedConfigRevision {
        root_dir: paths.root_dir.clone(),
        host_id: paths.host_id.clone(),
        revision,
    })
}

fn mark_shared_config_revision_seen(paths: &SharedConfigPaths, revision: String) {
    let Some(next) = shared_config_revision_snapshot(paths, revision) else {
        return;
    };
    let mut logged = logged_shared_config_revision()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *logged = Some(next);
}

fn log_shared_config_revision_if_changed(paths: &SharedConfigPaths) {
    let revision_path = paths.root_dir.join("cluster.revision");
    let Ok(revision) = fs::read_to_string(revision_path) else {
        return;
    };
    let Some(next) = shared_config_revision_snapshot(paths, revision) else {
        return;
    };

    let mut logged = logged_shared_config_revision()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if logged.as_ref() == Some(&next) {
        return;
    }

    tracing_event!(
        Level::INFO,
        root_dir = ?next.root_dir,
        host_id = %next.host_id,
        revision = %next.revision,
        "Using shared config root at new cluster revision"
    );
    *logged = Some(next);
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

fn clear_shared_config_state() {
    #[cfg(test)]
    {
        RETAINED_SHARED_CONFIG_SELECTION.with(|retained_selection| {
            *retained_selection.borrow_mut() = None;
        });
    }
}

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

#[cfg(test)]
pub(crate) fn set_human_backup_root_override_for_tests(path: Option<PathBuf>) -> Option<PathBuf> {
    HUMAN_BACKUP_ROOT_OVERRIDE.with(|override_path| override_path.replace(path))
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
            return apply_env_overrides(&settings);
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
                return apply_env_overrides(&settings);
            }
            self.save_settings(&settings)?;
            return apply_env_overrides(&settings);
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
        if let Err(error) = refresh_normal_config_recovery_backup(&self.paths, &flat_settings) {
            log_recovery_backup_error("local-config", &error);
        }

        Ok(())
    }

    fn save_network_binding(&self, network_binding: &NetworkBindingConfig) -> io::Result<()> {
        let flat_settings: Settings = read_toml_or_default(&self.paths.settings_path)?;
        let layered = LayeredConfig::from_flat_settings(&flat_settings);
        let mut persisted_settings = layered.resolve_flat_settings()?;
        persisted_settings.network_binding = network_binding.clone();
        self.save_settings(&persisted_settings)
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
            backup_shared_catalog_before_write(&self.paths, &current_layered.catalog)?;
            let current_count = current_layered.catalog.torrents.len();
            let next_count = next_layered.catalog.torrents.len();
            let large_drop = current_count.saturating_sub(next_count);
            if large_drop > 10 || (current_count > 0 && next_count * 4 < current_count * 3) {
                tracing_event!(
                    Level::WARN,
                    current_torrents = current_count,
                    next_torrents = next_count,
                    "Shared catalog save is reducing torrent count"
                );
            }
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

        let shared_host_changed = next_layered.host != current_layered.host;
        if shared_host_changed {
            let _ =
                write_toml_atomically_with_fingerprint(&self.paths.host_path, &next_layered.host)?;
        }

        if shared_settings_changed || shared_catalog_changed || shared_metadata_changed {
            let revision = write_shared_cluster_revision_marker(&self.paths.root_dir)?;
            mark_shared_config_revision_seen(&self.paths, revision);
        }
        Ok(())
    }

    fn save_network_binding(&self, network_binding: &NetworkBindingConfig) -> io::Result<()> {
        validate_shared_runtime_root(&self.paths)?;
        let (mut layered, _) = load_current_shared_layered(&self.paths, true)?;
        if layered.host.network_binding == *network_binding {
            return Ok(());
        }
        layered.host.network_binding = network_binding.clone();
        let _ = write_toml_atomically_with_fingerprint(&self.paths.host_path, &layered.host)?;
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
                let settings = backend.load_settings()?;
                log_shared_config_revision_if_changed(&backend.paths);
                Ok(settings)
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
                let settings = backend.load_settings_for_cli()?;
                log_shared_config_revision_if_changed(&backend.paths);
                Ok(settings)
            }
        }
    }

    fn save_settings(&self, settings: &Settings) -> io::Result<()> {
        match self {
            ConfigBackend::Normal(backend) => backend.save_settings(settings),
            ConfigBackend::Shared(backend) => backend.save_settings(settings),
        }
    }

    fn save_network_binding(&self, network_binding: &NetworkBindingConfig) -> io::Result<()> {
        match self {
            ConfigBackend::Normal(backend) => backend.save_network_binding(network_binding),
            ConfigBackend::Shared(backend) => backend.save_network_binding(network_binding),
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
                if upsert_torrent_metadata_entry(&mut metadata, entry) {
                    let _ = write_toml_atomically_with_fingerprint(
                        &backend.paths.metadata_path,
                        &metadata,
                    )?;
                }
                Ok(())
            }
            ConfigBackend::Shared(backend) => {
                // This shared metadata update is safe under today's lock-based
                // single-writer model. If concurrent shared writers are added
                // later, restore conflict detection here before writing.
                let mut metadata = read_torrent_metadata_or_default(&backend.paths.metadata_path)?;
                if upsert_torrent_metadata_entry(&mut metadata, entry) {
                    let _ = write_toml_atomically_with_fingerprint(
                        &backend.paths.metadata_path,
                        &metadata,
                    )?;
                }
                Ok(())
            }
        }
    }
}

fn upsert_torrent_metadata_entry(
    metadata: &mut TorrentMetadataConfig,
    entry: TorrentMetadataEntry,
) -> bool {
    if let Some(existing) = metadata
        .torrents
        .iter_mut()
        .find(|existing| existing.info_hash_hex == entry.info_hash_hex)
    {
        if *existing == entry {
            return false;
        }
        *existing = entry;
        true
    } else {
        metadata.torrents.push(entry);
        true
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
    let (config_dir, data_dir) = get_app_paths().ok_or_else(|| {
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
            data_dir,
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

fn resolve_additional_watch_paths_from_sources<I, K, V>(vars: I) -> Vec<PathBuf>
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<OsString>,
    V: Into<OsString>,
{
    let mut indexed_paths = vars
        .into_iter()
        .filter_map(|(key, value)| {
            let key = key.into();
            let value = value.into();
            let key = key.to_string_lossy();
            let key_upper = key.to_ascii_uppercase();
            let suffix = key_upper.strip_prefix(EXTRA_WATCH_PATH_PREFIX)?;

            if suffix.is_empty() || value.is_empty() {
                return None;
            }

            let index = suffix.parse::<usize>().ok();
            Some((index, suffix.to_string(), PathBuf::from(value)))
        })
        .collect::<Vec<_>>();

    indexed_paths.sort_by(|left, right| {
        left.0
            .unwrap_or(usize::MAX)
            .cmp(&right.0.unwrap_or(usize::MAX))
            .then_with(|| left.1.cmp(&right.1))
    });

    let mut paths = Vec::new();
    for (_, _, path) in indexed_paths {
        push_unique_path(&mut paths, path);
    }
    paths
}

pub fn additional_watch_paths() -> Vec<PathBuf> {
    resolve_additional_watch_paths_from_sources(env::vars_os())
}

fn normalized_watch_component(component: Component<'_>) -> String {
    let value = component.as_os_str().to_string_lossy().into_owned();
    #[cfg(windows)]
    {
        let mut value = value;
        value.make_ascii_lowercase();
        value
    }
    #[cfg(not(windows))]
    value
}

fn normalized_watch_components(path: &Path) -> Vec<String> {
    path.components()
        .filter(|component| *component != Component::CurDir)
        .map(normalized_watch_component)
        .collect()
}

fn component_prefix_matches(path: &[String], prefix: &[String]) -> bool {
    !prefix.is_empty() && path.starts_with(prefix)
}

fn watch_paths_overlap(left: &Path, right: &Path) -> bool {
    let left = normalized_watch_components(left);
    let right = normalized_watch_components(right);

    left == right
        || component_prefix_matches(&left, &right)
        || component_prefix_matches(&right, &left)
}

fn shared_watch_exclusion_paths() -> Vec<PathBuf> {
    [
        shared_root_path(),
        shared_inbox_path(),
        shared_processed_path(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn additional_host_watch_paths() -> Vec<PathBuf> {
    let excluded_paths = shared_watch_exclusion_paths();
    additional_watch_paths()
        .into_iter()
        .filter(|path| {
            !excluded_paths
                .iter()
                .any(|excluded_path| watch_paths_overlap(path, excluded_path))
        })
        .collect()
}

pub fn host_watch_paths(settings: &Settings) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Some(path) = resolve_host_watch_path(settings) {
        push_unique_path(&mut paths, path);
    }

    for path in additional_host_watch_paths() {
        push_unique_path(&mut paths, path);
    }

    paths
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

    for path in additional_host_watch_paths() {
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

pub fn save_network_binding(network_binding: &NetworkBindingConfig) -> io::Result<()> {
    resolve_config_backend()?.save_network_binding(network_binding)
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
    use std::ffi::OsString;
    use std::path::PathBuf;
    use tempfile::tempdir;

    struct EnvVarRestore {
        key: &'static str,
        value: Option<OsString>,
    }

    impl EnvVarRestore {
        fn capture(key: &'static str) -> Self {
            Self {
                key,
                value: env::var_os(key),
            }
        }
    }

    impl Drop for EnvVarRestore {
        fn drop(&mut self) {
            match &self.value {
                Some(value) => env::set_var(self.key, value),
                None => env::remove_var(self.key),
            }
        }
    }

    struct HumanBackupRootRestore {
        value: Option<PathBuf>,
    }

    impl HumanBackupRootRestore {
        fn set(path: PathBuf) -> Self {
            let value = set_human_backup_root_override_for_tests(Some(path));
            Self { value }
        }
    }

    impl Drop for HumanBackupRootRestore {
        fn drop(&mut self) {
            set_human_backup_root_override_for_tests(self.value.take());
        }
    }

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
            ui_layout_mode = "horizontal"
            ui_refresh_rate = "Rate20s"

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

        let settings: Settings =
            deserialize_versioned_toml(toml_str).expect("Failed to parse full TOML string");

        assert_eq!(settings.client_id, "test-client-id-123");
        assert_eq!(settings.client_port, 12345);
        assert_eq!(settings.lifetime_downloaded, 1000);
        assert_eq!(settings.global_upload_limit_bps, 51200);
        assert_eq!(settings.torrent_sort_column, TorrentSortColumn::Name);
        assert_eq!(settings.torrent_sort_direction, SortDirection::Descending);
        assert_eq!(settings.peer_sort_column, PeerSortColumn::Address);
        assert_eq!(settings.ui_layout_mode, UiLayoutMode::Horizontal);
        assert_eq!(settings.ui_refresh_rate, DataRate::Rate20s);
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

        let settings: Settings =
            deserialize_versioned_toml(toml_str).expect("Failed to parse partial TOML string");

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
    fn test_standalone_config_upgrades_and_remains_readable_by_legacy_schema() {
        #[derive(Deserialize)]
        struct LegacyTorrentProbe {
            name: String,
        }

        #[derive(Deserialize)]
        struct LegacySettingsProbe {
            client_id: String,
            client_port: u16,
            torrents: Vec<LegacyTorrentProbe>,
        }

        let legacy = r#"
            schema_version = 1
            client_id = "upgrade-node"
            client_port = 7411

            [[torrents]]
            name = "Sample Archive"
            torrent_or_magnet = "magnet:?xt=urn:btih:1111111111111111111111111111111111111111"
        "#;
        let upgraded: Settings =
            deserialize_versioned_toml(legacy).expect("load legacy standalone settings");

        assert_eq!(upgraded.network_binding, NetworkBindingConfig::default());
        assert_eq!(upgraded.torrents.len(), 1);

        let branch_written =
            serialize_versioned_toml(&upgraded).expect("serialize upgraded standalone settings");
        assert!(branch_written.contains("[network_binding]"));
        let rollback: LegacySettingsProbe = deserialize_versioned_toml(&branch_written)
            .expect("legacy schema should ignore the binding table");

        assert_eq!(rollback.client_id, "upgrade-node");
        assert_eq!(rollback.client_port, 7411);
        assert_eq!(rollback.torrents.len(), 1);
        assert_eq!(rollback.torrents[0].name, "Sample Archive");
    }

    #[test]
    fn test_random_client_port_config_forms_enable_randomization() {
        let _guard = watch_env_guard().lock().unwrap();
        let _client_port = EnvVarRestore::capture(CLIENT_PORT_ENV);
        let _port = EnvVarRestore::capture(PORT_ENV);
        env::remove_var(CLIENT_PORT_ENV);
        env::remove_var(PORT_ENV);

        let string_port: Settings = deserialize_versioned_toml(r#"client_port = "RANDOM""#)
            .expect("parse RANDOM client port");
        let string_port = apply_env_overrides(&string_port).expect("normalize RANDOM client port");
        assert_eq!(string_port.client_port, 0);
        assert!(string_port.randomize_client_port);
    }

    #[test]
    fn test_default_settings() {
        let toml_str = "";

        let settings: Settings =
            deserialize_versioned_toml(toml_str).expect("Failed to parse empty string");

        let default_settings = Settings::default();

        assert_eq!(settings.client_id, default_settings.client_id);
        assert_eq!(settings.client_port, 6681);
        assert_eq!(settings.lifetime_downloaded, 0);
        assert_eq!(settings.global_upload_limit_bps, UNLIMITED_RATE_LIMIT_BPS);
        assert_eq!(settings.torrent_sort_column, TorrentSortColumn::Up);
        assert_eq!(settings.peer_sort_direction, SortDirection::Descending);
        assert_eq!(settings.ui_layout_mode, UiLayoutMode::Auto);
        assert_eq!(settings.ui_refresh_rate, DataRate::Rate1s);
        assert!(settings.watch_folder.is_none());
        assert_eq!(settings.max_connected_peers, 2000);
        assert_eq!(settings.bootstrap_nodes, default_settings.bootstrap_nodes);
        assert!(settings.torrents.is_empty());
    }

    #[test]
    fn test_layout_setting_accepts_layout_alias_and_veritical_typo() {
        let settings: Settings = deserialize_versioned_toml(r#"layout = "veritical""#)
            .expect("layout alias should parse");

        assert_eq!(settings.ui_layout_mode, UiLayoutMode::Vertical);
    }

    #[test]
    fn test_invalid_ui_theme_type_does_not_fail_settings_parse() {
        let toml_str = r#"
            client_id = "theme-type-regression"
            client_port = 7777
            ui_theme = 123
        "#;

        let settings: Settings = deserialize_versioned_toml(toml_str)
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
            regex = "linux image"
            enabled = true
        "#;

        let settings: Settings = deserialize_versioned_toml(toml_str)
            .expect("Settings parsing should accept legacy rss.filters.regex key");

        assert_eq!(settings.rss.filters.len(), 1);
        assert_eq!(settings.rss.filters[0].query, "linux image");
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

        let settings: Settings = deserialize_versioned_toml(toml_str)
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

        let result: io::Result<Settings> = deserialize_versioned_toml(toml_str);

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
    fn test_apply_env_overrides_handles_supported_env_vars() {
        let _guard = watch_env_guard().lock().unwrap();
        let _client_port = EnvVarRestore::capture(CLIENT_PORT_ENV);
        let _default_download_folder = EnvVarRestore::capture(DEFAULT_DOWNLOAD_FOLDER_ENV);
        let _output_status_interval = EnvVarRestore::capture(OUTPUT_STATUS_INTERVAL_ENV);
        let download_dir = tempdir().expect("create download dir");

        env::set_var(CLIENT_PORT_ENV, "61234");
        env::set_var(DEFAULT_DOWNLOAD_FOLDER_ENV, download_dir.path());
        env::set_var(OUTPUT_STATUS_INTERVAL_ENV, "9");

        let settings = Settings {
            client_port: 7777,
            default_download_folder: Some(PathBuf::from("from-file")),
            output_status_interval: 3,
            ..Settings::default()
        };
        let resolved = apply_env_overrides(&settings).expect("apply env overrides");

        assert_eq!(resolved.client_port, 61234);
        assert_eq!(
            resolved.default_download_folder,
            Some(download_dir.path().to_path_buf())
        );
        assert_eq!(resolved.output_status_interval, 9);
    }

    #[test]
    fn test_apply_env_overrides_trims_numeric_env_and_matches_case_insensitively() {
        const LOWER_CLIENT_PORT_ENV: &str = "superseedr_client_port";

        let _guard = watch_env_guard().lock().unwrap();
        let _client_port = EnvVarRestore::capture(CLIENT_PORT_ENV);
        let _lower_client_port = EnvVarRestore::capture(LOWER_CLIENT_PORT_ENV);

        env::remove_var(CLIENT_PORT_ENV);
        env::set_var(LOWER_CLIENT_PORT_ENV, " 61235 ");

        let settings = Settings {
            client_port: 7777,
            ..Settings::default()
        };
        let resolved = apply_env_overrides(&settings).expect("apply env overrides");

        assert_eq!(resolved.client_port, 61235);
    }

    #[test]
    fn test_apply_env_overrides_accepts_random_port_values() {
        let _guard = watch_env_guard().lock().unwrap();
        let _client_port = EnvVarRestore::capture(CLIENT_PORT_ENV);
        let _port = EnvVarRestore::capture(PORT_ENV);
        env::remove_var(CLIENT_PORT_ENV);
        env::set_var(PORT_ENV, "RANDOM");

        let settings = Settings {
            client_port: 7777,
            ..Settings::default()
        };
        let resolved = apply_env_overrides(&settings).expect("apply PORT=RANDOM");
        assert_eq!(resolved.client_port, 7777);
        assert!(resolved.randomize_client_port);

        env::set_var(CLIENT_PORT_ENV, "61234");
        let resolved = apply_env_overrides(&settings).expect("apply explicit numeric port");
        assert_eq!(resolved.client_port, 61234);
        assert!(!resolved.randomize_client_port);

        env::set_var(CLIENT_PORT_ENV, " random ");
        let resolved = apply_env_overrides(&settings).expect("apply namespaced RANDOM port");
        assert!(resolved.randomize_client_port);
    }

    #[test]
    fn test_apply_env_overrides_invalid_numeric_env_reports_key() {
        let _guard = watch_env_guard().lock().unwrap();
        let _client_port = EnvVarRestore::capture(CLIENT_PORT_ENV);
        env::set_var(CLIENT_PORT_ENV, "not-a-port");

        let error = apply_env_overrides(&Settings::default())
            .expect_err("invalid client port env should fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains(CLIENT_PORT_ENV),
            "unexpected error: {error}"
        );
        assert!(
            error.to_string().contains("not-a-port"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn test_apply_env_overrides_rejects_empty_path_env() {
        let _guard = watch_env_guard().lock().unwrap();
        let _default_download_folder = EnvVarRestore::capture(DEFAULT_DOWNLOAD_FOLDER_ENV);

        env::set_var(DEFAULT_DOWNLOAD_FOLDER_ENV, "");

        let error = apply_env_overrides(&Settings::default())
            .expect_err("empty default download folder env should fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains(DEFAULT_DOWNLOAD_FOLDER_ENV),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn test_apply_env_overrides_expands_home_path_env() {
        let _guard = watch_env_guard().lock().unwrap();
        let _default_download_folder = EnvVarRestore::capture(DEFAULT_DOWNLOAD_FOLDER_ENV);
        let _home = EnvVarRestore::capture("HOME");
        let _user_profile = EnvVarRestore::capture("USERPROFILE");
        let _home_drive = EnvVarRestore::capture("HOMEDRIVE");
        let _home_path = EnvVarRestore::capture("HOMEPATH");
        let home = tempdir().expect("create home dir");

        env::set_var("HOME", home.path());
        env::set_var("USERPROFILE", home.path());
        env::remove_var("HOMEDRIVE");
        env::remove_var("HOMEPATH");
        env::set_var(DEFAULT_DOWNLOAD_FOLDER_ENV, "~");

        let resolved = apply_env_overrides(&Settings::default()).expect("apply env overrides");

        assert_eq!(
            resolved.default_download_folder,
            Some(home.path().to_path_buf())
        );
    }

    #[test]
    fn test_apply_env_overrides_ignores_unsupported_settings_vars() {
        let _guard = watch_env_guard().lock().unwrap();
        let _private_client = EnvVarRestore::capture("SUPERSEEDR_PRIVATE_CLIENT");
        let _watch_path = EnvVarRestore::capture("SUPERSEEDR_WATCH_PATH_1");

        env::set_var("SUPERSEEDR_PRIVATE_CLIENT", "true");
        env::set_var("SUPERSEEDR_WATCH_PATH_1", "/extra-watch");

        let settings = Settings {
            private_client: false,
            ..Settings::default()
        };
        let resolved = apply_env_overrides(&settings).expect("apply env overrides");

        assert!(!resolved.private_client);
        assert_eq!(
            additional_watch_paths(),
            vec![PathBuf::from("/extra-watch")]
        );
    }

    #[test]
    fn test_resolve_additional_watch_paths_from_sources_orders_and_deduplicates() {
        let paths = resolve_additional_watch_paths_from_sources([
            ("SUPERSEEDR_WATCH_PATH_2", "/watch-b"),
            ("SUPERSEEDR_WATCH_PATH_10", "/watch-z"),
            ("IGNORED", "/nope"),
            ("SUPERSEEDR_WATCH_PATH_1", "/watch-a"),
            ("SUPERSEEDR_WATCH_PATH_3", "/watch-b"),
            ("superseedr_watch_path_alpha", "/watch-alpha"),
            ("SUPERSEEDR_WATCH_PATH_4", ""),
        ]);

        assert_eq!(
            paths,
            vec![
                PathBuf::from("/watch-a"),
                PathBuf::from("/watch-b"),
                PathBuf::from("/watch-z"),
                PathBuf::from("/watch-alpha"),
            ]
        );
    }

    #[test]
    fn test_shared_config_is_discovered_from_current_mount_root() {
        let dir = tempdir().expect("create tempdir");
        let config_root = dir.path().join(SHARED_CONFIG_SUBDIR);
        fs::create_dir(&config_root).expect("create shared config root");

        let selection = shared_config_from_current_dir(dir.path())
            .expect("discover shared config from current mount root");

        assert_eq!(selection.source, SharedConfigSource::CurrentDirectory);
        assert_eq!(selection.mount_root, dir.path());
        assert_eq!(selection.config_root, config_root);
    }

    #[test]
    fn test_shared_config_is_discovered_from_current_config_root() {
        let dir = tempdir().expect("create tempdir");
        let mount_root = dir.path().join("shared-root");
        let config_root = mount_root.join(SHARED_CONFIG_SUBDIR);
        fs::create_dir_all(&config_root).expect("create shared config root");

        let selection = shared_config_from_current_dir(&config_root)
            .expect("discover shared config from current config root");

        assert_eq!(selection.source, SharedConfigSource::CurrentDirectory);
        assert_eq!(selection.mount_root, mount_root);
        assert_eq!(selection.config_root, config_root);
    }

    #[test]
    fn test_shared_config_is_not_discovered_from_arbitrary_current_directory() {
        let dir = tempdir().expect("create tempdir");

        assert_eq!(shared_config_from_current_dir(dir.path()), None);
    }

    #[test]
    fn test_current_directory_shared_config_takes_precedence_over_launcher() {
        let dir = tempdir().expect("create tempdir");
        let current_root = dir.path().join("current-root");
        fs::create_dir_all(current_root.join(SHARED_CONFIG_SUBDIR))
            .expect("create current shared config root");
        let launcher_root = dir.path().join("launcher-root");

        let selection = resolve_shared_config_selection_from_sources(
            None,
            Some(&current_root),
            Some(launcher_root),
        )
        .expect("resolve current-directory shared config");

        assert_eq!(selection.source, SharedConfigSource::CurrentDirectory);
        assert_eq!(selection.mount_root, current_root);
    }

    #[test]
    fn test_current_directory_shared_config_stays_selected_when_root_disappears() {
        let dir = tempdir().expect("create tempdir");
        let current_root = dir.path().join("current-root");
        let config_root = current_root.join(SHARED_CONFIG_SUBDIR);
        fs::create_dir_all(&config_root).expect("create current shared config root");
        let launcher_root = dir.path().join("launcher-root");
        let mut retained_selection = None;

        let initial = resolve_shared_config_selection_retaining_initial_decision(
            &mut retained_selection,
            None,
            Some(&current_root),
            Some(launcher_root.clone()),
        )
        .expect("resolve current-directory shared config");
        fs::remove_dir(&config_root).expect("make current shared config unavailable");

        let rediscovered = resolve_shared_config_selection_from_sources(
            None,
            Some(&current_root),
            Some(launcher_root.clone()),
        )
        .expect("fall back without retained selection");
        let retained = resolve_shared_config_selection_retaining_initial_decision(
            &mut retained_selection,
            None,
            Some(&current_root),
            Some(launcher_root),
        )
        .expect("retain current-directory shared config");

        assert_eq!(rediscovered.source, SharedConfigSource::Launcher);
        assert_eq!(retained, initial);
        assert_eq!(retained.source, SharedConfigSource::CurrentDirectory);
        assert_eq!(retained.config_root, config_root);
    }

    #[test]
    fn test_initial_standalone_decision_is_retained_when_shared_marker_appears() {
        let dir = tempdir().expect("create tempdir");
        let config_root = dir.path().join(SHARED_CONFIG_SUBDIR);
        let mut retained_selection = None;

        let initial = resolve_shared_config_selection_retaining_initial_decision(
            &mut retained_selection,
            None,
            Some(dir.path()),
            None,
        );
        fs::create_dir(&config_root).expect("make current shared config available");

        let rediscovered =
            resolve_shared_config_selection_from_sources(None, Some(dir.path()), None)
                .expect("discover newly available current-directory shared config");
        let retained = resolve_shared_config_selection_retaining_initial_decision(
            &mut retained_selection,
            None,
            Some(dir.path()),
            None,
        );

        assert_eq!(initial, None);
        assert_eq!(rediscovered.source, SharedConfigSource::CurrentDirectory);
        assert_eq!(rediscovered.config_root, config_root);
        assert_eq!(retained, None);
    }

    #[test]
    fn test_initial_launcher_decision_is_retained_when_shared_marker_appears() {
        let dir = tempdir().expect("create tempdir");
        let current_root = dir.path().join("current-root");
        fs::create_dir(&current_root).expect("create current root");
        let config_root = current_root.join(SHARED_CONFIG_SUBDIR);
        let launcher_root = dir.path().join("launcher-root");
        let mut retained_selection = None;

        let initial = resolve_shared_config_selection_retaining_initial_decision(
            &mut retained_selection,
            None,
            Some(&current_root),
            Some(launcher_root.clone()),
        )
        .expect("resolve launcher shared config");
        fs::create_dir(&config_root).expect("make current shared config available");

        let rediscovered = resolve_shared_config_selection_from_sources(
            None,
            Some(&current_root),
            Some(launcher_root),
        )
        .expect("discover newly available current-directory shared config");
        let retained = resolve_shared_config_selection_retaining_initial_decision(
            &mut retained_selection,
            None,
            Some(&current_root),
            None,
        )
        .expect("retain launcher shared config");

        assert_eq!(initial.source, SharedConfigSource::Launcher);
        assert_eq!(rediscovered.source, SharedConfigSource::CurrentDirectory);
        assert_eq!(retained, initial);
    }

    #[test]
    fn test_env_shared_config_takes_precedence_over_current_directory() {
        let dir = tempdir().expect("create tempdir");
        let env_root = dir.path().join("env-root");
        let current_root = dir.path().join("current-root");
        fs::create_dir_all(current_root.join(SHARED_CONFIG_SUBDIR))
            .expect("create current shared config root");

        let selection = resolve_shared_config_selection_from_sources(
            Some(env_root.clone()),
            Some(&current_root),
            None,
        )
        .expect("resolve environment shared config");

        assert_eq!(selection.source, SharedConfigSource::Env);
        assert_eq!(selection.mount_root, env_root);
    }

    #[test]
    fn test_shared_config_dir_env_relative_path_is_resolved_from_current_dir() {
        let _guard = watch_env_guard().lock().unwrap();
        let _shared_dir = EnvVarRestore::capture(SHARED_CONFIG_DIR_ENV);

        env::set_var(SHARED_CONFIG_DIR_ENV, "relative-shared-root");
        clear_shared_config_state();

        let current_dir = env::current_dir().expect("current dir");
        let selection = resolve_shared_config_selection()
            .expect("resolve shared config")
            .expect("shared config enabled");

        assert_eq!(
            selection.mount_root,
            current_dir.join("relative-shared-root")
        );
        assert_eq!(
            selection.config_root,
            current_dir
                .join("relative-shared-root")
                .join(SHARED_CONFIG_SUBDIR)
        );

        clear_shared_config_state();
    }

    #[test]
    fn test_shared_config_dir_env_matches_case_insensitively() {
        const LOWER_SHARED_CONFIG_DIR_ENV: &str = "superseedr_shared_config_dir";

        let _guard = watch_env_guard().lock().unwrap();
        let _shared_dir = EnvVarRestore::capture(SHARED_CONFIG_DIR_ENV);
        let _lower_shared_dir = EnvVarRestore::capture(LOWER_SHARED_CONFIG_DIR_ENV);
        let dir = tempdir().expect("create tempdir");

        env::remove_var(SHARED_CONFIG_DIR_ENV);
        env::set_var(LOWER_SHARED_CONFIG_DIR_ENV, dir.path());
        clear_shared_config_state();

        let selection = resolve_shared_config_selection()
            .expect("resolve shared config")
            .expect("shared config enabled");

        assert_eq!(selection.source, SharedConfigSource::Env);
        assert_eq!(selection.mount_root, dir.path());
        assert_eq!(selection.config_root, dir.path().join(SHARED_CONFIG_SUBDIR));

        clear_shared_config_state();
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

    #[cfg(windows)]
    #[test]
    fn test_shared_data_path_accepts_verbatim_unc_under_root() {
        let shared_mount_root = Path::new(r"\\Server\Share\Root");
        let absolute = Path::new(r"\\?\UNC\Server\Share\Root\downloads");

        let encoded =
            encode_shared_data_path(absolute, Some(shared_mount_root), "default_download_folder")
                .expect("encode shared path");

        assert_eq!(encoded, PathBuf::from("downloads"));
    }

    #[cfg(windows)]
    #[test]
    fn test_shared_data_path_accepts_case_variant_under_root() {
        let shared_mount_root = Path::new(r"C:\SharedRoot");
        let absolute = Path::new(r"c:\sharedroot\downloads");

        let encoded =
            encode_shared_data_path(absolute, Some(shared_mount_root), "default_download_folder")
                .expect("encode shared path");

        assert_eq!(encoded, PathBuf::from("downloads"));
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
            network_binding: NetworkBindingConfig::default(),
            watch_folder: Some(PathBuf::from("/watch")),
            always_show_add_location_prompt: true,
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
    fn test_host_config_round_trips_random_client_port() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("host.toml");
        let host = HostConfig {
            client_port: 0,
            ..HostConfig::default()
        };

        write_toml_atomically_with_fingerprint(&path, &host).expect("write host config");
        let persisted = fs::read_to_string(&path).expect("read host config");
        let loaded: HostConfig = read_toml_or_default(&path).expect("load host config");

        assert!(persisted.contains("client_port = \"RANDOM\""));
        assert_eq!(loaded.client_port, 0);
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
        let _guard = watch_env_guard().lock().unwrap();
        let _client_port = EnvVarRestore::capture(CLIENT_PORT_ENV);
        let _lower_client_port = EnvVarRestore::capture("superseedr_client_port");
        let _default_download_folder = EnvVarRestore::capture(DEFAULT_DOWNLOAD_FOLDER_ENV);
        let _lower_default_download_folder =
            EnvVarRestore::capture("superseedr_default_download_folder");
        let _output_status_interval = EnvVarRestore::capture(OUTPUT_STATUS_INTERVAL_ENV);
        let _lower_output_status_interval =
            EnvVarRestore::capture("superseedr_output_status_interval");
        env::remove_var(CLIENT_PORT_ENV);
        env::remove_var("superseedr_client_port");
        env::remove_var(DEFAULT_DOWNLOAD_FOLDER_ENV);
        env::remove_var("superseedr_default_download_folder");
        env::remove_var(OUTPUT_STATUS_INTERVAL_ENV);
        env::remove_var("superseedr_output_status_interval");

        let dir = tempdir().expect("create tempdir");
        let backend = NormalConfigBackend {
            paths: NormalConfigPaths {
                settings_path: dir.path().join("settings.toml"),
                metadata_path: dir.path().join("torrent_metadata.toml"),
                backup_dir: dir.path().join("backups_settings_files"),
                data_dir: dir.path().join("data"),
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
    fn test_normal_backend_preserves_random_client_port_across_save() {
        let _guard = watch_env_guard().lock().unwrap();
        let _client_port = EnvVarRestore::capture(CLIENT_PORT_ENV);
        let _port = EnvVarRestore::capture(PORT_ENV);
        env::remove_var(CLIENT_PORT_ENV);
        env::remove_var(PORT_ENV);

        let dir = tempdir().expect("create tempdir");
        let backend = NormalConfigBackend {
            paths: NormalConfigPaths {
                settings_path: dir.path().join("settings.toml"),
                metadata_path: dir.path().join("torrent_metadata.toml"),
                backup_dir: dir.path().join("backups_settings_files"),
                data_dir: dir.path().join("data"),
            },
        };
        let settings = Settings {
            client_port: 54321,
            randomize_client_port: true,
            ..Settings::default()
        };

        backend.save_settings(&settings).expect("save settings");
        let persisted =
            fs::read_to_string(&backend.paths.settings_path).expect("read persisted settings");
        let loaded = backend.load_settings().expect("load settings");

        assert!(persisted.contains("client_port = \"RANDOM\""));
        assert_eq!(loaded.client_port, 0);
        assert!(loaded.randomize_client_port);
    }

    #[test]
    fn test_normal_backend_fully_refreshes_human_backup_mirror() {
        let _guard = watch_env_guard().lock().unwrap();
        let _home = EnvVarRestore::capture("HOME");
        let _user_profile = EnvVarRestore::capture("USERPROFILE");
        let _home_drive = EnvVarRestore::capture("HOMEDRIVE");
        let _home_path = EnvVarRestore::capture("HOMEPATH");
        let dir = tempdir().expect("create tempdir");
        let home = dir.path().join("home");
        let _backup_root = HumanBackupRootRestore::set(home.join(".superseedr").join("backups"));
        env::set_var("HOME", &home);
        env::set_var("USERPROFILE", &home);
        env::remove_var("HOMEDRIVE");
        env::remove_var("HOMEPATH");

        let backend = NormalConfigBackend {
            paths: NormalConfigPaths {
                settings_path: dir.path().join("settings.toml"),
                metadata_path: dir.path().join("torrent_metadata.toml"),
                backup_dir: dir.path().join("backups_settings_files"),
                data_dir: dir.path().join("data"),
            },
        };
        let stale_path = home
            .join(".superseedr")
            .join("backups")
            .join("local-config")
            .join("latest")
            .join("stale.txt");
        fs::create_dir_all(stale_path.parent().expect("stale parent"))
            .expect("create stale backup dir");
        fs::write(&stale_path, "stale").expect("write stale backup marker");
        fs::write(
            stale_path
                .parent()
                .expect("stale parent")
                .join("torrent_metadata.toml"),
            "stale metadata",
        )
        .expect("write stale metadata backup");
        let cached_torrent = backend
            .paths
            .data_dir
            .join("torrents")
            .join("sample.torrent");
        fs::create_dir_all(cached_torrent.parent().expect("torrent parent"))
            .expect("create torrent cache");
        fs::write(&cached_torrent, b"sample torrent bytes").expect("write cached torrent");

        let settings = Settings {
            client_id: "local-backup-node".to_string(),
            client_port: 7777,
            ..Settings::default()
        };

        backend.save_settings(&settings).expect("save settings");

        let latest = home
            .join(".superseedr")
            .join("backups")
            .join("local-config")
            .join("latest");
        let backup_settings: Settings =
            read_toml_or_default(&latest.join("settings.toml")).expect("read backup settings");

        assert_eq!(backup_settings.client_id, "local-backup-node");
        assert!(latest.join("torrents").join("sample.torrent").exists());
        assert!(!latest.join("torrent_metadata.toml").exists());
        assert!(!latest.join("stale.txt").exists());
    }

    #[test]
    fn test_normal_backend_load_applies_supported_env_overrides() {
        let _guard = watch_env_guard().lock().unwrap();
        let _client_port = EnvVarRestore::capture(CLIENT_PORT_ENV);
        let _default_download_folder = EnvVarRestore::capture(DEFAULT_DOWNLOAD_FOLDER_ENV);
        let _output_status_interval = EnvVarRestore::capture(OUTPUT_STATUS_INTERVAL_ENV);
        let dir = tempdir().expect("create tempdir");
        let download_dir = dir.path().join("env-downloads");
        let backend = NormalConfigBackend {
            paths: NormalConfigPaths {
                settings_path: dir.path().join("settings.toml"),
                metadata_path: dir.path().join("torrent_metadata.toml"),
                backup_dir: dir.path().join("backups_settings_files"),
                data_dir: dir.path().join("data"),
            },
        };
        let settings = Settings {
            client_port: 7000,
            default_download_folder: Some(dir.path().join("file-downloads")),
            output_status_interval: 3,
            ..Settings::default()
        };

        backend.save_settings(&settings).expect("save settings");
        env::set_var(CLIENT_PORT_ENV, "61234");
        env::set_var(DEFAULT_DOWNLOAD_FOLDER_ENV, &download_dir);
        env::set_var(OUTPUT_STATUS_INTERVAL_ENV, "11");

        let loaded = backend.load_settings().expect("load settings");

        assert_eq!(loaded.client_port, 61234);
        assert_eq!(loaded.default_download_folder, Some(download_dir));
        assert_eq!(loaded.output_status_interval, 11);
    }

    #[test]
    fn test_normal_network_binding_save_does_not_persist_env_overrides() {
        let _guard = watch_env_guard().lock().unwrap();
        let _client_port = EnvVarRestore::capture(CLIENT_PORT_ENV);
        let _default_download_folder = EnvVarRestore::capture(DEFAULT_DOWNLOAD_FOLDER_ENV);
        let dir = tempdir().expect("create tempdir");
        let file_download_dir = dir.path().join("file-downloads");
        let env_download_dir = dir.path().join("env-downloads");
        let backend = NormalConfigBackend {
            paths: NormalConfigPaths {
                settings_path: dir.path().join("settings.toml"),
                metadata_path: dir.path().join("torrent_metadata.toml"),
                backup_dir: dir.path().join("backups_settings_files"),
                data_dir: dir.path().join("data"),
            },
        };
        let settings = Settings {
            client_port: 7000,
            default_download_folder: Some(file_download_dir.clone()),
            ..Settings::default()
        };
        backend.save_settings(&settings).expect("save settings");

        env::set_var(CLIENT_PORT_ENV, "61234");
        env::set_var(DEFAULT_DOWNLOAD_FOLDER_ENV, &env_download_dir);
        let loaded = backend.load_settings_for_cli().expect("load CLI settings");
        assert_eq!(loaded.client_port, 61234);
        assert_eq!(loaded.default_download_folder, Some(env_download_dir));

        let network_binding = NetworkBindingConfig {
            mode: crate::networking::NetworkBindingMode::Interface,
            interface: Some("interface-test0".to_string()),
            enable_ipv6: false,
            ..NetworkBindingConfig::default()
        };
        backend
            .save_network_binding(&network_binding)
            .expect("save only network binding");

        let persisted: Settings =
            read_toml_or_default(&backend.paths.settings_path).expect("read persisted settings");
        assert_eq!(persisted.client_port, 7000);
        assert_eq!(persisted.default_download_folder, Some(file_download_dir));
        assert_eq!(persisted.network_binding, network_binding);
    }

    #[test]
    fn test_normal_backend_first_run_applies_env_overrides_without_persisting_them() {
        let _guard = watch_env_guard().lock().unwrap();
        let _client_port = EnvVarRestore::capture(CLIENT_PORT_ENV);
        let dir = tempdir().expect("create tempdir");
        let backend = NormalConfigBackend {
            paths: NormalConfigPaths {
                settings_path: dir.path().join("settings.toml"),
                metadata_path: dir.path().join("torrent_metadata.toml"),
                backup_dir: dir.path().join("backups_settings_files"),
                data_dir: dir.path().join("data"),
            },
        };
        env::set_var(CLIENT_PORT_ENV, "61234");

        let loaded = backend.load_settings().expect("load settings");
        let persisted: Settings =
            read_toml_or_default(&backend.paths.settings_path).expect("read persisted settings");

        assert_eq!(loaded.client_port, 61234);
        assert_eq!(persisted.client_port, Settings::default().client_port);
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
    fn test_shared_network_binding_save_updates_only_the_host_layer() {
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
        let shared = SharedSettingsConfig {
            global_upload_limit_bps: 4321,
            ..SharedSettingsConfig::default()
        };
        let host = HostConfig {
            client_port: 9090,
            ..HostConfig::default()
        };
        write_toml_atomically(&backend.paths.settings_path, &shared).expect("seed shared settings");
        write_toml_atomically(&backend.paths.host_path, &host).expect("seed host settings");

        let network_binding = NetworkBindingConfig {
            mode: crate::networking::NetworkBindingMode::Interface,
            interface: Some("interface-test0".to_string()),
            enable_ipv6: false,
            ..NetworkBindingConfig::default()
        };
        backend
            .save_network_binding(&network_binding)
            .expect("save host network binding");

        let persisted_shared: SharedSettingsConfig =
            read_toml_or_default(&backend.paths.settings_path).expect("read shared settings");
        let persisted_host: HostConfig =
            read_toml_or_default(&backend.paths.host_path).expect("read host settings");
        assert_eq!(persisted_shared, shared);
        assert_eq!(persisted_host.client_port, 9090);
        assert_eq!(persisted_host.network_binding, network_binding);
    }

    #[test]
    fn test_shared_config_upgrades_and_remains_readable_by_legacy_host_schema() {
        #[derive(Deserialize)]
        struct LegacyHostProbe {
            client_port: u16,
            watch_folder: Option<PathBuf>,
        }

        let _guard = shared_backend_guard().lock().unwrap();
        clear_shared_config_state();
        let dir = tempdir().expect("create tempdir");
        let config_root = dir.path().join(SHARED_CONFIG_SUBDIR);
        let host_dir = config_root.join("hosts").join("upgrade-node");
        let backend = SharedConfigBackend {
            paths: SharedConfigPaths {
                mount_dir: dir.path().to_path_buf(),
                root_dir: config_root.clone(),
                settings_path: config_root.join("settings.toml"),
                catalog_path: config_root.join("catalog.toml"),
                metadata_path: config_root.join("torrent_metadata.toml"),
                host_dir: host_dir.clone(),
                host_path: host_dir.join("config.toml"),
                host_id: "upgrade-node".to_string(),
            },
        };
        write_toml_atomically(
            &backend.paths.settings_path,
            &SharedSettingsConfig::default(),
        )
        .expect("seed shared settings");
        let catalog = CatalogConfig {
            torrents: vec![CatalogTorrentSettings {
                torrent_or_magnet: "magnet:?xt=urn:btih:2222222222222222222222222222222222222222"
                    .to_string(),
                name: "Sample Collection".to_string(),
                ..CatalogTorrentSettings::default()
            }],
        };
        write_toml_atomically(&backend.paths.catalog_path, &catalog).expect("seed catalog");
        fs::create_dir_all(&backend.paths.host_dir).expect("create host directory");
        fs::write(
            &backend.paths.host_path,
            "schema_version = 1\nclient_port = 7412\nwatch_folder = \"/watch\"\n",
        )
        .expect("seed legacy host config");

        let mut upgraded = backend.load_settings().expect("load legacy shared config");
        assert_eq!(upgraded.network_binding, NetworkBindingConfig::default());
        assert_eq!(upgraded.client_port, 7412);
        assert_eq!(upgraded.torrents.len(), 1);
        upgraded.client_port = 7413;
        backend
            .save_settings(&upgraded)
            .expect("write upgraded host config");

        let branch_written =
            fs::read_to_string(&backend.paths.host_path).expect("read upgraded host config");
        assert!(branch_written.contains("[network_binding]"));
        let rollback: LegacyHostProbe = deserialize_versioned_toml(&branch_written)
            .expect("legacy host schema should ignore the binding table");
        let retained_catalog: CatalogConfig = read_toml_or_default(&backend.paths.catalog_path)
            .expect("read retained shared catalog");

        assert_eq!(rollback.client_port, 7413);
        assert_eq!(rollback.watch_folder, Some(PathBuf::from("/watch")));
        assert_eq!(retained_catalog, catalog);
    }

    #[test]
    fn test_scheduled_shared_backup_fully_refreshes_human_mirror() {
        let _guard = shared_backend_guard().lock().unwrap();
        let _home = EnvVarRestore::capture("HOME");
        let _user_profile = EnvVarRestore::capture("USERPROFILE");
        let _home_drive = EnvVarRestore::capture("HOMEDRIVE");
        let _home_path = EnvVarRestore::capture("HOMEPATH");
        clear_shared_config_state();
        let dir = tempdir().expect("create tempdir");
        let home = dir.path().join("home");
        let _backup_root = HumanBackupRootRestore::set(home.join(".superseedr").join("backups"));
        env::set_var("HOME", &home);
        env::set_var("USERPROFILE", &home);
        env::remove_var("HOMEDRIVE");
        env::remove_var("HOMEPATH");

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
        let stale_path = home
            .join(".superseedr")
            .join("backups")
            .join("shared-config")
            .join("latest")
            .join("stale.txt");
        fs::create_dir_all(stale_path.parent().expect("stale parent"))
            .expect("create stale backup dir");
        fs::write(&stale_path, "stale").expect("write stale backup marker");
        fs::write(
            stale_path
                .parent()
                .expect("stale parent")
                .join("torrent_metadata.toml"),
            "stale metadata",
        )
        .expect("write stale metadata backup");
        fs::write(
            stale_path
                .parent()
                .expect("stale parent")
                .join("cluster.revision"),
            "stale revision",
        )
        .expect("write stale revision backup");
        let shared_torrent = backend
            .paths
            .root_dir
            .join("torrents")
            .join("0123456789abcdef0123456789abcdef01234567.torrent");
        fs::create_dir_all(shared_torrent.parent().expect("shared torrent parent"))
            .expect("create shared torrent cache");
        fs::write(&shared_torrent, b"sample torrent bytes").expect("write shared torrent");
        let catalog_snapshot = backend
            .paths
            .root_dir
            .join("backups")
            .join("catalog")
            .join("catalog_20260523_10.toml");
        fs::create_dir_all(catalog_snapshot.parent().expect("catalog snapshot parent"))
            .expect("create catalog snapshot dir");
        fs::write(
            &catalog_snapshot,
            "[[torrents]]\nname = \"Previous Sample\"\n",
        )
        .expect("write catalog snapshot");

        let mut settings = backend.load_settings().expect("load shared settings");
        settings.client_id = "shared-backup-node".to_string();
        settings.client_port = 9090;
        settings.torrents.push(TorrentSettings {
            torrent_or_magnet: shared_torrent.to_string_lossy().to_string(),
            name: "Shared Sample".to_string(),
            ..TorrentSettings::default()
        });

        backend
            .save_settings(&settings)
            .expect("save shared settings");

        assert!(
            stale_path.exists(),
            "shared saves should leave the recovery mirror for the scheduled worker"
        );

        let (layered, _) = load_current_shared_layered(&backend.paths, true)
            .expect("load shared state for scheduled backup");
        refresh_shared_config_recovery_backup_tree(&backend.paths, &layered)
            .expect("run scheduled shared backup");

        let latest = home
            .join(".superseedr")
            .join("backups")
            .join("shared-config")
            .join("latest");
        let backup_settings: SharedSettingsConfig =
            read_toml_or_default(&latest.join("settings.toml")).expect("read backup settings");
        let backup_catalog: CatalogConfig =
            read_toml_or_default(&latest.join("catalog.toml")).expect("read backup catalog");
        let backup_host: HostConfig =
            read_toml_or_default(&latest.join("hosts").join("node-a").join("config.toml"))
                .expect("read backup host");

        assert_eq!(backup_settings.client_id, "shared-backup-node");
        assert_eq!(backup_catalog.torrents.len(), 1);
        assert_eq!(backup_catalog.torrents[0].name, "Shared Sample");
        assert_eq!(backup_host.client_port, 9090);
        assert!(latest
            .join("torrents")
            .join("0123456789abcdef0123456789abcdef01234567.torrent")
            .exists());
        assert_eq!(
            fs::read_to_string(
                latest
                    .join("backups")
                    .join("catalog")
                    .join("catalog_20260523_10.toml")
            )
            .expect("read backup catalog snapshot"),
            "[[torrents]]\nname = \"Previous Sample\"\n"
        );
        assert!(!latest.join("torrent_metadata.toml").exists());
        assert!(!latest.join("cluster.revision").exists());
        assert!(!latest.join("stale.txt").exists());
    }

    #[test]
    fn test_shared_catalog_backup_policy_scales_by_catalog_size() {
        assert_eq!(
            shared_catalog_backup_policy(999),
            SharedCatalogBackupPolicy {
                cadence_hours: 1,
                retained_backups: 16_384
            }
        );
        assert_eq!(
            shared_catalog_backup_policy(1_000),
            SharedCatalogBackupPolicy {
                cadence_hours: 3,
                retained_backups: 4_096
            }
        );
        assert_eq!(
            shared_catalog_backup_policy(10_000),
            SharedCatalogBackupPolicy {
                cadence_hours: 6,
                retained_backups: 1_024
            }
        );
        assert_eq!(
            shared_catalog_backup_policy(100_000),
            SharedCatalogBackupPolicy {
                cadence_hours: 12,
                retained_backups: 256
            }
        );
        assert_eq!(
            shared_catalog_backup_policy(1_000_000),
            SharedCatalogBackupPolicy {
                cadence_hours: 24,
                retained_backups: 64
            }
        );
    }

    #[test]
    fn test_shared_catalog_backup_deduplicates_current_roll_window() {
        let dir = tempdir().expect("create tempdir");
        let root_dir = dir.path().join("shared");
        let host_dir = root_dir.join("hosts").join("node-a");
        let paths = SharedConfigPaths {
            mount_dir: dir.path().to_path_buf(),
            root_dir: root_dir.clone(),
            settings_path: root_dir.join("settings.toml"),
            catalog_path: root_dir.join("catalog.toml"),
            metadata_path: root_dir.join("torrent_metadata.toml"),
            host_dir: host_dir.clone(),
            host_path: host_dir.join("config.toml"),
            host_id: "node-a".to_string(),
        };
        fs::create_dir_all(&paths.root_dir).expect("create shared root");
        let catalog = CatalogConfig {
            torrents: vec![CatalogTorrentSettings {
                torrent_or_magnet: "magnet:?xt=urn:btih:1111111111111111111111111111111111111111"
                    .to_string(),
                name: "Sample Item".to_string(),
                ..CatalogTorrentSettings::default()
            }],
        };
        write_toml_atomically(&paths.catalog_path, &catalog).expect("seed catalog");

        backup_shared_catalog_before_write(&paths, &catalog).expect("backup catalog");
        backup_shared_catalog_before_write(&paths, &catalog).expect("backup catalog again");

        let backup_dir = paths.root_dir.join("backups").join("catalog");
        let backups: Vec<_> = fs::read_dir(backup_dir)
            .expect("read backups")
            .filter_map(Result::ok)
            .collect();
        assert_eq!(backups.len(), 1);
    }

    #[test]
    fn test_shared_backend_backs_up_catalog_before_overwrite() {
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

        let mut settings = backend.load_settings().expect("load shared settings");
        settings.torrents = vec![
            TorrentSettings {
                torrent_or_magnet: "magnet:?xt=urn:btih:1111111111111111111111111111111111111111"
                    .to_string(),
                name: "Sample Alpha".to_string(),
                ..TorrentSettings::default()
            },
            TorrentSettings {
                torrent_or_magnet: "magnet:?xt=urn:btih:2222222222222222222222222222222222222222"
                    .to_string(),
                name: "Sample Beta".to_string(),
                ..TorrentSettings::default()
            },
        ];
        backend
            .save_settings(&settings)
            .expect("save initial catalog");

        settings.torrents.pop();
        backend
            .save_settings(&settings)
            .expect("save reduced catalog");

        let backup_dir = backend.paths.root_dir.join("backups").join("catalog");
        let backup_path = fs::read_dir(backup_dir)
            .expect("read backup dir")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .next()
            .expect("backup should exist");
        let backup_catalog: CatalogConfig =
            read_toml_or_default(&backup_path).expect("read backup catalog");
        let current_catalog: CatalogConfig =
            read_toml_or_default(&backend.paths.catalog_path).expect("read current catalog");

        assert_eq!(backup_catalog.torrents.len(), 2);
        assert_eq!(current_catalog.torrents.len(), 1);
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
    fn test_shared_backend_validates_env_overridden_default_download_folder() {
        let _guard = shared_backend_guard().lock().unwrap();
        let _default_download_folder = EnvVarRestore::capture(DEFAULT_DOWNLOAD_FOLDER_ENV);
        let _host_id = EnvVarRestore::capture(SHARED_HOST_ID_ENV);
        let _legacy_host_id = EnvVarRestore::capture(LEGACY_SHARED_HOST_ID_ENV);
        clear_shared_config_state();
        let dir = tempdir().expect("create tempdir");
        let shared_mount = dir.path().join("shared-mount");
        fs::create_dir_all(&shared_mount).expect("create shared mount");
        env::set_var(SHARED_HOST_ID_ENV, "node-a");
        env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        let backend = shared_backend_for_mount_root(&shared_mount).expect("shared backend");
        env::set_var(
            DEFAULT_DOWNLOAD_FOLDER_ENV,
            dir.path().join("outside-downloads"),
        );

        let error = backend
            .load_settings()
            .expect_err("env override outside shared root should fail validation");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("default_download_folder"),
            "unexpected error: {error}"
        );
        clear_shared_config_state();
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
                data_dir: temp.path().join("data"),
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
                data_dir: temp.path().join("data"),
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
                data_dir: dir.path().join("data"),
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
                data_dir: dir.path().join("data"),
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
                data_dir: dir.path().join("data"),
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
    fn test_save_settings_seeds_single_file_magnet_metadata() {
        let dir = tempdir().expect("create tempdir");
        let backend = NormalConfigBackend {
            paths: NormalConfigPaths {
                settings_path: dir.path().join("settings.toml"),
                metadata_path: dir.path().join("torrent_metadata.toml"),
                backup_dir: dir.path().join("backups_settings_files"),
                data_dir: dir.path().join("data"),
            },
        };
        let settings = Settings {
            torrents: vec![TorrentSettings {
                torrent_or_magnet: concat!(
                    "magnet:?xt=urn:btih:2222222222222222222222222222222222222222",
                    "&dn=Sample%20Node.mkv",
                    "&xl=12345"
                )
                .to_string(),
                validation_status: true,
                ..TorrentSettings::default()
            }],
            ..Settings::default()
        };

        backend.save_settings(&settings).expect("save settings");

        let saved_metadata: TorrentMetadataConfig =
            read_toml_or_default(&backend.paths.metadata_path).expect("load metadata");
        assert_eq!(saved_metadata.torrents.len(), 1);
        let entry = &saved_metadata.torrents[0];
        assert_eq!(
            entry.info_hash_hex,
            "2222222222222222222222222222222222222222"
        );
        assert_eq!(entry.torrent_name, "Sample Node.mkv");
        assert_eq!(entry.total_size, 12345);
        assert_eq!(entry.files.len(), 1);
        assert_eq!(entry.files[0].relative_path, "Sample Node.mkv");
        assert_eq!(entry.files[0].length, 12345);
    }

    #[test]
    fn test_upsert_torrent_metadata_overwrites_invalid_metadata() {
        let dir = tempdir().expect("create tempdir");
        let backend = NormalConfigBackend {
            paths: NormalConfigPaths {
                settings_path: dir.path().join("settings.toml"),
                metadata_path: dir.path().join("torrent_metadata.toml"),
                backup_dir: dir.path().join("backups_settings_files"),
                data_dir: dir.path().join("data"),
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

    #[test]
    fn test_upsert_torrent_metadata_entry_reports_unchanged_entries() {
        let entry = TorrentMetadataEntry {
            info_hash_hex: "2222222222222222222222222222222222222222".to_string(),
            torrent_name: "Queued Sample".to_string(),
            ..TorrentMetadataEntry::default()
        };
        let mut metadata = TorrentMetadataConfig {
            torrents: vec![entry.clone()],
        };

        assert!(!upsert_torrent_metadata_entry(&mut metadata, entry.clone()));
        assert_eq!(metadata.torrents, vec![entry.clone()]);

        let changed_entry = TorrentMetadataEntry {
            torrent_name: "Updated Sample".to_string(),
            ..entry
        };
        assert!(upsert_torrent_metadata_entry(
            &mut metadata,
            changed_entry.clone()
        ));
        assert_eq!(metadata.torrents, vec![changed_entry]);
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
    fn test_set_persisted_shared_config_does_not_write_shared_settings() {
        let _guard = shared_backend_guard().lock().unwrap();
        let temp = set_temp_app_paths();
        let shared_root = temp.path().join("shared-root");
        fs::create_dir_all(&shared_root).expect("create mounted shared root");

        let selection =
            set_persisted_shared_config(&shared_root).expect("persist shared config path");

        let settings_path = selection.config_root.join("settings.toml");
        assert!(!settings_path.exists());

        let loaded = load_settings().expect("load shared settings");
        assert_eq!(loaded.default_download_folder, Some(selection.mount_root));

        set_app_paths_override_for_tests(None);
        clear_shared_config_state();
    }

    #[test]
    fn test_set_persisted_shared_config_preserves_existing_default_download_folder() {
        let _guard = shared_backend_guard().lock().unwrap();
        let temp = set_temp_app_paths();
        let shared_root = temp.path().join("shared-root");
        let explicit_config_root = shared_root.join(SHARED_CONFIG_SUBDIR);
        fs::create_dir_all(&explicit_config_root).expect("create shared config root");
        write_toml_atomically(
            &explicit_config_root.join("settings.toml"),
            &SharedSettingsConfig {
                default_download_folder: Some(PathBuf::from("old-downloads")),
                ..SharedSettingsConfig::default()
            },
        )
        .expect("write stale shared settings");

        let selection = set_persisted_shared_config(&explicit_config_root)
            .expect("persist explicit shared config path");

        let raw_settings: SharedSettingsConfig =
            read_toml_or_default(&selection.config_root.join("settings.toml"))
                .expect("read shared settings");
        assert_eq!(
            raw_settings.default_download_folder,
            Some(PathBuf::from("old-downloads"))
        );

        let loaded = load_settings().expect("load shared settings");
        assert_eq!(
            loaded.default_download_folder,
            Some(shared_root.join("old-downloads"))
        );

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
        let _extra_watch = EnvVarRestore::capture("SUPERSEEDR_WATCH_PATH_1");
        let dir = tempdir().expect("create tempdir");

        env::set_var(SHARED_CONFIG_DIR_ENV, dir.path());
        env::set_var(SHARED_HOST_ID_ENV, "node-a");
        env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        env::set_var("SUPERSEEDR_WATCH_PATH_1", "/extra-watch");
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
        assert!(configured.contains(&PathBuf::from("/extra-watch")));
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
    fn test_host_watch_paths_exclude_additional_shared_config_overlaps() {
        let _guard = watch_env_guard().lock().unwrap();
        let _shared_dir = EnvVarRestore::capture(SHARED_CONFIG_DIR_ENV);
        let _host_id = EnvVarRestore::capture(SHARED_HOST_ID_ENV);
        let _legacy_host_id = EnvVarRestore::capture(LEGACY_SHARED_HOST_ID_ENV);
        let _extra_watch_1 = EnvVarRestore::capture("SUPERSEEDR_WATCH_PATH_1");
        let _extra_watch_2 = EnvVarRestore::capture("SUPERSEEDR_WATCH_PATH_2");
        let _extra_watch_3 = EnvVarRestore::capture("SUPERSEEDR_WATCH_PATH_3");
        let dir = tempdir().expect("create tempdir");

        env::set_var(SHARED_CONFIG_DIR_ENV, dir.path());
        env::set_var(SHARED_HOST_ID_ENV, "node-a");
        env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        clear_shared_config_state();

        let effective_root = dir.path().join(SHARED_CONFIG_SUBDIR);
        let shared_inbox = effective_root.join("inbox");
        let explicit_watch = dir.path().join("explicit-host-watch");
        let local_extra_watch = dir.path().join("local-extra-watch");
        env::set_var("SUPERSEEDR_WATCH_PATH_1", &shared_inbox);
        env::set_var("SUPERSEEDR_WATCH_PATH_2", &effective_root);
        env::set_var("SUPERSEEDR_WATCH_PATH_3", &local_extra_watch);

        let settings = Settings {
            watch_folder: Some(explicit_watch.clone()),
            ..Settings::default()
        };

        let host_paths = host_watch_paths(&settings);
        assert!(host_paths.contains(&explicit_watch));
        assert!(host_paths.contains(&local_extra_watch));
        assert!(!host_paths.contains(&shared_inbox));
        assert!(!host_paths.contains(&effective_root));

        let follower_paths = runtime_watch_paths(&settings, true, false);
        assert!(follower_paths.contains(&effective_root));
        assert!(follower_paths.contains(&explicit_watch));
        assert!(follower_paths.contains(&local_extra_watch));
        assert!(!follower_paths.contains(&shared_inbox));

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
    fn test_shared_host_id_env_matches_case_insensitively() {
        const LOWER_SHARED_HOST_ID_ENV: &str = "superseedr_shared_host_id";

        let _guard = watch_env_guard().lock().unwrap();
        let _shared_dir = EnvVarRestore::capture(SHARED_CONFIG_DIR_ENV);
        let _host_id = EnvVarRestore::capture(SHARED_HOST_ID_ENV);
        let _lower_host_id = EnvVarRestore::capture(LOWER_SHARED_HOST_ID_ENV);
        let _legacy_host_id = EnvVarRestore::capture(LEGACY_SHARED_HOST_ID_ENV);
        let dir = tempdir().expect("create tempdir");

        env::set_var(SHARED_CONFIG_DIR_ENV, dir.path());
        env::remove_var(SHARED_HOST_ID_ENV);
        env::set_var(LOWER_SHARED_HOST_ID_ENV, "lower-node");
        env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        clear_shared_config_state();

        assert_eq!(shared_host_id().as_deref(), Some("lower-node"));

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
        host_only.always_show_add_location_prompt = true;
        host_only.network_binding.mode = crate::networking::runtime::NetworkBindingMode::Interface;
        host_only.network_binding.interface = Some("vpn-test0".to_string());
        assert_eq!(
            classify_shared_mode_settings_change(&current, &host_only),
            SettingsChangeScope::HostOnly
        );

        let mut random_port = current.clone();
        random_port.randomize_client_port = true;
        assert_eq!(
            classify_shared_mode_settings_change(&current, &random_port),
            SettingsChangeScope::HostOnly
        );

        let mut current_random_port = current.clone();
        current_random_port.client_port = 49152;
        current_random_port.randomize_client_port = true;
        let mut fixed_port = current_random_port.clone();
        fixed_port.client_port = 4300;
        fixed_port.randomize_client_port = false;
        assert_eq!(
            classify_shared_mode_settings_change(&current_random_port, &fixed_port),
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
    fn test_network_binding_is_serialized_only_in_host_config() {
        let mut settings = Settings::default();
        settings.network_binding.mode = crate::networking::runtime::NetworkBindingMode::Interface;
        settings.network_binding.interface = Some("vpn-test0".to_string());
        settings.network_binding.enable_ipv6 = false;
        settings.network_binding.dns_policy = crate::networking::runtime::DnsPolicy::Bound;
        settings.network_binding.dns_servers = vec!["192.0.2.53:53".parse().unwrap()];

        let host = toml::to_string(&HostConfig::from_flat_settings(&settings))
            .expect("serialize host config");
        let shared = toml::to_string(
            &SharedSettingsConfig::from_settings(&settings, None)
                .expect("construct shared settings"),
        )
        .expect("serialize shared settings");

        assert!(host.contains("[network_binding]"));
        assert!(host.contains("mode = \"interface\""));
        assert!(host.contains("interface = \"vpn-test0\""));
        assert!(host.contains("dns_policy = \"bound\""));
        assert!(host.contains("192.0.2.53:53"));
        assert!(!shared.contains("network_binding"));
        assert!(!shared.contains("vpn-test0"));
        assert!(!shared.contains("192.0.2.53:53"));
    }

    #[test]
    fn test_runtime_watch_paths_differ_by_shared_role() {
        let _guard = watch_env_guard().lock().unwrap();
        let original_shared_dir = env::var_os(SHARED_CONFIG_DIR_ENV);
        let original_host_id = env::var_os(SHARED_HOST_ID_ENV);
        let original_legacy_host_id = env::var_os(LEGACY_SHARED_HOST_ID_ENV);
        let _extra_watch = EnvVarRestore::capture("SUPERSEEDR_WATCH_PATH_1");
        let dir = tempdir().expect("create tempdir");

        env::set_var(SHARED_CONFIG_DIR_ENV, dir.path());
        env::set_var(SHARED_HOST_ID_ENV, "node-a");
        env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        env::set_var("SUPERSEEDR_WATCH_PATH_1", "/extra-watch");
        clear_shared_config_state();

        let settings = Settings {
            watch_folder: Some(PathBuf::from("/host-watch")),
            ..Settings::default()
        };
        let effective_root = dir.path().join(SHARED_CONFIG_SUBDIR);

        let follower_paths = runtime_watch_paths(&settings, true, false);
        assert!(follower_paths.contains(&PathBuf::from("/host-watch")));
        assert!(follower_paths.contains(&PathBuf::from("/extra-watch")));
        assert!(follower_paths.contains(&effective_root));
        assert!(!follower_paths.contains(&effective_root.join("inbox")));

        let leader_paths = runtime_watch_paths(&settings, true, true);
        assert!(leader_paths.contains(&effective_root.join("inbox")));
        assert!(leader_paths.contains(&PathBuf::from("/extra-watch")));

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
        let _extra_watch = EnvVarRestore::capture("SUPERSEEDR_WATCH_PATH_1");
        let dir = tempdir().expect("create tempdir");

        env::set_var(SHARED_CONFIG_DIR_ENV, dir.path());
        env::set_var(SHARED_HOST_ID_ENV, "node-a");
        env::remove_var(LEGACY_SHARED_HOST_ID_ENV);
        env::set_var("SUPERSEEDR_WATCH_PATH_1", "/extra-watch");
        clear_shared_config_state();

        let settings = Settings::default();
        let effective_root = dir.path().join(SHARED_CONFIG_SUBDIR);
        let local_watch = get_watch_path().map(|(watch_path, _)| watch_path);

        let follower_paths = runtime_watch_paths(&settings, true, false);
        assert!(follower_paths.contains(&effective_root));
        assert!(follower_paths.contains(&PathBuf::from("/extra-watch")));
        assert!(!follower_paths.contains(&effective_root.join("inbox")));
        if let Some(local_watch) = &local_watch {
            assert!(follower_paths.contains(local_watch));
        }

        let leader_paths = runtime_watch_paths(&settings, true, true);
        assert!(leader_paths.contains(&effective_root.join("inbox")));
        assert!(leader_paths.contains(&PathBuf::from("/extra-watch")));
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
