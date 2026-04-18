// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{AppCommand, RssPreviewItem};
use crate::config::{RssAddedVia, RssFilterMode, RssHistoryEntry, Settings};
use crate::integrations::rss_ingest;
use crate::integrations::rss_url_safety::is_safe_rss_item_url;
use chrono::{Duration as ChronoDuration, Utc};
use feed_rs::parser;
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use reqwest::Client;
use sha1::{Digest, Sha1};
use std::collections::HashSet;
use tokio::sync::{broadcast, mpsc};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{self, Duration};

const MIN_POLL_INTERVAL_SECS: u64 = 30;
const REQUEST_TIMEOUT_SECS: u64 = 20;
const FEED_FETCH_MAX_ATTEMPTS: u32 = 3;
const FEED_RETRY_BASE_DELAY_MS: u64 = 400;
const FEED_RETRY_MAX_JITTER_MS: u64 = 250;

#[derive(Clone)]
struct CandidateItem {
    dedupe_key: String,
    title: String,
    link: Option<String>,
    guid: Option<String>,
    source: Option<String>,
    date_iso: Option<String>,
    sort_ts: i64,
}

pub fn spawn_rss_service(
    settings: Settings,
    initial_history: Vec<RssHistoryEntry>,
    app_command_tx: mpsc::Sender<AppCommand>,
    mut sync_now_rx: mpsc::Receiver<()>,
    mut downloaded_entry_rx: mpsc::Receiver<RssHistoryEntry>,
    mut settings_rx: tokio::sync::watch::Receiver<Settings>,
    shutdown_tx: broadcast::Sender<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut shutdown_rx = shutdown_tx.subscribe();
        let mut current_settings = settings;
        let mut poll_secs = current_settings
            .rss
            .poll_interval_secs
            .max(MIN_POLL_INTERVAL_SECS);
        let mut ticker = time::interval(Duration::from_secs(poll_secs));
        ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

        let mut downloaded_keys: HashSet<String> = initial_history
            .iter()
            .flat_map(|h| {
                identity_keys_for(
                    h.guid.as_deref(),
                    h.link.as_deref(),
                    h.title.as_str(),
                    h.source.as_deref(),
                    h.dedupe_key.as_str(),
                )
            })
            .collect();

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    break;
                }
                changed = settings_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    current_settings = settings_rx.borrow().clone();
                    poll_secs = current_settings
                        .rss
                        .poll_interval_secs
                        .max(MIN_POLL_INTERVAL_SECS);
                    ticker = time::interval(Duration::from_secs(poll_secs));
                    ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
                }
                maybe_entry = downloaded_entry_rx.recv() => {
                    if let Some(entry) = maybe_entry {
                        for key in identity_keys_for(
                            entry.guid.as_deref(),
                            entry.link.as_deref(),
                            entry.title.as_str(),
                            entry.source.as_deref(),
                            entry.dedupe_key.as_str(),
                        ) {
                            downloaded_keys.insert(key);
                        }
                    }
                }
                maybe_sync = sync_now_rx.recv() => {
                    if maybe_sync.is_none() {
                        break;
                    }
                    if !current_settings.rss.enabled {
                        continue;
                    }
                    if !run_sync_until_shutdown(
                        &current_settings,
                        &app_command_tx,
                        &mut downloaded_keys,
                        &mut shutdown_rx,
                    )
                    .await
                    {
                        break;
                    }
                    let now = Utc::now();
                    let next = now + ChronoDuration::seconds(poll_secs as i64);
                    let _ = app_command_tx.send(AppCommand::RssSyncStatusUpdated {
                        last_sync_at: Some(now.to_rfc3339()),
                        next_sync_at: Some(next.to_rfc3339()),
                    }).await;
                }
                _ = ticker.tick() => {
                    if !current_settings.rss.enabled {
                        continue;
                    }
                    if !run_sync_until_shutdown(
                        &current_settings,
                        &app_command_tx,
                        &mut downloaded_keys,
                        &mut shutdown_rx,
                    )
                    .await
                    {
                        break;
                    }
                    let now = Utc::now();
                    let next = now + ChronoDuration::seconds(poll_secs as i64);
                    let _ = app_command_tx.send(AppCommand::RssSyncStatusUpdated {
                        last_sync_at: Some(now.to_rfc3339()),
                        next_sync_at: Some(next.to_rfc3339()),
                    }).await;
                }
            }
        }
    })
}

