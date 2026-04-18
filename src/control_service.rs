// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::FilePriority;
use crate::config::{load_torrent_metadata, Settings, TorrentMetadataEntry, TorrentSettings};
use crate::integrations::control::{
    ControlFilePriorityOverride, ControlPriorityTarget, ControlRequest,
};
use crate::persistence::event_journal::{ControlOrigin, EventDetails};
use crate::storage::{FileInfo, MultiFileInfo};
use crate::torrent_file::parser::from_bytes;
use crate::torrent_identity::{decode_info_hash, info_hash_from_torrent_source};
use crate::torrent_manager::state::calculate_deletion_lists;
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

type TorrentFileList = Vec<(Vec<String>, u64)>;
type TorrentMetadataByInfoHash = HashMap<String, TorrentMetadataEntry>;

fn load_torrent_metadata_snapshot() -> Result<TorrentMetadataByInfoHash, String> {
    let metadata = match load_torrent_metadata() {
        Ok(metadata) => metadata,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error
                    .to_string()
                    .contains("Could not resolve application config directory") =>
        {
            return Ok(HashMap::new());
        }
        Err(error) => {
            return Err(format!(
                "Failed to load persisted torrent metadata: {}",
                error
            ));
        }
    };
    Ok(metadata
        .torrents
        .into_iter()
        .map(|entry| (entry.info_hash_hex.clone(), entry))
        .collect())
}

pub fn find_torrent_settings_index_by_info_hash(
    settings: &Settings,
    info_hash: &[u8],
) -> Option<usize> {
    settings.torrents.iter().position(|torrent| {
        info_hash_from_torrent_source(&torrent.torrent_or_magnet).as_deref() == Some(info_hash)
    })
}

pub fn describe_priority_target(target: &ControlPriorityTarget) -> String {
    match target {
        ControlPriorityTarget::FileIndex(index) => format!("index {}", index),
        ControlPriorityTarget::FilePath(path) => format!("path {}", path),
    }
}

pub fn online_control_success_message(request: &ControlRequest) -> String {
    match request {
        ControlRequest::Pause { info_hash_hex } => {
            format!("Queued pause request for torrent '{}'", info_hash_hex)
        }
        ControlRequest::Resume { info_hash_hex } => {
            format!("Queued resume request for torrent '{}'", info_hash_hex)
        }
        ControlRequest::Delete {
            info_hash_hex,
            delete_files,
        } => {
            if *delete_files {
                format!("Queued purge request for torrent '{}'", info_hash_hex)
            } else {
                format!("Queued remove request for torrent '{}'", info_hash_hex)
            }
        }
        ControlRequest::SetFilePriority {
            info_hash_hex,
            target,
            priority,
        } => format!(
            "Queued file priority request for torrent '{}' ({}) -> {:?}",
            info_hash_hex,
            describe_priority_target(target),
            priority
        ),
        ControlRequest::AddTorrentFile { source_path, .. } => format!(
            "Queued add request for torrent file '{}'",
            source_path.display()
        ),
        ControlRequest::AddMagnet { magnet_link, .. } => {
            let label = magnet_link
                .split('&')
                .next()
                .unwrap_or(magnet_link.as_str());
            format!("Queued add request for magnet '{}'", label)
        }
        ControlRequest::StatusNow
        | ControlRequest::StatusFollowStart { .. }
        | ControlRequest::StatusFollowStop => "Queued control request.".to_string(),
    }
}

pub fn control_event_details(request: &ControlRequest, origin: ControlOrigin) -> EventDetails {
    let (file_index, file_path) = match request.priority_target() {
        Some(ControlPriorityTarget::FileIndex(index)) => (Some(*index), None),
        Some(ControlPriorityTarget::FilePath(path)) => (None, Some(path.clone())),
        None => (None, None),
    };

    EventDetails::Control {
        origin,
        action: request.action_name().to_string(),
        target_info_hash_hex: request.target_info_hash_hex().map(str::to_string),
        file_index,
        file_path,
        priority: request
            .priority_value()
            .map(|priority| format!("{:?}", priority)),
    }
}

