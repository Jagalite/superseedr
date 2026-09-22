// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native preview execution. Runtime resources remain owned by `App`.

use super::*;

impl App {
    pub(super) fn open_manual_browser_for_torrent_file_with_archive(
        &mut self,
        path: PathBuf,
        archive_watched_input: bool,
    ) -> Result<(), String> {
        let buffer = fs::read(&path).map_err(|error| {
            format_filesystem_path_error("Failed to read torrent file", &path, &error)
        })?;
        let torrent = from_bytes(&buffer)
            .map_err(|_| "Failed to parse torrent file for preview.".to_string())?;

        let final_path = if archive_watched_input
            && (self.is_host_watch_path(&path) || self.is_shared_inbox_path(&path))
        {
            match archive_watch_file(&path, "torrent.added") {
                Ok(final_path) => {
                    self.update_pending_ingest_source_path(&path, final_path.clone());
                    final_path
                }
                Err(error) => {
                    tracing::error!("Failed to archive watched file for manual add: {}", error);
                    path.clone()
                }
            }
        } else {
            path.clone()
        };

        let info_hash = if torrent.info.meta_version == Some(2) {
            let mut hasher = Sha256::new();
            hasher.update(&torrent.info_dict_bencode);
            hasher.finalize()[0..20].to_vec()
        } else {
            let mut hasher = sha1::Sha1::new();
            hasher.update(&torrent.info_dict_bencode);
            hasher.finalize().to_vec()
        };

        let info_hash_hex = hex::encode(&info_hash);
        let default_container_name = format!("{} [{}]", torrent.info.name, info_hash_hex);
        let file_list = torrent.file_list();
        let should_enclose = file_list.len() > 1;
        let preview_payloads: Vec<(Vec<String>, TorrentPreviewPayload)> = file_list
            .into_iter()
            .enumerate()
            .map(|(idx, (parts, size))| {
                (
                    parts,
                    TorrentPreviewPayload {
                        file_index: Some(idx),
                        size,
                        priority: FilePriority::Normal,
                    },
                )
            })
            .collect();

        let preview_tree = RawNode::from_path_list(None, preview_payloads);
        let mut preview_state = TreeViewState::new();
        for node in &preview_tree {
            node.expand_all(&mut preview_state);
        }

        self.cleanup_pending_magnet_preview_runtime();
        self.app_state.pending_torrent_link.clear();
        self.app_state.pending_torrent_path = Some(final_path);
        let initial_path = self.get_initial_destination_path();
        let initial_pane = self.initial_download_selection_pane();
        let browser_generation = self.app_state.ui.file_browser.next_browser_generation();

        self.start_file_browser_fetch(
            browser_generation,
            initial_path,
            FileBrowserMode::DownloadLocSelection {
                target: DownloadSelectionTarget::PendingAdd,
                torrent_files: vec![],
                container_name: default_container_name.clone(),
                use_container: should_enclose,
                is_editing_name: false,
                preview_tree,
                preview_state,
                focused_pane: initial_pane,
                cursor_pos: 0,
                original_name_backup: default_container_name,
            },
            false,
            None,
        );
        Ok(())
    }

