// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::FilePriority;
use crate::fs_atomic::{
    deserialize_versioned_toml, serialize_versioned_toml, write_string_atomically,
};
use crate::tags::TagId;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ControlPriorityTarget {
    FileIndex(usize),
    FilePath(String),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct ControlFilePriorityOverride {
    pub file_index: usize,
    pub priority: FilePriority,
}

impl Default for ControlFilePriorityOverride {
    fn default() -> Self {
        Self {
            file_index: 0,
            priority: FilePriority::Normal,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ControlRequest {
    StatusNow,
    StatusFollowStart {
        interval_secs: u64,
    },
    StatusFollowStop,
    CreateTag {
        name: String,
    },
    CreateAndAssignTag {
        name: String,
        info_hashes: Vec<String>,
    },
    RenameTag {
        tag_id: TagId,
        new_name: String,
    },
    DeleteTag {
        tag_id: TagId,
    },
    AssignTag {
        tag_id: TagId,
        info_hash_hex: String,
    },
    RemoveTag {
        tag_id: TagId,
        info_hash_hex: String,
    },
    Pause {
        info_hash_hex: String,
    },
    Resume {
        info_hash_hex: String,
    },
    Delete {
        info_hash_hex: String,
        #[serde(default)]
        delete_files: bool,
    },
    SetFilePriority {
        info_hash_hex: String,
        target: ControlPriorityTarget,
        priority: FilePriority,
    },
    MoveTorrent {
        info_hash_hex: String,
        download_path: PathBuf,
    },
    SetTorrentConfig {
        info_hash_hex: String,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        #[serde(default)]
        file_priorities: Vec<ControlFilePriorityOverride>,
    },
    AddTorrentFile {
        source_path: PathBuf,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        #[serde(default)]
        validation_status: bool,
        #[serde(default)]
        file_priorities: Vec<ControlFilePriorityOverride>,
    },
    AddMagnet {
        magnet_link: String,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        #[serde(default)]
        validation_status: bool,
        #[serde(default)]
        file_priorities: Vec<ControlFilePriorityOverride>,
    },
}

impl ControlRequest {
    pub fn action_name(&self) -> &'static str {
        match self {
            Self::StatusNow => "status_now",
            Self::StatusFollowStart { .. } => "status_follow_start",
            Self::StatusFollowStop => "status_follow_stop",
            Self::CreateTag { .. } => "create_tag",
            Self::CreateAndAssignTag { .. } => "create_and_assign_tag",
            Self::RenameTag { .. } => "rename_tag",
            Self::DeleteTag { .. } => "delete_tag",
            Self::AssignTag { .. } => "assign_tag",
            Self::RemoveTag { .. } => "remove_tag",
            Self::Pause { .. } => "pause",
            Self::Resume { .. } => "resume",
            Self::Delete { .. } => "delete",
            Self::SetFilePriority { .. } => "set_file_priority",
            Self::MoveTorrent { .. } => "move_torrent",
            Self::SetTorrentConfig { .. } => "set_torrent_config",
            Self::AddTorrentFile { .. } => "add_torrent_file",
            Self::AddMagnet { .. } => "add_magnet",
        }
    }

    pub fn target_info_hash_hex(&self) -> Option<&str> {
        match self {
            Self::Pause { info_hash_hex }
            | Self::Resume { info_hash_hex }
            | Self::Delete { info_hash_hex, .. }
            | Self::SetFilePriority { info_hash_hex, .. }
            | Self::MoveTorrent { info_hash_hex, .. }
            | Self::SetTorrentConfig { info_hash_hex, .. } => Some(info_hash_hex.as_str()),
            Self::AssignTag { info_hash_hex, .. } | Self::RemoveTag { info_hash_hex, .. } => {
                Some(info_hash_hex.as_str())
            }
            Self::StatusNow
            | Self::StatusFollowStart { .. }
            | Self::StatusFollowStop
            | Self::CreateTag { .. }
            | Self::CreateAndAssignTag { .. }
            | Self::RenameTag { .. }
            | Self::DeleteTag { .. }
            | Self::AddTorrentFile { .. }
            | Self::AddMagnet { .. } => None,
        }
    }

    pub fn priority_target(&self) -> Option<&ControlPriorityTarget> {
        match self {
            Self::SetFilePriority { target, .. } => Some(target),
            _ => None,
        }
    }

    pub fn priority_value(&self) -> Option<FilePriority> {
        match self {
            Self::SetFilePriority { priority, .. } => Some(*priority),
            _ => None,
        }
    }
}

pub fn write_control_request(request: &ControlRequest, watch_path: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(watch_path)?;
    let content = serialize_versioned_toml(request)?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let content_hash = hex::encode(Sha1::digest(content.as_bytes()));
    let file_stem = format!("control-{}-{}", now_ms, content_hash);
    let final_path = watch_path.join(format!("{}.control", file_stem));
    write_string_atomically(&final_path, &content)?;
    Ok(final_path)
}

pub fn read_control_request(path: &Path) -> io::Result<ControlRequest> {
    let content = fs::read_to_string(path)?;
    deserialize_versioned_toml(&content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn round_trip_control_request_file() {
        let dir = tempdir().expect("create tempdir");
        let request = ControlRequest::SetFilePriority {
            info_hash_hex: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            target: ControlPriorityTarget::FilePath("folder/sample.bin".to_string()),
            priority: FilePriority::High,
        };

        let path = write_control_request(&request, dir.path()).expect("write control request");
        let loaded = read_control_request(&path).expect("read control request");

        assert_eq!(loaded, request);
        assert_eq!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("control")
        );
    }
}
