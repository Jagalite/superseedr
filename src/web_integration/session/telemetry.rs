// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser host telemetry; shared app transitions retain application policy.

use super::*;

impl BrowserSession {
    pub(crate) fn refresh_peer_management_screen(&mut self) {
        peers::recompute_peer_management_derived(&mut self.app_state, web_time::SystemTime::now());
    }

    pub(super) fn apply_telemetry_batch(&mut self, batch: &BrowserTelemetryBatch) {
        let state = &mut self.app_state;
        UiTelemetry::record_peer_activity(
            state,
            &batch.info_hash,
            batch.peers_discovered as u64,
            batch.peers_connected as u64,
            batch.peers_disconnected as u64,
            batch.blocks_received as u64,
            batch.blocks_sent as u64,
        );
        UiTelemetry::record_disk_reads(
            state,
            &batch.info_hash,
            batch.disk_read_bytes,
            &batch.disk_read_samples,
        );
        UiTelemetry::record_disk_writes(
            state,
            &batch.info_hash,
            batch.disk_write_bytes,
            &batch.disk_write_samples,
        );
        UiTelemetry::record_reads_finished(
            state,
            batch.disk_read_operations,
            batch.disk_read_latency,
        );
        UiTelemetry::record_writes_finished(
            state,
            batch.disk_write_operations,
            batch.disk_write_latency,
        );
        UiTelemetry::record_writes_completed(
            state,
            batch.disk_write_bytes,
            batch.receive_to_write_latency,
            batch.disk_write_operations,
        );
        if let Some(duration) = batch.disk_backoff {
            UiTelemetry::record_disk_backoff(state, duration);
        }
    }

    pub(super) fn record_torrent_completed_event(
        &mut self,
        info_hash: &[u8],
        torrent_name: String,
    ) {
        let info_hash_hex = hex::encode(info_hash);
        if self
            .app_state
            .event_journal_state
            .entries
            .iter()
            .any(|entry| {
                entry.event_type == EventType::TorrentCompleted
                    && entry.info_hash_hex.as_deref() == Some(info_hash_hex.as_str())
            })
        {
            return;
        }

        self.checkpoint_requested = true;
        append_event_journal_entry(
            &mut self.app_state.event_journal_state,
            EventJournalEntry {
                scope: EventScope::Host,
                ts_iso: chrono::Utc::now().to_rfc3339(),
                category: EventCategory::TorrentLifecycle,
                event_type: EventType::TorrentCompleted,
                torrent_name: Some(torrent_name),
                info_hash_hex: Some(info_hash_hex),
                message: Some("Torrent completed".to_string()),
                ..Default::default()
            },
        );
        self.app_state.ui.needs_redraw = true;
    }

