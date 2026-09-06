// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser host preview; shared app transitions retain application policy.

use super::*;

impl BrowserSession {
    pub(crate) fn default_download_path(&self) -> PathBuf {
        self.environment.download_root.clone()
    }

    pub(crate) fn accepts_pasted_text(&self, pasted_text: &str) -> bool {
        has_browser_magnet_scheme(pasted_text.trim())
    }

    pub(crate) fn begin_file_browser_fetch(
        &mut self,
        browser_generation: u64,
        path: PathBuf,
        browser_mode: FileBrowserMode,
        preserve_browser_mode: bool,
    ) -> bool {
        crate::app::reducer::preview::begin_file_tree_request(
            &mut self.app_state,
            browser_generation,
            path,
            browser_mode,
            preserve_browser_mode,
        )
        .is_some()
    }

    pub(crate) fn request_file_tree(
        &mut self,
        browser_generation: u64,
        path: PathBuf,
        browser_mode: FileBrowserMode,
        preserve_browser_mode: bool,
        highlight_path: Option<PathBuf>,
    ) {
        if self.begin_file_browser_fetch(
            browser_generation,
            path.clone(),
            browser_mode,
            preserve_browser_mode,
        ) {
            self.enqueue_command(BrowserCommand::FetchFileTree {
                browser_generation,
                path,
                highlight_path,
            });
        }
    }

    pub(crate) fn open_add_torrent_file_browser(&mut self) {
        let initial_path = self
            .client_configs
            .default_download_folder
            .clone()
            .unwrap_or_else(|| self.environment.file_browser_root.clone());
        let browser = &mut self.app_state.ui.file_browser;
        let browser_generation = browser.next_browser_generation();
        browser.return_to_torrent_management_on_close = false;
        if browser.state.current_path != initial_path || browser.data.is_empty() {
            self.request_file_tree(
                browser_generation,
                initial_path,
                FileBrowserMode::File(vec![".torrent".to_string()]),
                false,
                None,
            );
            return;
        }

        let browser = &mut self.app_state.ui.file_browser;
        browser.search_state = BrowserSearchState::Closed;
        browser.search_query.clear();
        browser.fetch_pending = false;
        browser.fetch_error = None;
        browser.browser_mode = FileBrowserMode::File(vec![".torrent".to_string()]);
        self.app_state.mode = AppMode::FileBrowser;
    }

    pub(crate) fn open_manual_magnet_browser(
        &mut self,
        magnet_link: String,
        container_name: String,
    ) {
        let preview_tree = canonical_browser_magnet_info_hash(&magnet_link)
            .and_then(|info_hash| self.app_state.torrents.get(&info_hash))
            .map(|display| display.file_preview_tree.clone())
            .unwrap_or_default();
        let mut preview_state = crate::tui::tree::TreeViewState::new();
        for node in &preview_tree {
            node.expand_all(&mut preview_state);
        }
        preview_state.cursor_path = preview_tree.first().map(|node| node.full_path.clone());
        self.app_state.pending_torrent_path = None;
        self.app_state.pending_torrent_link = magnet_link;
        let initial_path = self
            .client_configs
            .default_download_folder
            .clone()
            .unwrap_or_else(|| self.environment.download_root.clone());
        let focused_pane = if self.client_configs.default_download_folder.is_some() {
            BrowserPane::TorrentPreview
        } else {
            BrowserPane::FileSystem
        };
        let browser_generation = self.app_state.ui.file_browser.next_browser_generation();
        self.request_file_tree(
            browser_generation,
            initial_path,
            FileBrowserMode::DownloadLocSelection {
                target: DownloadSelectionTarget::PendingAdd,
                torrent_files: Vec::new(),
                container_name: container_name.clone(),
                use_container: true,
                is_editing_name: false,
                focused_pane,
                preview_tree,
                preview_state,
                cursor_pos: 0,
                original_name_backup: container_name,
            },
            false,
            None,
        );
    }

    pub(crate) fn open_manual_torrent_file_browser(&mut self, path: PathBuf) -> bool {
        let (container_name, preview_tree) =
            match &self.app_state.ui.file_browser.torrent_file_preview {
                TorrentFilePreviewState::Ready {
                    path: preview_path,
                    preview,
                } if preview_path == &path => (preview.name.clone(), preview.tree.clone()),
                _ => return false,
            };
        let file_count = preview_tree.iter().map(preview_file_count).sum::<usize>();
        let mut preview_state = crate::tui::tree::TreeViewState::new();
        for node in &preview_tree {
            node.expand_all(&mut preview_state);
        }
        preview_state.cursor_path = preview_tree.first().map(|node| node.full_path.clone());

        self.app_state.pending_torrent_link.clear();
        self.app_state.pending_torrent_path = Some(path);
        let initial_path = self
            .client_configs
            .default_download_folder
            .clone()
            .unwrap_or_else(|| self.environment.download_root.clone());
        let focused_pane = if self.client_configs.default_download_folder.is_some() {
            BrowserPane::TorrentPreview
        } else {
            BrowserPane::FileSystem
        };
        let browser_generation = self.app_state.ui.file_browser.next_browser_generation();
        self.request_file_tree(
            browser_generation,
            initial_path,
            FileBrowserMode::DownloadLocSelection {
                target: DownloadSelectionTarget::PendingAdd,
                torrent_files: Vec::new(),
                container_name: container_name.clone(),
                use_container: file_count > 1,
                is_editing_name: false,
                focused_pane,
                preview_tree,
                preview_state,
                cursor_pos: 0,
                original_name_backup: container_name,
            },
            false,
            None,
        );
        true
    }

