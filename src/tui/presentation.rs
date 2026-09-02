// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Narrow, renderer-only surface for alternate presentation backends.
//!
//! This module deliberately keeps Superseedr's display models private. Browser-owned code can
//! create the deterministic Milestone 1 fixture and pass it to the exact production draw
//! entrypoint, but it cannot reach native services or construct a second UI model.

use ratatui::{layout::Rect, Frame};

use crate::app::{AppMode, AppState, TorrentDisplayState};
use crate::config::Settings;
use crate::dht_service::{DhtStatus, DhtWaveTelemetry};
use crate::theme::Theme;

/// Production display state used by the renderer-only browser milestone.
pub struct PresentationState {
    app_state: AppState,
    dht_status: DhtStatus,
    dht_wave_telemetry: DhtWaveTelemetry,
    settings: Settings,
}

/// Narrow data-transfer surface for renderer-only presentation fixtures.
pub struct PresentationFixture {
    pub cpu_usage: f32,
    pub ram_usage_percent: f32,
    pub app_ram_usage: u64,
    pub run_time: u64,
    pub torrent_name: String,
    pub info_hash: Vec<u8>,
    pub pieces_total: u32,
    pub pieces_completed: u32,
    pub download_speed_bps: u64,
    pub upload_speed_bps: u64,
    pub connected_peers: usize,
    pub tcp_peer_count: usize,
    pub utp_peer_count: usize,
    pub total_size: u64,
    pub bytes_written: u64,
    pub eta_seconds: u64,
    pub activity_message: String,
    pub download_history: Vec<u64>,
    pub upload_history: Vec<u64>,
}

impl PresentationState {
    /// Builds production display state from browser-owned data without starting native services.
    pub fn from_fixture(width: u16, height: u16, fixture: PresentationFixture) -> Self {
        let settings = Settings::default();
        let mut app_state = AppState {
            mode: AppMode::Normal,
            screen_area: Rect::new(0, 0, width.max(1), height.max(1)),
            cpu_usage: fixture.cpu_usage,
            ram_usage_percent: fixture.ram_usage_percent,
            app_ram_usage: fixture.app_ram_usage,
            run_time: fixture.run_time,
            theme: Theme::builtin(settings.ui_theme),
            ..AppState::default()
        };
        app_state.ui.needs_redraw = true;

        let mut torrent = TorrentDisplayState::default();
        torrent.latest_state.info_hash = fixture.info_hash.clone();
        torrent.latest_state.torrent_name = fixture.torrent_name;
        torrent.latest_state.number_of_pieces_total = fixture.pieces_total;
        torrent.latest_state.number_of_pieces_completed = fixture.pieces_completed;
        torrent.latest_state.download_speed_bps = fixture.download_speed_bps;
        torrent.latest_state.upload_speed_bps = fixture.upload_speed_bps;
        torrent.latest_state.number_of_successfully_connected_peers = fixture.connected_peers;
        torrent.latest_state.tcp_peer_count = fixture.tcp_peer_count;
        torrent.latest_state.utp_peer_count = fixture.utp_peer_count;
        torrent.latest_state.total_size = fixture.total_size;
        torrent.latest_state.bytes_written = fixture.bytes_written;
        torrent.latest_state.eta = std::time::Duration::from_secs(fixture.eta_seconds);
        torrent.latest_state.activity_message = fixture.activity_message;
        torrent.smoothed_download_speed_bps = fixture.download_speed_bps;
        torrent.smoothed_upload_speed_bps = fixture.upload_speed_bps;
        torrent.download_history = fixture.download_history;
        torrent.upload_history = fixture.upload_history;
        app_state
            .torrents
            .insert(fixture.info_hash.clone(), torrent);
        app_state.torrent_list_order.push(fixture.info_hash);

        Self {
            app_state,
            dht_status: DhtStatus::default(),
            dht_wave_telemetry: DhtWaveTelemetry::default(),
            settings,
        }
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.app_state.screen_area = Rect::new(0, 0, width.max(1), height.max(1));
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn into_parts(self) -> (AppState, DhtStatus, DhtWaveTelemetry, Settings) {
        (
            self.app_state,
            self.dht_status,
            self.dht_wave_telemetry,
            self.settings,
        )
    }
}

/// Invokes Superseedr's exact production top-level renderer.
pub fn draw(frame: &mut Frame, state: &PresentationState) {
    crate::tui::render::draw(
        frame,
        &state.app_state,
        &state.dht_status,
        &state.dht_wave_telemetry,
        &state.settings,
    );
}
