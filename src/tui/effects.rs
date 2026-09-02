// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Data-only screen actions and external effects emitted by the shared TUI reducers.

use super::state::{
    ConfigUiState, DownloadSelectionTarget, FileBrowserMode, FilePriority, RssPreviewItem,
    TorrentControlState,
};
use crate::config::Settings;
use crate::integrations::control::{ControlFilePriorityOverride, ControlRequest};
use std::collections::HashMap;
use std::path::PathBuf;

pub struct DownloadConfirmPayload {
    pub base_path: PathBuf,
    pub container_name_to_use: Option<String>,
    pub file_priorities: HashMap<usize, FilePriority>,
    pub target: DownloadSelectionTarget,
    pub has_preview_files: bool,
}

pub enum ConfirmDecision {
    ToConfig(ConfigUiState),
    Download(DownloadConfirmPayload),
    File(PathBuf),
    None,
}

pub enum BrowserFsEffect {
    FetchFileTree {
        path: PathBuf,
        browser_mode: FileBrowserMode,
        highlight_path: Option<PathBuf>,
    },
}

pub enum BrowserDialogEffect {
    ExecuteConfirmDecision(ConfirmDecision),
    ToConfig(ConfigUiState),
    CleanupPendingLink,
    ToNormalAndClearPending,
    ClearSearch,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BrowserTransition {
    ToNormal,
    ToConfig,
    Close,
}

pub enum ConfigEffect {
    OpenPathBrowser {
        path: PathBuf,
        browser_mode: Box<FileBrowserMode>,
    },
    RefreshNetworkInterfaces,
    ApplySettings,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DeleteConfirmEffect {
    SendManagerCommand {
        info_hash: Vec<u8>,
        with_files: bool,
    },
    MarkDeleting {
        info_hash: Vec<u8>,
    },
    ToNormal,
}

pub enum JournalEffect {
    ReplaySource(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub enum UiEffect {
    ToPowerSaving,
    ToDeleteConfirm,
    OpenAddTorrentFileBrowser,
    OpenExistingTorrentFileBrowser(Vec<u8>),
    OpenConfigScreen,
    OpenRssScreen,
    OpenJournalScreen,
    OpenPeerManagementScreen,
    OpenTorrentManagementScreen,
    BroadcastManagerDataRate(u64),
    ApplyThemePrev,
    ApplyThemeNext,
    PersistVisualizationSelections,
    SendPause(Vec<u8>),
    SendResume(Vec<u8>),
    OpenHelpScreen,
    HandlePastedText(String),
}

pub enum RssRuntimeEffect {
    UpdateConfig(Box<Settings>),
    SyncNow,
    DownloadPreview(RssPreviewItem),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TorrentManagementEffect {
    ToNormal,
    SubmitControlRequest(ControlRequest),
    MarkControlState {
        info_hash: Vec<u8>,
        state: TorrentControlState,
        delete_files: bool,
    },
    OpenExistingTorrentFileBrowser(Vec<u8>),
}

/// External work that cannot be completed by the platform-neutral reducer.
///
/// Variants in this enum may cross the native/browser runtime boundary. Screen
/// transitions, cursor changes, and torrent-control state never belong here.
pub enum RuntimeEffect {
    FetchFileTree {
        browser_generation: u64,
        path: PathBuf,
        browser_mode: FileBrowserMode,
        preserve_browser_mode: bool,
        highlight_path: Option<PathBuf>,
    },
    ConfirmBrowserSelection(ConfirmDecision),
    CleanupPendingPreview(Vec<u8>),
    SyncTorrentFilePreview,
    ReplayJournalSource(PathBuf),
    OpenAddTorrentFileBrowser,
    OpenExistingTorrentFileBrowser(Vec<u8>),
    RefreshPeerManagement,
    ApplyConfig(Box<Settings>),
    RefreshConfigNetworkInterfaces(ConfigNetworkInterfaceRefresh),
    BroadcastManagerDataRate(u64),
    ApplyThemePrevious,
    ApplyThemeNext,
    PersistVisualizationSelections,
    SubmitControlRequest(ControlRequest),
    HandlePastedText(String),
    UpdateRssConfig(Box<Settings>),
    SyncRss,
    DownloadRssPreview(RssPreviewItem),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigNetworkInterfaceRefresh {
    OnOpen,
    Explicit,
}

/// Data returned by external execution for the shared reducer to apply.
pub enum RuntimeOutcome {
    BrowserTransition(BrowserTransition),
    BrowserConfig(ConfigUiState),
    ConfigApplied(Settings),
}

pub(crate) fn priority_overrides(
    priorities: HashMap<usize, FilePriority>,
) -> Vec<ControlFilePriorityOverride> {
    let mut overrides: Vec<_> = priorities
        .into_iter()
        .filter(|(_, priority)| !matches!(priority, FilePriority::Normal))
        .map(|(file_index, priority)| ControlFilePriorityOverride {
            file_index,
            priority,
        })
        .collect();
    overrides.sort_by_key(|value| value.file_index);
    overrides
}
