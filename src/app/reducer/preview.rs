// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! File acquisition and preview transitions shared by all app hosts.

use super::super::*;

pub(crate) struct TorrentPreviewRequest {
    pub browser_generation: u64,
    pub request_id: u64,
    pub path: PathBuf,
}

pub(crate) fn request_torrent_preview(state: &mut AppState) -> Option<TorrentPreviewRequest> {
    let selected_path = if matches!(state.mode, AppMode::FileBrowser)
        && matches!(
            &state.ui.file_browser.browser_mode,
            FileBrowserMode::File(_)
        )
        && !state.ui.file_browser.fetch_pending
    {
        state
            .ui
            .file_browser
            .state
            .cursor_path
            .as_ref()
            .filter(|path| {
                state.ui.file_browser.data.iter().any(|node| {
                    !node.is_dir
                        && &node.full_path == *path
                        && path
                            .extension()
                            .and_then(|extension| extension.to_str())
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("torrent"))
                })
            })
            .cloned()
    } else {
        None
    };

    let Some(path) = selected_path else {
        let file_browser = &mut state.ui.file_browser;
        if !matches!(
            file_browser.torrent_file_preview,
            TorrentFilePreviewState::Idle
        ) {
            file_browser.torrent_preview_request_id =
                file_browser.torrent_preview_request_id.wrapping_add(1);
            file_browser.torrent_file_preview = TorrentFilePreviewState::Idle;
            state.ui.needs_redraw = true;
        }
        return None;
    };

    if state.ui.file_browser.torrent_file_preview.path() == Some(path.as_path()) {
        return None;
    }

    let file_browser = &mut state.ui.file_browser;
    file_browser.torrent_preview_request_id =
        file_browser.torrent_preview_request_id.wrapping_add(1);
    let request_id = file_browser.torrent_preview_request_id;
    let browser_generation = file_browser.browser_generation;
    file_browser.torrent_file_preview = TorrentFilePreviewState::Loading {
        path: path.clone(),
        request_id,
    };
    state.ui.needs_redraw = true;

    Some(TorrentPreviewRequest {
        browser_generation,
        request_id,
        path,
    })
}

pub(crate) fn apply_torrent_preview_result(
    state: &mut AppState,
    browser_generation: u64,
    request_id: u64,
    path: PathBuf,
    result: Result<TorrentFilePreview, String>,
) -> bool {
    let browser = &mut state.ui.file_browser;
    if !matches!(state.mode, AppMode::FileBrowser)
        || browser_generation != browser.browser_generation
        || request_id != browser.torrent_preview_request_id
        || browser.state.cursor_path.as_ref() != Some(&path)
    {
        return false;
    }
    browser.torrent_file_preview = match result {
        Ok(preview) => TorrentFilePreviewState::Ready { path, preview },
        Err(message) => TorrentFilePreviewState::Error { path, message },
    };
    state.ui.needs_redraw = true;
    true
}

