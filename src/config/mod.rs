// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::app::{
    DataRate, DhtVisualization, DiskHealthVisualization, FilePriority, PeerStreamVisualization,
    TorrentControlState,
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

/// A host-owned ICE service. TURN credentials belong in the host configuration.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(default)]
pub struct WebRtcIceServer {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
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
    /// ICE discovery servers for WebTorrent; empty uses host candidates only.
    pub webtorrent_ice_servers: Vec<crate::config::WebRtcIceServer>,
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
    pub peer_stream_visualization: PeerStreamVisualization,
    pub disk_health_visualization: DiskHealthVisualization,
    pub dht_visualization: DhtVisualization,
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
            webtorrent_ice_servers: Vec::new(),
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
            peer_stream_visualization: PeerStreamVisualization::default(),
            disk_health_visualization: DiskHealthVisualization::default(),
            dht_visualization: DhtVisualization::default(),
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

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::*;