pub fn load_torrent_file_list_for_settings(
    torrent_settings: &TorrentSettings,
) -> Result<Vec<(Vec<String>, u64)>, String> {
    let metadata_by_info_hash = load_torrent_metadata_snapshot()?;
    if let Some(metadata_files) =
        load_torrent_file_list_from_metadata(torrent_settings, &metadata_by_info_hash)?
    {
        return Ok(metadata_files);
    }

    if torrent_settings.torrent_or_magnet.starts_with("magnet:") {
        return Err(
            "This torrent does not have a persisted .torrent source for file path lookup"
                .to_string(),
        );
    }

    let bytes = fs::read(&torrent_settings.torrent_or_magnet).map_err(|error| {
        format!(
            "Failed to read torrent metadata from '{}': {}",
            torrent_settings.torrent_or_magnet, error
        )
    })?;
    let torrent = from_bytes(&bytes).map_err(|error| {
        format!(
            "Failed to parse torrent metadata from '{}': {:?}",
            torrent_settings.torrent_or_magnet, error
        )
    })?;
    Ok(torrent.file_list())
}

fn load_torrent_file_list_from_metadata(
    torrent_settings: &TorrentSettings,
    metadata_by_info_hash: &TorrentMetadataByInfoHash,
) -> Result<Option<TorrentFileList>, String> {
    let Some(info_hash) = info_hash_from_torrent_source(&torrent_settings.torrent_or_magnet) else {
        return Ok(None);
    };
    let info_hash_hex = hex::encode(info_hash);
    let Some(entry) = metadata_by_info_hash.get(&info_hash_hex) else {
        return Ok(None);
    };
    if entry.files.is_empty() {
        return Ok(None);
    }
    Ok(Some(file_list_from_metadata_entry(entry)))
}

fn file_list_from_metadata_entry(entry: &TorrentMetadataEntry) -> Vec<(Vec<String>, u64)> {
    entry
        .files
        .iter()
        .map(|file| {
            (
                file.relative_path
                    .split('/')
                    .filter(|segment| !segment.is_empty())
                    .map(|segment| segment.to_string())
                    .collect(),
                file.length,
            )
        })
        .collect()
}

pub fn file_priorities_to_map(
    values: &[ControlFilePriorityOverride],
) -> HashMap<usize, FilePriority> {
    values
        .iter()
        .filter(|value| !matches!(value.priority, FilePriority::Normal))
        .map(|value| (value.file_index, value.priority))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TorrentFileListEntry {
    pub file_index: usize,
    pub relative_path: String,
    pub full_path: Option<PathBuf>,
    pub length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflinePurgePlan {
    pub info_hash_hex: String,
    pub files: Vec<PathBuf>,
    pub directories: Vec<PathBuf>,
}

fn torrent_settings_by_info_hash_hex<'a>(
    settings: &'a Settings,
    info_hash_hex: &str,
) -> Result<(usize, &'a TorrentSettings, Vec<u8>), String> {
    let info_hash = decode_info_hash(info_hash_hex)?;
    let index = find_torrent_settings_index_by_info_hash(settings, &info_hash)
        .ok_or_else(|| format!("Torrent '{}' was not found", info_hash_hex))?;
    let torrent = settings
        .torrents
        .get(index)
        .ok_or_else(|| format!("Torrent '{}' was not found", info_hash_hex))?;
    Ok((index, torrent, info_hash))
}

fn torrent_name_for_manifest(
    torrent_settings: &TorrentSettings,
    metadata_entry: Option<&TorrentMetadataEntry>,
) -> String {
    if let Some(entry) = metadata_entry {
        if !entry.torrent_name.is_empty() {
            return entry.torrent_name.clone();
        }
    }
    if !torrent_settings.name.is_empty() {
        return torrent_settings.name.clone();
    }
    "Unnamed Torrent".to_string()
}

fn torrent_metadata_entry_for_settings(
    torrent_settings: &TorrentSettings,
    metadata_by_info_hash: &TorrentMetadataByInfoHash,
) -> Result<Option<TorrentMetadataEntry>, String> {
    let Some(info_hash) = info_hash_from_torrent_source(&torrent_settings.torrent_or_magnet) else {
        return Ok(None);
    };
    let info_hash_hex = hex::encode(info_hash);
    Ok(metadata_by_info_hash.get(&info_hash_hex).cloned())
}

fn manifest_entries_for_torrent_settings(
    torrent_settings: &TorrentSettings,
    metadata_by_info_hash: &TorrentMetadataByInfoHash,
) -> Result<(String, bool, Vec<TorrentFileListEntry>), String> {
    if let Some(entry) =
        torrent_metadata_entry_for_settings(torrent_settings, metadata_by_info_hash)?
    {
        if !entry.files.is_empty() {
            let torrent_name = torrent_name_for_manifest(torrent_settings, Some(&entry));
            let files = entry
                .files
                .into_iter()
                .enumerate()
                .map(|(file_index, file)| TorrentFileListEntry {
                    file_index,
                    relative_path: file.relative_path,
                    full_path: None,
                    length: file.length,
                })
                .collect();
            return Ok((torrent_name, entry.is_multi_file, files));
        }
    }

    if torrent_settings.torrent_or_magnet.starts_with("magnet:") {
        return Err(
            "This torrent does not have persisted file metadata yet. Start the torrent once or use INFO_HASH_HEX without a file path."
                .to_string(),
        );
    }

    let bytes = fs::read(&torrent_settings.torrent_or_magnet).map_err(|error| {
        format!(
            "Failed to read torrent metadata from '{}': {}",
            torrent_settings.torrent_or_magnet, error
        )
    })?;
    let torrent = from_bytes(&bytes).map_err(|error| {
        format!(
            "Failed to parse torrent metadata from '{}': {:?}",
            torrent_settings.torrent_or_magnet, error
        )
    })?;
    let files = torrent
        .file_list()
        .into_iter()
        .enumerate()
        .map(|(file_index, (parts, length))| TorrentFileListEntry {
            file_index,
            relative_path: parts.join("/"),
            full_path: None,
            length,
        })
        .collect();
    Ok((
        torrent.info.name.clone(),
        !torrent.info.files.is_empty(),
        files,
    ))
}

fn normalize_match_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical;
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };

    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn resolve_torrent_roots(
    settings: &Settings,
    torrent_settings: &TorrentSettings,
    info_hash_hex: &str,
    is_multi_file: bool,
    torrent_name: &str,
) -> Result<(PathBuf, PathBuf), String> {
    let download_root = torrent_settings
        .download_path
        .clone()
        .or_else(|| settings.default_download_folder.clone())
        .ok_or_else(|| {
            format!(
                "Torrent '{}' does not have a resolved download path for purge",
                info_hash_hex
            )
        })?;

    let effective_root = if is_multi_file {
        match &torrent_settings.container_name {
            Some(name) if !name.is_empty() => download_root.join(name),
            Some(_) => download_root.clone(),
            None => download_root.join(format!("{} [{}]", torrent_name, info_hash_hex)),
        }
    } else {
        download_root.clone()
    };

    Ok((download_root, effective_root))
}

