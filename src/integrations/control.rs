// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::FilePriority;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::{read_control_request, write_control_request};

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
            Self::StatusNow
            | Self::StatusFollowStart { .. }
            | Self::StatusFollowStop
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