    pub fn refresh_browser_peer_manager(&mut self) {
        let snapshots = self
            .app_state
            .torrents
            .values()
            .map(|torrent| {
                (
                    torrent.latest_state.info_hash.clone(),
                    torrent.latest_state.torrent_name.clone(),
                    torrent.latest_state.peers.clone(),
                )
            })
            .collect::<Vec<_>>();
        let current_keys = snapshots
            .iter()
            .flat_map(|(info_hash, _, peers)| {
                peers
                    .iter()
                    .map(|peer| (info_hash.clone(), peer.address.clone()))
            })
            .collect::<HashSet<_>>();
        for (key, peer) in &mut self.browser_tracked_peers {
            if peer.is_active && !current_keys.contains(key) {
                peer.is_active = false;
                peer.disconnect_count = peer.disconnect_count.saturating_add(1);
            }
        }
        for (info_hash, torrent_name, peers) in snapshots {
            for peer in peers {
                let Ok(endpoint) = peer.address.parse::<std::net::SocketAddr>() else {
                    continue;
                };
                let key = (info_hash.clone(), peer.address.clone());
                let previous = self.browser_tracked_peers.get(&key);
                let inferred_connection_count =
                    previous.map_or(peer.connection_count, |previous| {
                        previous
                            .connection_count
                            .saturating_add(u64::from(!previous.is_active))
                            .max(peer.connection_count)
                    });
                let inferred_disconnect_count = previous
                    .map(|previous| previous.disconnect_count.max(peer.disconnect_count))
                    .unwrap_or(peer.disconnect_count);
                self.browser_tracked_peers.insert(
                    key,
                    PeerManagerTrackedPeer {
                        torrent_info_hash: info_hash.clone(),
                        torrent_name: torrent_name.clone(),
                        ip: endpoint.ip(),
                        is_active: true,
                        endpoints: vec![PeerManagerEndpointView {
                            address: peer.address.clone(),
                            total_downloaded: peer.total_downloaded,
                            total_uploaded: peer.total_uploaded,
                        }],
                        downloaded_evidence_bytes: peer.total_downloaded,
                        uploaded_evidence_bytes: peer.total_uploaded,
                        total_downloaded_bytes: peer.total_downloaded,
                        total_uploaded_bytes: peer.total_uploaded,
                        connection_count: inferred_connection_count,
                        disconnect_count: inferred_disconnect_count,
                        transfer_threshold_bytes: 64 * 1024,
                        reconnect_count: u32::try_from(inferred_connection_count.saturating_sub(1))
                            .unwrap_or(u32::MAX),
                        reconnect_limit: 4,
                        reconnect_window_secs: 300,
                        last_seen: Some(web_time::SystemTime::now()),
                        clients: vec![parse_peer_client(&peer.peer_id)],
                    },
                );
            }
        }
        self.browser_peer_metrics_updates = self.browser_peer_metrics_updates.saturating_add(1);
        let mut tracked_peers = self
            .browser_tracked_peers
            .values()
            .cloned()
            .collect::<Vec<_>>();
        tracked_peers.sort_by(|left, right| {
            left.torrent_info_hash
                .cmp(&right.torrent_info_hash)
                .then_with(|| left.ip.cmp(&right.ip))
                .then_with(|| left.endpoints[0].address.cmp(&right.endpoints[0].address))
        });
        self.app_state.peer_manager_view = Arc::new(PeerManagerView {
            registered_torrents: self.app_state.torrents.len(),
            metrics_updates: self.browser_peer_metrics_updates,
            tracked_peers,
        });
        peers::recompute_peer_management_derived(&mut self.app_state, web_time::SystemTime::now());
    }

    pub fn run_browser_second_tick(
        &mut self,
        cpu_usage: f32,
        ram_usage_percent: f32,
        app_ram_usage: u64,
        run_time: u64,
        sample_time_unix: u64,
    ) {
        let previous_torrent_sort = self.app_state.torrent_sort;
        let previous_peer_sort = self.app_state.peer_sort;
        UiTelemetry::on_second_tick_with_system_snapshot(
            &mut self.app_state,
            Some(SystemTelemetrySnapshot {
                cpu_usage,
                ram_usage_percent,
                app_ram_usage,
                run_time,
            }),
        );
        align_unpinned_peer_sort_with_visible_activity(&mut self.app_state);
        refresh_autosort_after_stats(
            &mut self.app_state,
            previous_torrent_sort,
            previous_peer_sort,
        );
        NetworkHistoryTelemetry::on_second_tick_at(&mut self.app_state, sample_time_unix);
        ActivityHistoryTelemetry::on_second_tick_at(&mut self.app_state, sample_time_unix);
        self.app_state.ui.needs_redraw = true;
    }

