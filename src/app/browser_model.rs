// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared application browser model definitions and transitions.

use super::*;

#[derive(Default)]
pub struct UiState {
    pub needs_redraw: bool,
    pub effects_phase_time: f64,
    pub effects_last_wall_time: f64,
    pub effects_speed_multiplier: f64,
    pub measured_fps: Option<f64>,
    pub fps_sample_started_at: Option<Instant>,
    pub fps_sample_frames: u32,
    pub frame_wake_lag_ratio_ema: Option<f64>,
    pub frame_wake_lag_secs_ema: Option<f64>,
    pub frame_draw_ratio_ema: Option<f64>,
    pub file_activity_download_phase: f64,
    pub file_activity_upload_phase: f64,
    pub swarm_availability_flash: SwarmAvailabilityFlashState,
    pub dht_wave: DhtWaveUiState,
    pub visualization_focus: VisualizationFocusState,
    pub selected_header: SelectedHeader,
    pub selected_torrent_index: usize,
    pub selected_peer_index: usize,
    pub is_searching: bool,
    pub search_query: String,
    pub config: ConfigUiState,
    pub delete_confirm: DeleteConfirmUiState,
    pub file_browser: FileBrowserUiState,
    pub help: HelpUiState,
    pub journal: JournalUiState,
    pub peer_management: PeerManagementUiState,
    pub torrent_management: TorrentManagementUiState,
    pub normal_paste_burst: PasteBurst,
    #[allow(dead_code)]
    pub rss: RssUiState,
}

impl UiState {
    pub(super) fn record_drawn_frame(&mut self, now: Instant) {
        let Some(sample_started_at) = self.fps_sample_started_at else {
            self.fps_sample_started_at = Some(now);
            self.fps_sample_frames = 0;
            return;
        };

        self.fps_sample_frames = self.fps_sample_frames.saturating_add(1);
        let elapsed = now.saturating_duration_since(sample_started_at);
        if elapsed < UI_FPS_SAMPLE_INTERVAL {
            return;
        }

        let elapsed_secs = elapsed.as_secs_f64();
        if elapsed_secs > 0.0 {
            self.measured_fps = Some(self.fps_sample_frames as f64 / elapsed_secs);
        }
        self.fps_sample_started_at = Some(now);
        self.fps_sample_frames = 0;
    }

    pub(super) fn update_responsiveness_ema(target: &mut Option<f64>, sample: f64) {
        *target = Some(match *target {
            Some(previous) => {
                (sample * UI_RESPONSIVENESS_EMA_ALPHA)
                    + (previous * (1.0 - UI_RESPONSIVENESS_EMA_ALPHA))
            }
            None => sample,
        });
    }

    pub(super) fn record_frame_wake(
        &mut self,
        scheduled_at: Instant,
        woke_at: Instant,
        target_frame_interval: Duration,
    ) {
        let wake_lag = woke_at.saturating_duration_since(scheduled_at);
        Self::update_responsiveness_ema(&mut self.frame_wake_lag_secs_ema, wake_lag.as_secs_f64());
        let target_secs = target_frame_interval.as_secs_f64();
        if target_secs > 0.0 {
            Self::update_responsiveness_ema(
                &mut self.frame_wake_lag_ratio_ema,
                wake_lag.as_secs_f64() / target_secs,
            );
        }
    }

    pub(super) fn record_draw_duration(
        &mut self,
        draw_duration: Duration,
        target_frame_interval: Duration,
    ) {
        let target_secs = target_frame_interval.as_secs_f64();
        if target_secs > 0.0 {
            Self::update_responsiveness_ema(
                &mut self.frame_draw_ratio_ema,
                draw_duration.as_secs_f64() / target_secs,
            );
        }
    }
}

#[derive(Default)]
pub struct DeleteConfirmUiState {
    pub info_hash: Vec<u8>,
    pub with_files: bool,
}