    pub(super) async fn open_manual_browser_for_payload(
        &mut self,
        source: IngestSource,
        payload: ResolvedAddPayload,
    ) -> Result<(), String> {
        match payload {
            ResolvedAddPayload::TorrentFile { source_path } => {
                if matches!(source, IngestSource::TorrentFile) {
                    let archive_watched_input = !self.is_shared_inbox_path(&source_path);
                    self.open_manual_browser_for_torrent_file_with_archive(
                        source_path,
                        archive_watched_input,
                    )
                } else {
                    self.cleanup_pending_magnet_preview_runtime();
                    self.app_state.pending_torrent_link.clear();
                    self.app_state.pending_torrent_path = Some(source_path);
                    let initial_path = self.get_initial_destination_path();
                    let initial_pane = self.initial_download_selection_pane();
                    let browser_generation =
                        self.app_state.ui.file_browser.next_browser_generation();
                    self.start_file_browser_fetch(
                        browser_generation,
                        initial_path,
                        FileBrowserMode::DownloadLocSelection {
                            target: DownloadSelectionTarget::PendingAdd,
                            torrent_files: vec![],
                            container_name: "New Torrent".to_string(),
                            use_container: true,
                            is_editing_name: false,
                            preview_tree: Vec::new(),
                            preview_state: TreeViewState::default(),
                            focused_pane: initial_pane,
                            cursor_pos: 0,
                            original_name_backup: "New Torrent".to_string(),
                        },
                        false,
                        None,
                    );
                    Ok(())
                }
            }
            ResolvedAddPayload::MagnetLink { magnet_link } => {
                self.cleanup_pending_magnet_preview_runtime();
                self.app_state.pending_torrent_path = None;
                self.app_state.pending_torrent_link = magnet_link.clone();
                let (btih, btmh) = parse_hybrid_hashes(&magnet_link);
                let pending_info_hash = btih.or(btmh);
                let initial_path = self.get_initial_destination_path();
                let initial_pane = self.initial_download_selection_pane();
                let browser_generation = self.app_state.ui.file_browser.next_browser_generation();
                let (container_name, use_container) = if self.is_current_shared_follower() {
                    (String::new(), false)
                } else {
                    (AWAITING_MAGNET_METADATA_LABEL.to_string(), true)
                };
                self.start_file_browser_fetch(
                    browser_generation,
                    initial_path,
                    FileBrowserMode::DownloadLocSelection {
                        target: DownloadSelectionTarget::PendingAdd,
                        torrent_files: vec![],
                        container_name: container_name.clone(),
                        use_container,
                        is_editing_name: false,
                        preview_tree: Vec::new(),
                        preview_state: TreeViewState::default(),
                        focused_pane: initial_pane,
                        cursor_pos: 0,
                        original_name_backup: container_name,
                    },
                    false,
                    None,
                );
                if !self.is_current_shared_follower() {
                    let ingest_result = self
                        .add_magnet_torrent(
                            "Fetching name...".to_string(),
                            magnet_link,
                            None,
                            false,
                            TorrentControlState::Running,
                            HashMap::new(),
                            None,
                        )
                        .await;
                    match ingest_result {
                        CommandIngestResult::Added { info_hash, .. } => {
                            let info_hash = info_hash.or_else(|| pending_info_hash.clone());
                            if let Some(info_hash) = info_hash {
                                self.app_state.pending_magnet_preview_info_hash =
                                    Some(info_hash.clone());
                                self.hydrate_pending_magnet_browser_from_display(&info_hash);
                            }
                        }
                        CommandIngestResult::Duplicate { info_hash, .. } => {
                            let info_hash = info_hash.or_else(|| pending_info_hash.clone());
                            if let Some(info_hash) = info_hash {
                                self.hydrate_pending_magnet_browser_from_display(&info_hash);
                            }
                        }
                        CommandIngestResult::Failed { message, .. }
                        | CommandIngestResult::Invalid { message, .. } => {
                            self.app_state.system_error = Some(message);
                        }
                    }
                }
                Ok(())
            }
        }
    }

    pub(crate) async fn open_manual_magnet_browser(
        &mut self,
        magnet_link: String,
    ) -> Result<(), String> {
        self.open_manual_browser_for_payload(
            IngestSource::MagnetFile,
            ResolvedAddPayload::MagnetLink { magnet_link },
        )
        .await
    }

    pub(crate) fn open_add_torrent_file_browser(&mut self) {
        let initial_path = self.get_initial_source_path();
        let browser_generation = self.app_state.ui.file_browser.next_browser_generation();
        self.app_state
            .ui
            .file_browser
            .return_to_torrent_management_on_close = false;
        self.start_file_browser_fetch(
            browser_generation,
            initial_path,
            FileBrowserMode::File(vec![".torrent".to_string()]),
            false,
            None,
        );
    }

