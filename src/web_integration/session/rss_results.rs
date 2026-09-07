// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser host rss; shared app transitions retain application policy.

use super::*;

impl BrowserSession {
    pub fn apply_browser_rss_sync(&mut self, last_sync_at: String, next_sync_at: String) {
        self.observe_service(crate::app::reducer::ServiceObservation::RssSync {
            last_sync_at: Some(last_sync_at),
            next_sync_at: Some(next_sync_at),
        });
    }

    pub fn apply_browser_rss_download(&mut self, item: &BrowserRssPreview, info_hash: &[u8]) {
        for preview in &mut self.app_state.rss_runtime.preview_items {
            if preview.dedupe_key == item.dedupe_key {
                preview.is_downloaded = true;
            }
        }
        let entry = RssHistoryEntry {
            dedupe_key: item.dedupe_key.clone(),
            info_hash: Some(hex::encode(info_hash)),
            guid: item.guid.clone(),
            link: item.link.clone(),
            title: item.title.clone(),
            source: item.source.clone(),
            date_iso: item.date_iso.clone().unwrap_or_default(),
            added_via: RssAddedVia::Manual,
        };
        if let Some(existing) = self
            .app_state
            .rss_runtime
            .history
            .iter_mut()
            .find(|existing| existing.dedupe_key == entry.dedupe_key)
        {
            *existing = entry;
        } else {
            self.app_state.rss_runtime.history.push(entry);
        }
        rss::recompute_rss_derived(&mut self.app_state, &self.client_configs);
        self.app_state.ui.needs_redraw = true;
    }
}
