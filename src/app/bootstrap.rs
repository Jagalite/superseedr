// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared app hydration. Hosts supply persisted facts and construct their own services.

use super::*;

pub(crate) fn initial_app_state(
    settings: &Settings,
    rss: RssPersistedState,
    journal: EventJournalState,
) -> AppState {
    let torrent_direction = if settings.torrent_sort_pinned {
        settings.torrent_sort_direction
    } else {
        settings.torrent_sort_column.default_direction()
    };
    let peer_direction = if settings.peer_sort_pinned {
        settings.peer_sort_direction
    } else {
        settings.peer_sort_column.default_direction()
    };
    AppState {
        ui: UiState {
            needs_redraw: true,
            visualization_focus: VisualizationFocusState::from_settings(settings),
            selected_header: if settings.torrent_sort_pinned {
                SelectedHeader::Torrent(torrent_sort_header(settings.torrent_sort_column))
            } else {
                SelectedHeader::default()
            },
            ..Default::default()
        },
        theme: Theme::builtin(settings.ui_theme),
        torrent_sort: (settings.torrent_sort_column, torrent_direction),
        peer_sort: (settings.peer_sort_column, peer_direction),
        torrent_sort_pinned: settings.torrent_sort_pinned,
        peer_sort_pinned: settings.peer_sort_pinned,
        data_rate: settings.ui_refresh_rate,
        rss_runtime: RssRuntimeState {
            history: rss.history,
            last_sync_at: rss.last_sync_at,
            feed_errors: rss.feed_errors,
            ..Default::default()
        },
        event_journal_state: journal,
        lifetime_downloaded_from_config: settings.lifetime_downloaded,
        lifetime_uploaded_from_config: settings.lifetime_uploaded,
        effective_download_limit_bps: settings.global_download_limit_bps,
        minute_disk_backoff_history_ms: VecDeque::with_capacity(24 * 60),
        ..Default::default()
    }
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppCapabilities {
    pub native_paths: bool,
    pub shared_cluster: bool,
    pub rss: bool,
    pub system_telemetry: bool,
    pub durable_catalog: bool,
    pub demo: bool,
}

impl AppCapabilities {
    pub(crate) fn native() -> Self {
        Self {
            native_paths: true,
            shared_cluster: true,
            rss: true,
            system_telemetry: true,
            durable_catalog: true,
            demo: false,
        }
    }
}
