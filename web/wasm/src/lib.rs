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
#[cfg(target_arch = "wasm32")]
pub use browser_demo::BrowserDemo;
#[cfg(all(test, target_arch = "wasm32"))]
use mocks::DemoCommandService;
#[cfg(not(target_arch = "wasm32"))]
use ratatui::Terminal;
#[cfg(not(target_arch = "wasm32"))]
use superseedr::presentation::{self, PresentationState};
#[cfg(all(test, target_arch = "wasm32"))]
use superseedr::web_integration::BrowserSession;

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

    fn advance(&mut self, delta_seconds: f64) {
        self.service.advance(&mut self.session, delta_seconds);
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
        assert!(
            frame.contains("Catppuccin Mocha"),
            "frame did not use Superseedr's native default theme"
        );
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_contracts {
    use super::*;
    use crate::ansi_backend::AnsiBackend;
    use ratatui::Terminal;
    use superseedr::terminal_event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use superseedr::web_integration::{BrowserCommand, BrowserScreen, BrowserTorrentControlState};
    use wasm_bindgen_test::wasm_bindgen_test;

    const FIXTURE_HASH_HEX: &str = "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";
    const MAGNET_HASH_HEX: &str = "0123456789abcdef0123456789abcdef01234567";
    const MAGNET: &str = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567";

    fn session() -> BrowserSession {
        BrowserSession::from_fixture(120, 40, milestone_one_fixture())
    }

    #[wasm_bindgen_test]
    fn browser_session_uses_the_native_default_theme() {
        assert_eq!(session().theme_name().to_string(), "Catppuccin Mocha");
    }

    fn rich_session() -> BrowserSession {
        let mut session = session();
        mocks::install_simulated_state(&mut session);
        session
    }

    fn render_plain(session: &BrowserSession) -> String {
        let mut terminal = Terminal::new(AnsiBackend::new(120, 40)).expect("terminal");
        terminal.clear().expect("clear");
        terminal.draw(|frame| session.draw(frame)).expect("draw");
        strip_ansi(&terminal.backend_mut().take_output())
    }

    fn strip_ansi(value: &str) -> String {
        let mut plain = String::with_capacity(value.len());
        let mut chars = value.chars().peekable();
        while let Some(character) = chars.next() {
            if character != '\u{1b}' {
                plain.push(character);
                continue;
            }
            if chars.next_if_eq(&'[').is_some() {
                for sequence in chars.by_ref() {
                    if ('@'..='~').contains(&sequence) {
                        break;
                    }
                }
            }
        }
        plain
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

    async fn key_and_flush(session: &mut BrowserSession, code: KeyCode, modifiers: KeyModifiers) {
        session
            .dispatch_event(Event::Key(KeyEvent::new(code, modifiers)))
            .await;
        session.dispatch_event(Event::Resize(120, 40)).await;
    }

    #[wasm_bindgen_test(async)]
    async fn explicit_paste_emits_exact_add_command_and_drain_is_nonblocking() {
        let mut session = session();
        assert!(session.drain_commands().is_empty());

        session
            .dispatch_event(Event::Paste(MAGNET.to_string()))
            .await;

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
        assert_eq!(
            session.delete_confirmation(),
            Some((&[0x5a; 20][..], false))
        );
        assert!(session.drain_commands().is_empty());

        session
            .dispatch_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
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
        assert_eq!(
            session.torrent_delete_files_hex(FIXTURE_HASH_HEX),
            Some(true)
        );
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

        key_and_flush(&mut harness.session, KeyCode::Char('D'), KeyModifiers::NONE).await;
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

    #[wasm_bindgen_test(async)]
    async fn dynamic_session_crosses_metadata_peers_stalls_checking_and_seeding() {
        let mut harness = DemoHarness::new(120, 40);
        harness
            .session
            .dispatch_event(Event::Paste(MAGNET.to_string()))
            .await;
        harness.fulfill_pending();

        assert_eq!(
            harness.service.phase_hex(MAGNET_HASH_HEX),
            Some(mocks::MockTorrentPhase::FetchingMetadata)
        );
        let metadata = harness
            .session
            .torrent_snapshot_hex(MAGNET_HASH_HEX)
            .expect("dynamic torrent");
        assert!(!metadata.data_available);
        assert_eq!(metadata.total_size, 0);
        assert_eq!(metadata.connected_peers, 0);

        let mut saw_metadata = false;
        let mut saw_peers = false;
        let mut saw_downloading = false;
        let mut saw_peer_stall = false;
        let mut saw_disk_stall = false;
        let mut saw_checking = false;
        let mut saw_seeding = false;
        let mut previous_bytes = 0;
        for _ in 0..160 {
            harness.advance(0.1);
            let phase = harness
                .service
                .phase_hex(MAGNET_HASH_HEX)
                .expect("phase remains present");
            let snapshot = harness
                .session
                .torrent_snapshot_hex(MAGNET_HASH_HEX)
                .expect("snapshot remains present");
            assert!(snapshot.bytes_written >= previous_bytes);
            assert!(snapshot.bytes_written <= snapshot.total_size || snapshot.total_size == 0);
            previous_bytes = snapshot.bytes_written;
            match phase {
                mocks::MockTorrentPhase::FetchingMetadata => saw_metadata = true,
                mocks::MockTorrentPhase::DiscoveringPeers => {
                    saw_peers |= snapshot.connected_peers > 0;
                    assert!(snapshot.data_available);
                    assert!(snapshot.total_size > 0);
                }
                mocks::MockTorrentPhase::Downloading => {
                    saw_downloading = true;
                    saw_peer_stall |=
                        harness.service.stall_hex(MAGNET_HASH_HEX) == Some(mocks::MockStall::Peer);
                    saw_disk_stall |=
                        harness.service.stall_hex(MAGNET_HASH_HEX) == Some(mocks::MockStall::Disk);
                }
                mocks::MockTorrentPhase::CheckingPieces => {
                    saw_checking = true;
                    assert_eq!(snapshot.bytes_written, snapshot.total_size);
                    assert_eq!(snapshot.download_speed_bps, 0);
                }
                mocks::MockTorrentPhase::Seeding => {
                    saw_seeding = true;
                    assert!(snapshot.is_complete);
                    assert_eq!(snapshot.pieces_completed, snapshot.pieces_total);
                    assert!(snapshot.upload_speed_bps > 0);
                    break;
                }
            }
        }

        assert!(saw_metadata);
        assert!(saw_peers);
        assert!(saw_downloading);
        assert!(saw_peer_stall);
        assert!(saw_disk_stall);
        assert!(saw_checking);
        assert!(saw_seeding);
        let seeded = harness
            .session
            .torrent_snapshot_hex(MAGNET_HASH_HEX)
            .expect("seeded snapshot");
        assert!(seeded.name.starts_with("Orbit Archive"));
        assert!(seeded.session_downloaded >= seeded.total_size);
        assert!(seeded.session_uploaded > 0);
        assert!(seeded.download_history_len > 10);
        assert_eq!(seeded.download_history_len, seeded.upload_history_len);
        assert!(harness.session.select_torrent_hex(MAGNET_HASH_HEX));
        let visualization = harness.session.visualization_snapshot();
        assert!(visualization.effects_phase_time > 0.0);
        assert!(visualization.total_upload_bps > 0);
        assert!(visualization.disk_read_bps > 0);
        assert!(visualization.file_download_phase > 0.0);
        assert!(visualization.file_upload_phase > 0.0);
        assert_ne!(visualization.disk_health_phase, 0.0);
        assert!(visualization.tracked_peers > 0);
        assert!(
            visualization.network_history_samples >= 120,
            "network history only contains {} samples",
            visualization.network_history_samples
        );
        assert!(
            visualization.activity_history_samples >= 120,
            "activity history only contains {} samples",
            visualization.activity_history_samples
        );
        assert!(visualization.peer_connected_events > 0);
        assert!(visualization.peer_connected_events <= 32);
        assert!(visualization.peer_discovered_events > 0);
        assert!(visualization.peer_disconnected_events > 0);
        assert!(visualization.recent_file_activity > 0);
        assert!(visualization.swarm_availability_samples > 0);
        assert!(visualization.dht_wave_initialized);
    }

    #[wasm_bindgen_test(async)]
    async fn equal_elapsed_time_is_independent_of_caller_partitioning() {
        let mut tenth_second_steps = DemoHarness::new(120, 40);
        let mut frame_steps = DemoHarness::new(120, 40);
        for harness in [&mut tenth_second_steps, &mut frame_steps] {
            harness
                .session
                .dispatch_event(Event::Paste(MAGNET.to_string()))
                .await;
            harness.fulfill_pending();
            harness.advance(1.5);
            assert_eq!(
                harness.service.phase_hex(MAGNET_HASH_HEX),
                Some(mocks::MockTorrentPhase::Downloading)
            );
        }

        for _ in 0..10 {
            tenth_second_steps.advance(0.1);
        }
        for _ in 0..60 {
            frame_steps.advance(1.0 / 60.0);
        }

        assert_eq!(
            tenth_second_steps.service.phase_hex(MAGNET_HASH_HEX),
            frame_steps.service.phase_hex(MAGNET_HASH_HEX)
        );
        assert_eq!(
            tenth_second_steps.service.stall_hex(MAGNET_HASH_HEX),
            frame_steps.service.stall_hex(MAGNET_HASH_HEX)
        );
        assert_eq!(
            tenth_second_steps
                .session
                .torrent_snapshot_hex(MAGNET_HASH_HEX),
            frame_steps.session.torrent_snapshot_hex(MAGNET_HASH_HEX)
        );
    }

    #[wasm_bindgen_test(async)]
    async fn pause_resume_freezes_and_continues_the_dynamic_session() {
        let mut harness = DemoHarness::new(120, 40);
        harness
            .session
            .dispatch_event(Event::Paste(MAGNET.to_string()))
            .await;
        harness.fulfill_pending();
        harness.advance(1.8);
        assert_eq!(
            harness.service.phase_hex(MAGNET_HASH_HEX),
            Some(mocks::MockTorrentPhase::Downloading)
        );
        assert!(harness.session.select_torrent_hex(MAGNET_HASH_HEX));

        key_and_flush(&mut harness.session, KeyCode::Char('p'), KeyModifiers::NONE).await;
        harness.fulfill_pending();
        let paused = harness
            .session
            .torrent_snapshot_hex(MAGNET_HASH_HEX)
            .expect("paused snapshot");
        harness.advance(1.0);
        let still_paused = harness
            .session
            .torrent_snapshot_hex(MAGNET_HASH_HEX)
            .expect("paused snapshot after time");
        assert_eq!(
            still_paused.control_state,
            BrowserTorrentControlState::Paused
        );
        assert_eq!(still_paused.bytes_written, paused.bytes_written);
        assert_eq!(still_paused.download_speed_bps, 0);

        key_and_flush(&mut harness.session, KeyCode::Char('p'), KeyModifiers::NONE).await;
        harness.fulfill_pending();
        harness.advance(0.5);
        let resumed = harness
            .session
            .torrent_snapshot_hex(MAGNET_HASH_HEX)
            .expect("resumed snapshot");
        assert_eq!(resumed.control_state, BrowserTorrentControlState::Running);
        assert!(resumed.bytes_written > paused.bytes_written);
    }

    #[wasm_bindgen_test(async)]
    async fn confirmed_delete_removes_the_dynamic_session_and_metrics() {
        let mut harness = DemoHarness::new(120, 40);
        harness
            .session
            .dispatch_event(Event::Paste(MAGNET.to_string()))
            .await;
        harness.fulfill_pending();
        assert!(harness.session.select_torrent_hex(MAGNET_HASH_HEX));

        key_and_flush(&mut harness.session, KeyCode::Char('d'), KeyModifiers::NONE).await;
        harness
            .session
            .dispatch_event(Event::Key(KeyEvent::new(
                KeyCode::Char('Y'),
                KeyModifiers::NONE,
            )))
            .await;
        harness.fulfill_pending();

        assert_eq!(harness.service.phase_hex(MAGNET_HASH_HEX), None);
        assert!(harness
            .session
            .torrent_snapshot_hex(MAGNET_HASH_HEX)
            .is_none());
    }

    #[wasm_bindgen_test(async)]
    async fn config_interaction_uses_the_production_reducer() {
        let mut session = rich_session();
        key_and_flush(&mut session, KeyCode::Char('c'), KeyModifiers::NONE).await;
        assert_eq!(session.screen(), BrowserScreen::Config);

        key_and_flush(&mut session, KeyCode::Char('x'), KeyModifiers::NONE).await;
        assert!(session.anonymize_names());

        key_and_flush(&mut session, KeyCode::Char('q'), KeyModifiers::NONE).await;
        assert_eq!(session.screen(), BrowserScreen::Normal);
    }

    #[wasm_bindgen_test(async)]
    async fn config_path_browser_transitions_and_applies_the_virtual_selection() {
        let mut harness = DemoHarness::new(120, 40);
        mocks::install_simulated_state(&mut harness.session);
        key_and_flush(&mut harness.session, KeyCode::Char('c'), KeyModifiers::NONE).await;
        for _ in 0..2 {
            key_and_flush(&mut harness.session, KeyCode::Down, KeyModifiers::NONE).await;
        }

        key_and_flush(&mut harness.session, KeyCode::Char(' '), KeyModifiers::NONE).await;

        let commands = harness.fulfill_pending();
        assert!(matches!(
            commands.as_slice(),
            [BrowserCommand::FetchFileTree { .. }]
        ));
        assert_eq!(harness.session.screen(), BrowserScreen::FileBrowser);
        let selected_path = harness.session.file_browser_current_path().clone();
        assert_eq!(harness.session.default_download_folder(), None);

        key_and_flush(
            &mut harness.session,
            KeyCode::Char('Y'),
            KeyModifiers::SHIFT,
        )
        .await;

        assert_eq!(harness.session.screen(), BrowserScreen::Config);
        assert_eq!(
            harness.session.default_download_folder(),
            Some(&selected_path)
        );
        assert!(harness.fulfill_pending().is_empty());
    }

    #[wasm_bindgen_test(async)]
    async fn file_browser_search_uses_the_production_reducers_and_mock_tree() {
        let mut session = rich_session();
        key_and_flush(&mut session, KeyCode::Char('a'), KeyModifiers::NONE).await;
        assert_eq!(session.screen(), BrowserScreen::FileBrowser);

        key_and_flush(&mut session, KeyCode::Char('/'), KeyModifiers::NONE).await;
        for character in "incoming".chars() {
            key_and_flush(&mut session, KeyCode::Char(character), KeyModifiers::NONE).await;
        }
        key_and_flush(&mut session, KeyCode::Enter, KeyModifiers::NONE).await;

        let rendered = render_plain(&session);
        assert!(rendered.contains("incoming-demo.torrent"));
        assert!(!rendered.contains("queued-example.torrent"));
    }

    #[wasm_bindgen_test(async)]
    async fn file_browser_parent_fetch_uses_the_shared_handler_without_a_runtime() {
        let mut harness = DemoHarness::new(120, 40);
        mocks::install_simulated_state(&mut harness.session);
        key_and_flush(&mut harness.session, KeyCode::Char('a'), KeyModifiers::NONE).await;
        assert_eq!(harness.session.screen(), BrowserScreen::FileBrowser);

        key_and_flush(&mut harness.session, KeyCode::Left, KeyModifiers::NONE).await;

        let commands = harness.fulfill_pending();
        assert!(matches!(
            commands.as_slice(),
            [BrowserCommand::FetchFileTree {
                path,
                highlight_path: Some(highlight_path),
                ..
            }] if path == std::path::Path::new("/")
                && highlight_path == std::path::Path::new("/simulated")
        ));
        assert_eq!(harness.session.screen(), BrowserScreen::FileBrowser);
    }

    #[wasm_bindgen_test(async)]
    async fn mocked_torrent_file_confirm_adds_through_the_shared_handler() {
        let mut harness = DemoHarness::new(120, 40);
        mocks::install_simulated_state(&mut harness.session);
        let initial_count = harness.session.torrent_count();
        key_and_flush(&mut harness.session, KeyCode::Char('a'), KeyModifiers::NONE).await;

        key_and_flush(
            &mut harness.session,
            KeyCode::Char('Y'),
            KeyModifiers::SHIFT,
        )
        .await;

        assert!(matches!(
            harness.fulfill_pending().as_slice(),
            [BrowserCommand::AddTorrentFromFile { path }]
                if path == std::path::Path::new("/simulated/incoming-demo.torrent")
        ));
        assert_eq!(harness.session.screen(), BrowserScreen::Normal);
        assert_eq!(harness.session.torrent_count(), initial_count + 1);
        let added_hash = harness
            .service
            .last_added_hash()
            .expect("file add session")
            .to_string();
        assert_eq!(
            harness.service.phase_hex(&added_hash),
            Some(mocks::MockTorrentPhase::DiscoveringPeers)
        );
        harness.advance(1.0);
        assert_eq!(
            harness.service.phase_hex(&added_hash),
            Some(mocks::MockTorrentPhase::Downloading)
        );
    }

    #[wasm_bindgen_test(async)]
    async fn existing_torrent_configuration_is_fulfilled_by_the_mock_service() {
        let mut harness = DemoHarness::new(120, 40);
        mocks::install_simulated_state(&mut harness.session);
        key_and_flush(
            &mut harness.session,
            KeyCode::Char('M'),
            KeyModifiers::SHIFT,
        )
        .await;
        let selected_hash = harness
            .session
            .torrent_management_cursor_hash_hex()
            .expect("management cursor");
        key_and_flush(&mut harness.session, KeyCode::Char('f'), KeyModifiers::NONE).await;
        assert_eq!(harness.session.screen(), BrowserScreen::FileBrowser);
        key_and_flush(&mut harness.session, KeyCode::Char(' '), KeyModifiers::NONE).await;

        key_and_flush(
            &mut harness.session,
            KeyCode::Char('Y'),
            KeyModifiers::SHIFT,
        )
        .await;

        let commands = harness.fulfill_pending();
        assert!(matches!(
            commands.as_slice(),
            [BrowserCommand::SetTorrentConfig {
                info_hash_hex,
                file_priorities,
                ..
            }] if info_hash_hex == &selected_hash && !file_priorities.is_empty()
        ));
        assert_eq!(harness.session.screen(), BrowserScreen::TorrentManagement);
        assert!(harness
            .session
            .torrent_file_priority_hex(&selected_hash, 0)
            .is_some());
    }

    #[wasm_bindgen_test(async)]
    async fn rss_search_and_navigation_use_the_production_reducer() {
        let mut session = rich_session();
        key_and_flush(&mut session, KeyCode::Char('r'), KeyModifiers::NONE).await;
        assert_eq!(session.screen(), BrowserScreen::Rss);

        key_and_flush(&mut session, KeyCode::Char('/'), KeyModifiers::NONE).await;
        session
            .dispatch_event(Event::Paste("Signal Garden".to_string()))
            .await;
        key_and_flush(&mut session, KeyCode::Enter, KeyModifiers::NONE).await;
        assert!(render_plain(&session).contains("Signal Garden Dispatch"));

        for _ in "Signal Garden".chars() {
            key_and_flush(&mut session, KeyCode::Backspace, KeyModifiers::NONE).await;
        }
        key_and_flush(&mut session, KeyCode::Enter, KeyModifiers::NONE).await;

        key_and_flush(&mut session, KeyCode::Char('q'), KeyModifiers::NONE).await;
        assert_eq!(session.screen(), BrowserScreen::Normal);
    }

    #[wasm_bindgen_test(async)]
    async fn journal_selection_uses_the_production_reducer_and_fixture_history() {
        let mut session = rich_session();
        key_and_flush(&mut session, KeyCode::Char('J'), KeyModifiers::SHIFT).await;
        assert_eq!(session.screen(), BrowserScreen::Journal);

        key_and_flush(&mut session, KeyCode::Down, KeyModifiers::NONE).await;
        assert!(render_plain(&session).contains("Simulated metadata resolved"));

        key_and_flush(&mut session, KeyCode::Char('q'), KeyModifiers::NONE).await;
        assert_eq!(session.screen(), BrowserScreen::Normal);
    }

    #[wasm_bindgen_test(async)]
    async fn peer_management_details_use_the_production_reducer_and_peer_model() {
        let mut session = rich_session();
        key_and_flush(&mut session, KeyCode::Char('P'), KeyModifiers::SHIFT).await;
        assert_eq!(session.screen(), BrowserScreen::PeerManagement);

        key_and_flush(&mut session, KeyCode::Enter, KeyModifiers::NONE).await;
        let rendered = render_plain(&session);
        assert!(rendered.contains("simulated-peer-"));
        assert!(rendered.contains("Peer Details"));

        key_and_flush(&mut session, KeyCode::Char('q'), KeyModifiers::NONE).await;
        assert_eq!(session.screen(), BrowserScreen::Normal);
    }

    #[wasm_bindgen_test(async)]
    async fn torrent_management_review_submits_through_the_production_reducer() {
        let mut session = rich_session();
        key_and_flush(&mut session, KeyCode::Char('M'), KeyModifiers::SHIFT).await;
        assert_eq!(session.screen(), BrowserScreen::TorrentManagement);
        let selected_hash = session
            .torrent_management_cursor_hash_hex()
            .expect("management cursor");

        key_and_flush(&mut session, KeyCode::Char(' '), KeyModifiers::NONE).await;
        key_and_flush(&mut session, KeyCode::Char('p'), KeyModifiers::NONE).await;
        assert!(session.drain_commands().is_empty());
        key_and_flush(&mut session, KeyCode::Char('Y'), KeyModifiers::SHIFT).await;
        assert!(render_plain(&session).contains("Review"));
        key_and_flush(&mut session, KeyCode::Enter, KeyModifiers::NONE).await;

        assert_eq!(
            session.drain_commands(),
            vec![BrowserCommand::Pause {
                info_hash_hex: selected_hash,
            }]
        );
        key_and_flush(&mut session, KeyCode::Char('q'), KeyModifiers::NONE).await;
        assert_eq!(session.screen(), BrowserScreen::Normal);
    }

    #[wasm_bindgen_test]
    fn lifecycle_fixture_exercises_every_simulated_torrent_stage_in_the_production_view() {
        let session = rich_session();
        let rendered = render_plain(&session);
        for name in [
            "Nebula Field Sample",
            "Orbit Archive 02",
            "Lattice Study",
            "Prism Notes",
            "Signal Garden",
            "Vector Almanac",
        ] {
            assert!(rendered.contains(name), "normal screen omitted {name}");
        }
    }

    #[wasm_bindgen_test]
    fn wasm_export_renders_from_the_webapp_session() {
        let frame = render_demo_frame_inner(120, 40);
        assert!(frame.starts_with("\x1b[2J"));
        assert!(frame.contains("Nebula Field Sample"));
    }
}
