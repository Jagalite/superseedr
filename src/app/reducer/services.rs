// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared interpretation of service and restore results.

use super::{AppEffect, AppState};
use crate::app::{
    ActivityHistoryPersistedState, FeedSyncError, NetworkHistoryPersistedState, RssPreviewItem,
};

pub(crate) enum ServiceObservation {
    RssPreview(Vec<RssPreviewItem>),
    RssSync {
        last_sync_at: Option<String>,
        next_sync_at: Option<String>,
    },
    RssFeedError {
        feed_url: String,
        error: Option<FeedSyncError>,
    },
    NetworkHistoryLoaded(NetworkHistoryPersistedState),
    ActivityHistoryLoaded(Box<ActivityHistoryPersistedState>),
}

pub(crate) fn reduce_service_observation(
    state: &mut AppState,
    observation: ServiceObservation,
) -> Vec<AppEffect> {
    state.ui.needs_redraw = true;
    match observation {
        ServiceObservation::RssPreview(items) => {
            state.rss_runtime.preview_items = items;
            vec![AppEffect::RefreshRss]
        }
        ServiceObservation::RssSync {
            last_sync_at,
            next_sync_at,
        } => {
            state.rss_runtime.last_sync_at = last_sync_at;
            state.rss_runtime.next_sync_at = next_sync_at;
            vec![AppEffect::RefreshRss, AppEffect::CheckpointRequested]
        }
        ServiceObservation::RssFeedError { feed_url, error } => {
            if let Some(error) = error {
                state.rss_runtime.feed_errors.insert(feed_url, error);
            } else {
                state.rss_runtime.feed_errors.remove(&feed_url);
            }
            vec![AppEffect::CheckpointRequested]
        }
        ServiceObservation::NetworkHistoryLoaded(loaded) => {
            crate::telemetry::network_history_telemetry::NetworkHistoryTelemetry::apply_loaded_state(state, loaded);
            state.network_history_restore_pending = false;
            Vec::new()
        }
        ServiceObservation::ActivityHistoryLoaded(loaded) => {
            crate::telemetry::activity_history_telemetry::ActivityHistoryTelemetry::apply_loaded_state(state, *loaded);
            state.activity_history_restore_pending = false;
            Vec::new()
        }
    }
}
