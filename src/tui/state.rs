// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared TUI state and effect-payload data with no runtime or I/O ownership.

use crate::config::Settings;
use crate::networking::{NetworkBindingConfig, NetworkInterfaceInfo};
use crate::tui::tree::{RawNode, TreeViewState};
use serde::{Deserialize, Serialize};
use strum_macros::EnumIter;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FilePriority {
    #[default]
    Normal,
    High,
    Skip,
    Mixed,
}

impl FilePriority {
    pub fn next(&self) -> Self {
        match self {
            Self::Normal => Self::Skip,
            Self::Skip => Self::High,
            Self::High => Self::Normal,
            Self::Mixed => Self::Normal,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TorrentPreviewPayload {
    pub file_index: Option<usize>,
    pub size: u64,
    pub priority: FilePriority,
}

impl std::ops::AddAssign for TorrentPreviewPayload {
    fn add_assign(&mut self, rhs: Self) {
        self.size += rhs.size;
        if self.priority != rhs.priority {
            self.priority = FilePriority::Mixed;
        }
    }
}

#[derive(Default, Debug, Clone, PartialEq)]
pub enum BrowserPane {
    #[default]
    FileSystem,
    TorrentPreview,
}

#[derive(Default, Debug, Clone, PartialEq)]
pub enum DownloadSelectionTarget {
    #[default]
    PendingAdd,
    ExistingTorrent {
        info_hash: Vec<u8>,
    },
}

pub(crate) const AWAITING_MAGNET_METADATA_LABEL: &str = "awaiting magnet metadata...";

#[derive(Default, Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum FileBrowserMode {
    #[default]
    Directory,
    File(Vec<String>),
    DownloadLocSelection {
        target: DownloadSelectionTarget,
        torrent_files: Vec<String>,
        container_name: String,
        use_container: bool,
        is_editing_name: bool,
        focused_pane: BrowserPane,
        preview_tree: Vec<RawNode<TorrentPreviewPayload>>,
        preview_state: TreeViewState,
        cursor_pos: usize,
        original_name_backup: String,
    },
    ConfigPathSelection {
        target_item: ConfigItem,
        shared_mode: bool,
        current_settings: Box<Settings>,
        selected_index: usize,
        items: Vec<ConfigItem>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, EnumIter)]
pub enum ConfigItem {
    ClientPort,
    NetworkBindingMode,
    NetworkInterface,
    NetworkIpv4Enabled,
    NetworkIpv6Enabled,
    NetworkIpv4Address,
    NetworkIpv6Address,
    NetworkDnsPolicy,
    NetworkDnsServers,
    DefaultDownloadFolder,
    WatchFolder,
    UiLayoutMode,
    DownloadMode,
    AlwaysShowAddLocationPrompt,
    GlobalDownloadLimit,
    GlobalUploadLimit,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConfigPane {
    #[default]
    Settings,
    Details,
}

#[derive(Default)]
#[allow(clippy::large_enum_variant)]
pub enum AppMode {
    Welcome,
    #[default]
    Normal,
    Help,
    Journal,
    PeerManagement,
    TorrentManagement,
    PowerSaving,
    DeleteConfirm,
    Config,
    FileBrowser,
    Rss,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum TorrentControlState {
    #[default]
    Running,
    Paused,
    Deleting,
}

#[derive(Default)]
pub struct ConfigNetworkInterfaceInventory {
    pub interfaces: Vec<NetworkInterfaceInfo>,
    pub loading: bool,
    pub error: Option<String>,
    request_id: u64,
}

impl ConfigNetworkInterfaceInventory {
    pub(crate) fn begin_refresh(&mut self) -> u64 {
        self.request_id = self.request_id.wrapping_add(1);
        self.interfaces.clear();
        self.loading = true;
        self.error = None;
        self.request_id
    }

    pub(crate) fn finish_refresh(
        &mut self,
        request_id: u64,
        result: Result<Vec<NetworkInterfaceInfo>, String>,
    ) -> bool {
        if request_id != self.request_id {
            return false;
        }
        self.loading = false;
        match result {
            Ok(interfaces) => {
                self.interfaces = interfaces;
                self.error = None;
            }
            Err(error) => {
                self.interfaces.clear();
                self.error = Some(error);
            }
        }
        true
    }
}

#[derive(Default)]
pub struct ConfigUiState {
    pub settings_edit: Box<Settings>,
    pub selected_index: usize,
    pub items: Vec<ConfigItem>,
    pub active_pane: ConfigPane,
    pub editing: Option<ConfigEditState>,
    pub reset_confirmation: Option<ConfigItem>,
    pub network_interface_selection_pending: bool,
    pub network_interface_inventory: ConfigNetworkInterfaceInventory,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConfigEditState {
    pub item: ConfigItem,
    pub buffer: String,
    pub cursor: usize,
    pub select_all: bool,
    pub network_binding_on_cancel: Option<NetworkBindingConfig>,
}

#[derive(Default, Clone)]
#[allow(dead_code)]
pub struct RssPreviewItem {
    pub dedupe_key: String,
    pub title: String,
    pub link: Option<String>,
    pub guid: Option<String>,
    pub source: Option<String>,
    pub date_iso: Option<String>,
    pub is_match: bool,
    pub is_downloaded: bool,
}
