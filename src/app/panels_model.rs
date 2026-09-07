// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared application panels model definitions and transitions.

use super::*;

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalFilter {
    #[default]
    All,
    Queue,
    Commands,
    Health,
    Network,
}

impl JournalFilter {
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Queue,
            Self::Queue => Self::Commands,
            Self::Commands => Self::Health,
            Self::Health => Self::Network,
            Self::Network => Self::All,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::All => Self::Network,
            Self::Queue => Self::All,
            Self::Commands => Self::Queue,
            Self::Health => Self::Commands,
            Self::Network => Self::Health,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "ALL",
            Self::Queue => "INGEST",
            Self::Commands => "COMMANDS",
            Self::Health => "HEALTH",
            Self::Network => "NETWORK",
        }
    }
}

pub struct JournalUiState {
    pub filter: JournalFilter,
    pub selected_index: usize,
    pub scroll_offset: usize,
    pub status_message: Option<String>,
    pub is_searching: bool,
    pub search_query: String,
    pub search_mode: SearchMode,
}

impl Default for JournalUiState {
    fn default() -> Self {
        Self {
            filter: JournalFilter::default(),
            selected_index: 0,
            scroll_offset: 0,
            status_message: None,
            is_searching: false,
            search_query: String::new(),
            search_mode: SearchMode::Regex,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TorrentManagementReviewCache {
    pub(crate) pause: Vec<String>,
    pub(crate) resume: Vec<String>,
    pub(crate) delete: Vec<String>,
    pub(crate) purge: Vec<String>,
    pub(crate) longest_line_width: usize,
}

pub struct TorrentManagementUiState {
    pub selected_index: usize,
    pub cursor_hash: Option<Vec<u8>>,
    pub selected_hashes: HashSet<Vec<u8>>,
    pub pending_commands: Vec<TorrentManagementPendingCommand>,
    pub is_searching: bool,
    pub search_query: String,
    pub search_mode: SearchMode,
    pub selected_column_index: usize,
    pub sort_column_index: Option<usize>,
    pub sort_direction: SortDirection,
    pub status_message: Option<String>,
    pub confirm_submit: bool,
    pub review_scroll_offset: usize,
    pub input_latch: Option<crate::terminal_event::KeyCode>,
    pub(crate) review_cache: Option<TorrentManagementReviewCache>,
}

impl Default for TorrentManagementUiState {
    fn default() -> Self {
        Self {
            selected_index: 0,
            cursor_hash: None,
            selected_hashes: HashSet::new(),
            pending_commands: Vec::new(),
            is_searching: false,
            search_query: String::new(),
            search_mode: SearchMode::Regex,
            selected_column_index: 1,
            sort_column_index: Some(1),
            sort_direction: SortDirection::Ascending,
            status_message: None,
            confirm_submit: false,
            review_scroll_offset: 0,
            input_latch: None,
            review_cache: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchMode {
    #[default]
    Fuzzy,
    Regex,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PeerManagementFilter {
    #[default]
    All,
    Active,
    Recent,
    Restricted,
}

impl PeerManagementFilter {
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Active,
            Self::Active => Self::Recent,
            Self::Recent => Self::Restricted,
            Self::Restricted => Self::All,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::All => Self::Restricted,
            Self::Active => Self::All,
            Self::Recent => Self::Active,
            Self::Restricted => Self::Recent,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "ALL",
            Self::Active => "ACTIVE",
            Self::Recent => "RECENT",
            Self::Restricted => "RESTRICTED",
        }
    }
}

pub struct PeerManagementUiState {
    pub selected_index: usize,
    pub filter: PeerManagementFilter,
    pub is_searching: bool,
    pub search_query: String,
    pub search_mode: SearchMode,
    pub selected_column_index: usize,
    pub sort_column_index: Option<usize>,
    pub sort_direction: SortDirection,
    pub show_details: bool,
    pub details_peer_ip: Option<IpAddr>,
    pub details_scroll_offset: usize,
    pub details_is_searching: bool,
    pub details_search_query: String,
    pub details_search_mode: SearchMode,
    pub status_message: Option<String>,
}

impl Default for PeerManagementUiState {
    fn default() -> Self {
        Self {
            selected_index: 0,
            filter: PeerManagementFilter::All,
            is_searching: false,
            search_query: String::new(),
            search_mode: SearchMode::Regex,
            selected_column_index: 9,
            sort_column_index: Some(9),
            sort_direction: SortDirection::Descending,
            show_details: false,
            details_peer_ip: None,
            details_scroll_offset: 0,
            details_is_searching: false,
            details_search_query: String::new(),
            details_search_mode: SearchMode::Regex,
            status_message: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BrowserSearchState {
    #[default]
    Closed,
    Editing,
    Applied,
}

impl BrowserSearchState {
    pub fn is_editing(self) -> bool {
        matches!(self, Self::Editing)
    }

    pub fn is_visible(self) -> bool {
        !matches!(self, Self::Closed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TorrentManagementPendingCommand {
    pub info_hash: Vec<u8>,
    pub request: ControlRequest,
    pub state: TorrentControlState,
    pub delete_files: bool,
}

#[derive(Default)]
#[allow(dead_code)]
pub struct RssUiState {
    pub active_screen: RssScreen,
    pub focused_section: RssSectionFocus,
    pub selected_feed_index: usize,
    pub selected_filter_index: usize,
    pub selected_explorer_index: usize,
    pub selected_history_index: usize,
    pub is_searching: bool,
    pub search_query: String,
    pub is_editing: bool,
    pub edit_buffer: String,
    pub filter_draft: String,
    pub add_feed_buffer: String,
    pub add_filter_buffer: String,
    pub add_filter_mode: RssFilterMode,
    pub delete_confirm_armed: bool,
    pub status_message: Option<String>,
    pub last_sync_request_at: Option<Instant>,
}

#[derive(Default, Clone)]
pub struct RssRuntimeState {
    pub history: Vec<RssHistoryEntry>,
    pub preview_items: Vec<RssPreviewItem>,
    pub last_sync_at: Option<String>,
    pub next_sync_at: Option<String>,
    pub feed_errors: HashMap<String, FeedSyncError>,
}

#[derive(Default, Clone)]
pub struct RssFilterRuntimeStat {
    pub downloaded_matches: usize,
    pub history_age: String,
}

#[derive(Default, Clone)]
pub struct RssDerivedState {
    pub explorer_items: Vec<RssPreviewItem>,
    pub explorer_combined_match: Vec<bool>,
    pub explorer_prioritise_matches: bool,
    pub history_hash_by_dedupe: HashMap<String, Vec<u8>>,
    pub filter_runtime_stats: HashMap<usize, RssFilterRuntimeStat>,
}

/// Platform-resolved paths exposed to the platform-neutral TUI as inert data.
///
/// Native startup populates these from the host configuration environment. The
/// browser supplies virtual paths from its simulation fixture. Rendering and
/// reducers never query the host filesystem or process environment directly.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct RuntimePathView {
    pub shared_mode: bool,
    pub settings_path: Option<PathBuf>,
    pub log_files_path: Option<PathBuf>,
    pub fallback_watch_path: Option<PathBuf>,
    pub shared_inbox_path: Option<PathBuf>,
}

impl RuntimePathView {
    pub fn resolved_watch_path(&self, settings: &Settings) -> Option<PathBuf> {
        settings
            .watch_folder
            .clone()
            .or_else(|| self.fallback_watch_path.clone())
    }
}