pub struct FileBrowserUiState {
    pub state: TreeViewState,
    pub data: Vec<RawNode<FileMetadata>>,
    pub browser_mode: FileBrowserMode,
    pub search_state: BrowserSearchState,
    pub search_query: String,
    pub search_mode: SearchMode,
    pub fetch_request_id: u64,
    pub fetch_pending: bool,
    pub fetch_error: Option<String>,
    pub browser_generation: u64,
    pub torrent_preview_request_id: u64,
    pub torrent_file_preview: TorrentFilePreviewState,
    pub return_to_torrent_management_on_close: bool,
}

impl Default for FileBrowserUiState {
    fn default() -> Self {
        Self {
            state: TreeViewState::default(),
            data: Vec::new(),
            browser_mode: FileBrowserMode::default(),
            search_state: BrowserSearchState::default(),
            search_query: String::new(),
            search_mode: SearchMode::Regex,
            fetch_request_id: 0,
            fetch_pending: false,
            fetch_error: None,
            browser_generation: 0,
            torrent_preview_request_id: 0,
            torrent_file_preview: TorrentFilePreviewState::Idle,
            return_to_torrent_management_on_close: false,
        }
    }
}

impl FileBrowserUiState {
    pub fn next_browser_generation(&mut self) -> u64 {
        self.browser_generation = self.browser_generation.wrapping_add(1);
        self.browser_generation
    }

    pub fn invalidate_browser_generation(&mut self) {
        let _ = self.next_browser_generation();
        self.fetch_request_id = self.fetch_request_id.wrapping_add(1);
        self.fetch_pending = false;
        self.fetch_error = None;
        self.torrent_preview_request_id = self.torrent_preview_request_id.wrapping_add(1);
        self.torrent_file_preview = TorrentFilePreviewState::Idle;
    }
}

