// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Settings interpretation and catalog effects, independent of host resources.

use super::*;

#[derive(Default)]
pub struct SettingsApplication {
    pub revision: u64,
    /// The last requested configuration. Effective configuration remains `client_configs`.
    pub requested: Option<Box<Settings>>,
    pub pending: bool,
    pub last_error: Option<String>,
}

pub(crate) fn begin_settings_application(state: &mut AppState, requested: &Settings) -> u64 {
    let operation = &mut state.settings_application;
    operation.revision = operation
        .revision
        .checked_add(1)
        .expect("settings revision exhausted");
    operation.requested = Some(Box::new(requested.clone()));
    operation.pending = true;
    operation.revision
}

pub(crate) fn finish_settings_application(
    state: &mut AppState,
    revision: u64,
    error: Option<String>,
) {
    let operation = &mut state.settings_application;
    if revision != operation.revision || !operation.pending {
        return;
    }
    let previous_error = operation.last_error.clone();
    operation.pending = false;
    operation.last_error = error.clone();
    if error.is_some() || state.system_error == previous_error {
        state.system_error = error;
    }
    state.ui.needs_redraw = true;
}

pub(crate) fn apply_settings_projection(
    state: &mut AppState,
    old: &Settings,
    effective: &Settings,
) {
    if old.ui_theme != effective.ui_theme {
        state.theme = Theme::builtin(effective.ui_theme);
    }
    state.data_rate = effective.ui_refresh_rate;
    if old.global_download_limit_bps != effective.global_download_limit_bps {
        state.effective_download_limit_bps = effective.global_download_limit_bps;
    }
    state.ui.needs_redraw = true;
}

pub(crate) fn apply_torrent_configuration(
    state: &mut AppState,
    info_hash: &[u8],
    download_path: Option<PathBuf>,
    container_name: Option<String>,
    file_priorities: HashMap<usize, FilePriority>,
) -> bool {
    let Some(runtime) = state.torrents.get_mut(info_hash) else {
        return false;
    };
    runtime.latest_state.download_path = download_path;
    runtime.latest_state.container_name = container_name;
    runtime.latest_state.file_priorities = file_priorities;
    if !runtime.file_preview_tree.is_empty() {
        runtime.file_preview_tree = rebuild_torrent_preview_tree(
            &runtime.file_preview_tree,
            &runtime.latest_state.file_priorities,
        );
    }
    state.ui.needs_redraw = true;
    true
}

pub(crate) enum CatalogEffect {
    Configure {
        info_hash: Vec<u8>,
        commands: Vec<ManagerCommand>,
    },
    ReaderOnly(TorrentSettings),
    Stop(Vec<u8>),
    Restore(TorrentSettings),
}

