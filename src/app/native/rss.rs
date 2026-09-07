// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native rss execution. Runtime resources remain owned by `App`.

use super::*;

impl App {
    pub(super) fn refresh_rss_derived(&mut self) {
        crate::tui::screens::rss::recompute_rss_derived(&mut self.app_state, &self.client_configs);
    }

    pub(super) async fn download_rss_preview_item(&mut self, item: RssPreviewItem) {
        let Some(link) = item.link.clone() else {
            tracing_event!(
                Level::INFO,
                "Skipping RSS manual download: item has no link"
            );
            return;
        };

        let (added, info_hash, command_path) = if link.starts_with("magnet:") {
            let command_path = rss_ingest::write_magnet(&self.client_configs, link.as_str())
                .await
                .ok();
            let (v1_hash, v2_hash) = parse_hybrid_hashes(link.as_str());
            (command_path.is_some(), v1_hash.or(v2_hash), command_path)
        } else if link.starts_with("http://") || link.starts_with("https://") {
            self.download_rss_torrent_from_url(link.as_str()).await
        } else {
            tracing_event!(
                Level::INFO,
                "Skipping RSS manual download: unsupported link scheme '{}'",
                link
            );
            (false, None, None)
        };

        if !added {
            return;
        }

        if let Some(command_path) = command_path.clone() {
            let ingest_kind = ingest_kind_from_path(&command_path).unwrap_or_default();
            self.record_rss_queued(command_path, IngestOrigin::RssManual, ingest_kind);
        }

        for preview in &mut self.app_state.rss_runtime.preview_items {
            if preview.dedupe_key == item.dedupe_key {
                preview.is_downloaded = true;
            }
        }

        let entry = RssHistoryEntry {
            dedupe_key: item.dedupe_key.clone(),
            info_hash: info_hash.map(hex::encode),
            guid: item.guid.clone(),
            link: item.link.clone(),
            title: item.title.clone(),
            source: item.source.clone(),
            date_iso: item
                .date_iso
                .clone()
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
            added_via: crate::config::RssAddedVia::Manual,
        };
        let existing_idx = self
            .app_state
            .rss_runtime
            .history
            .iter()
            .position(|existing| existing.dedupe_key == entry.dedupe_key);
        if let Some(idx) = existing_idx {
            if self.app_state.rss_runtime.history[idx].info_hash.is_none()
                && entry.info_hash.is_some()
            {
                self.app_state.rss_runtime.history[idx].info_hash = entry.info_hash.clone();
                self.save_state_to_disk();
            }
        } else {
            self.app_state.rss_runtime.history.push(entry);
            self.save_state_to_disk();
        }

        if let Some(history_entry) = self
            .app_state
            .rss_runtime
            .history
            .iter()
            .find(|h| h.dedupe_key == item.dedupe_key)
            .cloned()
        {
            let _ = self.rss_downloaded_entry_tx.try_send(history_entry);
        }

        self.refresh_rss_derived();
    }

    pub(super) async fn download_rss_torrent_from_url(
        &mut self,
        url: &str,
    ) -> (bool, Option<Vec<u8>>, Option<PathBuf>) {
        let active_network = match self.network_activation.try_active() {
            Ok(active) => active,
            Err(error) => {
                tracing_event!(Level::WARN, %error, "RSS manual download deferred while networking is unavailable");
                return (false, None, None);
            }
        };
        let network_scope = active_network.scope().clone();

        let client = match network_scope.lease().rss_http_client() {
            Ok(client) => client,
            Err(error) => {
                tracing_event!(Level::WARN, %error, "RSS manual download deferred because its HTTP client is unavailable");
                return (false, None, None);
            }
        };

        let request = match client.get(url) {
            Ok(request) => request,
            Err(error) => {
                tracing_event!(Level::WARN, %error, "RSS manual download blocked by network policy");
                return (false, None, None);
            }
        };
        let response = match network_scope.run(request.send()).await {
            Ok(Ok(resp)) => resp,
            Err(error) => {
                tracing_event!(Level::WARN, %error, "RSS manual download canceled after network invalidation");
                return (false, None, None);
            }
            Ok(Err(e)) => {
                tracing_event!(
                    Level::ERROR,
                    "RSS manual download request failed for {}: {}",
                    url,
                    e
                );
                return (false, None, None);
            }
        };
        if !response.status().is_success() {
            tracing_event!(
                Level::ERROR,
                "RSS manual download HTTP status {} for {}",
                response.status(),
                url
            );
            return (false, None, None);
        }

        let bytes = match network_scope.run(response.bytes()).await {
            Ok(Ok(b)) => b,
            Err(error) => {
                tracing_event!(Level::WARN, %error, "RSS manual download body canceled after network invalidation");
                return (false, None, None);
            }
            Ok(Err(e)) => {
                tracing_event!(
                    Level::ERROR,
                    "RSS manual download body read failed for {}: {}",
                    url,
                    e
                );
                return (false, None, None);
            }
        };
        if bytes.len() > RSS_MAX_TORRENT_DOWNLOAD_BYTES {
            tracing_event!(
                Level::ERROR,
                "RSS manual download exceeded max size for {} ({} bytes)",
                url,
                bytes.len()
            );
            return (false, None, None);
        }
        let Some(info_hash) = info_hash_from_torrent_bytes(bytes.as_ref()) else {
            tracing_event!(
                Level::ERROR,
                "RSS manual download produced invalid torrent payload for {}",
                url
            );
            return (false, None, None);
        };

        match rss_ingest::write_torrent_bytes(&self.client_configs, url, bytes.as_ref()).await {
            Ok(path) => (true, Some(info_hash), Some(path)),
            Err(e) => {
                tracing_event!(
                    Level::ERROR,
                    "RSS manual download failed to queue torrent file for {}: {}",
                    url,
                    e
                );
                (false, None, None)
            }
        }
    }
}

pub(super) fn rss_settings_changed(old_settings: &Settings, new_settings: &Settings) -> bool {
    new_settings.rss != old_settings.rss
}

pub(super) fn prune_rss_feed_errors(
    feed_errors: &mut HashMap<String, FeedSyncError>,
    settings: &Settings,
) -> bool {
    let configured_feed_urls: std::collections::HashSet<&str> = settings
        .rss
        .feeds
        .iter()
        .map(|feed| feed.url.as_str())
        .collect();
    let before = feed_errors.len();
    feed_errors.retain(|feed_url, _| configured_feed_urls.contains(feed_url.as_str()));
    feed_errors.len() != before
}
