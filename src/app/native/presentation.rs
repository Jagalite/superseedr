// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native presentation execution. Runtime resources remain owned by `App`.

use super::*;

impl App {
    pub(super) fn should_draw_this_frame(
        mode: &AppMode,
        ui_needs_redraw: bool,
        normal_animation_active: bool,
    ) -> bool {
        match mode {
            AppMode::PowerSaving => ui_needs_redraw,
            // The one-second stats tick dirties these time-based screens often enough to
            // refresh live ages and restriction countdowns without continuously redrawing.
            AppMode::Journal | AppMode::PeerManagement => ui_needs_redraw,
            AppMode::Normal => ui_needs_redraw || normal_animation_active,
            _ => true,
        }
    }

    pub(super) fn normal_mode_animation_active(
        app_state: &AppState,
        layout_mode: UiLayoutMode,
        dht_wave_telemetry: Option<&DhtWaveTelemetry>,
        now: Instant,
    ) -> bool {
        if app_state.theme.effects.enabled() {
            return true;
        }

        if Self::disk_health_has_current_signal(app_state) {
            return true;
        }

        if Self::dht_wave_animation_active(&app_state.ui.dht_wave, dht_wave_telemetry) {
            return true;
        }

        if app_state.ui.swarm_availability_flash.has_active_flash(now) {
            return true;
        }

        app_state
            .torrent_list_order
            .get(app_state.ui.selected_torrent_index)
            .and_then(|info_hash| app_state.torrents.get(info_hash))
            .is_some_and(|torrent| {
                Self::selected_torrent_animation_active(torrent, now)
                    || (app_state.ui.visualization_focus.peer_stream
                        != PeerStreamVisualization::Classic
                        && (torrent.latest_state.number_of_successfully_connected_peers > 0
                            || !torrent.latest_state.peers.is_empty())
                        && Self::peer_stream_panel_visible(app_state, layout_mode))
            })
    }

    pub(super) fn peer_stream_panel_visible(
        app_state: &AppState,
        layout_mode: UiLayoutMode,
    ) -> bool {
        let layout_ctx = LayoutContext::new(
            app_state.screen_area,
            app_state,
            layout_mode,
            DEFAULT_SIDEBAR_PERCENT,
        );
        calculate_layout(app_state.screen_area, &layout_ctx)
            .peer_stream
            .is_some_and(|area| {
                area.width >= PEER_STREAM_MIN_WIDTH && area.height >= PEER_STREAM_MIN_HEIGHT
            })
    }

    pub(super) fn disk_health_has_current_signal(app_state: &AppState) -> bool {
        app_state.avg_disk_read_bps > 0
            || app_state.avg_disk_write_bps > 0
            || app_state.read_iops > 0
            || app_state.write_iops > 0
            || app_state.max_disk_backoff_this_tick_ms > 0
    }

    #[cfg(test)]
    pub(super) fn disk_health_phase_speed(app_state: &AppState) -> f64 {
        crate::tui::animation::disk_health_phase_speed(app_state)
    }

    pub(super) fn dht_wave_animation_active(
        wave: &DhtWaveUiState,
        telemetry: Option<&DhtWaveTelemetry>,
    ) -> bool {
        if telemetry.is_some_and(|telemetry| {
            telemetry.active_lookups > 0
                || telemetry.active_user_lookups > 0
                || telemetry.inflight_ipv4_queries > 0
                || telemetry.inflight_ipv6_queries > 0
                || telemetry.unique_peers_found_last_10s > 0
        }) {
            return true;
        }

        wave.query_load > 0.01
            || wave.discovery_boost > 0.01
            || wave.query_surge > 0.01
            || (wave.phase_speed > 0.05
                && (wave.amplitude > 0.02 || wave.harmonic_amplitude > 0.01))
    }

    pub(super) fn selected_torrent_animation_active(
        torrent: &TorrentDisplayState,
        now: Instant,
    ) -> bool {
        if torrent.smoothed_download_speed_bps > 0
            || torrent.smoothed_upload_speed_bps > 0
            || torrent.disk_read_speed_bps > 0
            || torrent.disk_write_speed_bps > 0
            || torrent.peers_discovered_this_tick > 0
            || torrent.peers_connected_this_tick > 0
            || torrent.peers_disconnected_this_tick > 0
        {
            return true;
        }

        let metrics = &torrent.latest_state;
        if metrics.blocks_in_this_tick > 0
            || metrics.blocks_out_this_tick > 0
            || metrics
                .blocks_in_history
                .iter()
                .rev()
                .take(NORMAL_ANIMATION_RECENT_BLOCK_ROWS)
                .any(|&blocks| blocks > 0)
            || metrics
                .blocks_out_history
                .iter()
                .rev()
                .take(NORMAL_ANIMATION_RECENT_BLOCK_ROWS)
                .any(|&blocks| blocks > 0)
        {
            return true;
        }

        if torrent
            .peer_discovery_history
            .iter()
            .chain(torrent.peer_connection_history.iter())
            .chain(torrent.peer_disconnect_history.iter())
            .rev()
            .take(NORMAL_ANIMATION_RECENT_PEER_EVENTS)
            .any(|&events| events > 0)
        {
            return true;
        }

        torrent.recent_file_activity.values().any(|activity| {
            [activity.download_at, activity.upload_at]
                .into_iter()
                .flatten()
                .any(|seen_at| {
                    now.saturating_duration_since(seen_at) <= NORMAL_ANIMATION_FILE_ACTIVITY_WINDOW
                })
        })
    }

    pub(super) fn normal_idle_frame_check_interval(target_frame_interval: Duration) -> Duration {
        target_frame_interval.max(NORMAL_IDLE_FRAME_CHECK_INTERVAL)
    }

    pub(super) fn advance_next_draw_time(
        next_draw_time: &mut Instant,
        frame_started_at: Instant,
        target_frame_interval: Duration,
    ) {
        *next_draw_time += target_frame_interval;
        while *next_draw_time <= frame_started_at {
            *next_draw_time += target_frame_interval;
        }
    }

    pub(super) fn tick_ui_effects_clock(&mut self) {
        let dht_status = self.dht_service.current_status();
        let dht_wave_telemetry = self.dht_service.current_wave_telemetry();
        advance_ui_effects_for_frame(
            &mut self.app_state,
            &self.client_configs,
            &dht_status,
            &dht_wave_telemetry,
        );
    }

    pub(super) fn refresh_system_warning(&mut self) {
        let dht_warning = self.dht_service.current_warning();
        let base_and_network = compose_system_warning(
            self.base_system_warning.as_deref(),
            self.network_warning.as_deref(),
        );
        self.app_state.system_warning =
            compose_system_warning(base_and_network.as_deref(), dht_warning.as_deref());
    }

    pub(super) fn refresh_peer_management_derived(&mut self, now: SystemTime) {
        crate::tui::screens::peers::recompute_peer_management_derived(&mut self.app_state, now);
    }

    pub(crate) fn refresh_peer_management_screen(&mut self) {
        sync_peer_policy_to_app_state(&mut self.app_state, &mut self.peer_policy_rx);
        sync_peer_manager_view_to_app_state(&mut self.app_state, &mut self.peer_manager_view_rx);
        self.refresh_peer_management_derived(SystemTime::now());
    }

    // Constantly ensures all table selected indices are in-bounds
    pub(super) fn clamp_selected_indices(&mut self) {
        clamp_selected_indices_in_state(&mut self.app_state);
    }

    pub fn sort_and_filter_torrent_list(&mut self) {
        sort_and_filter_torrent_list_state(&mut self.app_state);
    }
}