fn full_file_paths_for_torrent(
    settings: &Settings,
    info_hash_hex: &str,
    torrent_settings: &TorrentSettings,
    metadata_by_info_hash: &TorrentMetadataByInfoHash,
) -> Result<Vec<PathBuf>, String> {
    let (torrent_name, is_multi_file, files) =
        manifest_entries_for_torrent_settings(torrent_settings, metadata_by_info_hash)?;
    let (_, effective_root) = resolve_torrent_roots(
        settings,
        torrent_settings,
        info_hash_hex,
        is_multi_file,
        &torrent_name,
    )?;

    Ok(files
        .into_iter()
        .map(|file| {
            let mut path = effective_root.clone();
            for segment in file
                .relative_path
                .split('/')
                .filter(|segment| !segment.is_empty())
            {
                path.push(segment);
            }
            path
        })
        .collect())
}

pub fn list_torrent_files(
    settings: &Settings,
    info_hash_hex: &str,
) -> Result<Vec<TorrentFileListEntry>, String> {
    let metadata_by_info_hash = load_torrent_metadata_snapshot()?;
    let (_, torrent_settings, _) = torrent_settings_by_info_hash_hex(settings, info_hash_hex)?;
    let (_, _, mut files) =
        manifest_entries_for_torrent_settings(torrent_settings, &metadata_by_info_hash)?;
    if let Ok(paths) = full_file_paths_for_torrent(
        settings,
        info_hash_hex,
        torrent_settings,
        &metadata_by_info_hash,
    ) {
        for (entry, path) in files.iter_mut().zip(paths) {
            entry.full_path = Some(path);
        }
    }
    Ok(files)
}

