// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

mod ansi_backend;

use ansi_backend::AnsiBackend;
use ratatui::Terminal;
use superseedr::presentation::{self, PresentationFixture, PresentationState};
use wasm_bindgen::prelude::*;

/// Renders one deterministic ANSI frame using Superseedr's production TUI draw entrypoint.
#[wasm_bindgen(js_name = renderDemoFrame)]
pub fn render_demo_frame(cols: u16, rows: u16) -> String {
    render_demo_frame_inner(cols, rows)
}

fn render_demo_frame_inner(cols: u16, rows: u16) -> String {
    let width = cols.max(1);
    let height = rows.max(1);
    let backend = AnsiBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("ANSI backend initialization is infallible");
    let state = PresentationState::from_fixture(width, height, milestone_one_fixture());

    terminal
        .clear()
        .expect("ANSI backend clearing is infallible");
    terminal
        .draw(|frame| presentation::draw(frame, &state))
        .expect("ANSI rendering is infallible");
    terminal.backend_mut().take_output()
}

fn milestone_one_fixture() -> PresentationFixture {
    PresentationFixture {
        cpu_usage: 18.0,
        ram_usage_percent: 31.0,
        app_ram_usage: 148 * 1024 * 1024,
        run_time: 127,
        torrent_name: "Nebula Field Sample".to_owned(),
        info_hash: vec![0x5a; 20],
        pieces_total: 256,
        pieces_completed: 96,
        download_speed_bps: 2_400_000,
        upload_speed_bps: 180_000,
        connected_peers: 7,
        tcp_peer_count: 5,
        utp_peer_count: 2,
        total_size: 4_294_967_296,
        bytes_written: 1_610_612_736,
        eta_seconds: 1_940,
        activity_message: "Receiving fictional sample data".to_owned(),
        download_history: vec![1_600_000, 2_000_000, 2_400_000],
        upload_history: vec![120_000, 150_000, 180_000],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_renderer_returns_non_empty_ansi() {
        let frame = render_demo_frame_inner(120, 40);

        assert!(!frame.is_empty());
        assert!(
            frame.starts_with("\x1b[2J"),
            "frame did not start with a full terminal clear: {frame:?}"
        );
        assert!(
            frame.contains("\x1b["),
            "frame did not contain ANSI: {frame:?}"
        );
        assert!(
            frame.contains("Nebula Field Sample"),
            "frame did not contain the fictional production-renderer fixture"
        );
    }
}
