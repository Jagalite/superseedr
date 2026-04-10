// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::TorrentMetrics;
use crate::config::Settings;
use crate::dht_service::{configured_status_from_settings, DhtStatus};
use crate::fs_atomic::{
    deserialize_versioned_json, serialize_versioned_json, write_string_atomically,
};
use serde::de::Error;
use serde::ser::SerializeStruct;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::torrent_identity::info_hash_from_torrent_source;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AppOutputState {
    pub run_time: u64,
    pub cpu_usage: f32,
    pub ram_usage_percent: f32,
    pub total_download_bps: u64,
    pub total_upload_bps: u64,
    pub status_config: StatusConfig,
    #[serde(default)]
    pub dht: DhtStatus,
    #[serde(
        serialize_with = "serialize_torrents_hex",
        deserialize_with = "deserialize_torrents_hex"
    )]
    pub torrents: HashMap<Vec<u8>, TorrentMetrics>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct StatusConfig {
    pub client_port: u16,
    pub output_status_interval: u64,
    pub shared_mode: bool,
    pub host_id: Option<String>,
    pub default_download_folder: Option<PathBuf>,
    pub watch_folder: Option<PathBuf>,
}

pub fn serialize_torrents_hex<S>(
    map: &HashMap<Vec<u8>, TorrentMetrics>,
    s: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap;
    let mut map_ser = s.serialize_map(Some(map.len()))?;
    for (k, v) in map {
        map_ser.serialize_entry(&hex::encode(k), &StatusTorrentMetrics::new(v))?;
    }
    map_ser.end()
}

pub fn deserialize_torrents_hex<'de, D>(
    deserializer: D,
) -> Result<HashMap<Vec<u8>, TorrentMetrics>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = HashMap::<String, TorrentMetrics>::deserialize(deserializer)?;
    raw.into_iter()
        .map(|(key, value)| {
            hex::decode(&key)
                .map(|decoded| (decoded, value))
                .map_err(D::Error::custom)
        })
        .collect()
}

struct StatusTorrentMetrics<'a> {
    metrics: &'a TorrentMetrics,
}

impl<'a> StatusTorrentMetrics<'a> {
    fn new(metrics: &'a TorrentMetrics) -> Self {
        Self { metrics }
    }
}

impl Serialize for StatusTorrentMetrics<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("TorrentMetrics", 29)?;
        state.serialize_field("info_hash_hex", &hex::encode(&self.metrics.info_hash))?;
        state.serialize_field("torrent_control_state", &self.metrics.torrent_control_state)?;
        state.serialize_field("delete_files", &self.metrics.delete_files)?;
        state.serialize_field("info_hash", &self.metrics.info_hash)?;
        state.serialize_field("torrent_or_magnet", &self.metrics.torrent_or_magnet)?;
        state.serialize_field("torrent_name", &self.metrics.torrent_name)?;
        state.serialize_field("download_path", &self.metrics.download_path)?;
        state.serialize_field("container_name", &self.metrics.container_name)?;
        state.serialize_field("is_multi_file", &self.metrics.is_multi_file)?;
        state.serialize_field("file_count", &self.metrics.file_count)?;
        state.serialize_field("file_priorities", &self.metrics.file_priorities)?;
        state.serialize_field("data_available", &self.metrics.data_available)?;
        state.serialize_field("is_complete", &self.metrics.is_complete)?;
        state.serialize_field(
            "number_of_successfully_connected_peers",
            &self.metrics.number_of_successfully_connected_peers,
        )?;
        state.serialize_field(
            "number_of_pieces_total",
            &self.metrics.number_of_pieces_total,
        )?;
        state.serialize_field(
            "number_of_pieces_completed",
            &self.metrics.number_of_pieces_completed,
        )?;
        state.serialize_field("download_speed_bps", &self.metrics.download_speed_bps)?;
        state.serialize_field("upload_speed_bps", &self.metrics.upload_speed_bps)?;
        state.serialize_field(
            "bytes_downloaded_this_tick",
            &self.metrics.bytes_downloaded_this_tick,
        )?;
        state.serialize_field(
            "bytes_uploaded_this_tick",
            &self.metrics.bytes_uploaded_this_tick,
        )?;
        state.serialize_field(
            "session_total_downloaded",
            &self.metrics.session_total_downloaded,
        )?;
        state.serialize_field(
            "session_total_uploaded",
            &self.metrics.session_total_uploaded,
        )?;
        state.serialize_field("eta", &self.metrics.eta)?;
        state.serialize_field("activity_message", &self.metrics.activity_message)?;
        state.serialize_field("next_announce_in", &self.metrics.next_announce_in)?;
        state.serialize_field("total_size", &self.metrics.total_size)?;
        state.serialize_field("bytes_written", &self.metrics.bytes_written)?;
        state.serialize_field("blocks_in_this_tick", &self.metrics.blocks_in_this_tick)?;
        state.serialize_field("blocks_out_this_tick", &self.metrics.blocks_out_this_tick)?;
        state.end()
    }
}

