// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Application projections of metadata already verified by the torrent runtime.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::app::{
    build_torrent_preview_tree, parse_hybrid_hashes, torrent_file_count, AppState,
    DownloadSelectionTarget, FileBrowserMode, FilePriority,
};
use crate::torrent_file::Torrent;

pub(super) fn reduce_metadata_loaded(
    state: &mut AppState,
    info_hash: &[u8],
    torrent: &Torrent,
) -> HashMap<usize, FilePriority> {
    let mut file_priorities = HashMap::new();
    if let Some(display) = state.torrents.get_mut(info_hash) {
        display.latest_state.is_multi_file = !torrent.info.files.is_empty();
        display.latest_state.file_count = Some(torrent_file_count(torrent));
        display.latest_state.total_size = torrent.info.total_length().max(0) as u64;
        file_priorities = display.latest_state.file_priorities.clone();
        display.file_preview_tree =
            build_torrent_preview_tree(torrent.file_list(), &file_priorities);
    }
    hydrate_active_preview(state, info_hash, torrent, &file_priorities);
    file_priorities
}

fn hydrate_active_preview(
    state: &mut AppState,
    info_hash: &[u8],
    torrent: &Torrent,
    file_priorities: &HashMap<usize, FilePriority>,
) {
    let FileBrowserMode::DownloadLocSelection {
        target,
        preview_tree,
        preview_state,
        container_name,
        original_name_backup,
        use_container,
        ..
    } = &mut state.ui.file_browser.browser_mode
    else {
        return;
    };

    let matches_target = match target {
        DownloadSelectionTarget::PendingAdd => {
            let (v1_hash, v2_hash) = parse_hybrid_hashes(&state.pending_torrent_link);
            v1_hash.as_deref() == Some(info_hash) || v2_hash.as_deref() == Some(info_hash)
        }
        DownloadSelectionTarget::ExistingTorrent { info_hash: target } => {
            target.as_slice() == info_hash
        }
    };
    // Late metadata must not replace another preview or reset the user's edits.
    if !matches_target || !preview_tree.is_empty() {
        return;
    }

    let file_list = torrent.file_list();
    let has_multiple_files = file_list.len() > 1;
    let priorities = match target {
        DownloadSelectionTarget::ExistingTorrent { .. } => file_priorities.clone(),
        DownloadSelectionTarget::PendingAdd => HashMap::new(),
    };
    *preview_tree = build_torrent_preview_tree(file_list, &priorities);
    let name = format!("{} [{}]", torrent.info.name, hex::encode(info_hash));
    *container_name = name.clone();
    *original_name_backup = name;
    *use_container = has_multiple_files;
    if let Some(first) = preview_tree.first() {
        preview_state.cursor_path = Some(PathBuf::from(&first.name));
    }
    for node in preview_tree.iter_mut() {
        node.expand_all(preview_state);
    }
    state.ui.needs_redraw = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::torrent_manager_protocol::ManagerEvent;
    use crate::app::{
        reduce_app_action, AppAction, AppEffect, BrowserPane, TorrentDisplayState, TorrentMetrics,
    };
    use crate::torrent_file::{Info, InfoFile};

    fn metadata() -> Torrent {
        Torrent {
            info: Info {
                name: "Fictional Orchard".into(),
                files: ["first.bin", "second.bin"]
                    .into_iter()
                    .map(|name| InfoFile {
                        length: 16,
                        path: vec![name.into()],
                        md5sum: None,
                        attr: None,
                    })
                    .collect(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn preview(target: DownloadSelectionTarget) -> FileBrowserMode {
        FileBrowserMode::DownloadLocSelection {
            target,
            torrent_files: Vec::new(),
            container_name: String::new(),
            use_container: false,
            is_editing_name: false,
            preview_tree: Vec::new(),
            preview_state: Default::default(),
            focused_pane: BrowserPane::TorrentPreview,
            cursor_pos: 0,
            original_name_backup: String::new(),
        }
    }

    fn deliver(state: &mut AppState, info_hash: Vec<u8>) -> Vec<AppEffect> {
        reduce_app_action(
            state,
            AppAction::ManagerEvent(ManagerEvent::MetadataLoaded {
                info_hash,
                torrent: Box::new(metadata()),
            }),
        )
    }

    #[test]
    fn existing_torrent_metadata_preserves_priorities_in_display_preview_and_effect() {
        let info_hash = vec![0x31; 20];
        let priorities = HashMap::from([(0, FilePriority::Skip), (1, FilePriority::High)]);
        let mut state = AppState::default();
        state.torrents.insert(
            info_hash.clone(),
            TorrentDisplayState {
                latest_state: TorrentMetrics {
                    info_hash: info_hash.clone(),
                    file_priorities: priorities.clone(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        state.ui.file_browser.browser_mode = preview(DownloadSelectionTarget::ExistingTorrent {
            info_hash: info_hash.clone(),
        });

        let effects = deliver(&mut state, info_hash.clone());

        let display = &state.torrents[&info_hash];
        assert_eq!(display.latest_state.file_count, Some(2));
        assert_eq!(display.latest_state.total_size, 32);
        assert_eq!(display.latest_state.file_priorities, priorities);
        assert!(display.latest_state.is_multi_file);
        assert!(matches!(
            effects.as_slice(),
            [AppEffect::MetadataLoaded { file_priorities, .. }] if file_priorities == &priorities
        ));
        let FileBrowserMode::DownloadLocSelection {
            preview_tree,
            preview_state,
            ..
        } = &state.ui.file_browser.browser_mode
        else {
            panic!("expected active file preview");
        };
        assert_eq!(preview_tree.len(), 2);
        for node in preview_tree {
            let file_index = node.payload.file_index.expect("leaf index");
            assert_eq!(node.payload.priority, priorities[&file_index]);
        }
        assert_eq!(preview_state.cursor_path, Some(PathBuf::from("first.bin")));
    }

    #[test]
    fn late_metadata_does_not_hydrate_an_unrelated_pending_add() {
        let mut state = AppState {
            pending_torrent_link: format!("magnet:?xt=urn:btih:{}", hex::encode([0x32; 20])),
            ..Default::default()
        };
        state.ui.file_browser.browser_mode = preview(DownloadSelectionTarget::PendingAdd);
        deliver(&mut state, vec![0x31; 20]);
        let FileBrowserMode::DownloadLocSelection {
            preview_tree,
            container_name,
            ..
        } = &state.ui.file_browser.browser_mode
        else {
            panic!("expected active file preview");
        };
        assert!(preview_tree.is_empty());
        assert!(container_name.is_empty());
        assert!(state.torrents.is_empty());
    }

    #[test]
    fn hybrid_magnet_matches_v2_runtime_identity_without_creating_a_runtime_torrent() {
        // The current runtime keys v2 torrents by the first 20 bytes of the SHA-256 hash.
        let info_hash = vec![0x41; 20];
        let mut state = AppState {
            pending_torrent_link: format!(
                "magnet:?xt=urn:btih:{}&xt=urn:btmh:1220{}",
                hex::encode([0x42; 20]),
                hex::encode([0x41; 32])
            ),
            ..Default::default()
        };
        state.ui.file_browser.browser_mode = preview(DownloadSelectionTarget::PendingAdd);
        deliver(&mut state, info_hash);
        let FileBrowserMode::DownloadLocSelection {
            preview_tree,
            use_container,
            ..
        } = &state.ui.file_browser.browser_mode
        else {
            panic!("expected active file preview");
        };
        assert_eq!(preview_tree.len(), 2);
        assert!(*use_container);
        assert!(state.torrents.is_empty());
    }

    #[test]
    fn repeated_metadata_preserves_active_preview_edits() {
        let info_hash = vec![0x31; 20];
        let mut state = AppState::default();
        state.ui.file_browser.browser_mode = preview(DownloadSelectionTarget::ExistingTorrent {
            info_hash: info_hash.clone(),
        });
        deliver(&mut state, info_hash.clone());
        if let FileBrowserMode::DownloadLocSelection {
            preview_tree,
            preview_state,
            container_name,
            ..
        } = &mut state.ui.file_browser.browser_mode
        {
            preview_tree[0].payload.priority = FilePriority::Skip;
            preview_state.cursor_path = Some(PathBuf::from("second.bin"));
            *container_name = "User-selected folder".into();
        }

        deliver(&mut state, info_hash);

        let FileBrowserMode::DownloadLocSelection {
            preview_tree,
            preview_state,
            container_name,
            ..
        } = &state.ui.file_browser.browser_mode
        else {
            panic!("expected active file preview");
        };
        assert_eq!(preview_tree[0].payload.priority, FilePriority::Skip);
        assert_eq!(preview_state.cursor_path, Some(PathBuf::from("second.bin")));
        assert_eq!(container_name, "User-selected folder");
    }
}