    pub fn apply_browser_telemetry(&mut self, update: BrowserTelemetryUpdate) {
        let BrowserTelemetryUpdate {
            cpu_usage,
            ram_usage_percent,
            app_ram_usage,
            run_time,
            disk_read_bps,
            disk_write_bps,
            dht_routing_nodes,
            dht_active_lookups,
            dht_inflight_ipv4_queries,
            dht_inflight_ipv6_queries,
            dht_peers_found,
            dht_demand_power_scale_halves,
            filesystem,
            journal,
            rss,
        } = update;
        self.apply_browser_runtime_telemetry(BrowserRuntimeTelemetryUpdate {
            cpu_usage,
            ram_usage_percent,
            app_ram_usage,
            run_time,
            disk_read_bps,
            disk_write_bps,
            disk_warning_active: false,
        });
        self.apply_browser_dht_telemetry(BrowserDhtTelemetryUpdate {
            routing_nodes: dht_routing_nodes,
            active_lookups: dht_active_lookups,
            inflight_ipv4_queries: dht_inflight_ipv4_queries,
            inflight_ipv6_queries: dht_inflight_ipv6_queries,
            dht_peers_found,
            demand_power_scale_halves: dht_demand_power_scale_halves,
        });

        let state = &mut self.app_state;

        let base_path = self.environment.file_browser_root.clone();
        state.ui.file_browser.state.current_path = base_path.clone();
        state.ui.file_browser.data = filesystem
            .iter()
            .map(|file| RawNode {
                name: file.relative_path.clone(),
                full_path: base_path.join(&file.relative_path),
                children: Vec::new(),
                payload: FileMetadata {
                    size: file.size,
                    modified: UNIX_EPOCH
                        + Duration::from_secs(self.environment.file_modified_unix_secs),
                },
                is_dir: false,
            })
            .collect();
        state.ui.file_browser.state.cursor_path = state
            .ui
            .file_browser
            .data
            .first()
            .map(|node| node.full_path.clone());

        state.event_journal_state.entries = journal
            .into_iter()
            .enumerate()
            .map(|(index, entry)| {
                let (category, event_type) = browser_journal_event(entry.kind);
                EventJournalEntry {
                    id: index as u64 + 1,
                    scope: EventScope::Host,
                    ts_iso: entry.timestamp,
                    category,
                    event_type,
                    torrent_name: entry.torrent_name,
                    message: Some(entry.message),
                    ..Default::default()
                }
            })
            .collect();
        state.event_journal_state.next_id = state.event_journal_state.entries.len() as u64 + 1;

        let mut seen_feed_urls = HashSet::new();
        self.client_configs.rss.feeds = rss
            .iter()
            .filter(|item| seen_feed_urls.insert(item.feed_url.clone()))
            .map(|item| RssFeed {
                url: item.feed_url.clone(),
                enabled: true,
            })
            .collect();
        let mut seen_filter_queries = HashSet::new();
        self.client_configs.rss.filters = rss
            .iter()
            .filter(|item| seen_filter_queries.insert(item.filter_query.clone()))
            .map(|item| RssFilter {
                query: item.filter_query.clone(),
                mode: RssFilterMode::Fuzzy,
                enabled: true,
            })
            .collect();
        state.rss_runtime.preview_items = rss
            .into_iter()
            .map(|item| RssPreviewItem {
                dedupe_key: item.dedupe_key,
                title: item.item_title,
                link: Some(item.item_link),
                source: Some(item.feed_url),
                date_iso: Some(item.timestamp),
                is_match: true,
                ..Default::default()
            })
            .collect();
        rss::recompute_rss_derived(state, &self.client_configs);

        state.ui.needs_redraw = true;
    }

    pub fn append_browser_journal_entry(&mut self, entry: BrowserJournalUpdate) {
        let (category, event_type) = browser_journal_event(entry.kind);
        append_event_journal_entry(
            &mut self.app_state.event_journal_state,
            EventJournalEntry {
                scope: EventScope::Host,
                ts_iso: entry.timestamp,
                category,
                event_type,
                torrent_name: entry.torrent_name,
                message: Some(entry.message),
                ..Default::default()
            },
        );
    }

