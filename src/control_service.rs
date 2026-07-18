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
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use sysinfo::Disks;

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

pub fn validate_move_download_path(path: &Path) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() {
        return Err("Move path must not be empty".to_string());
    }
    if !path.exists() {
        return Err(format!("Move path does not exist: {}", path.display()));
    }
    if !path.is_dir() {
        return Err(format!("Move path must be a directory: {}", path.display()));
    }
    fs::canonicalize(path).map_err(|error| {
        format!(
            "Failed to resolve move path '{}': {}",
            path.display(),
            error
        )
    })
}

pub fn build_move_torrent_request(
    settings: &Settings,
    info_hash_hex: &str,
    path: &Path,
) -> Result<ControlRequest, String> {
    let info_hash = decode_info_hash(info_hash_hex)?;
    let Some(_) = find_torrent_settings_index_by_info_hash(settings, &info_hash) else {
        return Err(format!("Torrent '{}' was not found", info_hash_hex));
    };

    Ok(ControlRequest::MoveTorrent {
        info_hash_hex: hex::encode(info_hash),
        download_path: validate_move_download_path(path)?,
    })
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
        ControlRequest::MoveTorrent {
            info_hash_hex,
            download_path,
        } => format!(
            "Queued move request for torrent '{}' -> '{}'",
            info_hash_hex,
            download_path.display()
        ),
        ControlRequest::SetTorrentConfig { info_hash_hex, .. } => {
            format!(
                "Queued torrent config request for torrent '{}'",
                info_hash_hex
            )
        }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovePayloadPlan {
    pub info_hash_hex: String,
    pub destination_root: PathBuf,
    pub files: Vec<(PathBuf, PathBuf)>,
    pub source_directories: Vec<PathBuf>,
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

    let effective_root = match &torrent_settings.container_name {
        Some(name) if !name.is_empty() => download_root.join(name),
        Some(_) => download_root.clone(),
        None if is_multi_file => {
            download_root.join(format!("{} [{}]", torrent_name, info_hash_hex))
        }
        None => download_root.clone(),
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

pub fn build_move_payload_plan(
    settings: &Settings,
    info_hash_hex: &str,
    destination_root: &Path,
) -> Result<MovePayloadPlan, String> {
    let destination_root = validate_move_download_path(destination_root)?;
    let metadata_by_info_hash = load_torrent_metadata_snapshot()?;
    let (_, torrent_settings, _) = torrent_settings_by_info_hash_hex(settings, info_hash_hex)?;
    let (torrent_name, is_multi_file, files) =
        manifest_entries_for_torrent_settings(torrent_settings, &metadata_by_info_hash)?;

    let (source_download_root, source_effective_root) = resolve_torrent_roots(
        settings,
        torrent_settings,
        info_hash_hex,
        is_multi_file,
        &torrent_name,
    )?;
    let mut destination_settings = torrent_settings.clone();
    destination_settings.download_path = Some(destination_root);
    let (_, destination_effective_root) = resolve_torrent_roots(
        settings,
        &destination_settings,
        info_hash_hex,
        is_multi_file,
        &torrent_name,
    )?;

    let mut move_files = Vec::new();
    let mut current_offset = 0;
    let multi_file_info = MultiFileInfo {
        files: files
            .into_iter()
            .map(|file| {
                let mut source_path = source_effective_root.clone();
                let mut destination_path = destination_effective_root.clone();
                for segment in file
                    .relative_path
                    .split('/')
                    .filter(|segment| !segment.is_empty())
                {
                    source_path.push(segment);
                    destination_path.push(segment);
                }
                move_files.push((source_path.clone(), destination_path));

                let file_info = FileInfo {
                    path: source_path,
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
    let (_, source_directories) = calculate_deletion_lists(
        &multi_file_info,
        &source_download_root,
        torrent_settings.container_name.as_deref(),
    );

    Ok(MovePayloadPlan {
        info_hash_hex: info_hash_hex.to_string(),
        destination_root: destination_effective_root,
        files: move_files,
        source_directories,
    })
}

#[cfg(any(windows, test))]
fn normalize_windows_mount_match_path(path: &str) -> String {
    let normalized = path.replace('/', "\\");
    let normalized = if normalized
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("\\\\?\\UNC\\"))
    {
        format!("\\\\{}", &normalized[8..])
    } else if normalized
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("\\\\?\\"))
    {
        normalized[4..].to_string()
    } else {
        normalized
    };
    normalized.to_lowercase()
}

#[cfg(any(windows, test))]
fn windows_path_matches_mount(path: &str, mount_point: &str) -> bool {
    let path = normalize_windows_mount_match_path(path);
    let mount_point = normalize_windows_mount_match_path(mount_point);
    if path == mount_point {
        return true;
    }
    if mount_point.ends_with('\\') {
        path.starts_with(&mount_point)
    } else {
        path.strip_prefix(&mount_point)
            .is_some_and(|suffix| suffix.starts_with('\\'))
    }
}

#[cfg(windows)]
fn path_matches_disk_mount(path: &Path, mount_point: &Path) -> bool {
    windows_path_matches_mount(&path.to_string_lossy(), &mount_point.to_string_lossy())
}

#[cfg(not(windows))]
fn path_matches_disk_mount(path: &Path, mount_point: &Path) -> bool {
    path.starts_with(mount_point)
}

fn disk_mount_for_path(path: &Path) -> Option<PathBuf> {
    let disks = Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|disk| path_matches_disk_mount(path, disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().as_os_str().len())
        .map(|disk| disk.mount_point().to_path_buf())
}

fn paths_share_disk_mount(left: &Path, right: &Path) -> bool {
    match (disk_mount_for_path(left), disk_mount_for_path(right)) {
        (Some(left_mount), Some(right_mount)) => left_mount == right_mount,
        _ => false,
    }
}

fn available_space_for_path(path: &Path) -> Option<u64> {
    let disks = Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|disk| path_matches_disk_mount(path, disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().as_os_str().len())
        .map(|disk| disk.available_space())
}

fn required_destination_space_for_move_with<F>(
    plan: &MovePayloadPlan,
    mut paths_share_mount: F,
) -> Result<u64, String>
where
    F: FnMut(&Path, &Path) -> bool,
{
    let mut required_space = 0_u64;
    for (source, destination) in &plan.files {
        if !source.exists() || paths_share_mount(source, destination) {
            continue;
        }
        required_space = required_space.saturating_add(
            fs::metadata(source)
                .map_err(|error| {
                    format!(
                        "Failed to read metadata for '{}': {}",
                        source.display(),
                        error
                    )
                })?
                .len(),
        );
    }
    Ok(required_space)
}

fn ensure_destination_space_for_move_with_available(
    destination_root: &Path,
    required_space: u64,
    available_space: u64,
) -> Result<(), String> {
    if available_space < required_space {
        return Err(format!(
            "Not enough free space at '{}' for move: available={} required={}",
            destination_root.display(),
            available_space,
            required_space
        ));
    }
    Ok(())
}

pub fn ensure_destination_space_for_move(plan: &MovePayloadPlan) -> Result<(), String> {
    let required_space = required_destination_space_for_move_with(plan, paths_share_disk_mount)?;
    if required_space == 0 {
        return Ok(());
    }
    let Some(available_space) = available_space_for_path(&plan.destination_root) else {
        return Err(format!(
            "Could not determine available space at '{}' for move",
            plan.destination_root.display()
        ));
    };
    ensure_destination_space_for_move_with_available(
        &plan.destination_root,
        required_space,
        available_space,
    )
}

fn same_existing_file(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn metadata_len(path: &Path) -> Result<u64, String> {
    fs::metadata(path)
        .map_err(|error| {
            format!(
                "Failed to read metadata for '{}': {}",
                path.display(),
                error
            )
        })
        .map(|metadata| metadata.len())
}

fn verify_moved_destination(
    source: &Path,
    destination: &Path,
    source_len: u64,
) -> Result<(), String> {
    let destination_metadata = fs::metadata(destination).map_err(|error| {
        format!(
            "Failed to read moved destination metadata '{}': {}",
            destination.display(),
            error
        )
    })?;
    if !destination_metadata.is_file() || destination_metadata.len() != source_len {
        return Err(format!(
            "Move metadata check failed for '{}' -> '{}'",
            source.display(),
            destination.display()
        ));
    }
    Ok(())
}

fn copy_for_cross_device_move(
    source: &Path,
    destination: &Path,
    source_len: u64,
) -> Result<(), String> {
    let copied_len = fs::copy(source, destination).map_err(|error| {
        format!(
            "Failed to copy '{}' to '{}' after cross-volume move fallback: {}",
            source.display(),
            destination.display(),
            error
        )
    })?;
    if copied_len != source_len {
        return Err(format!(
            "Cross-volume move copied {} bytes but expected {} bytes for '{}' -> '{}'",
            copied_len,
            source_len,
            source.display(),
            destination.display()
        ));
    }
    verify_moved_destination(source, destination, source_len)
}

fn preflight_move_payload_files(plan: &MovePayloadPlan) -> Result<(), String> {
    let mut destinations = HashSet::new();

    for (source, destination) in &plan.files {
        if !source.exists() || same_existing_file(source, destination) {
            continue;
        }
        if !source.is_file() {
            return Err(format!("Move source is not a file: {}", source.display()));
        }
        if !destinations.insert(destination.clone()) {
            return Err(format!(
                "Move plan contains duplicate destination: {}",
                destination.display()
            ));
        }
        if destination.exists() {
            return Err(format!(
                "Move destination already exists: {}",
                destination.display()
            ));
        }
    }

    for (_, destination) in &plan.files {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("Failed to create '{}': {}", parent.display(), error))?;
        }
    }

    Ok(())
}

#[derive(Debug)]
pub struct OfflineMoveTransaction {
    next_settings: Settings,
    success_message: String,
    staged_files: Vec<(PathBuf, PathBuf)>,
    source_directories: Vec<PathBuf>,
}

impl OfflineMoveTransaction {
    pub fn next_settings(&self) -> &Settings {
        &self.next_settings
    }

    pub fn commit(self) -> String {
        let cleanup_errors = finalize_staged_move_files(&self.staged_files);
        for error in &cleanup_errors {
            tracing::warn!("{}", error);
        }
        for dir_path in &self.source_directories {
            if let Err(error) = fs::remove_dir(dir_path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::info!("Skipped dir deletion {:?}: {}", dir_path, error);
                }
            }
        }

        if cleanup_errors.is_empty() {
            self.success_message
        } else {
            format!(
                "{}. The destination is active, but {} source file(s) could not be removed.",
                self.success_message,
                cleanup_errors.len()
            )
        }
    }

    pub fn rollback(self) -> Result<(), String> {
        rollback_staged_move_files(&self.staged_files)
    }
}

fn ensure_copy_space(destination: &Path, required_space: u64) -> Result<(), String> {
    let check_path = destination.parent().unwrap_or(destination);
    let Some(available_space) = available_space_for_path(check_path) else {
        return Err(format!(
            "Could not determine available space at '{}' for move",
            check_path.display()
        ));
    };
    ensure_destination_space_for_move_with_available(check_path, required_space, available_space)
}

fn stage_move_file(source: &Path, destination: &Path, source_len: u64) -> Result<(), String> {
    if paths_share_disk_mount(source, destination) {
        match fs::hard_link(source, destination) {
            Ok(()) => return verify_moved_destination(source, destination, source_len),
            Err(error) if destination.exists() => {
                return Err(format!(
                    "Move destination appeared while staging '{}': {}",
                    destination.display(),
                    error
                ));
            }
            Err(_) => ensure_copy_space(destination, source_len)?,
        }
    }

    copy_for_cross_device_move(source, destination, source_len)
}

fn rollback_staged_move_files(files: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    let mut errors = Vec::new();
    for (source, destination) in files.iter().rev() {
        if !source.exists() {
            errors.push(format!(
                "Cannot remove staged destination '{}' because source is missing: {}",
                destination.display(),
                source.display()
            ));
            continue;
        }
        if let Err(error) = fs::remove_file(destination) {
            if error.kind() != io::ErrorKind::NotFound {
                errors.push(format!(
                    "Failed to remove staged destination '{}': {}",
                    destination.display(),
                    error
                ));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn finalize_staged_move_files(files: &[(PathBuf, PathBuf)]) -> Vec<String> {
    let mut errors = Vec::new();
    for (source, _) in files {
        if let Err(error) = fs::remove_file(source) {
            if error.kind() != io::ErrorKind::NotFound {
                errors.push(format!(
                    "Move committed but source could not be removed '{}': {}",
                    source.display(),
                    error
                ));
            }
        }
    }
    errors
}

fn stage_move_payload_files(plan: &MovePayloadPlan) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    preflight_move_payload_files(plan)?;
    let mut staged_files = Vec::new();

    for (source, destination) in &plan.files {
        if !source.exists() || same_existing_file(source, destination) {
            continue;
        }
        let source_len = metadata_len(source)?;
        if let Err(error) = stage_move_file(source, destination, source_len) {
            if source.exists() && destination.exists() {
                let _ = fs::remove_file(destination);
            }
            let rollback_error = rollback_staged_move_files(&staged_files).err();
            return Err(match rollback_error {
                Some(rollback_error) => {
                    format!("{}. Rollback failed: {}", error, rollback_error)
                }
                None => error,
            });
        }
        if !source.exists() {
            let error = format!(
                "Move staging unexpectedly removed source: {}",
                source.display()
            );
            let _ = fs::remove_file(destination);
            let rollback_error = rollback_staged_move_files(&staged_files).err();
            return Err(match rollback_error {
                Some(rollback_error) => {
                    format!("{}. Rollback failed: {}", error, rollback_error)
                }
                None => error,
            });
        }
        staged_files.push((source.clone(), destination.clone()));
    }

    Ok(staged_files)
}

pub fn prepare_offline_move_transaction(
    settings: &Settings,
    info_hash_hex: &str,
    download_path: &Path,
) -> Result<OfflineMoveTransaction, String> {
    let info_hash = decode_info_hash(info_hash_hex)?;
    let Some(index) = find_torrent_settings_index_by_info_hash(settings, &info_hash) else {
        return Err(format!("Torrent '{}' was not found", info_hash_hex));
    };
    let canonical_download_path = validate_move_download_path(download_path)?;
    let move_plan = build_move_payload_plan(settings, info_hash_hex, &canonical_download_path)?;
    ensure_destination_space_for_move(&move_plan)?;
    let staged_files = stage_move_payload_files(&move_plan)?;

    let mut next_settings = settings.clone();
    next_settings.torrents[index].download_path = Some(canonical_download_path.clone());
    Ok(OfflineMoveTransaction {
        next_settings,
        success_message: format!(
            "Moved {} file(s) and updated download path for torrent '{}' to '{}'",
            staged_files.len(),
            info_hash_hex,
            canonical_download_path.display()
        ),
        staged_files,
        source_directories: move_plan.source_directories,
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
    MoveTorrent {
        info_hash_hex: String,
        download_path: PathBuf,
    },
    AddTorrentFile {
        source_path: PathBuf,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        validation_status: bool,
        file_priorities: HashMap<usize, FilePriority>,
    },
    AddMagnet {
        magnet_link: String,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        validation_status: bool,
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
        ControlRequest::MoveTorrent {
            info_hash_hex,
            download_path,
        } => {
            let info_hash = decode_info_hash(info_hash_hex)?;
            let Some(_index) = find_torrent_settings_index_by_info_hash(settings, &info_hash)
            else {
                return Err(format!("Torrent '{}' was not found", info_hash_hex));
            };
            let canonical_download_path = validate_move_download_path(download_path)?;
            build_move_payload_plan(settings, info_hash_hex, &canonical_download_path)?;
            Ok(ControlExecutionPlan::MoveTorrent {
                info_hash_hex: info_hash_hex.clone(),
                download_path: canonical_download_path,
            })
        }
        ControlRequest::SetTorrentConfig {
            info_hash_hex,
            download_path,
            container_name,
            file_priorities,
        } => {
            let info_hash = decode_info_hash(info_hash_hex)?;
            let Some(index) = find_torrent_settings_index_by_info_hash(settings, &info_hash) else {
                return Err(format!("Torrent '{}' was not found", info_hash_hex));
            };
            let mut next_settings = settings.clone();
            next_settings.torrents[index].download_path = download_path.clone();
            next_settings.torrents[index].container_name = container_name.clone();
            next_settings.torrents[index].file_priorities = file_priorities_to_map(file_priorities);
            Ok(ControlExecutionPlan::ApplySettings {
                next_settings,
                success_message: format!("Updated torrent config for '{}'", info_hash_hex),
            })
        }
        ControlRequest::AddTorrentFile {
            source_path,
            download_path,
            container_name,
            validation_status,
            file_priorities,
        } => Ok(ControlExecutionPlan::AddTorrentFile {
            source_path: source_path.clone(),
            download_path: effective_add_download_path(settings, download_path),
            container_name: container_name.clone(),
            validation_status: *validation_status,
            file_priorities: file_priorities_to_map(file_priorities),
        }),
        ControlRequest::AddMagnet {
            magnet_link,
            download_path,
            container_name,
            validation_status,
            file_priorities,
        } => Ok(ControlExecutionPlan::AddMagnet {
            magnet_link: magnet_link.clone(),
            download_path: effective_add_download_path(settings, download_path),
            container_name: container_name.clone(),
            validation_status: *validation_status,
            file_priorities: file_priorities_to_map(file_priorities),
        }),
    }
}

fn effective_add_download_path(settings: &Settings, explicit: &Option<PathBuf>) -> Option<PathBuf> {
    explicit
        .clone()
        .or_else(|| settings.default_download_folder.clone())
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
    if let ControlRequest::MoveTorrent {
        info_hash_hex,
        download_path,
    } = request
    {
        let transaction = prepare_offline_move_transaction(settings, info_hash_hex, download_path)?;
        *settings = transaction.next_settings().clone();
        return Ok(transaction.commit());
    }

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
        ControlExecutionPlan::MoveTorrent { .. } => {
            unreachable!("offline move requests are handled before planning")
        }
        ControlExecutionPlan::AddTorrentFile {
            source_path,
            download_path,
            container_name,
            validation_status,
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
                validation_status,
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
            validation_status,
            file_priorities,
        } => {
            let name =
                magnet_display_name(&magnet_link).unwrap_or_else(|| "Queued Magnet".to_string());
            settings.torrents.push(TorrentSettings {
                torrent_or_magnet: magnet_link,
                name,
                download_path,
                container_name,
                file_priorities,
                validation_status,
                ..TorrentSettings::default()
            });
            Ok("Queued magnet for the next runtime".to_string())
        }
    }
}

fn magnet_display_name(magnet_link: &str) -> Option<String> {
    for raw_part in magnet_link.split('&') {
        let part = raw_part.strip_prefix("magnet:?").unwrap_or(raw_part);
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if key.eq_ignore_ascii_case("dn") {
            let value_for_decode = value.replace('+', "%20");
            if let Ok(decoded) = urlencoding::decode(&value_for_decode) {
                let name = decoded.trim();
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        apply_offline_control_request, apply_offline_purge,
        ensure_destination_space_for_move_with_available, find_torrent_settings_index_by_info_hash,
        list_torrent_files, plan_control_request, required_destination_space_for_move_with,
        resolve_purge_target_info_hash, resolve_target_info_hash, resolve_torrent_roots,
        windows_path_matches_mount, ControlExecutionPlan, MovePayloadPlan,
    };
    use crate::config::{set_app_paths_override_for_tests, Settings, TorrentSettings};
    use crate::integrations::control::{
        ControlFilePriorityOverride, ControlPriorityTarget, ControlRequest,
    };
    use std::collections::HashMap;
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
    fn move_space_check_counts_only_files_crossing_disk_mounts() {
        let source_root = tempfile::tempdir().expect("create source root");
        let destination_root = tempfile::tempdir().expect("create destination root");
        let source_a = source_root.path().join("a.bin");
        let source_b = source_root.path().join("b.bin");
        fs::write(&source_a, [1_u8; 7]).expect("write source a");
        fs::write(&source_b, [2_u8; 11]).expect("write source b");
        let destination_a = destination_root.path().join("a.bin");
        let destination_b = destination_root.path().join("b.bin");
        let plan = MovePayloadPlan {
            info_hash_hex: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            destination_root: destination_root.path().to_path_buf(),
            files: vec![
                (source_a.clone(), destination_a),
                (source_b.clone(), destination_b),
            ],
            source_directories: Vec::new(),
        };

        let required = required_destination_space_for_move_with(&plan, |source, _destination| {
            source == source_a.as_path()
        })
        .expect("calculate required move space");

        assert_eq!(required, 11);
    }

    #[test]
    fn extended_platform_paths_match_drive_and_network_mounts() {
        assert!(windows_path_matches_mount(
            r"\\?\C:\Downloads\payload.bin",
            r"C:\"
        ));
        assert!(windows_path_matches_mount(
            r"\\?\UNC\server\share\payload\item.bin",
            r"\\server\share"
        ));
        assert!(windows_path_matches_mount(
            r"\\?\c:\DATA\payload.bin",
            r"C:\data"
        ));
        assert!(!windows_path_matches_mount(
            r"\\?\C:\database\payload.bin",
            r"C:\data"
        ));
    }

    #[test]
    fn single_file_effective_root_honors_nonempty_container_name() {
        let download_root = PathBuf::from("/tmp/sample-downloads");
        let torrent = TorrentSettings {
            download_path: Some(download_root.clone()),
            container_name: Some("single-file-container".to_string()),
            ..TorrentSettings::default()
        };

        let (resolved_download_root, effective_root) = resolve_torrent_roots(
            &Settings::default(),
            &torrent,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            false,
            "sample-item.bin",
        )
        .expect("resolve single-file roots");

        assert_eq!(resolved_download_root, download_root);
        assert_eq!(effective_root, download_root.join("single-file-container"));
    }

    #[test]
    fn move_space_check_rejects_when_available_space_is_too_low() {
        let destination_root = PathBuf::from("/tmp/superseedr-low-space-test");

        let error = ensure_destination_space_for_move_with_available(&destination_root, 1024, 512)
            .expect_err("low free space should fail");

        assert!(error.contains("Not enough free space"));
        assert!(error.contains("available=512"));
        assert!(error.contains("required=1024"));
    }

    #[test]
    fn move_payload_preflights_later_destination_conflicts_before_moving_any_file() {
        let source_root = tempfile::tempdir().expect("create source root");
        let destination_root = tempfile::tempdir().expect("create destination root");
        let source_a = source_root.path().join("a.bin");
        let source_b = source_root.path().join("b.bin");
        fs::write(&source_a, b"alpha").expect("write source a");
        fs::write(&source_b, b"bravo").expect("write source b");
        let destination_a = destination_root.path().join("a.bin");
        let destination_b = destination_root.path().join("b.bin");
        fs::write(&destination_b, b"existing").expect("write conflicting destination");
        let plan = MovePayloadPlan {
            info_hash_hex: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            destination_root: destination_root.path().to_path_buf(),
            files: vec![
                (source_a.clone(), destination_a.clone()),
                (source_b.clone(), destination_b),
            ],
            source_directories: Vec::new(),
        };

        let error = super::stage_move_payload_files(&plan).expect_err("later conflict should fail");

        assert!(error.contains("already exists"));
        assert_eq!(fs::read(&source_a).expect("source a remains"), b"alpha");
        assert_eq!(fs::read(&source_b).expect("source b remains"), b"bravo");
        assert!(!destination_a.exists());
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
    fn set_torrent_config_replaces_location_container_and_priorities() {
        let _guard = shared_env_guard().lock().unwrap();
        let settings = Settings {
            torrents: vec![TorrentSettings {
                torrent_or_magnet: "magnet:?xt=urn:btih:1111111111111111111111111111111111111111"
                    .to_string(),
                name: "Sample Node".to_string(),
                file_priorities: HashMap::from([(0, crate::app::FilePriority::High)]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let request = ControlRequest::SetTorrentConfig {
            info_hash_hex: "1111111111111111111111111111111111111111".to_string(),
            download_path: Some(PathBuf::from("/downloads/next")),
            container_name: Some(String::new()),
            file_priorities: vec![
                ControlFilePriorityOverride {
                    file_index: 0,
                    priority: crate::app::FilePriority::Normal,
                },
                ControlFilePriorityOverride {
                    file_index: 1,
                    priority: crate::app::FilePriority::Skip,
                },
            ],
        };

        match plan_control_request(&settings, &request).expect("plan config update") {
            ControlExecutionPlan::ApplySettings { next_settings, .. } => {
                let torrent = &next_settings.torrents[0];
                assert_eq!(
                    torrent.download_path,
                    Some(PathBuf::from("/downloads/next"))
                );
                assert_eq!(torrent.container_name, Some(String::new()));
                assert_eq!(
                    torrent.file_priorities,
                    HashMap::from([(1, crate::app::FilePriority::Skip)])
                );
            }
            other => panic!("unexpected plan: {:?}", other),
        }
    }

    #[test]
    fn offline_add_control_preserves_validation_status() {
        let _guard = shared_env_guard().lock().unwrap();
        let mut settings = Settings::default();
        let request = ControlRequest::AddMagnet {
            magnet_link: "magnet:?xt=urn:btih:2222222222222222222222222222222222222222".to_string(),
            download_path: None,
            container_name: None,
            validation_status: true,
            file_priorities: Vec::new(),
        };

        apply_offline_control_request(&mut settings, &request).expect("apply add");

        assert_eq!(settings.torrents.len(), 1);
        assert!(settings.torrents[0].validation_status);
    }

    #[test]
    fn add_control_uses_default_download_folder_when_path_is_absent() {
        let _guard = shared_env_guard().lock().unwrap();
        let mut settings = Settings {
            default_download_folder: Some(PathBuf::from("/downloads/default")),
            ..Default::default()
        };
        let request = ControlRequest::AddMagnet {
            magnet_link: "magnet:?xt=urn:btih:3333333333333333333333333333333333333333".to_string(),
            download_path: None,
            container_name: None,
            validation_status: true,
            file_priorities: Vec::new(),
        };

        match plan_control_request(&settings, &request).expect("plan add") {
            ControlExecutionPlan::AddMagnet { download_path, .. } => {
                assert_eq!(download_path, Some(PathBuf::from("/downloads/default")));
            }
            other => panic!("unexpected plan: {:?}", other),
        }

        apply_offline_control_request(&mut settings, &request).expect("apply add");
        assert_eq!(
            settings.torrents[0].download_path,
            Some(PathBuf::from("/downloads/default"))
        );
    }

    #[test]
    fn add_control_explicit_download_path_overrides_default() {
        let _guard = shared_env_guard().lock().unwrap();
        let settings = Settings {
            default_download_folder: Some(PathBuf::from("/downloads/default")),
            ..Default::default()
        };
        let request = ControlRequest::AddTorrentFile {
            source_path: PathBuf::from("/tmp/sample.torrent"),
            download_path: Some(PathBuf::from("/downloads/explicit")),
            container_name: None,
            validation_status: true,
            file_priorities: Vec::new(),
        };

        match plan_control_request(&settings, &request).expect("plan add") {
            ControlExecutionPlan::AddTorrentFile { download_path, .. } => {
                assert_eq!(download_path, Some(PathBuf::from("/downloads/explicit")));
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