pub fn resolve_target_info_hash(
    settings: &Settings,
    target: &str,
    command_name: &str,
) -> Result<String, String> {
    if decode_info_hash(target).is_ok() {
        let (_, _, _) = torrent_settings_by_info_hash_hex(settings, target)?;
        return Ok(target.to_string());
    }

    let normalized_target = normalize_match_path(Path::new(target));
    let mut matches = Vec::new();
    let metadata_by_info_hash = load_torrent_metadata_snapshot()?;

    for torrent in &settings.torrents {
        let Some(info_hash) = info_hash_from_torrent_source(&torrent.torrent_or_magnet) else {
            continue;
        };
        let info_hash_hex = hex::encode(info_hash);
        let Ok(paths) =
            full_file_paths_for_torrent(settings, &info_hash_hex, torrent, &metadata_by_info_hash)
        else {
            continue;
        };
        if paths
            .into_iter()
            .map(|path| normalize_match_path(&path))
            .any(|path| path == normalized_target)
        {
            matches.push(info_hash_hex);
        }
    }

    matches.sort();
    matches.dedup();

    match matches.len() {
        0 => Err(format!(
            "No torrent matched file path '{}'. Use `superseedr files <info-hash>` to inspect a torrent or rerun `superseedr {} <info-hash>`.",
            target, command_name
        )),
        1 => Ok(matches.remove(0)),
        _ => Err(format!(
            "File path '{}' matched multiple torrents. Re-run with INFO_HASH_HEX using `superseedr {} <info-hash>`.",
            target, command_name
        )),
    }
}

pub fn resolve_purge_target_info_hash(settings: &Settings, target: &str) -> Result<String, String> {
    resolve_target_info_hash(settings, target, "purge")
}

pub fn build_offline_purge_plan(
    settings: &Settings,
    info_hash_hex: &str,
) -> Result<OfflinePurgePlan, String> {
    let metadata_by_info_hash = load_torrent_metadata_snapshot()?;
    let (_, torrent_settings, _) = torrent_settings_by_info_hash_hex(settings, info_hash_hex)?;
    let (torrent_name, is_multi_file, files) =
        manifest_entries_for_torrent_settings(torrent_settings, &metadata_by_info_hash)?;
    if files.is_empty() {
        return Err(format!(
            "Torrent '{}' does not have persisted file paths available for offline purge",
            info_hash_hex
        ));
    }

    let (download_root, effective_root) = resolve_torrent_roots(
        settings,
        torrent_settings,
        info_hash_hex,
        is_multi_file,
        &torrent_name,
    )?;

    let mut current_offset = 0;
    let multi_file_info = MultiFileInfo {
        files: files
            .into_iter()
            .map(|file| {
                let mut path = effective_root.clone();
                for segment in file
                    .relative_path
                    .split('/')
                    .filter(|segment| !segment.is_empty())
                {
                    path.push(segment);
                }

                let file_info = FileInfo {
                    path,
                    length: file.length,
                    global_start_offset: current_offset,
                    is_padding: false,
                    is_skipped: matches!(
                        torrent_settings.file_priorities.get(&file.file_index),
                        Some(FilePriority::Skip)
                    ),
                };
                current_offset += file.length;
                file_info
            })
            .collect(),
        total_size: current_offset,
    };

    let (files, directories) = calculate_deletion_lists(
        &multi_file_info,
        &download_root,
        torrent_settings.container_name.as_deref(),
    );

    Ok(OfflinePurgePlan {
        info_hash_hex: info_hash_hex.to_string(),
        files,
        directories,
    })
}

pub fn apply_offline_purge(settings: &mut Settings, info_hash_hex: &str) -> Result<String, String> {
    let plan = build_offline_purge_plan(settings, info_hash_hex)?;

    for file_path in &plan.files {
        if let Err(error) = fs::remove_file(file_path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(format!("Failed to delete file {:?}: {}", file_path, error));
            }
        }
    }

    for dir_path in &plan.directories {
        if let Err(error) = fs::remove_dir(dir_path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::info!("Skipped dir deletion {:?}: {}", dir_path, error);
            }
        }
    }

    let info_hash = decode_info_hash(info_hash_hex)?;
    settings.torrents.retain(|torrent| {
        info_hash_from_torrent_source(&torrent.torrent_or_magnet).as_deref()
            != Some(info_hash.as_slice())
    });

    Ok(format!("Purged torrent '{}'", info_hash_hex))
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum ControlExecutionPlan {
    StatusNow,
    StatusFollowStart {
        interval_secs: u64,
    },
    StatusFollowStop,
    ApplySettings {
        next_settings: Settings,
        success_message: String,
    },
    AddTorrentFile {
        source_path: PathBuf,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        file_priorities: HashMap<usize, FilePriority>,
    },
    AddMagnet {
        magnet_link: String,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        file_priorities: HashMap<usize, FilePriority>,
    },
}