pub fn dump(
    output_data: AppOutputState,
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
    mirror_to_leader_path: bool,
    generation: u64,
    latest_generation: Arc<AtomicU64>,
) {
    let file_path = host_status_file_path().unwrap_or_else(|_| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("status_files")
            .join("app_state.json")
    });
    let leader_path = if mirror_to_leader_path {
        crate::config::shared_leader_status_path()
    } else {
        None
    };
    let mut shutdown_rx = shutdown_tx.subscribe();

    tokio::spawn(async move {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                tracing::debug!("Status dump aborted due to application shutdown");
            }
            result = tokio::task::spawn_blocking(move || {
                if should_skip_status_dump(generation, &latest_generation) {
                    return Ok::<(), io::Error>(());
                }
                if let Some(parent) = file_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let json = serialize_versioned_json(&output_data)?;
                if should_skip_status_dump(generation, &latest_generation) {
                    return Ok::<(), io::Error>(());
                }
                write_string_atomically(&file_path, &json)?;
                if let Some(leader_path) = leader_path {
                    if should_skip_status_dump(generation, &latest_generation) {
                        return Ok::<(), io::Error>(());
                    }
                    if let Some(parent) = leader_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    write_string_atomically(&leader_path, &json)?;
                }
                Ok::<(), io::Error>(())
            }) => {
                if let Ok(Err(e)) = result {
                    tracing::error!("Failed to write status dump: {:?}", e);
                }
            }
        }
    });
}

fn should_skip_status_dump(generation: u64, latest_generation: &AtomicU64) -> bool {
    generation != latest_generation.load(Ordering::Acquire)
}

pub fn host_status_file_path() -> io::Result<PathBuf> {
    if let Some(shared_path) = crate::config::shared_status_path() {
        return Ok(shared_path);
    }

    let base_path = crate::config::runtime_data_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Could not resolve app data directory",
        )
    })?;
    Ok(base_path.join("status_files").join("app_state.json"))
}

pub fn cluster_status_file_path() -> io::Result<PathBuf> {
    if let Some(shared_path) = crate::config::shared_leader_status_path() {
        return Ok(shared_path);
    }

    host_status_file_path()
}

pub fn status_file_path() -> io::Result<PathBuf> {
    cluster_status_file_path()
}

pub fn read_cluster_output_state() -> io::Result<AppOutputState> {
    let content = fs::read_to_string(cluster_status_file_path()?)?;
    deserialize_versioned_json(&content)
}

pub fn offline_output_state(settings: &Settings) -> AppOutputState {
    let torrents = settings
        .torrents
        .iter()
        .filter_map(torrent_metrics_from_settings)
        .map(|metrics| (metrics.info_hash.clone(), metrics))
        .collect();

    AppOutputState {
        run_time: 0,
        cpu_usage: 0.0,
        ram_usage_percent: 0.0,
        total_download_bps: 0,
        total_upload_bps: 0,
        status_config: status_config_from_settings(settings),
        dht: configured_status_from_settings(settings),
        torrents,
    }
}

pub fn offline_output_json(settings: &Settings) -> io::Result<String> {
    serde_json::to_string_pretty(&offline_output_state(settings)).map_err(io::Error::other)
}

fn torrent_metrics_from_settings(
    torrent_settings: &crate::config::TorrentSettings,
) -> Option<TorrentMetrics> {
    let info_hash = info_hash_from_torrent_source(&torrent_settings.torrent_or_magnet)?;

    Some(TorrentMetrics {
        torrent_control_state: torrent_settings.torrent_control_state.clone(),
        info_hash,
        torrent_or_magnet: torrent_settings.torrent_or_magnet.clone(),
        torrent_name: torrent_settings.name.clone(),
        download_path: torrent_settings.download_path.clone(),
        container_name: torrent_settings.container_name.clone(),
        file_priorities: torrent_settings.file_priorities.clone(),
        is_complete: torrent_settings.validation_status,
        activity_message: "Offline settings snapshot".to_string(),
        ..Default::default()
    })
}