    pub fn replace_history(&mut self, seed: BrowserHistorySeed) {
        let history_len = seed
            .total_download_history
            .len()
            .max(seed.total_upload_history.len())
            .max(seed.disk_read_history.len())
            .max(seed.disk_write_history.len());
        if history_len == 0 {
            return;
        }
        let value_at = |history: &[u64], index: usize| {
            let offset = history_len.saturating_sub(history.len());
            index
                .checked_sub(offset)
                .and_then(|local| history.get(local))
                .copied()
                .unwrap_or_default()
        };
        let torrent_histories = seed
            .torrent_histories
            .iter()
            .map(|torrent| {
                (
                    hex::encode(&torrent.info_hash),
                    &torrent.download_history,
                    &torrent.upload_history,
                )
            })
            .collect::<Vec<_>>();
        let state = &mut self.app_state;

        state.network_history_state = NetworkHistoryPersistedState::default();
        state.network_history_rollups = NetworkHistoryRollupState::default();
        state.activity_history_state = ActivityHistoryPersistedState::default();
        state.activity_history_rollups = ActivityHistoryRollupState::default();

        for index in 0..history_len {
            let ts_unix = seed
                .end_unix_secs
                .saturating_sub((history_len - 1 - index) as u64);
            let download_bps = value_at(&seed.total_download_history, index);
            let upload_bps = value_at(&seed.total_upload_history, index);
            let disk_read_bps = value_at(&seed.disk_read_history, index);
            let disk_write_bps = value_at(&seed.disk_write_history, index);
            let backoff_ms = value_at(&seed.disk_backoff_history_ms, index);
            state.network_history_rollups.ingest_second_sample(
                &mut state.network_history_state,
                ts_unix,
                download_bps,
                upload_bps,
                backoff_ms,
            );
            state.activity_history_rollups.cpu.ingest_second_sample(
                &mut state.activity_history_state.cpu,
                ts_unix,
                (seed.cpu_usage.clamp(0.0, 100.0) * 10.0).round() as u64,
                0,
            );
            state.activity_history_rollups.ram.ingest_second_sample(
                &mut state.activity_history_state.ram,
                ts_unix,
                (seed.ram_usage_percent.clamp(0.0, 100.0) * 10.0).round() as u64,
                0,
            );
            state.activity_history_rollups.disk.ingest_second_sample(
                &mut state.activity_history_state.disk,
                ts_unix,
                disk_read_bps,
                disk_write_bps,
            );
            state.activity_history_rollups.tuning.ingest_second_sample(
                &mut state.activity_history_state.tuning,
                ts_unix,
                state.current_tuning_score,
                state.last_tuning_score,
            );
            for (key, download_history, upload_history) in &torrent_histories {
                let series = state
                    .activity_history_state
                    .torrents
                    .entry(key.clone())
                    .or_default();
                state
                    .activity_history_rollups
                    .torrents
                    .entry(key.clone())
                    .or_default()
                    .ingest_second_sample(
                        series,
                        ts_unix,
                        value_at(download_history, index),
                        value_at(upload_history, index),
                    );
            }
        }

        state.total_download_history = seed.total_download_history.clone();
        state.total_upload_history = seed.total_upload_history.clone();
        state.avg_download_history = seed.total_download_history.clone();
        state.avg_upload_history = seed.total_upload_history.clone();
        state.disk_read_history = seed.disk_read_history.clone();
        state.disk_write_history = seed.disk_write_history.clone();
        state.avg_disk_read_bps = seed.disk_read_bps;
        state.avg_disk_write_bps = seed.disk_write_bps;
        state.avg_disk_write_completed_bps = seed.disk_write_bps;
        state.disk_backoff_history_ms = seed.disk_backoff_history_ms.into();
        if seed.disk_read_bps > 0 {
            state.global_disk_read_history_log = VecDeque::from([DiskIoOperation {
                piece_index: 7,
                offset: 0,
                length: 32 * 1024,
            }]);
        }
        if seed.disk_write_bps > 0 {
            state.global_disk_write_history_log = VecDeque::from([DiskIoOperation {
                piece_index: 9,
                offset: 64 * 1024,
                length: 32 * 1024,
            }]);
        }
    }

