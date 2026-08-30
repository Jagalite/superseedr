// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

mod ansi_backend;
#[cfg(target_arch = "wasm32")]
mod browser_demo;
#[cfg(target_arch = "wasm32")]
mod mocks;

use superseedr::presentation::PresentationFixture;
use wasm_bindgen::prelude::*;

#[cfg(not(target_arch = "wasm32"))]
use ansi_backend::AnsiBackend;
#[cfg(not(target_arch = "wasm32"))]
use ratatui::Terminal;
#[cfg(not(target_arch = "wasm32"))]
use superseedr::presentation::{self, PresentationState};
#[cfg(all(test, target_arch = "wasm32"))]
use mocks::DemoCommandService;
#[cfg(all(test, target_arch = "wasm32"))]
use superseedr::web_integration::BrowserSession;
#[cfg(target_arch = "wasm32")]
pub use browser_demo::BrowserDemo;

/// Renders one deterministic ANSI frame using Superseedr's production TUI draw entrypoint.
#[wasm_bindgen(js_name = renderDemoFrame)]
pub fn render_demo_frame(cols: u16, rows: u16) -> String {
    render_demo_frame_inner(cols, rows)
}

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(target_arch = "wasm32")]
fn render_demo_frame_inner(cols: u16, rows: u16) -> String {
    let mut demo = BrowserDemo::new(cols, rows);
    demo.render_frame()
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

#[cfg(all(test, target_arch = "wasm32"))]
struct DemoHarness {
    session: BrowserSession,
    service: DemoCommandService,
}

#[cfg(all(test, target_arch = "wasm32"))]
impl DemoHarness {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            session: BrowserSession::from_fixture(cols, rows, milestone_one_fixture()),
            service: DemoCommandService::default(),
        }
    }

    fn fulfill_pending(&mut self) -> Vec<superseedr::web_integration::BrowserCommand> {
        self.service.fulfill_pending(&mut self.session)
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

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_contracts {
    use super::*;
    use superseedr::terminal_event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    };
    use superseedr::web_integration::{
        BrowserCommand, BrowserTorrentControlState,
    };
    use wasm_bindgen_test::wasm_bindgen_test;

    const FIXTURE_HASH_HEX: &str =
        "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";
    const MAGNET: &str =
        "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567";

    fn session() -> BrowserSession {
        BrowserSession::from_fixture(120, 40, milestone_one_fixture())
    }

    #[wasm_bindgen_test(async)]
    async fn retained_browser_terminal_refresh_and_resize_are_self_contained() {
        let mut demo = BrowserDemo::new(80, 24);

        let initial = demo.render_frame();
        assert!(initial.starts_with("\x1b[2J"));
        assert!(!demo.render_frame().starts_with("\x1b[2J"));

        demo.resize(96, 30).await;
        assert_eq!(demo.columns(), 96);
        assert_eq!(demo.rows(), 30);
        assert!(demo.force_refresh().starts_with("\x1b[2J"));

        assert!(demo.dispatch_key("p".to_owned(), 0, 0).await);
        demo.resize(96, 30).await;
        assert!(demo.selected_torrent_paused());
    }

    async fn key_and_flush(
        session: &mut BrowserSession,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) {
        session
            .dispatch_event(Event::Key(KeyEvent::new(code, modifiers)))
            .await;
        session.dispatch_event(Event::Resize(120, 40)).await;
    }

    #[wasm_bindgen_test(async)]
    async fn explicit_paste_emits_exact_add_command_and_drain_is_nonblocking() {
        let mut session = session();
        assert!(session.drain_commands().is_empty());

        session.dispatch_event(Event::Paste(MAGNET.to_string())).await;

        assert_eq!(
            session.drain_commands(),
            vec![BrowserCommand::AddMagnet {
                magnet_link: MAGNET.to_string(),
                download_path: None,
                container_name: None,
                validation_status: false,
            }]
        );
        assert!(session.drain_commands().is_empty());
    }

    #[wasm_bindgen_test(async)]
    async fn paste_burst_uses_the_production_translation_and_add_path() {
        let mut session = session();
        for character in MAGNET.chars() {
            session
                .dispatch_event(Event::Key(KeyEvent::new(
                    KeyCode::Char(character),
                    KeyModifiers::NONE,
                )))
                .await;
        }

        assert!(session.drain_commands().is_empty());
        session.dispatch_event(Event::Resize(100, 32)).await;

        assert_eq!(
            session.drain_commands(),
            vec![BrowserCommand::AddMagnet {
                magnet_link: MAGNET.to_string(),
                download_path: None,
                container_name: None,
                validation_status: false,
            }]
        );
        assert_eq!(session.screen_size(), (100, 32));
    }

    #[wasm_bindgen_test(async)]
    async fn pause_resume_preserves_state_and_fifo_command_order() {
        let mut session = session();

        key_and_flush(&mut session, KeyCode::Char('p'), KeyModifiers::NONE).await;
        assert_eq!(
            session.torrent_control_state_hex(FIXTURE_HASH_HEX),
            Some(BrowserTorrentControlState::Paused)
        );
        key_and_flush(&mut session, KeyCode::Char('p'), KeyModifiers::NONE).await;
        assert_eq!(
            session.torrent_control_state_hex(FIXTURE_HASH_HEX),
            Some(BrowserTorrentControlState::Running)
        );

        assert_eq!(
            session.drain_commands(),
            vec![
                BrowserCommand::Pause {
                    info_hash_hex: FIXTURE_HASH_HEX.to_string(),
                },
                BrowserCommand::Resume {
                    info_hash_hex: FIXTURE_HASH_HEX.to_string(),
                },
            ]
        );
    }

    #[wasm_bindgen_test(async)]
    async fn delete_cancel_and_confirmation_preserve_the_production_gate() {
        let mut session = session();

        key_and_flush(&mut session, KeyCode::Char('d'), KeyModifiers::NONE).await;
        assert_eq!(session.delete_confirmation(), Some((&[0x5a; 20][..], false)));
        assert!(session.drain_commands().is_empty());

        session
            .dispatch_event(Event::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
            .await;
        assert!(session.delete_confirmation().is_none());
        assert!(session.drain_commands().is_empty());

        key_and_flush(&mut session, KeyCode::Char('D'), KeyModifiers::NONE).await;
        assert_eq!(session.delete_confirmation(), Some((&[0x5a; 20][..], true)));
        assert!(session.drain_commands().is_empty());

        session
            .dispatch_event(Event::Key(KeyEvent::new(
                KeyCode::Char('Y'),
                KeyModifiers::NONE,
            )))
            .await;
        assert!(session.delete_confirmation().is_none());
        assert_eq!(
            session.torrent_control_state_hex(FIXTURE_HASH_HEX),
            Some(BrowserTorrentControlState::Deleting)
        );
        assert_eq!(session.torrent_delete_files_hex(FIXTURE_HASH_HEX), Some(true));
        assert_eq!(
            session.drain_commands(),
            vec![BrowserCommand::Delete {
                info_hash_hex: FIXTURE_HASH_HEX.to_string(),
                delete_files: true,
            }]
        );
        assert!(session.drain_commands().is_empty());
    }

    #[wasm_bindgen_test(async)]
    async fn stale_selection_never_targets_another_torrent() {
        let mut session = session();
        assert!(session.remove_torrent_hex(FIXTURE_HASH_HEX));

        key_and_flush(&mut session, KeyCode::Char('p'), KeyModifiers::NONE).await;
        key_and_flush(&mut session, KeyCode::Char('d'), KeyModifiers::NONE).await;

        assert!(session.delete_confirmation().is_none());
        assert!(session.drain_commands().is_empty());
    }

    #[wasm_bindgen_test(async)]
    async fn modifiers_press_repeat_release_and_resize_retain_terminal_semantics() {
        let mut session = session();

        session
            .dispatch_event(Event::Key(KeyEvent::new(
                KeyCode::Char('p'),
                KeyModifiers::CONTROL,
            )))
            .await;
        session
            .dispatch_event(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('p'),
                KeyModifiers::NONE,
                KeyEventKind::Repeat,
            )))
            .await;
        session
            .dispatch_event(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('p'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )))
            .await;
        session.dispatch_event(Event::Resize(88, 24)).await;

        assert_eq!(session.screen_size(), (88, 24));
        assert_eq!(
            session.torrent_control_state_hex(FIXTURE_HASH_HEX),
            Some(BrowserTorrentControlState::Running)
        );
        assert!(session.drain_commands().is_empty());

        key_and_flush(&mut session, KeyCode::Char('p'), KeyModifiers::NONE).await;
        assert_eq!(
            session.torrent_control_state_hex(FIXTURE_HASH_HEX),
            Some(BrowserTorrentControlState::Paused)
        );
    }

    #[wasm_bindgen_test(async)]
    async fn browser_owned_mock_service_fulfills_commands_in_memory() {
        let mut harness = DemoHarness::new(120, 40);
        let initial_count = harness.session.torrent_count();

        harness
            .session
            .dispatch_event(Event::Paste(MAGNET.to_string()))
            .await;
        assert_eq!(
            harness.fulfill_pending(),
            vec![BrowserCommand::AddMagnet {
                magnet_link: MAGNET.to_string(),
                download_path: None,
                container_name: None,
                validation_status: false,
            }]
        );
        assert_eq!(harness.session.torrent_count(), initial_count + 1);

        key_and_flush(
            &mut harness.session,
            KeyCode::Char('D'),
            KeyModifiers::NONE,
        )
        .await;
        harness
            .session
            .dispatch_event(Event::Key(KeyEvent::new(
                KeyCode::Char('Y'),
                KeyModifiers::NONE,
            )))
            .await;
        assert_eq!(
            harness.fulfill_pending(),
            vec![BrowserCommand::Delete {
                info_hash_hex: FIXTURE_HASH_HEX.to_string(),
                delete_files: true,
            }]
        );
        assert_eq!(harness.session.torrent_count(), initial_count);
    }

    #[wasm_bindgen_test]
    fn wasm_export_renders_from_the_webapp_session() {
        let frame = render_demo_frame_inner(120, 40);
        assert!(frame.starts_with("\x1b[2J"));
        assert!(frame.contains("Nebula Field Sample"));
    }
}