    pub(crate) fn open_existing_torrent_file_browser(&mut self, info_hash: Vec<u8>) {
        let Some(display) = self.app_state.torrents.get(&info_hash) else {
            return;
        };
        let return_to_torrent_management =
            matches!(self.app_state.mode, AppMode::TorrentManagement);
        let mut preview_state = crate::tui::tree::TreeViewState::new();
        for node in &display.file_preview_tree {
            node.expand_all(&mut preview_state);
        }
        preview_state.cursor_path = display
            .file_preview_tree
            .first()
            .map(|node| node.full_path.clone());
        let initial_path = display
            .latest_state
            .download_path
            .clone()
            .or_else(|| self.client_configs.default_download_folder.clone())
            .unwrap_or_else(|| self.environment.download_root.clone());
        let preview_tree = display.file_preview_tree.clone();
        let fetch_browser_mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::ExistingTorrent {
                info_hash: info_hash.clone(),
            },
            torrent_files: Vec::new(),
            container_name: String::new(),
            use_container: false,
            is_editing_name: false,
            focused_pane: BrowserPane::TorrentPreview,
            preview_tree: Vec::new(),
            preview_state: crate::tui::tree::TreeViewState::default(),
            cursor_pos: 0,
            original_name_backup: String::new(),
        };

        let browser = &mut self.app_state.ui.file_browser;
        let needs_file_tree_fetch =
            browser.state.current_path != initial_path || browser.data.is_empty();
        browser.invalidate_browser_generation();
        let browser_generation = browser.browser_generation;
        if needs_file_tree_fetch {
            browser.state = crate::tui::tree::TreeViewState {
                current_path: initial_path.clone(),
                ..crate::tui::tree::TreeViewState::default()
            };
            browser.data.clear();
        } else {
            browser.fetch_pending = false;
            browser.fetch_error = None;
        }
        browser.search_state = BrowserSearchState::Closed;
        browser.search_query.clear();
        browser.return_to_torrent_management_on_close = return_to_torrent_management;
        browser.browser_mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::ExistingTorrent { info_hash },
            torrent_files: Vec::new(),
            container_name: String::new(),
            use_container: false,
            is_editing_name: false,
            focused_pane: BrowserPane::TorrentPreview,
            preview_tree,
            preview_state,
            cursor_pos: 0,
            original_name_backup: String::new(),
        };
        self.app_state.mode = AppMode::FileBrowser;
        self.app_state.ui.needs_redraw = true;
        if needs_file_tree_fetch {
            self.request_file_tree(
                browser_generation,
                initial_path,
                fetch_browser_mode,
                true,
                None,
            );
        }
    }

    pub(crate) fn sync_torrent_file_preview(&mut self) {
        self.sync_browser_torrent_preview_request();
    }

    pub fn apply_browser_file_tree(
        &mut self,
        browser_generation: u64,
        path: PathBuf,
        entries: Vec<BrowserFileTreeEntry>,
        highlight_path: Option<PathBuf>,
    ) -> bool {
        let browser = &self.app_state.ui.file_browser;
        if browser_generation != browser.browser_generation {
            return false;
        }
        let request_id = browser.fetch_request_id;
        let data = entries
            .into_iter()
            .map(|entry| RawNode {
                name: entry.name.clone(),
                full_path: path.join(entry.name),
                children: Vec::new(),
                payload: FileMetadata {
                    size: entry.size,
                    modified: UNIX_EPOCH
                        + Duration::from_secs(self.environment.file_modified_unix_secs),
                },
                is_dir: entry.is_dir,
            })
            .collect();
        let applied = crate::app::reducer::preview::apply_file_tree_result(
            &mut self.app_state,
            request_id,
            path,
            data,
            highlight_path,
        );
        if applied {
            self.sync_browser_torrent_preview_request();
        }
        applied
    }

    pub(super) fn sync_browser_torrent_preview_request(&mut self) {
        let Some(crate::app::reducer::preview::TorrentPreviewRequest {
            browser_generation,
            request_id,
            path,
        }) = crate::app::reducer::preview::request_torrent_preview(&mut self.app_state)
        else {
            return;
        };
        self.pending_browser_commands
            .push_back(BrowserCommand::FetchTorrentPreview {
                browser_generation,
                request_id,
                path,
            });
        self.app_state.ui.needs_redraw = true;
    }

    pub fn apply_browser_torrent_preview(
        &mut self,
        browser_generation: u64,
        request_id: u64,
        path: PathBuf,
        name: String,
        protocol_version: String,
        files: Vec<BrowserTorrentPreviewFile>,
    ) -> bool {
        let total_size = files.iter().map(|file| file.size).sum();
        let tree = build_torrent_preview_tree(
            files
                .into_iter()
                .map(|file| {
                    (
                        file.relative_path
                            .split('/')
                            .filter(|segment| !segment.is_empty())
                            .map(str::to_string)
                            .collect(),
                        file.size,
                    )
                })
                .collect(),
            &Default::default(),
        );
        crate::app::reducer::preview::apply_torrent_preview_result(
            &mut self.app_state,
            browser_generation,
            request_id,
            path,
            Ok(TorrentFilePreview {
                name,
                protocol_version,
                total_size,
                tree,
            }),
        )
    }
}