pub(crate) fn apply_file_tree_result(
    state: &mut AppState,
    request_id: u64,
    path: PathBuf,
    mut data: Vec<RawNode<FileMetadata>>,
    highlight_path: Option<PathBuf>,
) -> bool {
    if matches!(state.mode, AppMode::FileBrowser) {
        if request_id != state.ui.file_browser.fetch_request_id
            || path != state.ui.file_browser.state.current_path
        {
            tracing::debug!(
                target: "superseedr",
                request_id,
                ?path,
                current_request_id = state.ui.file_browser.fetch_request_id,
                current_path = ?state.ui.file_browser.state.current_path,
                "Ignoring stale file browser data"
            );
            return false;
        }

        let screen_area = state.screen_area;
        let pending_torrent_path = state.pending_torrent_path.is_some();
        let pending_torrent_link = !state.pending_torrent_link.is_empty();
        let file_browser = &mut state.ui.file_browser;
        file_browser.fetch_pending = false;
        file_browser.fetch_error = None;
        // --- 1. Apply Dynamic Sorting ---
        if let FileBrowserMode::File(extensions) = &file_browser.browser_mode {
            let target_exts: Vec<String> = extensions.iter().map(|e| e.to_lowercase()).collect();
            let has_target_files = data.iter().any(|node| {
                !node.is_dir
                    && target_exts
                        .iter()
                        .any(|ext| node.name.to_lowercase().ends_with(ext))
            });

            if !has_target_files {
                data.sort_by_key(|node| node.name.to_lowercase());
            } else {
                data.sort_by(|a, b| {
                    let a_matches = target_exts
                        .iter()
                        .any(|ext| a.name.to_lowercase().ends_with(ext));
                    let b_matches = target_exts
                        .iter()
                        .any(|ext| b.name.to_lowercase().ends_with(ext));

                    // 1. Priority: Torrents first
                    if a_matches != b_matches {
                        return b_matches.cmp(&a_matches);
                    }

                    // 2. Priority: Folders second (ensures folders follow torrents directly)
                    if a.is_dir != b.is_dir {
                        return b.is_dir.cmp(&a.is_dir); // Changed order to put folders higher
                    }

                    // 3. Final: Sort by newest date
                    b.payload.modified.cmp(&a.payload.modified)
                });
            }
        }

        // --- 2. Update Data ---
        file_browser.data = data;

        // --- 3. Select and scroll within the rows the screen actually renders ---
        reconcile_file_browser_cursor_after_fetch(
            file_browser,
            highlight_path,
            screen_area,
            pending_torrent_path,
            pending_torrent_link,
        );

        state.ui.needs_redraw = true;
        return true;
    }
    false
}

pub(crate) fn apply_file_tree_failure(
    state: &mut AppState,
    request_id: u64,
    path: PathBuf,
    message: String,
) {
    if matches!(state.mode, AppMode::FileBrowser)
        && request_id == state.ui.file_browser.fetch_request_id
        && path == state.ui.file_browser.state.current_path
    {
        let file_browser = &mut state.ui.file_browser;
        file_browser.fetch_pending = false;
        file_browser.data.clear();
        file_browser.state.cursor_path = None;
        file_browser.state.top_most_offset = 0;
        file_browser.torrent_file_preview = TorrentFilePreviewState::Idle;
        file_browser.fetch_error = Some(message);
        state.ui.needs_redraw = true;
    }
}

pub(crate) fn begin_file_tree_request(
    state: &mut AppState,
    browser_generation: u64,
    path: PathBuf,
    browser_mode: FileBrowserMode,
    preserve_browser_mode: bool,
) -> Option<u64> {
    if browser_generation != state.ui.file_browser.browser_generation {
        tracing::debug!(
            target: "superseedr",
            browser_generation,
            current_browser_generation = state.ui.file_browser.browser_generation,
            ?path,
            "Ignoring stale file browser fetch"
        );
        return None;
    }

    let request_id = state.ui.file_browser.fetch_request_id.wrapping_add(1);
    {
        let file_browser = &mut state.ui.file_browser;
        file_browser.fetch_request_id = request_id;
        file_browser.fetch_pending = true;
        file_browser.fetch_error = None;
        file_browser.torrent_preview_request_id =
            file_browser.torrent_preview_request_id.wrapping_add(1);
        file_browser.torrent_file_preview = TorrentFilePreviewState::Idle;
        if !preserve_browser_mode {
            file_browser.search_state = BrowserSearchState::Closed;
            file_browser.search_query.clear();
        }
    }

    if matches!(state.mode, AppMode::FileBrowser) {
        let file_browser = &mut state.ui.file_browser;
        file_browser.state.current_path = path.clone();
        file_browser.state.cursor_path = None;
        file_browser.state.top_most_offset = 0;
        file_browser.state.expanded_paths.clear();
        file_browser.state.selected_paths.clear();
        file_browser.data.clear();
        file_browser.browser_mode = if preserve_browser_mode {
            merge_file_browser_mode_for_fetch(&file_browser.browser_mode, browser_mode)
        } else {
            browser_mode
        };
    } else {
        let mut tree_state = crate::tui::tree::TreeViewState::new();
        tree_state.current_path = path.clone();
        state.ui.file_browser.state = tree_state;
        state.ui.file_browser.data = Vec::new();
        state.ui.file_browser.browser_mode = browser_mode;
        state.mode = AppMode::FileBrowser;
    }

    state.ui.needs_redraw = true;
    Some(request_id)
}