pub fn plan_control_request(
    settings: &Settings,
    request: &ControlRequest,
) -> Result<ControlExecutionPlan, String> {
    match request {
        ControlRequest::StatusNow => Ok(ControlExecutionPlan::StatusNow),
        ControlRequest::StatusFollowStart { interval_secs } => {
            Ok(ControlExecutionPlan::StatusFollowStart {
                interval_secs: (*interval_secs).max(1),
            })
        }
        ControlRequest::StatusFollowStop => Ok(ControlExecutionPlan::StatusFollowStop),
        ControlRequest::Pause { info_hash_hex } => {
            let info_hash = decode_info_hash(info_hash_hex)?;
            let Some(index) = find_torrent_settings_index_by_info_hash(settings, &info_hash) else {
                return Err(format!("Torrent '{}' was not found", info_hash_hex));
            };
            let mut next_settings = settings.clone();
            next_settings.torrents[index].torrent_control_state =
                crate::app::TorrentControlState::Paused;
            Ok(ControlExecutionPlan::ApplySettings {
                next_settings,
                success_message: format!("Paused torrent '{}'", info_hash_hex),
            })
        }
        ControlRequest::Resume { info_hash_hex } => {
            let info_hash = decode_info_hash(info_hash_hex)?;
            let Some(index) = find_torrent_settings_index_by_info_hash(settings, &info_hash) else {
                return Err(format!("Torrent '{}' was not found", info_hash_hex));
            };
            let mut next_settings = settings.clone();
            next_settings.torrents[index].torrent_control_state =
                crate::app::TorrentControlState::Running;
            Ok(ControlExecutionPlan::ApplySettings {
                next_settings,
                success_message: format!("Resumed torrent '{}'", info_hash_hex),
            })
        }
        ControlRequest::Delete {
            info_hash_hex,
            delete_files,
        } => {
            let info_hash = decode_info_hash(info_hash_hex)?;
            let Some(index) = find_torrent_settings_index_by_info_hash(settings, &info_hash) else {
                return Err(format!("Torrent '{}' was not found", info_hash_hex));
            };
            let mut next_settings = settings.clone();
            if *delete_files {
                next_settings.torrents[index].torrent_control_state =
                    crate::app::TorrentControlState::Deleting;
                next_settings.torrents[index].delete_files = true;
            } else {
                next_settings.torrents.retain(|torrent| {
                    info_hash_from_torrent_source(&torrent.torrent_or_magnet).as_deref()
                        != Some(info_hash.as_slice())
                });
            }
            Ok(ControlExecutionPlan::ApplySettings {
                next_settings,
                success_message: if *delete_files {
                    format!("Queued purge for torrent '{}'", info_hash_hex)
                } else {
                    format!("Removed torrent '{}'", info_hash_hex)
                },
            })
        }
        ControlRequest::SetFilePriority {
            info_hash_hex,
            target,
            priority,
        } => {
            let info_hash = decode_info_hash(info_hash_hex)?;
            let Some(index) = find_torrent_settings_index_by_info_hash(settings, &info_hash) else {
                return Err(format!("Torrent '{}' was not found", info_hash_hex));
            };
            let mut next_settings = settings.clone();
            let torrent_settings = next_settings
                .torrents
                .get(index)
                .cloned()
                .ok_or_else(|| format!("Torrent '{}' was not found", info_hash_hex))?;
            let file_index = resolve_priority_file_index(&torrent_settings, target)?;
            if matches!(priority, FilePriority::Normal) {
                next_settings.torrents[index]
                    .file_priorities
                    .remove(&file_index);
            } else {
                next_settings.torrents[index]
                    .file_priorities
                    .insert(file_index, *priority);
            }
            Ok(ControlExecutionPlan::ApplySettings {
                next_settings,
                success_message: format!(
                    "Set file priority for torrent '{}' at index {} to {:?}",
                    info_hash_hex, file_index, priority
                ),
            })
        }
        ControlRequest::AddTorrentFile {
            source_path,
            download_path,
            container_name,
            file_priorities,
        } => Ok(ControlExecutionPlan::AddTorrentFile {
            source_path: source_path.clone(),
            download_path: download_path.clone(),
            container_name: container_name.clone(),
            file_priorities: file_priorities_to_map(file_priorities),
        }),
        ControlRequest::AddMagnet {
            magnet_link,
            download_path,
            container_name,
            file_priorities,
        } => Ok(ControlExecutionPlan::AddMagnet {
            magnet_link: magnet_link.clone(),
            download_path: download_path.clone(),
            container_name: container_name.clone(),
            file_priorities: file_priorities_to_map(file_priorities),
        }),
    }
}

