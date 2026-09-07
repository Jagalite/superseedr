// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared application file preview definitions and transitions.

use super::*;

pub(super) struct TorrentPreviewFileEntry {
    pub(super) parts: Vec<String>,
    pub(super) file_index: usize,
    pub(super) size: u64,
}

pub(crate) fn merge_file_browser_mode_for_fetch(
    current: &FileBrowserMode,
    incoming: FileBrowserMode,
) -> FileBrowserMode {
    match (current, incoming) {
        (
            FileBrowserMode::DownloadLocSelection {
                target: current_target,
                torrent_files: current_torrent_files,
                container_name: current_container_name,
                use_container: current_use_container,
                is_editing_name: current_is_editing_name,
                focused_pane: current_focused_pane,
                preview_tree: current_preview_tree,
                preview_state: current_preview_state,
                cursor_pos: current_cursor_pos,
                original_name_backup: current_original_name_backup,
            },
            FileBrowserMode::DownloadLocSelection {
                target,
                torrent_files,
                container_name,
                use_container,
                is_editing_name,
                focused_pane,
                preview_tree,
                preview_state,
                cursor_pos,
                original_name_backup,
            },
        ) => {
            if current_target == &target {
                FileBrowserMode::DownloadLocSelection {
                    target: current_target.clone(),
                    torrent_files: current_torrent_files.clone(),
                    container_name: current_container_name.clone(),
                    use_container: *current_use_container,
                    is_editing_name: *current_is_editing_name,
                    focused_pane: current_focused_pane.clone(),
                    preview_tree: current_preview_tree.clone(),
                    preview_state: current_preview_state.clone(),
                    cursor_pos: *current_cursor_pos,
                    original_name_backup: current_original_name_backup.clone(),
                }
            } else {
                FileBrowserMode::DownloadLocSelection {
                    target,
                    torrent_files,
                    container_name,
                    use_container,
                    is_editing_name,
                    focused_pane,
                    preview_tree,
                    preview_state,
                    cursor_pos,
                    original_name_backup,
                }
            }
        }
        (_, incoming) => incoming,
    }
}

#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub size: u64,
    pub modified: std::time::SystemTime,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TorrentFilePreview {
    pub name: String,
    pub protocol_version: String,
    pub total_size: u64,
    pub tree: Vec<RawNode<TorrentPreviewPayload>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum TorrentFilePreviewState {
    #[default]
    Idle,
    Loading {
        path: PathBuf,
        request_id: u64,
    },
    Ready {
        path: PathBuf,
        preview: TorrentFilePreview,
    },
    Error {
        path: PathBuf,
        message: String,
    },
}

impl TorrentFilePreviewState {
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Idle => None,
            Self::Loading { path, .. } | Self::Ready { path, .. } | Self::Error { path, .. } => {
                Some(path)
            }
        }
    }
}