async fn run_sync_until_shutdown(
    settings: &Settings,
    app_command_tx: &mpsc::Sender<AppCommand>,
    downloaded_keys: &mut HashSet<String>,
    shutdown_rx: &mut broadcast::Receiver<()>,
) -> bool {
    tokio::select! {
        _ = run_sync(settings, app_command_tx, downloaded_keys) => true,
        _ = shutdown_rx.recv() => false,
    }
}

async fn run_sync(
    settings: &Settings,
    app_command_tx: &mpsc::Sender<AppCommand>,
    downloaded_keys: &mut HashSet<String>,
) {
    let enabled_feed_urls: Vec<String> = settings
        .rss
        .feeds
        .iter()
        .filter(|f| f.enabled)
        .map(|f| f.url.clone())
        .collect();
    if enabled_feed_urls.is_empty() {
        let _ = app_command_tx
            .send(AppCommand::RssPreviewUpdated(Vec::new()))
            .await;
        return;
    }
    let client = match std::panic::catch_unwind(|| {
        Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
    }) {
        Ok(Ok(client)) => client,
        Ok(Err(e)) => {
            tracing::error!("RSS sync skipped: HTTP client build error: {}", e);
            return;
        }
        Err(_) => {
            tracing::error!("RSS sync skipped: HTTP client build panicked");
            return;
        }
    };

    let matcher = SkimMatcherV2::default();
    let enabled_filters = enabled_filters(settings);

    let mut aggregated = Vec::new();

    const FEED_FETCH_CONCURRENCY: usize = 6;
    let mut pending = enabled_feed_urls.into_iter();
    let mut fetches = JoinSet::new();

    for _ in 0..FEED_FETCH_CONCURRENCY {
        let Some(feed_url) = pending.next() else {
            break;
        };
        let client_cloned = client.clone();
        fetches.spawn(async move {
            let result =
                fetch_and_parse_feed_with_retry(&client_cloned, &feed_url, FEED_FETCH_MAX_ATTEMPTS)
                    .await;
            (feed_url, result)
        });
    }

    while let Some(task_result) = fetches.join_next().await {
        match task_result {
            Ok((feed_url, Ok(mut items))) => {
                let _ = app_command_tx
                    .send(AppCommand::RssFeedErrorUpdated {
                        feed_url,
                        error: None,
                    })
                    .await;
                aggregated.append(&mut items);
            }
            Ok((feed_url, Err(e))) => {
                let _ = app_command_tx
                    .send(AppCommand::RssFeedErrorUpdated {
                        feed_url,
                        error: Some(crate::config::FeedSyncError {
                            message: e,
                            occurred_at_iso: Utc::now().to_rfc3339(),
                        }),
                    })
                    .await;
            }
            Err(e) => {
                tracing::error!("RSS feed fetch task join error: {}", e);
            }
        }

        if let Some(feed_url) = pending.next() {
            let client_cloned = client.clone();
            fetches.spawn(async move {
                let result = fetch_and_parse_feed_with_retry(
                    &client_cloned,
                    &feed_url,
                    FEED_FETCH_MAX_ATTEMPTS,
                )
                .await;
                (feed_url, result)
            });
        }
    }

    aggregated.sort_by_key(|item| std::cmp::Reverse(item.sort_ts));

    let mut title_seen = HashSet::new();
    let mut preview_items = Vec::new();

    for item in aggregated {
        if preview_items.len() >= settings.rss.max_preview_items {
            break;
        }

        let title_key = normalize_title(&item.title);
        if !title_seen.insert(title_key) {
            continue;
        }

        let identity_keys = identity_keys_for(
            item.guid.as_deref(),
            item.link.as_deref(),
            item.title.as_str(),
            item.source.as_deref(),
            item.dedupe_key.as_str(),
        );
        let is_match = title_matches_filters(item.title.as_str(), &enabled_filters, &matcher);
        let mut is_downloaded = identity_keys.iter().any(|k| downloaded_keys.contains(k));

        if is_match && !is_downloaded {
            let (added, info_hash, command_path) = auto_ingest_item(settings, &client, &item).await;
            if added {
                is_downloaded = true;
                for key in &identity_keys {
                    downloaded_keys.insert(key.clone());
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
                        .unwrap_or_else(|| Utc::now().to_rfc3339()),
                    added_via: RssAddedVia::Auto,
                };

                let _ = app_command_tx
                    .send(AppCommand::RssDownloadSelected {
                        entry,
                        command_path,
                    })
                    .await;
            }
        }

        preview_items.push(RssPreviewItem {
            dedupe_key: item.dedupe_key,
            title: item.title,
            link: item.link,
            guid: item.guid,
            source: item.source,
            date_iso: item.date_iso,
            is_match,
            is_downloaded,
        });
    }

    let _ = app_command_tx
        .send(AppCommand::RssPreviewUpdated(preview_items))
        .await;
}