pub fn resolve_priority_file_index(
    torrent_settings: &TorrentSettings,
    target: &ControlPriorityTarget,
) -> Result<usize, String> {
    let file_list = load_torrent_file_list_for_settings(torrent_settings)?;
    match target {
        ControlPriorityTarget::FileIndex(index) => {
            if *index < file_list.len() {
                Ok(*index)
            } else {
                Err(format!(
                    "File index {} is out of range for torrent '{}' ({} files)",
                    index,
                    torrent_settings.name,
                    file_list.len()
                ))
            }
        }
        ControlPriorityTarget::FilePath(path) => {
            let normalized_target = path.replace('\\', "/");
            file_list
                .into_iter()
                .enumerate()
                .find_map(|(index, (parts, _))| {
                    (parts.join("/") == normalized_target).then_some(index)
                })
                .ok_or_else(|| {
                    format!(
                        "No file matching '{}' was found in torrent '{}'",
                        path, torrent_settings.name
                    )
                })
        }
    }
}

pub fn apply_offline_control_request(
    settings: &mut Settings,
    request: &ControlRequest,
) -> Result<String, String> {
    match plan_control_request(settings, request)? {
        ControlExecutionPlan::StatusNow
        | ControlExecutionPlan::StatusFollowStart { .. }
        | ControlExecutionPlan::StatusFollowStop => {
            Err("Status commands require a running superseedr instance".to_string())
        }
        ControlExecutionPlan::ApplySettings {
            next_settings,
            success_message,
        } => {
            *settings = next_settings;
            Ok(success_message)
        }
        ControlExecutionPlan::AddTorrentFile {
            source_path,
            download_path,
            container_name,
            file_priorities,
        } => {
            let name = source_path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("Queued Torrent")
                .to_string();
            settings.torrents.push(TorrentSettings {
                torrent_or_magnet: source_path.to_string_lossy().to_string(),
                name,
                download_path,
                container_name,
                file_priorities,
                ..TorrentSettings::default()
            });
            Ok(format!(
                "Queued torrent file '{}' for the next runtime",
                source_path.display()
            ))
        }
        ControlExecutionPlan::AddMagnet {
            magnet_link,
            download_path,
            container_name,
            file_priorities,
        } => {
            settings.torrents.push(TorrentSettings {
                torrent_or_magnet: magnet_link,
                name: "Queued Magnet".to_string(),
                download_path,
                container_name,
                file_priorities,
                ..TorrentSettings::default()
            });
            Ok("Queued magnet for the next runtime".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_offline_control_request, apply_offline_purge,
        find_torrent_settings_index_by_info_hash, list_torrent_files, plan_control_request,
        resolve_purge_target_info_hash, resolve_target_info_hash, ControlExecutionPlan,
    };
    use crate::config::{set_app_paths_override_for_tests, Settings, TorrentSettings};
    use crate::integrations::control::{ControlPriorityTarget, ControlRequest};
    use std::fs;
    use std::path::PathBuf;

    fn shared_env_guard() -> &'static std::sync::Mutex<()> {
        crate::config::shared_env_guard_for_tests()
    }

    fn write_sample_torrent_file() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let torrent = crate::torrent_file::Torrent {
            info: crate::torrent_file::Info {
                name: "sample-pack".to_string(),
                piece_length: 16_384,
                pieces: vec![0; 20],
                files: vec![
                    crate::torrent_file::InfoFile {
                        length: 10,
                        path: vec!["folder".to_string(), "alpha.bin".to_string()],
                        md5sum: None,
                        attr: None,
                    },
                    crate::torrent_file::InfoFile {
                        length: 20,
                        path: vec!["folder".to_string(), "beta.bin".to_string()],
                        md5sum: None,
                        attr: None,
                    },
                ],
                ..Default::default()
            },
            announce: Some("http://tracker.test".to_string()),
            ..Default::default()
        };
        let bytes = serde_bencode::to_bytes(&torrent).expect("serialize torrent");
        let path = dir
            .path()
            .join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.torrent");
        fs::write(&path, bytes).expect("write torrent fixture");
        (dir, path.to_string_lossy().to_string())
    }

    #[test]
    fn offline_hybrid_magnet_lookup_prefers_btih_identity() {
        let _guard = shared_env_guard().lock().unwrap();
        let magnet = concat!(
            "magnet:?xt=urn:btih:1111111111111111111111111111111111111111",
            "&xt=urn:btmh:1220aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        let settings = Settings {
            torrents: vec![TorrentSettings {
                torrent_or_magnet: magnet.to_string(),
                name: "Sample Hybrid".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };

        assert_eq!(
            find_torrent_settings_index_by_info_hash(&settings, &[0x11; 20]),
            Some(0)
        );
    }

    #[test]
    fn offline_delete_targets_hybrid_magnet_by_btih() {
        let _guard = shared_env_guard().lock().unwrap();
        let magnet = concat!(
            "magnet:?xt=urn:btih:1111111111111111111111111111111111111111",
            "&xt=urn:btmh:1220aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        let mut settings = Settings {
            torrents: vec![TorrentSettings {
                torrent_or_magnet: magnet.to_string(),
                name: "Sample Hybrid".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let result = apply_offline_control_request(
            &mut settings,
            &ControlRequest::Delete {
                info_hash_hex: "1111111111111111111111111111111111111111".to_string(),
                delete_files: false,
            },
        );

        assert!(result.is_ok());
        assert!(settings.torrents.is_empty());
    }

    #[test]
    fn priority_file_path_resolution_still_requires_torrent_metadata() {
        let _guard = shared_env_guard().lock().unwrap();
        let mut settings = Settings {
            torrents: vec![TorrentSettings {
                torrent_or_magnet: "magnet:?xt=urn:btih:1111111111111111111111111111111111111111"
                    .to_string(),
                name: "Magnet".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let result = apply_offline_control_request(
            &mut settings,
            &ControlRequest::SetFilePriority {
                info_hash_hex: "1111111111111111111111111111111111111111".to_string(),
                target: ControlPriorityTarget::FilePath("folder/item.bin".to_string()),
                priority: crate::app::FilePriority::High,
            },
        );

        assert!(result.is_err());
    }

    #[test]
    fn files_list_uses_torrent_source_when_metadata_is_missing() {
        let _guard = shared_env_guard().lock().unwrap();
        let (_dir, torrent_path) = write_sample_torrent_file();
        let settings = Settings {
            torrents: vec![TorrentSettings {
                torrent_or_magnet: torrent_path,
                name: "Sample Pack".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let files = list_torrent_files(&settings, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .expect("list files");

        assert_eq!(files.len(), 2);
        assert_eq!(files[0].relative_path, "folder/alpha.bin");
        assert_eq!(files[1].relative_path, "folder/beta.bin");
    }

    #[test]
    fn purge_target_can_resolve_from_unique_file_path() {
        let _guard = shared_env_guard().lock().unwrap();
        let dir = tempfile::tempdir().expect("create tempdir");
        let (_torrent_dir, torrent_path) = write_sample_torrent_file();
        let download_root = dir.path().join("downloads");
        let target = download_root
            .join("payload")
            .join("folder")
            .join("beta.bin");
        let settings = Settings {
            torrents: vec![TorrentSettings {
                torrent_or_magnet: torrent_path,
                name: "Sample Pack".to_string(),
                download_path: Some(download_root),
                container_name: Some("payload".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };

        let resolved =
            resolve_purge_target_info_hash(&settings, target.to_str().expect("target path"))
                .expect("resolve path");

        assert_eq!(resolved, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    }

    #[test]
    fn command_specific_target_resolution_uses_callers_command_name() {
        let _guard = shared_env_guard().lock().unwrap();
        let dir = tempfile::tempdir().expect("create tempdir");
        let (_torrent_dir, torrent_path) = write_sample_torrent_file();
        let download_root = dir.path().join("downloads");
        let target = download_root.join("payload").join("missing.bin");
        let settings = Settings {
            torrents: vec![TorrentSettings {
                torrent_or_magnet: torrent_path,
                name: "Sample Pack".to_string(),
                download_path: Some(download_root),
                container_name: Some("payload".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };

        let error = resolve_target_info_hash(&settings, target.to_str().expect("target"), "info")
            .expect_err("missing file should fail");

        assert!(error.contains("superseedr info <info-hash>"));
        assert!(!error.contains("superseedr purge <info-hash>"));
    }

    #[test]
    fn offline_purge_deletes_files_and_removes_torrent() {
        let _guard = shared_env_guard().lock().unwrap();
        let dir = tempfile::tempdir().expect("create tempdir");
        let (_torrent_dir, torrent_path) = write_sample_torrent_file();
        let download_root = dir.path().join("downloads");
        let file_a = download_root
            .join("payload")
            .join("folder")
            .join("alpha.bin");
        let file_b = download_root
            .join("payload")
            .join("folder")
            .join("beta.bin");
        fs::create_dir_all(file_a.parent().expect("parent")).expect("create dirs");
        fs::write(&file_a, b"alpha").expect("write alpha");
        fs::write(&file_b, b"beta").expect("write beta");

        let mut settings = Settings {
            torrents: vec![TorrentSettings {
                torrent_or_magnet: torrent_path,
                name: "Sample Pack".to_string(),
                download_path: Some(download_root),
                container_name: Some("payload".to_string()),
                ..Default::default()
            }],
            ..Settings::default()
        };

        let result = apply_offline_purge(&mut settings, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

        assert!(result.is_ok());
        assert!(settings.torrents.is_empty());
        assert!(!file_a.exists());
        assert!(!file_b.exists());
    }

    #[test]
    fn control_plan_and_offline_apply_share_pause_and_purge_mutations() {
        let _guard = shared_env_guard().lock().unwrap();
        let mut settings = Settings {
            torrents: vec![TorrentSettings {
                torrent_or_magnet: "magnet:?xt=urn:btih:1111111111111111111111111111111111111111"
                    .to_string(),
                name: "Sample Node".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let pause = ControlRequest::Pause {
            info_hash_hex: "1111111111111111111111111111111111111111".to_string(),
        };
        match plan_control_request(&settings, &pause).expect("plan pause") {
            ControlExecutionPlan::ApplySettings { next_settings, .. } => {
                assert_eq!(
                    next_settings.torrents[0].torrent_control_state,
                    crate::app::TorrentControlState::Paused
                );
            }
            other => panic!("unexpected plan: {:?}", other),
        }

        apply_offline_control_request(&mut settings, &pause).expect("apply pause");
        assert_eq!(
            settings.torrents[0].torrent_control_state,
            crate::app::TorrentControlState::Paused
        );

        let purge = ControlRequest::Delete {
            info_hash_hex: "1111111111111111111111111111111111111111".to_string(),
            delete_files: true,
        };
        match plan_control_request(&settings, &purge).expect("plan purge") {
            ControlExecutionPlan::ApplySettings { next_settings, .. } => {
                assert_eq!(
                    next_settings.torrents[0].torrent_control_state,
                    crate::app::TorrentControlState::Deleting
                );
                assert!(next_settings.torrents[0].delete_files);
            }
            other => panic!("unexpected plan: {:?}", other),
        }
    }

    #[test]
    fn files_and_path_resolution_treat_invalid_metadata_as_missing() {
        let _guard = shared_env_guard().lock().unwrap();
        let dir = tempfile::tempdir().expect("create tempdir");
        let config_dir = dir.path().join("config");
        let data_dir = dir.path().join("data");
        set_app_paths_override_for_tests(Some((config_dir.clone(), data_dir)));
        fs::create_dir_all(&config_dir).expect("create config dir");
        fs::write(
            config_dir.join("torrent_metadata.toml"),
            "not = [valid toml",
        )
        .expect("write invalid metadata");

        let settings = Settings {
            torrents: vec![TorrentSettings {
                torrent_or_magnet: "magnet:?xt=urn:btih:1111111111111111111111111111111111111111"
                    .to_string(),
                name: "Sample Queue".to_string(),
                download_path: Some(PathBuf::from("/downloads")),
                ..Default::default()
            }],
            ..Default::default()
        };

        let files_error = list_torrent_files(&settings, "1111111111111111111111111111111111111111")
            .expect_err("magnet without persisted metadata should still fail");
        assert!(files_error.contains("does not have persisted file metadata yet"));

        let resolve_error = resolve_target_info_hash(&settings, "/downloads/item.bin", "info")
            .expect_err("invalid metadata should be treated as missing metadata");
        assert!(resolve_error.contains("No torrent matched file path"));

        set_app_paths_override_for_tests(None);
    }
}