pub(super) fn reconcile_file_browser_cursor_after_fetch(
    file_browser: &mut FileBrowserUiState,
    highlight_path: Option<PathBuf>,
    screen_area: Rect,
    pending_torrent_path: bool,
    pending_torrent_link: bool,
) {
    file_browser.state.top_most_offset = 0;

    // Preview-pane searches apply to the torrent tree, not the filesystem list.
    // Match the renderer's filter selection when choosing a post-fetch cursor.
    let filesystem_search_query = if matches!(
        focused_pane(&file_browser.browser_mode),
        BrowserPane::FileSystem
    ) {
        file_browser.search_query.as_str()
    } else {
        ""
    };
    let visible_paths: Vec<PathBuf> = TreeProjection::new(
        &file_browser.data,
        &file_browser.state,
        build_filesystem_filter(
            &file_browser.browser_mode,
            filesystem_search_query,
            file_browser.search_mode,
        ),
        usize::MAX,
    )
    .visible_window()
    .iter()
    .map(|item| item.path.clone())
    .collect();

    let cursor_path = highlight_path
        .filter(|target| visible_paths.iter().any(|path| path == target))
        .or_else(|| visible_paths.first().cloned());
    file_browser.state.cursor_path = cursor_path.clone();

    let has_preview = preview_content_for_selection(
        &file_browser.browser_mode,
        pending_torrent_path,
        pending_torrent_link,
        &file_browser.state,
        &file_browser.data,
    );
    let pane = focused_pane(&file_browser.browser_mode);
    let list_height = calculate_list_height(
        screen_area,
        has_preview,
        file_browser.search_state.is_visible(),
        &pane,
    )
    .max(1);

    if let Some(index) = cursor_path
        .as_ref()
        .and_then(|path| visible_paths.iter().position(|candidate| candidate == path))
    {
        if index >= list_height {
            file_browser.state.top_most_offset = index.saturating_sub(list_height / 2);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HelpSection {
    #[default]
    General,
    Torrents,
    Graphs,
    Legends,
    Screens,
    Paths,
    Build,
}

pub struct HelpUiState {
    pub active_section: HelpSection,
    pub scroll_offset: usize,
    pub is_searching: bool,
    pub search_query: String,
    pub search_mode: SearchMode,
}

impl Default for HelpUiState {
    fn default() -> Self {
        Self {
            active_section: HelpSection::default(),
            scroll_offset: 0,
            is_searching: false,
            search_query: String::new(),
            search_mode: SearchMode::Regex,
        }
    }
}

pub fn build_torrent_preview_tree(
    file_list: Vec<(Vec<String>, u64)>,
    file_priorities: &HashMap<usize, FilePriority>,
) -> Vec<RawNode<TorrentPreviewPayload>> {
    let entries = file_list
        .into_iter()
        .enumerate()
        .map(|(idx, (parts, size))| TorrentPreviewFileEntry {
            parts,
            file_index: idx,
            size,
        })
        .collect();

    build_torrent_preview_tree_from_entries(entries, file_priorities)
}

pub(super) fn build_torrent_preview_tree_from_entries(
    file_entries: Vec<TorrentPreviewFileEntry>,
    file_priorities: &HashMap<usize, FilePriority>,
) -> Vec<RawNode<TorrentPreviewPayload>> {
    let file_count = file_entries.len();
    let preview_payloads: Vec<(Vec<String>, TorrentPreviewPayload)> = file_entries
        .into_iter()
        .map(|entry| {
            (
                entry.parts,
                TorrentPreviewPayload {
                    file_index: Some(entry.file_index),
                    size: entry.size,
                    priority: file_priorities
                        .get(&entry.file_index)
                        .copied()
                        .unwrap_or(FilePriority::Normal),
                },
            )
        })
        .collect();

    let mut tree = RawNode::from_path_list(None, preview_payloads);
    refresh_torrent_preview_directory_priorities(&mut tree);
    tracing::debug!(
        target: "superseedr",
        file_count,
        tree_roots = tree.len(),
        "Built torrent preview tree"
    );
    tree
}

pub fn refresh_torrent_preview_directory_priorities(nodes: &mut [RawNode<TorrentPreviewPayload>]) {
    for node in nodes {
        refresh_torrent_preview_node_priority(node);
    }
}

pub fn apply_torrent_preview_file_priorities(
    nodes: &mut [RawNode<TorrentPreviewPayload>],
    file_priorities: &HashMap<usize, FilePriority>,
) {
    for node in nodes.iter_mut() {
        if let Some(file_index) = node.payload.file_index {
            node.payload.priority = file_priorities
                .get(&file_index)
                .copied()
                .unwrap_or(FilePriority::Normal);
        }
        apply_torrent_preview_file_priorities(&mut node.children, file_priorities);
    }
    refresh_torrent_preview_directory_priorities(nodes);
}

pub(super) fn refresh_torrent_preview_node_priority(
    node: &mut RawNode<TorrentPreviewPayload>,
) -> FilePriority {
    if !node.is_dir {
        return node.payload.priority;
    }

    let mut common = None;
    let mut mixed = false;
    for child in &mut node.children {
        let child_priority = refresh_torrent_preview_node_priority(child);
        match common {
            Some(priority) if priority != child_priority => mixed = true,
            Some(_) => {}
            None => common = Some(child_priority),
        }
    }

    node.payload.priority = if mixed {
        FilePriority::Mixed
    } else {
        common.unwrap_or(node.payload.priority)
    };
    node.payload.priority
}

pub(super) fn collect_torrent_preview_files(
    node: &RawNode<TorrentPreviewPayload>,
    path: &mut Vec<String>,
    files: &mut Vec<TorrentPreviewFileEntry>,
) {
    path.push(node.name.clone());
    if node.is_dir {
        for child in &node.children {
            collect_torrent_preview_files(child, path, files);
        }
    } else if let Some(file_index) = node.payload.file_index {
        files.push(TorrentPreviewFileEntry {
            parts: path.clone(),
            file_index,
            size: node.payload.size,
        });
    }
    path.pop();
}

pub(super) fn rebuild_torrent_preview_tree(
    existing_tree: &[RawNode<TorrentPreviewPayload>],
    file_priorities: &HashMap<usize, FilePriority>,
) -> Vec<RawNode<TorrentPreviewPayload>> {
    let mut files = Vec::new();
    let mut path = Vec::new();
    for node in existing_tree {
        collect_torrent_preview_files(node, &mut path, &mut files);
    }
    build_torrent_preview_tree_from_entries(files, file_priorities)
}