fn enabled_filters(settings: &Settings) -> Vec<(String, RssFilterMode)> {
    settings
        .rss
        .filters
        .iter()
        .filter(|f| f.enabled)
        .map(|f| (f.query.trim().to_string(), f.mode))
        .filter(|(q, _)| !q.is_empty())
        .collect()
}

fn title_matches_filters(
    title: &str,
    filters: &[(String, RssFilterMode)],
    matcher: &SkimMatcherV2,
) -> bool {
    if filters.is_empty() {
        return false;
    }
    let title_lc = title.to_lowercase();
    filters.iter().any(|(filter, mode)| match mode {
        RssFilterMode::Fuzzy => matcher
            .fuzzy_match(&title_lc, &filter.to_lowercase())
            .is_some(),
        RssFilterMode::Regex => regex::RegexBuilder::new(filter)
            .case_insensitive(true)
            .build()
            .map(|re| re.is_match(title))
            .unwrap_or(false),
    })
}

async fn fetch_and_parse_feed(
    client: &Client,
    feed_url: &str,
) -> Result<Vec<CandidateItem>, String> {
    let response = client
        .get(feed_url)
        .send()
        .await
        .map_err(|e| format!("feed request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("feed HTTP status {}", response.status()));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("feed body read failed: {e}"))?;

    let feed = parser::parse(bytes.as_ref()).map_err(|e| format!("feed parse failed: {e}"))?;
    let source_name = feed
        .title
        .as_ref()
        .map(|t| t.content.clone())
        .filter(|s| !s.trim().is_empty());

    let mut out = Vec::new();
    for entry in feed.entries {
        let title = entry
            .title
            .as_ref()
            .map(|t| t.content.clone())
            .unwrap_or_else(|| "Untitled".to_string());

        let link = entry.links.iter().find_map(|l| {
            if l.href.trim().is_empty() {
                None
            } else {
                Some(l.href.clone())
            }
        });

        let guid = if entry.id.trim().is_empty() {
            None
        } else {
            Some(entry.id.clone())
        };

        let published = entry
            .published
            .or(entry.updated)
            .map(|dt| dt.with_timezone(&Utc));

        let dedupe_key = dedupe_key_for(
            guid.as_deref(),
            link.as_deref(),
            title.as_str(),
            source_name.as_deref(),
        );

        out.push(CandidateItem {
            dedupe_key,
            title,
            link,
            guid,
            source: source_name.clone(),
            date_iso: published.map(|dt| dt.to_rfc3339()),
            sort_ts: published.map(|dt| dt.timestamp()).unwrap_or(0),
        });
    }

    Ok(out)
}

fn retry_delay_ms(feed_url: &str, attempt_index: u32) -> u64 {
    let digest = Sha1::digest(format!("{feed_url}:{attempt_index}").as_bytes());
    let jitter =
        (u16::from_le_bytes([digest[0], digest[1]]) as u64) % (FEED_RETRY_MAX_JITTER_MS + 1);
    let exponential = FEED_RETRY_BASE_DELAY_MS * (1u64 << attempt_index.min(4));
    exponential + jitter
}

async fn fetch_and_parse_feed_with_retry(
    client: &Client,
    feed_url: &str,
    max_attempts: u32,
) -> Result<Vec<CandidateItem>, String> {
    let attempts = max_attempts.max(1);
    let mut last_error: Option<String> = None;

    for attempt in 1..=attempts {
        match fetch_and_parse_feed(client, feed_url).await {
            Ok(items) => return Ok(items),
            Err(err) => {
                last_error = Some(err);
                if attempt < attempts {
                    let delay_ms = retry_delay_ms(feed_url, attempt - 1);
                    time::sleep(Duration::from_millis(delay_ms)).await;
                }
            }
        }
    }

    Err(format!(
        "feed sync failed after {} attempts: {}",
        attempts,
        last_error.unwrap_or_else(|| "unknown error".to_string())
    ))
}

fn dedupe_key_for(
    guid: Option<&str>,
    link: Option<&str>,
    title: &str,
    source: Option<&str>,
) -> String {
    if let Some(g) = guid.filter(|v| !v.trim().is_empty()) {
        return format!("guid:{}", g.trim());
    }
    if let Some(l) = link.filter(|v| !v.trim().is_empty()) {
        return format!("link:{}", l.trim());
    }

    let normalized_title = normalize_title(title);
    let normalized_source = normalize_title(source.unwrap_or(""));
    format!("title_source:{}::{}", normalized_title, normalized_source)
}

fn identity_keys_for(
    guid: Option<&str>,
    link: Option<&str>,
    title: &str,
    source: Option<&str>,
    primary_key: &str,
) -> Vec<String> {
    let mut keys = HashSet::new();
    let primary = primary_key.trim();
    if !primary.is_empty() {
        keys.insert(primary.to_string());
    }
    if let Some(g) = guid.filter(|v| !v.trim().is_empty()) {
        keys.insert(format!("guid:{}", g.trim()));
    }
    if let Some(l) = link.filter(|v| !v.trim().is_empty()) {
        keys.insert(format!("link:{}", l.trim()));
    }
    let normalized_title = normalize_title(title);
    let normalized_source = normalize_title(source.unwrap_or(""));
    keys.insert(format!(
        "title_source:{}::{}",
        normalized_title, normalized_source
    ));
    keys.into_iter().collect()
}

fn normalize_title(input: &str) -> String {
    input
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

async fn auto_ingest_item(
    settings: &Settings,
    client: &Client,
    item: &CandidateItem,
) -> (bool, Option<Vec<u8>>, Option<std::path::PathBuf>) {
    let Some(link) = &item.link else {
        return (false, None, None);
    };

    if link.starts_with("magnet:") {
        let command_path = rss_ingest::write_magnet(settings, link.as_str()).await.ok();
        let (v1_hash, v2_hash) = crate::app::parse_hybrid_hashes(link.as_str());
        return (command_path.is_some(), v1_hash.or(v2_hash), command_path);
    }

    if !(link.starts_with("http://") || link.starts_with("https://")) {
        return (false, None, None);
    }

    match fetch_torrent_bytes(client, link).await {
        Ok(bytes) => {
            let Some(info_hash) = crate::app::info_hash_from_torrent_bytes(&bytes) else {
                return (false, None, None);
            };
            let command_path = rss_ingest::write_torrent_bytes(settings, link.as_str(), &bytes)
                .await
                .ok();
            (command_path.is_some(), Some(info_hash), command_path)
        }
        Err(_) => (false, None, None),
    }
}

async fn fetch_torrent_bytes(client: &Client, url: &str) -> Result<Vec<u8>, String> {
    if !is_safe_rss_item_url(url).await {
        return Err("torrent URL blocked by RSS network safety policy".to_string());
    }

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("torrent request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("torrent HTTP status {}", response.status()));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("torrent body read failed: {e}"))?;

    if bytes.len() > crate::app::RSS_MAX_TORRENT_DOWNLOAD_BYTES {
        return Err("torrent payload exceeds max allowed size".to_string());
    }

    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    struct LocalWatchPathTestGuard {
        _env_guard: std::sync::MutexGuard<'static, ()>,
        _local_paths: tempfile::TempDir,
        original_shared_dir: Option<std::ffi::OsString>,
        original_shared_host_id: Option<std::ffi::OsString>,
    }

    impl LocalWatchPathTestGuard {
        fn new() -> Self {
            let env_guard = crate::config::shared_env_guard_for_tests()
                .lock()
                .expect("shared env guard lock poisoned");

            let local_paths = tempfile::tempdir().expect("create local app paths");
            let config_dir = local_paths.path().join("config");
            let data_dir = local_paths.path().join("data");
            crate::config::set_app_paths_override_for_tests(Some((config_dir, data_dir)));

            let original_shared_dir = env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
            let original_shared_host_id = env::var_os("SUPERSEEDR_SHARED_HOST_ID");
            env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
            env::remove_var("SUPERSEEDR_SHARED_HOST_ID");
            crate::config::clear_shared_config_state_for_tests();

            Self {
                _env_guard: env_guard,
                _local_paths: local_paths,
                original_shared_dir,
                original_shared_host_id,
            }
        }
    }

    impl Drop for LocalWatchPathTestGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.original_shared_dir {
                env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", value);
            } else {
                env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
            }

            if let Some(value) = &self.original_shared_host_id {
                env::set_var("SUPERSEEDR_SHARED_HOST_ID", value);
            } else {
                env::remove_var("SUPERSEEDR_SHARED_HOST_ID");
            }

            crate::config::set_app_paths_override_for_tests(None);
            crate::config::clear_shared_config_state_for_tests();
        }
    }

    #[test]
    fn dedupe_key_prefers_guid_then_link_then_title_source() {
        let a = dedupe_key_for(Some("guid-1"), Some("https://x"), "Title", Some("Feed"));
        assert_eq!(a, "guid:guid-1");

        let b = dedupe_key_for(None, Some("https://x"), "Title", Some("Feed"));
        assert_eq!(b, "link:https://x");

        let c = dedupe_key_for(None, None, "Title  One", Some("Feed  A"));
        assert_eq!(c, "title_source:title one::feed a");
    }

    #[test]
    fn normalize_title_compacts_whitespace_and_case() {
        assert_eq!(normalize_title("  SampleAlpha   ISO  "), "samplealpha iso");
    }

    #[test]
    fn retry_delay_has_jitter_and_increases_with_attempt() {
        let first = retry_delay_ms("https://example.test/rss.xml", 0);
        let second = retry_delay_ms("https://example.test/rss.xml", 1);

        assert!(first >= FEED_RETRY_BASE_DELAY_MS);
        assert!(first <= FEED_RETRY_BASE_DELAY_MS + FEED_RETRY_MAX_JITTER_MS);
        assert!(second >= FEED_RETRY_BASE_DELAY_MS * 2);
        assert!(second <= FEED_RETRY_BASE_DELAY_MS * 2 + FEED_RETRY_MAX_JITTER_MS);
    }

    #[test]
    fn retry_delay_is_deterministic_for_same_input() {
        let a = retry_delay_ms("https://example.test/rss.xml", 2);
        let b = retry_delay_ms("https://example.test/rss.xml", 2);
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn rss_service_disabled_waits_for_shutdown() {
        let mut settings = Settings::default();
        settings.rss.enabled = false;
        let (tx, mut rx) = mpsc::channel::<AppCommand>(2);
        let (sync_tx, sync_rx) = mpsc::channel::<()>(2);
        let (_downloaded_entry_tx, downloaded_entry_rx) = mpsc::channel::<RssHistoryEntry>(2);
        let (settings_tx, settings_rx) = tokio::sync::watch::channel(settings.clone());
        let (shutdown_tx, _) = broadcast::channel(1);

        let handle = spawn_rss_service(
            settings,
            Vec::new(),
            tx,
            sync_rx,
            downloaded_entry_rx,
            settings_rx,
            shutdown_tx.clone(),
        );
        drop(sync_tx);
        drop(settings_tx);
        tokio::task::yield_now().await;

        let _ = shutdown_tx.send(());
        let join_result = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(join_result.is_ok());

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn rss_service_applies_runtime_settings_update_on_sync_now() {
        let settings = Settings::default();
        let (tx, mut rx) = mpsc::channel::<AppCommand>(8);
        let (sync_tx, sync_rx) = mpsc::channel::<()>(2);
        let (_downloaded_entry_tx, downloaded_entry_rx) = mpsc::channel::<RssHistoryEntry>(2);
        let (settings_tx, settings_rx) = tokio::sync::watch::channel(settings.clone());
        let (shutdown_tx, _) = broadcast::channel(1);

        let handle = spawn_rss_service(
            settings,
            Vec::new(),
            tx,
            sync_rx,
            downloaded_entry_rx,
            settings_rx,
            shutdown_tx.clone(),
        );
        tokio::task::yield_now().await;

        // Enable RSS at runtime with no feeds (network-free path):
        // run_sync should emit RssPreviewUpdated(Vec::new()).
        let mut updated = Settings::default();
        updated.rss.enabled = true;
        settings_tx.send(updated).expect("send settings update");
        sync_tx.send(()).await.expect("send sync trigger");

        let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for command");
        match got {
            Some(AppCommand::RssPreviewUpdated(items)) => assert!(items.is_empty()),
            other => panic!("unexpected command: {:?}", other.map(|_| "non-preview")),
        }

        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rss_sync_match_and_auto_ingest_magnet_end_to_end() {
        let _guard = LocalWatchPathTestGuard::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
        let addr = listener.local_addr().expect("listener addr");
        let feed_url = format!("http://{}/rss.xml", addr);
        let magnet = "magnet:?xt=urn:btih:0123456789ABCDEF0123456789ABCDEF01234567&dn=SampleAlpha%20Episode%2001";
        let magnet_xml = magnet.replace('&', "&amp;");
        let feed_body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>Sample Feed</title>
    <item>
      <title>SampleAlpha Episode 01</title>
      <guid>guid-samplealpha-1</guid>
      <link>{}</link>
      <pubDate>Fri, 20 Feb 2026 00:00:00 GMT</pubDate>
    </item>
  </channel>
</rss>"#,
            magnet_xml
        );

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/rss+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                feed_body.len(),
                feed_body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });

        let temp = tempfile::tempdir().expect("tempdir");
        let watch_folder = temp.path().join("watch");

        let mut settings = Settings::default();
        settings.rss.enabled = true;
        settings.watch_folder = Some(watch_folder.clone());
        settings.rss.feeds.push(crate::config::RssFeed {
            url: feed_url,
            enabled: true,
        });
        settings.rss.filters.push(crate::config::RssFilter {
            query: "samplealpha".to_string(),
            mode: RssFilterMode::Fuzzy,
            enabled: true,
        });

        let (tx, mut rx) = mpsc::channel::<AppCommand>(16);
        let (sync_tx, sync_rx) = mpsc::channel::<()>(2);
        let (_downloaded_entry_tx, downloaded_entry_rx) = mpsc::channel::<RssHistoryEntry>(2);
        let (_settings_tx, settings_rx) = tokio::sync::watch::channel(settings.clone());
        let (shutdown_tx, _) = broadcast::channel(1);

        let handle = spawn_rss_service(
            settings,
            Vec::new(),
            tx,
            sync_rx,
            downloaded_entry_rx,
            settings_rx,
            shutdown_tx.clone(),
        );

        sync_tx.send(()).await.expect("send sync trigger");

        let mut got_download: Option<RssHistoryEntry> = None;
        let mut got_preview: Option<Vec<RssPreviewItem>> = None;
        let deadline = time::Instant::now() + Duration::from_secs(3);
        while time::Instant::now() < deadline && (got_download.is_none() || got_preview.is_none()) {
            let recv = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
            let Some(cmd) = recv.ok().flatten() else {
                continue;
            };
            match cmd {
                AppCommand::RssDownloadSelected { entry, .. } => got_download = Some(entry),
                AppCommand::RssPreviewUpdated(items) => got_preview = Some(items),
                _ => {}
            }
        }

        let download = got_download.expect("expected RssDownloadSelected");
        assert_eq!(download.added_via, RssAddedVia::Auto);
        assert_eq!(download.guid.as_deref(), Some("guid-samplealpha-1"));
        assert_eq!(download.link.as_deref(), Some(magnet));

        let preview = got_preview.expect("expected RssPreviewUpdated");
        assert_eq!(preview.len(), 1);
        assert!(preview[0].is_match);
        assert!(preview[0].is_downloaded);

        let mut entries = std::fs::read_dir(&watch_folder)
            .expect("read watch folder")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect watch entries");
        entries.sort_by_key(|e| e.file_name());
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].path().extension().and_then(|e| e.to_str()),
            Some("magnet")
        );
        let written = std::fs::read_to_string(entries[0].path()).expect("read written magnet");
        assert_eq!(written, magnet);

        server.await.expect("join server");
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn rss_service_shutdown_interrupts_inflight_sync() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let feed_url = format!("http://{}/rss.xml", addr);

        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.expect("accept request");
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let mut settings = Settings::default();
        settings.rss.enabled = true;
        settings.rss.feeds.push(crate::config::RssFeed {
            url: feed_url,
            enabled: true,
        });

        let (tx, _rx) = mpsc::channel::<AppCommand>(8);
        let (sync_tx, sync_rx) = mpsc::channel::<()>(2);
        let (_downloaded_entry_tx, downloaded_entry_rx) = mpsc::channel::<RssHistoryEntry>(2);
        let (_settings_tx, settings_rx) = tokio::sync::watch::channel(settings.clone());
        let (shutdown_tx, _) = broadcast::channel(1);

        let handle = spawn_rss_service(
            settings,
            Vec::new(),
            tx,
            sync_rx,
            downloaded_entry_rx,
            settings_rx,
            shutdown_tx.clone(),
        );

        sync_tx.send(()).await.expect("send sync trigger");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = shutdown_tx.send(());

        let join_result = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(
            join_result.is_ok(),
            "shutdown should interrupt in-flight sync without waiting for request timeout"
        );

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rss_max_preview_items_zero_skips_processing_and_auto_ingest() {
        let _guard = LocalWatchPathTestGuard::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
        let addr = listener.local_addr().expect("listener addr");
        let feed_url = format!("http://{}/rss.xml", addr);
        let magnet = "magnet:?xt=urn:btih:1111111111111111111111111111111111111111&dn=SampleBeta%20Episode%2001";
        let magnet_xml = magnet.replace('&', "&amp;");
        let feed_body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>Sample Feed</title>
    <item>
      <title>SampleBeta Episode 01</title>
      <guid>guid-samplebeta-1</guid>
      <link>{}</link>
      <pubDate>Fri, 20 Feb 2026 00:00:00 GMT</pubDate>
    </item>
  </channel>
</rss>"#,
            magnet_xml
        );

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/rss+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                feed_body.len(),
                feed_body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });

        let temp = tempfile::tempdir().expect("tempdir");
        let watch_folder = temp.path().join("watch");

        let mut settings = Settings::default();
        settings.rss.enabled = true;
        settings.rss.max_preview_items = 0;
        settings.watch_folder = Some(watch_folder.clone());
        settings.rss.feeds.push(crate::config::RssFeed {
            url: feed_url,
            enabled: true,
        });
        settings.rss.filters.push(crate::config::RssFilter {
            query: "samplebeta".to_string(),
            mode: RssFilterMode::Fuzzy,
            enabled: true,
        });

        let (tx, mut rx) = mpsc::channel::<AppCommand>(16);
        let (sync_tx, sync_rx) = mpsc::channel::<()>(2);
        let (_downloaded_entry_tx, downloaded_entry_rx) = mpsc::channel::<RssHistoryEntry>(2);
        let (_settings_tx, settings_rx) = tokio::sync::watch::channel(settings.clone());
        let (shutdown_tx, _) = broadcast::channel(1);

        let handle = spawn_rss_service(
            settings,
            Vec::new(),
            tx,
            sync_rx,
            downloaded_entry_rx,
            settings_rx,
            shutdown_tx.clone(),
        );

        sync_tx.send(()).await.expect("send sync trigger");

        let mut got_preview: Option<Vec<RssPreviewItem>> = None;
        let mut got_download = false;
        let deadline = time::Instant::now() + Duration::from_secs(3);
        while time::Instant::now() < deadline && got_preview.is_none() {
            let recv = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
            let Some(cmd) = recv.ok().flatten() else {
                continue;
            };
            match cmd {
                AppCommand::RssDownloadSelected { .. } => got_download = true,
                AppCommand::RssPreviewUpdated(items) => got_preview = Some(items),
                _ => {}
            }
        }

        let preview = got_preview.expect("expected RssPreviewUpdated");
        assert!(preview.is_empty());
        assert!(
            !got_download,
            "must not auto-ingest when preview cap is zero"
        );
        assert!(
            std::fs::read_dir(&watch_folder).is_err(),
            "watch folder should remain untouched"
        );

        server.await.expect("join server");
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }
}