    pub fn apply_browser_runtime_telemetry(&mut self, update: BrowserRuntimeTelemetryUpdate) {
        let disk_warning_active = update.disk_warning_active;
        let state = &mut self.app_state;
        state.cpu_usage = update.cpu_usage;
        state.ram_usage_percent = update.ram_usage_percent;
        state.app_ram_usage = update.app_ram_usage;
        state.run_time = update.run_time;
        Self::set_browser_disk_warning_state(state, disk_warning_active);

        state.ui.needs_redraw = true;
    }

    pub fn apply_browser_dht_telemetry(&mut self, update: BrowserDhtTelemetryUpdate) {
        self.dht_status.health.enabled = true;
        self.dht_status.health.cached_ipv4_routes = update.routing_nodes;
        self.dht_status.health.cached_ipv6_routes = 0;
        self.dht_status.health.active_ipv4_routes = update.routing_nodes;
        self.dht_status.health.active_ipv6_routes = 0;
        self.dht_status.health.inflight_lookups = update.active_lookups;
        self.dht_status.health.inflight_ipv4_queries = update.inflight_ipv4_queries;
        self.dht_status.health.inflight_ipv6_queries = update.inflight_ipv6_queries;
        self.dht_status.health.dht_size_estimate = None;
        self.dht_wave_telemetry.active_lookups = update.active_lookups;
        self.dht_wave_telemetry.active_user_lookups = 0;
        self.dht_wave_telemetry.inflight_ipv4_queries = update.inflight_ipv4_queries;
        self.dht_wave_telemetry.inflight_ipv6_queries = update.inflight_ipv6_queries;
        self.dht_wave_telemetry.unique_peers_found_last_10s = update.dht_peers_found;
        self.dht_wave_telemetry.demand_power_scale_halves = if update.demand_power_scale_halves == 0
        {
            2
        } else {
            update.demand_power_scale_halves
        };
        self.dht_wave_telemetry.demand_power_multiplier = self
            .dht_wave_telemetry
            .demand_power_scale_halves
            .div_ceil(2);

        self.app_state.ui.needs_redraw = true;
    }

    pub fn set_browser_disk_warning(&mut self, active: bool) {
        Self::set_browser_disk_warning_state(&mut self.app_state, active);
    }

    pub(super) fn set_browser_disk_warning_state(state: &mut AppState, active: bool) {
        if active {
            if state.system_warning.is_none() {
                state.system_warning = Some(BROWSER_DISK_WARNING.to_string());
            }
        } else if state.system_warning.as_deref() == Some(BROWSER_DISK_WARNING) {
            state.system_warning = None;
        }
    }

    pub fn advance_browser_visualizations(&mut self, delta_seconds: f64) {
        let delta_seconds = delta_seconds.clamp(0.0, 30.0);
        if delta_seconds == 0.0 {
            return;
        }
        let mut remaining = delta_seconds;
        while remaining > f64::EPSILON {
            let step = remaining.min(0.25);
            advance_ui_effects_for_elapsed(
                &mut self.app_state,
                &self.client_configs,
                &self.dht_status,
                &self.dht_wave_telemetry,
                step,
            );
            remaining -= step;
        }
        self.fps_sample_elapsed += delta_seconds;
        self.fps_sample_frames = self.fps_sample_frames.saturating_add(1);
        if self.fps_sample_elapsed >= 1.0 {
            let measured = f64::from(self.fps_sample_frames) / self.fps_sample_elapsed;
            let target = self.target_fps();
            // requestAnimationFrame commonly reports one frame below its nominal refresh rate
            // because the sampling window straddles a callback boundary. Keep genuine misses
            // visible while avoiding a false 59/60 oscillation in the unchanged production footer.
            self.app_state.ui.measured_fps = Some(if measured >= target * 0.98 {
                target
            } else {
                measured
            });
            self.fps_sample_elapsed = 0.0;
            self.fps_sample_frames = 0;
        }
        self.app_state.ui.needs_redraw = true;
    }
}