    pub(crate) fn open_existing_torrent_file_browser(&mut self, info_hash: Vec<u8>) {
        let Some(display) = self.app_state.torrents.get(&info_hash) else {
            return;
        };
        let return_to_torrent_management_on_close =
            matches!(self.app_state.mode, AppMode::TorrentManagement);
        let metrics = display.latest_state.clone();
        let mut preview_tree = display.file_preview_tree.clone();
        if preview_tree.is_empty() {
            if let Some(metadata) = self.persisted_torrent_metadata_cache.get(&info_hash) {
                let files = metadata
                    .files
                    .iter()
                    .map(|file| {
                        (
                            file.relative_path
                                .split('/')
                                .filter(|segment| !segment.is_empty())
                                .map(|segment| segment.to_string())
                                .collect::<Vec<_>>(),
                            file.length,
                        )
                    })
                    .collect();
                preview_tree = build_torrent_preview_tree(files, &metrics.file_priorities);
            }
        }

        let mut preview_state = TreeViewState::new();
        for node in &preview_tree {
            node.expand_all(&mut preview_state);
        }
        preview_state.cursor_path = preview_tree.first().map(|node| node.full_path.clone());

        let initial_path = metrics
            .download_path
            .clone()
            .or_else(|| self.client_configs.default_download_folder.clone())
            .unwrap_or_else(|| self.get_initial_destination_path());

        let should_abandon_pending_magnet_preview = !self.app_state.pending_torrent_link.is_empty();
        self.app_state.pending_torrent_path = None;
        self.app_state.pending_torrent_link.clear();
        if should_abandon_pending_magnet_preview {
            self.cleanup_pending_magnet_preview_runtime();
        }
        self.app_state
            .ui
            .file_browser
            .invalidate_browser_generation();
        self.app_state.ui.file_browser.state = TreeViewState {
            current_path: initial_path,
            ..TreeViewState::default()
        };
        self.app_state.ui.file_browser.data.clear();
        self.app_state.ui.file_browser.search_state = BrowserSearchState::Closed;
        self.app_state.ui.file_browser.search_query.clear();
        self.app_state
            .ui
            .file_browser
            .return_to_torrent_management_on_close = return_to_torrent_management_on_close;
        self.app_state.ui.file_browser.browser_mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::ExistingTorrent { info_hash },
            torrent_files: vec![],
            container_name: String::new(),
            use_container: false,
            is_editing_name: false,
            preview_tree,
            preview_state,
            focused_pane: BrowserPane::TorrentPreview,
            cursor_pos: 0,
            original_name_backup: String::new(),
        };
        self.app_state.mode = AppMode::FileBrowser;
    }

    pub(super) fn start_file_browser_fetch(
        &mut self,
        browser_generation: u64,
        path: PathBuf,
        browser_mode: FileBrowserMode,
        preserve_browser_mode: bool,
        highlight_path: Option<PathBuf>,
    ) {
        let Some(request_id) = crate::app::reducer::preview::begin_file_tree_request(
            &mut self.app_state,
            browser_generation,
            path.clone(),
            browser_mode,
            preserve_browser_mode,
        ) else {
            return;
        };
        let tx = self.app_command_tx.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let path_clone = path;
        let highlight_clone = highlight_path;
        self.background_tasks.spawn(async move {
            tokio::select! {
                result = build_fs_tree(&path_clone, 0) => {
                    let command = match result {
                        Ok(nodes) => AppCommand::UpdateFileBrowserData {
                            request_id,
                            path: path_clone,
                            data: nodes,
                            highlight_path: highlight_clone,
                        },
                        Err(error) => AppCommand::FileBrowserFetchFailed {
                            request_id,
                            message: format_filesystem_path_error(
                                "Failed to open file browser directory",
                                &path_clone,
                                &error,
                            ),
                            path: path_clone,
                        },
                    };
                    let _ = tx.send(command).await;
                }
                _ = shutdown_rx.recv() => {
                    tracing::debug!("Aborting FileBrowser crawl due to shutdown");
                }
            }
        });
    }

    pub(crate) fn sync_torrent_file_preview(&mut self) {
        let Some(crate::app::reducer::preview::TorrentPreviewRequest {
            browser_generation,
            request_id,
            path,
        }) = crate::app::reducer::preview::request_torrent_preview(&mut self.app_state)
        else {
            return;
        };

        let tx = self.app_command_tx.clone();
        self.background_tasks.spawn(async move {
            let load_path = path.clone();
            let result =
                tokio::task::spawn_blocking(move || load_torrent_file_preview(load_path.as_path()))
                    .await
                    .map_err(|error| format!("Torrent preview task failed: {error}"))
                    .and_then(|result| result);
            let _ = tx
                .send(AppCommand::UpdateTorrentFilePreview {
                    browser_generation,
                    request_id,
                    path,
                    result,
                })
                .await;
        });
    }

    pub(super) fn hydrate_pending_magnet_browser_from_display(&mut self, info_hash: &[u8]) {
        let Some(display) = self.app_state.torrents.get(info_hash) else {
            return;
        };
        if display.file_preview_tree.is_empty() {
            return;
        }

        let FileBrowserMode::DownloadLocSelection {
            target,
            preview_tree,
            preview_state,
            container_name,
            original_name_backup,
            use_container,
            ..
        } = &mut self.app_state.ui.file_browser.browser_mode
        else {
            return;
        };
        if !matches!(target, DownloadSelectionTarget::PendingAdd) || !preview_tree.is_empty() {
            return;
        }

        let info_hash_hex = hex::encode(info_hash);
        let name = format!("{} [{}]", display.latest_state.torrent_name, info_hash_hex);
        *container_name = name.clone();
        *original_name_backup = name;
        *use_container = display.latest_state.file_count.unwrap_or(1) > 1;
        *preview_tree = display.file_preview_tree.clone();
        if let Some(first) = preview_tree.first() {
            preview_state.cursor_path = Some(std::path::PathBuf::from(&first.name));
        }
        for node in preview_tree.iter_mut() {
            node.expand_all(preview_state);
        }
        self.app_state.ui.needs_redraw = true;
    }

    pub(crate) fn accepts_pasted_text(&self, pasted_text: &str) -> bool {
        crate::tui::runtime::native_pasted_text_supported(pasted_text)
    }

    pub fn find_most_common_download_path(&mut self) -> Option<PathBuf> {
        let mut counts: HashMap<PathBuf, usize> = HashMap::new();

        for state in self.app_state.torrents.values() {
            if let Some(download_path) = &state.latest_state.download_path {
                if let Some(parent_path) = download_path.parent() {
                    *counts.entry(parent_path.to_path_buf()).or_insert(0) += 1;
                }
            }
        }

        counts
            .into_iter()
            .max_by_key(|&(_, count)| count)
            .map(|(path, _)| path)
    }

    pub fn get_initial_source_path(&self) -> PathBuf {
        UserDirs::new()
            .and_then(|ud| ud.download_dir().map(|p| p.to_path_buf()))
            .or_else(|| UserDirs::new().map(|ud| ud.home_dir().to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    pub fn get_initial_destination_path(&mut self) -> PathBuf {
        self.client_configs
            .default_download_folder
            .clone()
            .or_else(|| self.find_most_common_download_path())
            .or_else(|| UserDirs::new().and_then(|ud| ud.download_dir().map(|p| p.to_path_buf())))
            .or_else(|| UserDirs::new().map(|ud| ud.home_dir().to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    pub(super) fn initial_download_selection_pane(&self) -> BrowserPane {
        if self.client_configs.default_download_folder.is_some() {
            BrowserPane::TorrentPreview
        } else {
            BrowserPane::FileSystem
        }
    }
}

pub(super) fn format_filesystem_path_error(action: &str, path: &Path, error: &io::Error) -> String {
    let detail = match error.kind() {
        ErrorKind::NotFound => "file or directory was not found".to_string(),
        ErrorKind::PermissionDenied => "permission denied".to_string(),
        ErrorKind::IsADirectory => {
            "expected a file here, but the path points to a directory".to_string()
        }
        ErrorKind::NotADirectory => {
            "expected a directory component in the path, but found a file".to_string()
        }
        _ if path.is_dir() => {
            "expected a file here, but the path points to a directory".to_string()
        }
        _ => error.to_string(),
    };

    format!("{} {:?}: {}", action, path, detail)
}

pub(super) fn load_torrent_file_preview(path: &Path) -> Result<TorrentFilePreview, String> {
    let file_bytes = fs::read(path).map_err(|error| {
        format_filesystem_path_error("Failed to read torrent preview", path, &error)
    })?;
    let torrent = from_bytes(&file_bytes)
        .map_err(|error| format!("Failed to parse torrent preview: {error}"))?;
    let protocol_version = match torrent.info.meta_version {
        Some(2) if !torrent.info.pieces.is_empty() => "BitTorrent v2 (Hybrid)",
        Some(2) => "BitTorrent v2 (Pure)",
        _ => "BitTorrent v1",
    }
    .to_string();

    Ok(TorrentFilePreview {
        name: torrent.info.name.clone(),
        protocol_version,
        total_size: torrent.info.total_length().max(0) as u64,
        tree: build_torrent_preview_tree(torrent.file_list(), &HashMap::new()),
    })
}