/// Plans application runtime membership. TorrentState retains logical torrent/peer authority.
pub(crate) fn reconcile_catalog(
    state: &mut AppState,
    old: &Settings,
    new: &Settings,
    is_shared_follower: bool,
) -> Vec<CatalogEffect> {
    if !runtime_torrent_settings_changed(old, new) {
        return Vec::new();
    }
    let by_hash = |settings: &Settings| {
        settings
            .torrents
            .iter()
            .filter_map(|torrent| {
                info_hash_from_torrent_source(&torrent.torrent_or_magnet)
                    .map(|hash| (hash, torrent.clone()))
            })
            .collect::<HashMap<_, _>>()
    };
    let old_by_hash = by_hash(old);
    let new_by_hash = by_hash(new);
    let mut effects = Vec::new();
    for (info_hash, torrent) in &new_by_hash {
        apply_torrent_configuration(
            state,
            info_hash,
            torrent
                .download_path
                .clone()
                .or_else(|| new.default_download_folder.clone()),
            torrent.container_name.clone(),
            torrent.file_priorities.clone(),
        );
        if let Some(runtime) = state.torrents.get_mut(info_hash) {
            runtime.latest_state.torrent_name = torrent.name.clone();
            runtime.added_at_unix_secs = torrent.added_at_unix_secs;
            runtime.latest_state.torrent_control_state = torrent.torrent_control_state.clone();
            runtime.latest_state.delete_files = torrent.delete_files;
            runtime.latest_state.download_mode = torrent.download_mode;
        }
        if is_shared_follower && !torrent.validation_status {
            effects.push(CatalogEffect::ReaderOnly(torrent.clone()));
            continue;
        }
        let Some(previous) = old_by_hash.get(info_hash) else {
            continue;
        };
        let mut commands = Vec::new();
        if previous.download_mode != torrent.download_mode {
            commands.push(ManagerCommand::SetDownloadMode(torrent.download_mode));
        }
        if previous.torrent_control_state != torrent.torrent_control_state {
            commands.push(match torrent.torrent_control_state {
                TorrentControlState::Paused => ManagerCommand::Pause,
                TorrentControlState::Running => ManagerCommand::Resume,
                TorrentControlState::Deleting if torrent.delete_files => ManagerCommand::DeleteFile,
                TorrentControlState::Deleting => ManagerCommand::Shutdown,
            });
        }
        if old.default_download_folder != new.default_download_folder
            || previous.download_path != torrent.download_path
            || previous.container_name != torrent.container_name
            || previous.file_priorities != torrent.file_priorities
        {
            if let Some(torrent_data_path) = torrent
                .download_path
                .clone()
                .or_else(|| new.default_download_folder.clone())
            {
                commands.push(ManagerCommand::SetUserTorrentConfig {
                    torrent_data_path,
                    file_priorities: torrent.file_priorities.clone(),
                    container_name: torrent.container_name.clone(),
                });
            }
        }
        effects.push(CatalogEffect::Configure {
            info_hash: info_hash.clone(),
            commands,
        });
    }
    effects.extend(
        old_by_hash
            .keys()
            .filter(|hash| !new_by_hash.contains_key(*hash))
            .cloned()
            .map(CatalogEffect::Stop),
    );
    effects.extend(
        new_by_hash
            .into_iter()
            .filter(|(hash, _)| !old_by_hash.contains_key(hash))
            .map(|(_, torrent)| CatalogEffect::Restore(torrent)),
    );
    effects
}

pub(super) fn runtime_torrent_settings_changed(
    old_settings: &Settings,
    new_settings: &Settings,
) -> bool {
    old_settings.torrents != new_settings.torrents
        || old_settings.default_download_folder != new_settings.default_download_folder
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequential_catalog_change_routes_mode_before_resume() {
        let entry = TorrentSettings {
            torrent_or_magnet: "magnet:?xt=urn:btih:5555555555555555555555555555555555555555"
                .into(),
            torrent_control_state: TorrentControlState::Paused,
            ..Default::default()
        };
        let old = Settings {
            torrents: vec![entry],
            ..Default::default()
        };
        let mut new = old.clone();
        new.torrents[0].download_mode = crate::config::DownloadMode::Sequential;
        new.torrents[0].torrent_control_state = TorrentControlState::Running;
        let mut state = AppState::default();
        let effects = reconcile_catalog(&mut state, &old, &new, false);
        assert_eq!(effects.len(), 1);
        let CatalogEffect::Configure { commands, .. } = &effects[0] else {
            panic!("expected manager configuration")
        };
        assert_eq!(
            commands,
            &vec![
                ManagerCommand::SetDownloadMode(crate::config::DownloadMode::Sequential),
                ManagerCommand::Resume
            ]
        );
        assert!(reconcile_catalog(&mut state, &new, &new, false).is_empty());
    }

    #[test]
    fn superseded_settings_outcome_does_not_finish_the_new_request() {
        let mut state = AppState::default();
        let mut settings = Settings::default();
        let first = begin_settings_application(&mut state, &settings);
        settings.client_port = 42123;
        let second = begin_settings_application(&mut state, &settings);
        finish_settings_application(&mut state, first, Some("old rebind failed".into()));
        assert!(state.settings_application.pending);
        assert_eq!(
            state
                .settings_application
                .requested
                .as_ref()
                .unwrap()
                .client_port,
            42123
        );
        finish_settings_application(&mut state, second, None);
        assert!(!state.settings_application.pending);
        assert_eq!(state.system_error, None);
    }
    #[test]
    fn settings_success_does_not_hide_an_unrelated_checkpoint_failure() {
        let mut state = AppState::default();
        let revision = begin_settings_application(&mut state, &Settings::default());
        state.system_error = Some("checkpoint storage unavailable".into());
        finish_settings_application(&mut state, revision, None);
        assert_eq!(
            state.system_error.as_deref(),
            Some("checkpoint storage unavailable")
        );
    }
}
