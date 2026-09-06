// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser host view; shared app transitions retain application policy.

use super::*;

impl BrowserSession {
    pub fn capabilities(&self) -> crate::app::AppCapabilities {
        self.app_state.capabilities
    }

    pub fn screen_size(&self) -> (u16, u16) {
        (
            self.app_state.screen_area.width,
            self.app_state.screen_area.height,
        )
    }

    pub fn theme_name(&self) -> ThemeName {
        self.client_configs.ui_theme
    }

    pub fn rendered_theme_name(&self) -> ThemeName {
        self.app_state.theme.name
    }

    pub fn target_fps(&self) -> f64 {
        if matches!(self.app_state.mode, AppMode::PowerSaving) {
            1.0
        } else {
            self.app_state.data_rate.target_fps()
        }
    }

    pub fn effective_graph_mode(&self) -> &'static str {
        if self.app_state.graph_mode == crate::app::GraphDisplayMode::Auto {
            self.app_state.auto_graph_window.effective_mode.to_string()
        } else {
            self.app_state.graph_mode.to_string()
        }
    }

    pub fn browser_download_limit_bps(&self) -> Option<u64> {
        let limit = self.client_configs.global_download_limit_bps;
        (!crate::config::is_unlimited_rate_limit_bps(limit)).then_some(limit)
    }

    pub fn browser_upload_limit_bps(&self) -> Option<u64> {
        let limit = self.client_configs.global_upload_limit_bps;
        (!crate::config::is_unlimited_rate_limit_bps(limit)).then_some(limit)
    }

    pub fn effective_download_limit_bps(&self) -> u64 {
        self.browser_download_limit_bps().unwrap_or_default()
    }

    pub fn configured_download_mode(&self) -> crate::config::DownloadMode {
        self.client_configs.download_mode
    }

    pub fn configured_upload_limit_bps(&self) -> u64 {
        self.client_configs.global_upload_limit_bps
    }

    pub fn fps_label(&self) -> String {
        crate::tui::screens::normal::footer_fps_label(&self.app_state)
    }

    pub fn screen(&self) -> BrowserScreen {
        match self.app_state.mode {
            AppMode::Welcome => BrowserScreen::Welcome,
            AppMode::Normal => BrowserScreen::Normal,
            AppMode::Help => BrowserScreen::Help,
            AppMode::Journal => BrowserScreen::Journal,
            AppMode::PeerManagement => BrowserScreen::PeerManagement,
            AppMode::TorrentManagement => BrowserScreen::TorrentManagement,
            AppMode::PowerSaving => BrowserScreen::PowerSaving,
            AppMode::DeleteConfirm => BrowserScreen::DeleteConfirm,
            AppMode::Config => BrowserScreen::Config,
            AppMode::FileBrowser => BrowserScreen::FileBrowser,
            AppMode::Rss => BrowserScreen::Rss,
        }
    }

    pub fn key_text_input_active(&self) -> bool {
        let state = &self.app_state;
        match state.mode {
            AppMode::Normal => state.ui.is_searching,
            AppMode::Help => state.ui.help.is_searching,
            AppMode::Journal => state.ui.journal.is_searching,
            AppMode::PeerManagement => {
                state.ui.peer_management.is_searching
                    || state.ui.peer_management.details_is_searching
            }
            AppMode::TorrentManagement => state.ui.torrent_management.is_searching,
            AppMode::FileBrowser => {
                state.ui.file_browser.search_state.is_editing()
                    || matches!(
                        &state.ui.file_browser.browser_mode,
                        FileBrowserMode::DownloadLocSelection {
                            is_editing_name: true,
                            ..
                        }
                    )
            }
            AppMode::Rss => state.ui.rss.is_editing || state.ui.rss.is_searching,
            AppMode::Welcome | AppMode::PowerSaving | AppMode::DeleteConfirm | AppMode::Config => {
                false
            }
        }
    }

    pub fn normal_search_query(&self) -> &str {
        &self.app_state.ui.search_query
    }

    pub fn torrent_management_search_query(&self) -> &str {
        &self.app_state.ui.torrent_management.search_query
    }

    pub fn file_browser_search_query(&self) -> &str {
        &self.app_state.ui.file_browser.search_query
    }

    pub fn web_quit_key_enabled(&self) -> bool {
        matches!(self.app_state.mode, AppMode::Normal)
            && !self.app_state.ui.is_searching
            && !self.app_state.ui.visualization_focus.active
    }

    pub fn should_quit(&self) -> bool {
        self.app_state.should_quit
    }

    pub(crate) fn is_current_shared_follower(&self) -> bool {
        false
    }

    pub fn torrent_completion_journal_count_hex(&self, info_hash_hex: &str) -> usize {
        self.app_state
            .event_journal_state
            .entries
            .iter()
            .filter(|entry| {
                entry.event_type == EventType::TorrentCompleted
                    && entry.info_hash_hex.as_deref() == Some(info_hash_hex)
            })
            .count()
    }

    pub fn torrent_preview_state(&self) -> &'static str {
        match self.app_state.ui.file_browser.torrent_file_preview {
            TorrentFilePreviewState::Idle => "idle",
            TorrentFilePreviewState::Loading { .. } => "loading",
            TorrentFilePreviewState::Ready { .. } => "ready",
            TorrentFilePreviewState::Error { .. } => "error",
        }
    }

    pub fn torrent_preview_name(&self) -> &str {
        match &self.app_state.ui.file_browser.torrent_file_preview {
            TorrentFilePreviewState::Ready { preview, .. } => preview.name.as_str(),
            _ => "",
        }
    }

    pub fn torrent_preview_file_count(&self) -> usize {
        fn count_files(nodes: &[RawNode<crate::app::TorrentPreviewPayload>]) -> usize {
            nodes
                .iter()
                .map(|node| {
                    if node.is_dir {
                        count_files(&node.children)
                    } else {
                        1
                    }
                })
                .sum()
        }

        match &self.app_state.ui.file_browser.torrent_file_preview {
            TorrentFilePreviewState::Ready { preview, .. } => count_files(&preview.tree),
            _ => 0,
        }
    }

    pub fn rss_poll_interval_secs(&self) -> u64 {
        self.client_configs.rss.poll_interval_secs.max(1)
    }

    pub fn torrent_control_state_hex(
        &self,
        info_hash_hex: &str,
    ) -> Option<BrowserTorrentControlState> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        self.app_state.torrents.get(&info_hash).map(|torrent| {
            match torrent.latest_state.torrent_control_state {
                TorrentControlState::Running => BrowserTorrentControlState::Running,
                TorrentControlState::Paused => BrowserTorrentControlState::Paused,
                TorrentControlState::Deleting => BrowserTorrentControlState::Deleting,
            }
        })
    }

    pub fn torrent_delete_files_hex(&self, info_hash_hex: &str) -> Option<bool> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        self.app_state
            .torrents
            .get(&info_hash)
            .map(|torrent| torrent.latest_state.delete_files)
    }

    pub fn torrent_file_priority_hex(
        &self,
        info_hash_hex: &str,
        file_index: usize,
    ) -> Option<BrowserFilePriority> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        self.app_state
            .torrents
            .get(&info_hash)?
            .latest_state
            .file_priorities
            .get(&file_index)
            .and_then(|priority| match priority {
                FilePriority::High => Some(BrowserFilePriority::High),
                FilePriority::Skip => Some(BrowserFilePriority::Skip),
                FilePriority::Normal | FilePriority::Mixed => None,
            })
    }

    pub fn default_download_folder(&self) -> Option<&PathBuf> {
        self.client_configs.default_download_folder.as_ref()
    }

    pub fn browser_network_interface_count(&self) -> usize {
        self.app_state
            .ui
            .config
            .network_interface_inventory
            .interfaces
            .len()
    }

    pub fn browser_network_interface_refreshes(&self) -> u64 {
        self.browser_network_interface_refreshes
    }

    pub fn file_browser_current_path(&self) -> &PathBuf {
        &self.app_state.ui.file_browser.state.current_path
    }

    pub fn file_browser_cursor_path(&self) -> Option<&PathBuf> {
        self.app_state.ui.file_browser.state.cursor_path.as_ref()
    }

    pub fn delete_confirmation(&self) -> Option<(&[u8], bool)> {
        matches!(self.app_state.mode, AppMode::DeleteConfirm).then_some((
            self.app_state.ui.delete_confirm.info_hash.as_slice(),
            self.app_state.ui.delete_confirm.with_files,
        ))
    }

    pub fn torrent_count(&self) -> usize {
        self.app_state.torrents.len()
    }

    pub fn session_transfer_totals(&self) -> (u64, u64) {
        (
            self.app_state.session_total_downloaded,
            self.app_state.session_total_uploaded,
        )
    }

    pub fn lifetime_transfer_totals(&self) -> (u64, u64) {
        (
            self.app_state
                .lifetime_downloaded_from_config
                .saturating_add(self.app_state.session_total_downloaded),
            self.app_state
                .lifetime_uploaded_from_config
                .saturating_add(self.app_state.session_total_uploaded),
        )
    }

    pub fn rss_feed_count(&self) -> usize {
        self.client_configs.rss.feeds.len()
    }

    pub fn rss_enabled_feed_count(&self) -> usize {
        self.client_configs
            .rss
            .feeds
            .iter()
            .filter(|feed| feed.enabled)
            .count()
    }

    pub fn rss_history_count(&self) -> usize {
        self.app_state.rss_runtime.history.len()
    }

    pub fn rss_downloaded_preview_count(&self) -> usize {
        self.app_state
            .rss_runtime
            .preview_items
            .iter()
            .filter(|item| item.is_downloaded)
            .count()
    }

    pub fn rss_preview_count(&self) -> usize {
        self.app_state.rss_runtime.preview_items.len()
    }

    pub fn rss_last_sync_at(&self) -> Option<&str> {
        self.app_state.rss_runtime.last_sync_at.as_deref()
    }

    pub fn rss_next_sync_at(&self) -> Option<&str> {
        self.app_state.rss_runtime.next_sync_at.as_deref()
    }

    pub fn latest_completion_timestamp(&self) -> Option<&str> {
        self.app_state
            .event_journal_state
            .entries
            .iter()
            .rev()
            .find(|entry| entry.event_type == EventType::TorrentCompleted)
            .map(|entry| entry.ts_iso.as_str())
    }

    pub fn latest_completion_age_secs(&self) -> Option<i64> {
        let timestamp = self.latest_completion_timestamp()?;
        let completed_at = chrono::DateTime::parse_from_rfc3339(timestamp).ok()?;
        Some(
            chrono::Utc::now()
                .signed_duration_since(completed_at.with_timezone(&chrono::Utc))
                .num_seconds(),
        )
    }

    pub fn rss_sync_window_secs(&self) -> Option<i64> {
        let last = chrono::DateTime::parse_from_rfc3339(self.rss_last_sync_at()?).ok()?;
        let next = chrono::DateTime::parse_from_rfc3339(self.rss_next_sync_at()?).ok()?;
        Some(next.signed_duration_since(last).num_seconds())
    }

    pub fn system_warning(&self) -> Option<&str> {
        self.app_state.system_warning.as_deref()
    }

    pub fn system_error(&self) -> Option<&str> {
        self.app_state.system_error.as_deref()
    }

    pub fn torrent_sort_column(&self) -> &'static str {
        match self.app_state.torrent_sort.0 {
            TorrentSortColumn::Name => "name",
            TorrentSortColumn::Up => "up",
            TorrentSortColumn::Down => "down",
            TorrentSortColumn::Progress => "progress",
        }
    }

    pub fn torrent_sort_pinned(&self) -> bool {
        self.app_state.torrent_sort_pinned
    }

    pub fn torrent_sort_direction(&self) -> &'static str {
        match self.app_state.torrent_sort.1 {
            SortDirection::Ascending => "ascending",
            SortDirection::Descending => "descending",
        }
    }

    pub fn ordered_torrent_rates(&self) -> Vec<(u64, u64)> {
        self.app_state
            .torrent_list_order
            .iter()
            .filter_map(|info_hash| self.app_state.torrents.get(info_hash))
            .map(|torrent| {
                (
                    torrent.smoothed_download_speed_bps,
                    torrent.smoothed_upload_speed_bps,
                )
            })
            .collect()
    }

    pub fn anonymize_names(&self) -> bool {
        self.app_state.anonymize_torrent_names
    }

    pub fn selected_torrent_hash_hex(&self) -> Option<String> {
        self.app_state
            .torrent_list_order
            .get(self.app_state.ui.selected_torrent_index)
            .map(hex::encode)
    }

    pub fn selected_peer_rates(&self) -> Vec<(String, u64, u64)> {
        self.app_state
            .torrent_list_order
            .get(self.app_state.ui.selected_torrent_index)
            .and_then(|info_hash| self.app_state.torrents.get(info_hash))
            .map(|torrent| {
                torrent
                    .latest_state
                    .peers
                    .iter()
                    .map(|peer| {
                        (
                            peer.address.clone(),
                            peer.download_speed_bps,
                            peer.upload_speed_bps,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn peer_manager_metrics_updates(&self) -> u64 {
        self.browser_peer_metrics_updates
    }

    pub fn selected_peer_rate_frame_updates(&self) -> u64 {
        self.browser_selected_peer_rate_frame_updates
    }

    pub fn selected_peer_rate_frame_changes(&self) -> u64 {
        self.browser_selected_peer_rate_frame_changes
    }

    pub fn oldest_peer_last_seen_age_secs(&self) -> Option<u64> {
        let now = web_time::SystemTime::now();
        self.app_state
            .peer_manager_view
            .tracked_peers
            .iter()
            .filter_map(|peer| peer.last_seen)
            .map(|last_seen| now.duration_since(last_seen).unwrap_or_default().as_secs())
            .max()
    }

    pub fn select_torrent_hex(&mut self, info_hash_hex: &str) -> bool {
        let Ok(info_hash) = hex::decode(info_hash_hex) else {
            return false;
        };
        let Some(index) = self
            .app_state
            .torrent_list_order
            .iter()
            .position(|candidate| candidate == &info_hash)
        else {
            return false;
        };
        self.app_state.ui.selected_torrent_index = index;
        self.app_state.ui.needs_redraw = true;
        true
    }

    pub fn torrent_snapshot_hex(&self, info_hash_hex: &str) -> Option<BrowserTorrentSnapshot> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        let torrent = self.app_state.torrents.get(&info_hash)?;
        let latest = &torrent.latest_state;
        Some(BrowserTorrentSnapshot {
            info_hash_hex: info_hash_hex.to_string(),
            name: latest.torrent_name.clone(),
            control_state: match latest.torrent_control_state {
                TorrentControlState::Running => BrowserTorrentControlState::Running,
                TorrentControlState::Paused => BrowserTorrentControlState::Paused,
                TorrentControlState::Deleting => BrowserTorrentControlState::Deleting,
            },
            activity: latest.activity_message.clone(),
            pieces_total: latest.number_of_pieces_total,
            pieces_completed: latest.number_of_pieces_completed,
            total_size: latest.total_size,
            bytes_written: latest.bytes_written,
            download_speed_bps: latest.download_speed_bps,
            upload_speed_bps: latest.upload_speed_bps,
            bytes_downloaded_this_tick: latest.bytes_downloaded_this_tick,
            bytes_uploaded_this_tick: latest.bytes_uploaded_this_tick,
            eta: latest.eta,
            next_announce_in: latest.next_announce_in,
            connected_peers: latest.number_of_successfully_connected_peers,
            tcp_peers: latest.tcp_peer_count,
            utp_peers: latest.utp_peer_count,
            beneficial_tcp_peers: latest.beneficial_tcp_peer_count,
            beneficial_utp_peers: latest.beneficial_utp_peer_count,
            session_downloaded: latest.session_total_downloaded,
            session_uploaded: latest.session_total_uploaded,
            data_available: latest.data_available,
            is_complete: latest.is_complete,
            download_history_len: torrent.download_history.len(),
            upload_history_len: torrent.upload_history.len(),
        })
    }

    pub fn torrent_download_path_hex(&self, info_hash_hex: &str) -> Option<&PathBuf> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        self.app_state
            .torrents
            .get(&info_hash)?
            .latest_state
            .download_path
            .as_ref()
    }

    pub fn torrent_container_name_hex(&self, info_hash_hex: &str) -> Option<&str> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        self.app_state
            .torrents
            .get(&info_hash)?
            .latest_state
            .container_name
            .as_deref()
    }

    pub fn selected_torrent_snapshot(&self) -> Option<BrowserTorrentSnapshot> {
        self.selected_torrent_hash_hex()
            .and_then(|info_hash| self.torrent_snapshot_hex(&info_hash))
    }

    pub fn visualization_snapshot(&self) -> BrowserVisualizationSnapshot {
        let selected = self
            .app_state
            .torrent_list_order
            .get(self.app_state.ui.selected_torrent_index)
            .and_then(|info_hash| self.app_state.torrents.get(info_hash));
        BrowserVisualizationSnapshot {
            total_download_bps: self
                .app_state
                .torrents
                .values()
                .map(|torrent| torrent.latest_state.download_speed_bps)
                .sum(),
            total_upload_bps: self
                .app_state
                .torrents
                .values()
                .map(|torrent| torrent.latest_state.upload_speed_bps)
                .sum(),
            disk_read_bps: self.app_state.avg_disk_read_bps,
            disk_write_bps: self.app_state.avg_disk_write_bps,
            effects_phase_time: self.app_state.ui.effects_phase_time,
            file_download_phase: self.app_state.ui.file_activity_download_phase,
            file_upload_phase: self.app_state.ui.file_activity_upload_phase,
            disk_health_phase: self.app_state.disk_health_phase,
            disk_health_state_level: self.app_state.disk_health_state_level,
            tracked_peers: self.app_state.peer_manager_view.tracked_peers.len(),
            network_history_samples: self.app_state.network_history_state.tiers.second_1s.len(),
            activity_history_samples: self
                .app_state
                .activity_history_state
                .cpu
                .tiers
                .second_1s
                .len(),
            peer_connected_events: selected
                .map(|torrent| torrent.peer_connection_history.iter().sum())
                .unwrap_or_default(),
            peer_discovered_events: selected
                .map(|torrent| torrent.peer_discovery_history.iter().sum())
                .unwrap_or_default(),
            peer_disconnected_events: selected
                .map(|torrent| torrent.peer_disconnect_history.iter().sum())
                .unwrap_or_default(),
            blocks_received_events: selected
                .map(|torrent| torrent.latest_state.blocks_in_history.iter().sum())
                .unwrap_or_default(),
            blocks_sent_events: selected
                .map(|torrent| torrent.latest_state.blocks_out_history.iter().sum())
                .unwrap_or_default(),
            read_iops: self.app_state.read_iops,
            write_iops: self.app_state.write_iops,
            disk_read_latency_micros: self.app_state.avg_disk_read_latency.as_micros() as u64,
            disk_write_latency_micros: self.app_state.avg_disk_write_latency.as_micros() as u64,
            recv_to_write_latency_micros: self.app_state.recv_to_write_p95.as_micros() as u64,
            recent_file_activity: selected
                .map(|torrent| torrent.recent_file_activity.len())
                .unwrap_or_default(),
            recent_file_download_activity: selected
                .map(|torrent| {
                    torrent
                        .recent_file_activity
                        .values()
                        .filter(|activity| activity.download_at.is_some())
                        .count()
                })
                .unwrap_or_default(),
            recent_file_upload_activity: selected
                .map(|torrent| {
                    torrent
                        .recent_file_activity
                        .values()
                        .filter(|activity| activity.upload_at.is_some())
                        .count()
                })
                .unwrap_or_default(),
            swarm_availability_samples: selected
                .map(|torrent| torrent.swarm_availability_history.len())
                .unwrap_or_default(),
            dht_wave_initialized: self.app_state.ui.dht_wave.initialized,
            dht_active_queries: self.dht_wave_telemetry.inflight_ipv4_queries
                + self.dht_wave_telemetry.inflight_ipv6_queries,
            dht_peers_found: self.dht_wave_telemetry.unique_peers_found_last_10s,
            dht_query_load: self.app_state.ui.dht_wave.query_load,
        }
    }

    pub fn torrent_management_cursor_hash_hex(&self) -> Option<String> {
        self.app_state
            .ui
            .torrent_management
            .cursor_hash
            .as_deref()
            .map(hex::encode)
    }
}