pub fn status_config_from_settings(settings: &Settings) -> StatusConfig {
    StatusConfig {
        client_port: settings.client_port,
        output_status_interval: settings.output_status_interval,
        shared_mode: crate::config::is_shared_config_mode(),
        host_id: crate::config::shared_host_id(),
        default_download_folder: settings.default_download_folder.clone(),
        watch_folder: settings.watch_folder.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::TorrentMetrics;
    use crate::config::{Settings, TorrentSettings};
    use std::collections::HashMap;

    #[test]
    fn test_serialize_torrents_hex_keys() {
        let mut torrents = HashMap::new();

        // Create a fake info hash (5 bytes for simplicity)
        // 0xAA = 170, 0xBB = 187, etc.
        let info_hash = vec![0xAA, 0xBB, 0xCC, 0x12, 0x34];
        let info_hash_key = info_hash.clone();

        let metrics = TorrentMetrics {
            info_hash, // This field will still serialize to [170, 187, ...]
            torrent_name: "Test Torrent".to_string(),
            ..Default::default()
        };

        torrents.insert(info_hash_key, metrics);

        let output = AppOutputState {
            run_time: 100,
            cpu_usage: 5.5,
            ram_usage_percent: 10.0,
            total_download_bps: 1024,
            total_upload_bps: 512,
            status_config: StatusConfig {
                client_port: 8080,
                output_status_interval: 15,
                shared_mode: false,
                host_id: None,
                default_download_folder: None,
                watch_folder: None,
            },
            dht: DhtStatus::default(),
            torrents,
        };

        let json = serde_json::to_string(&output).expect("Serialization failed");

        // The key in the JSON map MUST be the hex string "aabbcc1234"
        assert!(
            json.contains("\"aabbcc1234\":"),
            "JSON should contain hex-encoded key"
        );
        assert!(
            json.contains("\"info_hash_hex\":\"aabbcc1234\""),
            "JSON should contain info_hash_hex in the torrent payload"
        );

        // We removed the negative assertion (!json.contains("[170,187")) because
        // the 'metrics.info_hash' field inside the object is expected to be a byte array.
    }

    #[test]
    fn offline_output_json_builds_snapshot_from_settings() {
        let settings = Settings {
            client_port: 6681,
            output_status_interval: 10,
            watch_folder: Some("/watch".into()),
            default_download_folder: Some("/downloads".into()),
            torrents: vec![TorrentSettings {
                torrent_or_magnet: "magnet:?xt=urn:btih:1111111111111111111111111111111111111111"
                    .to_string(),
                name: "Sample Alpha".to_string(),
                validation_status: true,
                ..Default::default()
            }],
            ..Default::default()
        };

        let json = offline_output_json(&settings).expect("serialize offline output");

        assert!(json.contains("\"status_config\""));
        assert!(json.contains("\"dht\""));
        assert!(json.contains("\"client_port\": 6681"));
        assert!(json.contains("\"output_status_interval\": 10"));
        assert!(json.contains("\"watch_folder\": \"/watch\""));
        assert!(json.contains("\"default_download_folder\": \"/downloads\""));
        assert!(json.contains("\"1111111111111111111111111111111111111111\""));
        assert!(json.contains("Offline settings snapshot"));
    }

    #[test]
    fn stale_status_dump_generations_are_skipped() {
        let latest_generation = AtomicU64::new(4);

        assert!(should_skip_status_dump(3, &latest_generation));
        assert!(!should_skip_status_dump(4, &latest_generation));
    }

    #[test]
    fn offline_output_state_includes_bootstrap_family_counts() {
        let settings = Settings {
            bootstrap_nodes: vec![
                "127.0.0.1:6881".to_string(),
                "[::1]:6881".to_string(),
                "not-an-address".to_string(),
            ],
            ..Default::default()
        };

        let output = offline_output_state(&settings);

        assert_eq!(output.dht.health.ipv4_bootstrap_nodes, 1);
        assert_eq!(output.dht.health.ipv6_bootstrap_nodes, 1);
    }

    #[test]
    fn read_cluster_output_state_defaults_missing_dht_field() {
        let json = r#"{
          "version": 1,
          "run_time": 0,
          "cpu_usage": 0.0,
          "ram_usage_percent": 0.0,
          "total_download_bps": 0,
          "total_upload_bps": 0,
          "status_config": {
            "client_port": 0,
            "output_status_interval": 0,
            "shared_mode": false,
            "host_id": null,
            "default_download_folder": null,
            "watch_folder": null
          },
          "torrents": {}
        }"#;

        let output: AppOutputState =
            deserialize_versioned_json(json).expect("deserialize legacy status snapshot");

        assert_eq!(output.dht, DhtStatus::default());
    }
}
