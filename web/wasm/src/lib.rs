// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

mod ansi_backend;
#[cfg(target_arch = "wasm32")]
mod browser_demo;
#[cfg(target_arch = "wasm32")]
mod mocks;
#[cfg(target_arch = "wasm32")]
mod scenarios;

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

    fn for_scenario(cols: u16, rows: u16, scenario: scenarios::ScenarioId) -> Self {
        let mut session = BrowserSession::from_fixture(cols, rows, milestone_one_fixture());
        let mut service = DemoCommandService::for_scenario(scenario);
        service.install_initial_state(&mut session);
        Self { session, service }
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
        rich_session_at(120, 40)
    }

    fn rich_session_at(columns: u16, rows: u16) -> BrowserSession {
        let mut session = BrowserSession::from_fixture(columns, rows, milestone_one_fixture());
        mocks::install_simulated_state(&mut session);
        session
    }

    fn render_plain(session: &BrowserSession) -> String {
        render_plain_at(session, 120, 40)
    }

    fn render_plain_at(session: &BrowserSession, columns: u16, rows: u16) -> String {
        let mut terminal = Terminal::new(AnsiBackend::new(columns, rows)).expect("terminal");
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

    #[derive(Debug, PartialEq, Eq)]
    struct ScenarioTorrentObservable {
        snapshot: superseedr::web_integration::BrowserTorrentSnapshot,
        phase: Option<mocks::MockTorrentPhase>,
        stall: Option<mocks::MockStall>,
        disk_state: Option<mocks::MockDiskState>,
        missing_pieces: Option<usize>,
        peer_discovered_events: u64,
        peer_connected_events: u64,
        peer_disconnected_events: u64,
        swarm_availability_samples: usize,
    }

    fn scenario_observables(harness: &mut DemoHarness) -> Vec<ScenarioTorrentObservable> {
        harness
            .service
            .torrent_hashes()
            .into_iter()
            .map(|hash| {
                assert!(harness.session.select_torrent_hex(&hash));
                let visualization = harness.session.visualization_snapshot();
                ScenarioTorrentObservable {
                    snapshot: harness
                        .session
                        .torrent_snapshot_hex(&hash)
                        .expect("scenario torrent snapshot"),
                    phase: harness.service.phase_hex(&hash),
                    stall: harness.service.stall_hex(&hash),
                    disk_state: harness.service.disk_state_hex(&hash),
                    missing_pieces: harness.service.missing_pieces_hex(&hash),
                    peer_discovered_events: visualization.peer_discovered_events,
                    peer_connected_events: visualization.peer_connected_events,
                    peer_disconnected_events: visualization.peer_disconnected_events,
                    swarm_availability_samples: visualization.swarm_availability_samples,
                }
            })
            .collect()
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
        let mut checking_download_rates = Vec::new();
        let mut checking_upload_rates = Vec::new();
        let mut seeding_download_rate = None;
        for _ in 0..480 {
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
                    assert!(snapshot.total_size >= 96 * 1024 * 1024);
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
                    let (published_download, raw_download, published_upload, raw_upload) = harness
                        .service
                        .rate_state_hex(MAGNET_HASH_HEX)
                        .expect("checking rate state");
                    assert_eq!(raw_download, 0);
                    assert_eq!(raw_upload, 0);
                    assert_eq!(snapshot.download_speed_bps, published_download);
                    assert_eq!(snapshot.upload_speed_bps, published_upload);
                    checking_download_rates.push(published_download);
                    checking_upload_rates.push(published_upload);
                }
                mocks::MockTorrentPhase::Seeding => {
                    saw_seeding = true;
                    assert!(snapshot.is_complete);
                    assert_eq!(snapshot.pieces_completed, snapshot.pieces_total);
                    let (_, raw_download, _, _) = harness
                        .service
                        .rate_state_hex(MAGNET_HASH_HEX)
                        .expect("seeding rate state");
                    assert_eq!(raw_download, 0);
                    seeding_download_rate = Some(snapshot.download_speed_bps);
                    if snapshot.upload_speed_bps > 0 {
                        break;
                    }
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
        assert!(checking_download_rates.len() >= 2);
        assert!(checking_upload_rates.len() >= 2);
        assert!(checking_download_rates[0] > *checking_download_rates.last().unwrap());
        assert!(checking_upload_rates[0] > *checking_upload_rates.last().unwrap());
        assert!(*checking_download_rates.last().unwrap() > 0);
        assert!(*checking_upload_rates.last().unwrap() > 0);
        assert!(seeding_download_rate
            .is_some_and(|rate| { rate > 0 && rate < checking_download_rates[0] }));
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
        for _ in 0..20 {
            let visualization = harness.session.visualization_snapshot();
            if visualization.total_upload_bps > 0 && visualization.disk_read_bps > 0 {
                break;
            }
            harness.advance(0.1);
        }
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
        assert!(visualization.peer_connected_events > 2);
        assert!(visualization.peer_discovered_events > 5);
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

    #[wasm_bindgen_test]
    fn browser_metrics_preserve_production_units_and_event_semantics() {
        let mut harness = DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::Downloading);
        assert!(harness.session.select_torrent_hex(FIXTURE_HASH_HEX));
        let before = harness
            .session
            .torrent_snapshot_hex(FIXTURE_HASH_HEX)
            .expect("downloading fixture");
        let blocks_before = harness.session.visualization_snapshot();

        harness.advance(1.0);
        assert!(harness.session.select_torrent_hex(FIXTURE_HASH_HEX));

        let after = harness
            .session
            .torrent_snapshot_hex(FIXTURE_HASH_HEX)
            .expect("advanced downloading fixture");
        let visualization = harness.session.visualization_snapshot();
        let transferred_bytes = after
            .session_downloaded
            .saturating_sub(before.session_downloaded);
        let received_blocks = visualization
            .blocks_received_events
            .saturating_sub(blocks_before.blocks_received_events);

        assert!(transferred_bytes > 0);
        assert_eq!(received_blocks, transferred_bytes / 16_384);
        assert!(visualization.read_iops > 1);
        assert!(visualization.write_iops > 1);
        assert!(visualization.disk_read_latency_micros > 0);
        assert!(visualization.disk_write_latency_micros > 0);
        assert!(visualization.recv_to_write_latency_micros > 0);
        assert!(after.bytes_downloaded_this_tick > 0);
        assert!(after.eta > std::time::Duration::ZERO);
        assert!(after.eta < std::time::Duration::MAX);
        assert!(after.next_announce_in > std::time::Duration::ZERO);
        assert!(after.next_announce_in <= std::time::Duration::from_secs(30 * 60));
        assert_eq!(after.connected_peers, after.tcp_peers + after.utp_peers);
        assert!(after.tcp_peers > 0);
        assert!(after.utp_peers > 0);
        let beneficial_peers = after.beneficial_tcp_peers + after.beneficial_utp_peers;
        assert!(beneficial_peers > 0);
        assert!(beneficial_peers <= after.connected_peers);
        assert!(visualization.recent_file_download_activity > 0);
        assert!(visualization.recent_file_upload_activity > 0);
    }

    #[wasm_bindgen_test]
    fn browser_downloads_share_a_variable_three_hundred_megabit_link() {
        let mut harness = DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::Downloading);
        assert!(harness.session.select_torrent_hex(FIXTURE_HASH_HEX));
        let initial = harness
            .session
            .torrent_snapshot_hex(FIXTURE_HASH_HEX)
            .expect("downloading fixture");
        assert_eq!(initial.total_size, 3 * 1024 * 1024 * 1024 / 2);

        let mut positive_selected_rates = Vec::new();
        let mut aggregate_rates = std::collections::BTreeSet::new();
        for _ in 0..900 {
            harness.advance(1.0 / 60.0);
            let (published, raw, _, _) = harness
                .service
                .rate_state_hex(FIXTURE_HASH_HEX)
                .expect("download rate state");
            let total = harness.session.visualization_snapshot().total_download_bps;
            assert!(raw <= mocks::MAX_SIMULATED_LINK_BPS);
            assert!(published <= mocks::MAX_SIMULATED_LINK_BPS);
            assert!(total <= mocks::MAX_SIMULATED_LINK_BPS);
            if raw > 0 {
                positive_selected_rates.push(raw);
            }
            aggregate_rates.insert(total);
        }

        let minimum = positive_selected_rates
            .iter()
            .copied()
            .min()
            .expect("active rate");
        let maximum = positive_selected_rates
            .iter()
            .copied()
            .max()
            .expect("active rate");
        assert!(maximum > minimum.saturating_mul(3));
        assert!(aggregate_rates.iter().copied().max().unwrap_or_default() >= 240_000_000);
        assert!(aggregate_rates.len() > 100);
    }

    #[wasm_bindgen_test]
    fn mixed_catalog_downloads_progress_concurrently() {
        let mut harness = DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::Mixed);
        let hashes = harness.service.torrent_hashes();
        let initial_bytes = hashes
            .iter()
            .filter_map(|hash| {
                harness
                    .session
                    .torrent_snapshot_hex(hash)
                    .map(|snapshot| (hash.clone(), snapshot.bytes_written))
            })
            .collect::<std::collections::HashMap<_, _>>();

        harness.advance(3.0);

        let progressed = hashes
            .iter()
            .filter(|hash| {
                harness
                    .session
                    .torrent_snapshot_hex(hash)
                    .is_some_and(|snapshot| {
                        snapshot.bytes_written
                            > initial_bytes.get(*hash).copied().unwrap_or_default()
                    })
            })
            .count();
        assert!(
            progressed >= 7,
            "expected concurrent mixed downloads, but only {progressed} torrents advanced"
        );
    }

    #[wasm_bindgen_test]
    fn mixed_catalog_keeps_uneven_torrent_shares_inside_the_shared_link() {
        let mut harness = DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::Mixed);
        let hashes = harness.service.torrent_hashes();
        let mut maximum_download_ratio = 0_u64;
        let mut maximum_upload_ratio = 0_u64;
        let mut saturated_combined_samples = 0_usize;
        let mut download_priority_samples = 0_usize;

        for _ in 0..600 {
            harness.advance(1.0 / 60.0);
            let mut download_rates = Vec::new();
            let mut upload_rates = Vec::new();
            for hash in &hashes {
                let (_, raw_download, _, raw_upload) = harness
                    .service
                    .rate_state_hex(hash)
                    .expect("mixed fixture rate state");
                if raw_download > 0 {
                    download_rates.push(raw_download);
                }
                if raw_upload > 0 {
                    upload_rates.push(raw_upload);
                }
            }

            if let (Some(minimum), Some(maximum)) = (
                download_rates.iter().copied().min(),
                download_rates.iter().copied().max(),
            ) {
                maximum_download_ratio = maximum_download_ratio.max(maximum / minimum.max(1));
            }
            if let (Some(minimum), Some(maximum)) = (
                upload_rates.iter().copied().min(),
                upload_rates.iter().copied().max(),
            ) {
                maximum_upload_ratio = maximum_upload_ratio.max(maximum / minimum.max(1));
            }

            let totals = harness.session.visualization_snapshot();
            assert!(totals.total_download_bps <= mocks::MAX_SIMULATED_LINK_BPS);
            assert!(totals.total_upload_bps <= mocks::MAX_SIMULATED_LINK_BPS);
            let combined = totals
                .total_download_bps
                .saturating_add(totals.total_upload_bps);
            assert!(combined <= mocks::MAX_SIMULATED_LINK_BPS);
            saturated_combined_samples += usize::from(combined >= 240_000_000);
            download_priority_samples +=
                usize::from(totals.total_download_bps > totals.total_upload_bps);
        }

        assert!(
            maximum_download_ratio >= 8,
            "download shares were only {maximum_download_ratio}x apart"
        );
        assert!(
            maximum_upload_ratio >= 8,
            "upload shares were only {maximum_upload_ratio}x apart"
        );
        assert!(saturated_combined_samples > 300);
        assert!(download_priority_samples > 300);
    }

    #[wasm_bindgen_test]
    fn completed_torrents_use_a_deterministic_fifteen_percent_upload_duty_cycle() {
        let mut harness = DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::Mixed);
        let hashes = harness.service.torrent_hashes();
        let mut seeding_samples = 0_u64;
        let mut active_upload_samples = 0_u64;

        for _ in 0..1_200 {
            harness.advance(1.0 / 60.0);
            for hash in &hashes {
                if harness.service.phase_hex(hash) != Some(mocks::MockTorrentPhase::Seeding) {
                    continue;
                }
                seeding_samples += 1;
                let (_, _, _, raw_upload) = harness
                    .service
                    .rate_state_hex(hash)
                    .expect("seeding fixture rate state");
                active_upload_samples += u64::from(raw_upload > 0);
            }
        }

        assert!(seeding_samples > 0);
        let active_percent = active_upload_samples * 100 / seeding_samples;
        assert!(
            (8..=22).contains(&active_percent),
            "active seeding duty cycle was {active_percent}%"
        );
    }

    #[wasm_bindgen_test]
    fn peer_stream_spreads_bounded_random_bursts_across_telemetry_samples() {
        let mut harness = DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::Mixed);
        let mut previous = harness.session.visualization_snapshot();
        let mut discovery_deltas = std::collections::BTreeSet::new();
        let mut active_samples = 0;

        for _ in 0..20 {
            harness.advance(1.0);
            let current = harness.session.visualization_snapshot();
            let delta = current
                .peer_discovered_events
                .saturating_sub(previous.peer_discovered_events);
            discovery_deltas.insert(delta);
            active_samples += usize::from(delta > 0);
            previous = current;
        }

        assert!(
            previous.peer_discovered_events > 10,
            "discovered {} peers",
            previous.peer_discovered_events
        );
        assert!(
            previous.peer_discovered_events < 100,
            "discovered {} peers",
            previous.peer_discovered_events
        );
        assert!(
            previous.peer_connected_events > 3,
            "connected {} peers",
            previous.peer_connected_events
        );
        assert!(
            previous.peer_disconnected_events > 1,
            "disconnected {} peers",
            previous.peer_disconnected_events
        );
        assert!(previous.dht_peers_found >= 2_000);
        assert!(active_samples >= 10);
        assert!(discovery_deltas.len() >= 3);
        let minimum = discovery_deltas.iter().copied().min().unwrap_or_default();
        let maximum = discovery_deltas.iter().copied().max().unwrap_or_default();
        assert!(maximum > minimum);
        assert!(maximum <= 6);
    }

    #[wasm_bindgen_test]
    fn browser_drives_active_dht_and_weighted_disk_orb_states() {
        let mut profile_counts = [0_usize; 3];
        for epoch in 0..2_000 {
            let index = match mocks::simulated_disk_load(
                scenarios::ScenarioId::Mixed,
                f64::from(epoch) * 2.0,
            ) {
                mocks::MockDiskLoad::Busy => 0,
                mocks::MockDiskLoad::Strain => 1,
                mocks::MockDiskLoad::Chaos => 2,
            };
            profile_counts[index] += 1;
        }
        assert!(profile_counts[0] > profile_counts[1] * 2);
        assert!(profile_counts[1] > profile_counts[2] * 3);
        assert!(profile_counts[2] > 0);

        let mut harness = DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::Mixed);
        let mut query_counts = std::collections::BTreeSet::new();
        let mut disk_levels = std::collections::BTreeSet::new();
        for _ in 0..80 {
            harness.advance(0.25);
            let visualization = harness.session.visualization_snapshot();
            query_counts.insert(visualization.dht_active_queries);
            disk_levels.insert(visualization.disk_health_state_level);
            assert!(visualization.dht_active_queries >= 72);
            assert!(visualization.dht_peers_found >= 2_000);
        }

        let visualization = harness.session.visualization_snapshot();
        assert!(visualization.dht_query_load > 0.5);
        assert!(query_counts.len() > 5);
        assert!(
            disk_levels.contains(&1),
            "observed disk levels: {disk_levels:?}"
        );
        assert!(
            disk_levels.contains(&2),
            "observed disk levels: {disk_levels:?}"
        );
        assert!(
            disk_levels.contains(&3),
            "observed disk levels: {disk_levels:?}"
        );
    }

    #[wasm_bindgen_test]
    fn peer_lifetime_totals_and_connection_counters_are_identity_stable() {
        let mut harness = DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::Swarm);
        let mut observed = std::collections::HashMap::<String, (u64, u64, u64, u64)>::new();
        let mut saw_reconnect = false;

        for _ in 0..900 {
            harness.advance(1.0 / 60.0);
            for peer in harness
                .service
                .peers_hex(FIXTURE_HASH_HEX)
                .expect("swarm peer rows")
            {
                if let Some(previous) = observed.get(&peer.address) {
                    assert!(peer.total_downloaded >= previous.0);
                    assert!(peer.total_uploaded >= previous.1);
                    assert!(peer.connection_count >= previous.2);
                    assert!(peer.disconnect_count >= previous.3);
                }
                saw_reconnect |= peer.connection_count > 1;
                observed.insert(
                    peer.address,
                    (
                        peer.total_downloaded,
                        peer.total_uploaded,
                        peer.connection_count,
                        peer.disconnect_count,
                    ),
                );
            }
        }

        assert!(observed.len() > 10);
        assert!(saw_reconnect);
        let current = harness
            .session
            .torrent_snapshot_hex(FIXTURE_HASH_HEX)
            .expect("current swarm snapshot")
            .connected_peers;
        assert!(harness.session.visualization_snapshot().tracked_peers > current);
    }

    #[wasm_bindgen_test]
    fn active_swarm_churn_and_transfer_lulls_remain_coherent() {
        let mut harness = DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::Seeding);
        let hash = FIXTURE_HASH_HEX;
        let mut peer_counts = std::collections::BTreeSet::new();
        let mut peer_rosters = std::collections::BTreeSet::new();
        let mut nonzero_raw_upload_rates = Vec::new();
        let mut saw_no_upload_recipients = false;
        let mut saw_average_decay_during_lull = false;
        let mut saw_positive_upload = false;
        let mut previous_published_upload = None;

        for _ in 0..720 {
            harness.advance(1.0 / 60.0);
            let snapshot = harness
                .session
                .torrent_snapshot_hex(hash)
                .expect("seeding torrent snapshot");
            let roster = harness
                .service
                .peer_ids_hex(hash)
                .expect("seeding peer roster");
            let recipients = harness
                .service
                .upload_recipient_count_hex(hash)
                .expect("seeding upload recipients");
            let (published_download, _, published_upload, raw_upload) = harness
                .service
                .rate_state_hex(hash)
                .expect("seeding rate state");
            let peer_rates = harness
                .service
                .aggregate_peer_rates_hex(hash)
                .expect("seeding peer rates");

            assert_eq!(
                peer_rates,
                (published_download, published_upload),
                "aggregate torrent rates diverged from the connected peer rows"
            );
            peer_counts.insert(snapshot.connected_peers);
            peer_rosters.insert(roster);
            if recipients == 0 {
                assert_eq!(raw_upload, 0);
                saw_no_upload_recipients = true;
                saw_average_decay_during_lull |= previous_published_upload
                    .is_some_and(|previous| published_upload > 0 && published_upload < previous);
            }
            saw_positive_upload |= published_upload > 0;
            if raw_upload > 0 {
                nonzero_raw_upload_rates.push(raw_upload);
            }
            previous_published_upload = Some(published_upload);
        }

        assert!(peer_counts.len() > 1, "peer count never changed");
        assert!(peer_rosters.len() > 2, "peer identities never churned");
        assert!(
            saw_no_upload_recipients,
            "no upload-recipient lull occurred"
        );
        assert!(
            saw_average_decay_during_lull,
            "the published upload average did not decay through the recipient lull"
        );
        assert!(saw_positive_upload, "the swarm never resumed uploading");
        let min_rate = nonzero_raw_upload_rates
            .iter()
            .copied()
            .min()
            .expect("nonzero upload rate");
        let max_rate = nonzero_raw_upload_rates
            .iter()
            .copied()
            .max()
            .expect("nonzero upload rate");
        assert!(
            max_rate > min_rate.saturating_mul(2),
            "upload envelope lacked meaningful lulls and bursts: {min_rate}..={max_rate}"
        );
    }

    #[wasm_bindgen_test]
    fn new_seeding_peers_start_empty_then_download_within_the_shared_link() {
        let mut harness = DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::Seeding);
        let hash = FIXTURE_HASH_HEX;
        let initial_addresses = harness
            .service
            .peers_hex(hash)
            .expect("initial seeding peers")
            .into_iter()
            .map(|peer| peer.address)
            .collect::<std::collections::HashSet<_>>();
        let mut new_peer_address = None;
        let mut saw_high_speed_peer = false;

        for _ in 0..900 {
            harness.advance(1.0 / 60.0);
            let peers = harness.service.peers_hex(hash).expect("seeding peers");
            let published_upload = harness
                .service
                .rate_state_hex(hash)
                .expect("seeding rate state")
                .2;
            assert_eq!(
                peers.iter().map(|peer| peer.upload_speed_bps).sum::<u64>(),
                published_upload
            );
            assert!(peers
                .iter()
                .all(|peer| peer.upload_speed_bps <= mocks::MAX_SIMULATED_LINK_BPS));
            saw_high_speed_peer |= peers.iter().any(|peer| peer.upload_speed_bps >= 5_000_000);

            if let Some(peer) = peers
                .iter()
                .find(|peer| !initial_addresses.contains(&peer.address))
            {
                assert!(peer.bitfield.iter().all(|piece| !*piece));
                assert_eq!(peer.upload_speed_bps, 0);
                new_peer_address = Some(peer.address.clone());
                break;
            }
        }

        let new_peer_address = new_peer_address.expect("a new peer joined the seeding swarm");
        let mut acquired_piece = false;
        for _ in 0..1_800 {
            harness.advance(1.0 / 60.0);
            let peers = harness
                .service
                .peers_hex(hash)
                .expect("advanced seeding peers");
            saw_high_speed_peer |= peers.iter().any(|peer| peer.upload_speed_bps >= 5_000_000);
            if peers
                .into_iter()
                .find(|peer| peer.address == new_peer_address)
                .is_some_and(|peer| peer.bitfield.iter().any(|piece| *piece))
            {
                acquired_piece = true;
                break;
            }
        }

        assert!(acquired_piece, "the new peer never began downloading");
        assert!(
            saw_high_speed_peer,
            "the randomized swarm never exercised a meaningful peer rate"
        );
    }

    #[wasm_bindgen_test(async)]
    async fn live_torrent_metrics_publish_at_sixty_hz() {
        let mut harness = DemoHarness::new(120, 40);
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

        let mut published_bytes = Vec::new();
        for _ in 0..6 {
            harness.advance(1.0 / 60.0);
            published_bytes.push(
                harness
                    .session
                    .torrent_snapshot_hex(MAGNET_HASH_HEX)
                    .expect("frame-rate torrent snapshot")
                    .bytes_written,
            );
        }

        assert!(
            published_bytes.windows(2).all(|pair| pair[1] > pair[0]),
            "torrent progress did not publish on every 60 Hz model step: {published_bytes:?}"
        );
    }

    #[wasm_bindgen_test]
    fn browser_session_starts_with_the_production_sixty_fps_rate() {
        let mut harness = DemoHarness::new(120, 40);

        assert_eq!(harness.session.target_fps(), 60.0);
        assert_eq!(harness.session.fps_label(), "60 fps");
        for _ in 0..60 {
            harness.advance(1.0 / 60.0);
        }
        assert_eq!(harness.session.fps_label(), "60 fps");
    }

    #[wasm_bindgen_test]
    fn browser_autosort_tracks_download_and_upload_activity() {
        let mut downloading =
            DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::Downloading);
        downloading.advance(1.0);
        assert_eq!(downloading.session.torrent_sort_column(), "down");
        assert!(!downloading.session.torrent_sort_pinned());
        let download_rates: Vec<u64> = downloading
            .session
            .ordered_torrent_rates()
            .into_iter()
            .map(|(download, _)| download)
            .collect();
        assert!(
            download_rates.windows(2).all(|rates| rates[0] >= rates[1]),
            "download order was not descending: {download_rates:?}"
        );
        assert!(download_rates.windows(2).any(|rates| rates[0] > rates[1]));

        let mut seeding = DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::Seeding);
        for _ in 0..400 {
            seeding.advance(0.1);
            let rates = seeding.session.ordered_torrent_rates();
            let upload_rates = rates.iter().map(|(_, upload)| *upload).collect::<Vec<_>>();
            if upload_rates.iter().filter(|upload| **upload > 0).count() >= 2
                && upload_rates.windows(2).all(|rates| rates[0] >= rates[1])
                && upload_rates.windows(2).any(|rates| rates[0] > rates[1])
            {
                break;
            }
        }
        assert_eq!(seeding.session.torrent_sort_column(), "up");
        assert!(!seeding.session.torrent_sort_pinned());
        let upload_rates: Vec<u64> = seeding
            .session
            .ordered_torrent_rates()
            .into_iter()
            .map(|(_, upload)| upload)
            .collect();
        assert!(
            upload_rates.windows(2).all(|rates| rates[0] >= rates[1]),
            "upload order was not descending with direction {}: {upload_rates:?}",
            seeding.session.torrent_sort_direction()
        );
        assert!(upload_rates.windows(2).any(|rates| rates[0] > rates[1]));
    }

    #[wasm_bindgen_test]
    fn declarative_scenario_catalog_exposes_each_defining_initial_state() {
        for scenario in scenarios::ScenarioId::ALL {
            let mut harness = DemoHarness::for_scenario(120, 40, scenario);
            let diagnostics = harness.service.diagnostics();
            assert_eq!(diagnostics.name, scenario.name());
            assert_eq!(
                harness.session.torrent_count(),
                scenario.preset().sessions.len()
            );
            match scenario {
                scenarios::ScenarioId::Downloading => {
                    assert_eq!(diagnostics.downloading, 3);
                    assert!(harness.session.visualization_snapshot().total_download_bps > 0);
                }
                scenarios::ScenarioId::Seeding => {
                    assert_eq!(diagnostics.seeding, 3);
                    for _ in 0..400 {
                        if harness.session.visualization_snapshot().total_upload_bps > 0 {
                            break;
                        }
                        harness.advance(0.1);
                    }
                    assert!(harness.session.visualization_snapshot().total_upload_bps > 0);
                }
                scenarios::ScenarioId::Mixed => {
                    assert_eq!(diagnostics.metadata, 1);
                    assert_eq!(diagnostics.peers, 1);
                    assert_eq!(diagnostics.downloading, 8);
                    assert_eq!(diagnostics.checking, 2);
                    assert_eq!(diagnostics.seeding, 3);
                    assert_eq!(diagnostics.paused, 0);
                    assert_eq!(diagnostics.deleting, 0);
                    for hash in harness.service.torrent_hashes() {
                        let snapshot = harness
                            .session
                            .torrent_snapshot_hex(&hash)
                            .expect("mixed ISO fixture");
                        assert!(snapshot.name.ends_with(".iso"));
                        assert!(
                            snapshot.total_size == 0
                                || snapshot.total_size == 3 * 1024 * 1024 * 1024 / 2
                        );
                    }
                }
                scenarios::ScenarioId::Swarm => {
                    assert_eq!(diagnostics.downloading, 2);
                    assert_eq!(diagnostics.peers, 1);
                    assert!(diagnostics.max_peers >= 16);
                    assert!(diagnostics.max_peers <= 20);
                }
                scenarios::ScenarioId::MissingPieces => {
                    assert_eq!(diagnostics.missing_pieces, 4);
                    assert!(diagnostics.warning);
                    assert!(
                        harness
                            .session
                            .visualization_snapshot()
                            .swarm_availability_samples
                            > 0
                    );
                }
                scenarios::ScenarioId::DiskPressure => {
                    assert_eq!(diagnostics.disk_state, mocks::MockDiskState::Pressure);
                    assert!(diagnostics.warning);
                    assert!(harness.session.visualization_snapshot().disk_write_bps > 0);
                }
                scenarios::ScenarioId::DiskError => {
                    assert_eq!(diagnostics.disk_state, mocks::MockDiskState::Error);
                    assert!(diagnostics.warning);
                    let snapshot = harness
                        .session
                        .selected_torrent_snapshot()
                        .expect("error torrent");
                    assert!(snapshot.activity.contains("disk error"));
                }
                scenarios::ScenarioId::Recovery => {
                    assert_eq!(diagnostics.disk_state, mocks::MockDiskState::Error);
                    assert_eq!(diagnostics.missing_pieces, 3);
                    assert!(diagnostics.warning);
                }
            }
        }
    }

    #[wasm_bindgen_test]
    fn peer_rates_and_swarm_availability_are_varied_and_evolve() {
        let mut swarm = DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::Swarm);
        let peers_before = swarm
            .service
            .peers_hex(FIXTURE_HASH_HEX)
            .expect("swarm peers");
        let torrent = swarm
            .session
            .torrent_snapshot_hex(FIXTURE_HASH_HEX)
            .expect("swarm torrent");
        let mut download_rates = peers_before
            .iter()
            .map(|peer| peer.download_speed_bps)
            .collect::<Vec<_>>();
        download_rates.sort_unstable();
        download_rates.dedup();
        assert!(download_rates.len() >= 8);
        assert_eq!(
            peers_before
                .iter()
                .map(|peer| peer.download_speed_bps)
                .sum::<u64>(),
            torrent.download_speed_bps
        );
        assert!(swarm.service.diagnostics().availability_levels >= 3);
        let pieces_before = peers_before
            .iter()
            .flat_map(|peer| &peer.bitfield)
            .filter(|has_piece| **has_piece)
            .count();

        swarm.advance(0.8);
        let peers_after = swarm
            .service
            .peers_hex(FIXTURE_HASH_HEX)
            .expect("advanced swarm peers");
        let pieces_after = peers_after
            .iter()
            .flat_map(|peer| &peer.bitfield)
            .filter(|has_piece| **has_piece)
            .count();
        assert!(pieces_after > pieces_before);
        assert!(swarm.service.diagnostics().piece_acquisitions > 0);

        let mut seeding = DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::Seeding);
        for _ in 0..400 {
            if seeding
                .session
                .torrent_snapshot_hex(FIXTURE_HASH_HEX)
                .is_some_and(|torrent| torrent.upload_speed_bps > 0)
            {
                break;
            }
            seeding.advance(0.1);
        }
        let seed_peers = seeding
            .service
            .peers_hex(FIXTURE_HASH_HEX)
            .expect("seeding peers");
        let seed_torrent = seeding
            .session
            .torrent_snapshot_hex(FIXTURE_HASH_HEX)
            .expect("seeding torrent");
        let mut upload_rates = seed_peers
            .iter()
            .map(|peer| peer.upload_speed_bps)
            .collect::<Vec<_>>();
        upload_rates.sort_unstable();
        upload_rates.dedup();
        assert!(upload_rates.len() >= 3);
        assert_eq!(
            seed_peers
                .iter()
                .map(|peer| peer.upload_speed_bps)
                .sum::<u64>(),
            seed_torrent.upload_speed_bps
        );
        assert!(seed_peers
            .iter()
            .any(|peer| peer.bitfield.iter().any(|has_piece| !*has_piece)));
        assert!(seeding.service.diagnostics().availability_levels >= 3);
    }

    #[wasm_bindgen_test]
    fn torrent_and_peer_rates_use_the_native_five_second_average() {
        let mut harness = DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::Downloading);
        harness.advance(0.1);
        let (average_before, _, _, _) = harness
            .service
            .rate_state_hex(FIXTURE_HASH_HEX)
            .expect("first 60 Hz rate state");
        let peers_before = harness
            .service
            .peers_hex(FIXTURE_HASH_HEX)
            .expect("initial peer rates");

        harness.advance(1.0 / 60.0);

        let (average_after, sample_after, _, _) = harness
            .service
            .rate_state_hex(FIXTURE_HASH_HEX)
            .expect("advanced rate state");
        let alpha = 1.0 - (-(1.0 / 60.0) / mocks::RATE_SMOOTHING_PERIOD_SECONDS).exp();
        let expected =
            (sample_after as f64).mul_add(alpha, average_before as f64 * (1.0 - alpha)) as u64;
        assert!(average_after.abs_diff(expected) <= 1);
        assert_ne!(average_after, average_before);
        assert!(average_after.abs_diff(average_before) < sample_after.abs_diff(average_before));

        // Published per-peer rates remain heterogeneous, but the production-style EMA keeps every
        // visible row from stepping abruptly while still updating on the 60 Hz manager cadence.
        let peers_after = harness
            .service
            .peers_hex(FIXTURE_HASH_HEX)
            .expect("smoothed peer rates");
        assert_eq!(peers_before.len(), peers_after.len());
        for (before, after) in peers_before.iter().zip(&peers_after) {
            assert!(
                after.download_speed_bps.abs_diff(before.download_speed_bps)
                    <= before.download_speed_bps / 4
            );
        }
    }

    #[wasm_bindgen_test]
    fn missing_pieces_wait_for_the_scheduled_peer_then_resume() {
        let mut harness = DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::MissingPieces);
        let hash = harness.service.torrent_hashes().remove(0);
        harness.advance(2.0);
        let waiting = harness
            .session
            .torrent_snapshot_hex(&hash)
            .expect("waiting torrent");
        assert_eq!(harness.service.missing_pieces_hex(&hash), Some(4));
        assert!(waiting.activity.contains("missing pieces"));

        harness.advance(1.0);
        assert_eq!(harness.service.missing_pieces_hex(&hash), Some(0));
        assert!(harness.service.diagnostics().recovered);
        assert!(
            harness
                .session
                .torrent_snapshot_hex(&hash)
                .expect("resumed torrent")
                .bytes_written
                > waiting.bytes_written
        );
        assert!((4..=6).contains(&harness.service.diagnostics().max_peers));
    }

    #[wasm_bindgen_test]
    fn disk_scenarios_drive_warning_journal_backoff_and_recovery() {
        let mut pressure = DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::DiskPressure);
        assert_eq!(
            pressure.service.diagnostics().disk_state,
            mocks::MockDiskState::Pressure
        );
        assert!(pressure.session.visualization_snapshot().disk_write_bps > 0);
        pressure.advance(4.0);
        assert_eq!(
            pressure.service.diagnostics().disk_state,
            mocks::MockDiskState::Healthy
        );
        assert!(pressure.service.diagnostics().recovered);

        let mut error = DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::DiskError);
        let hash = error.service.torrent_hashes().remove(0);
        let stalled = error
            .session
            .torrent_snapshot_hex(&hash)
            .expect("disk error torrent");
        assert_eq!(stalled.download_speed_bps, 0);
        error.session.set_screen(BrowserScreen::Journal);
        let journal = render_plain(&error.session);
        assert!(journal.contains("Missing"));
        assert!(journal.contains("Simulated disk write error"));
        error.advance(2.2);
        assert_eq!(
            error.service.diagnostics().disk_state,
            mocks::MockDiskState::Recovering
        );
        error.advance(1.3);
        assert_eq!(
            error.service.diagnostics().disk_state,
            mocks::MockDiskState::Healthy
        );
        assert!(error.service.diagnostics().recovered);
        assert!(
            error
                .session
                .torrent_snapshot_hex(&hash)
                .expect("recovered torrent")
                .bytes_written
                > stalled.bytes_written
        );

        let mut recovery = DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::Recovery);
        recovery.advance(3.0);
        let diagnostics = recovery.service.diagnostics();
        assert_eq!(diagnostics.disk_state, mocks::MockDiskState::Healthy);
        assert_eq!(diagnostics.missing_pieces, 0);
        assert!(diagnostics.recovered);
        assert!(!diagnostics.warning);
    }

    #[wasm_bindgen_test]
    fn every_scenario_is_invariant_to_frame_delta_partitioning() {
        for scenario in scenarios::ScenarioId::ALL {
            let mut tenth_second = DemoHarness::for_scenario(120, 40, scenario);
            let mut frame_delta = DemoHarness::for_scenario(120, 40, scenario);
            for _ in 0..50 {
                tenth_second.advance(0.1);
            }
            for _ in 0..300 {
                frame_delta.advance(1.0 / 60.0);
            }

            assert_eq!(
                tenth_second.service.diagnostics(),
                frame_delta.service.diagnostics()
            );
            assert_eq!(
                scenario_observables(&mut tenth_second),
                scenario_observables(&mut frame_delta),
                "scenario {} diverged across equal elapsed partitions",
                scenario.name()
            );
        }
    }

    #[wasm_bindgen_test(async)]
    async fn scenario_sessions_use_shared_add_pause_resume_and_delete_handlers() {
        let mut harness = DemoHarness::for_scenario(120, 40, scenarios::ScenarioId::Downloading);
        harness
            .session
            .dispatch_event(Event::Paste(MAGNET.to_string()))
            .await;
        harness.fulfill_pending();
        assert_eq!(harness.session.torrent_count(), 4);
        assert!(harness
            .session
            .torrent_snapshot_hex(MAGNET_HASH_HEX)
            .is_some());

        let hash = FIXTURE_HASH_HEX.to_string();
        assert!(harness.session.select_torrent_hex(&hash));

        key_and_flush(&mut harness.session, KeyCode::Char('p'), KeyModifiers::NONE).await;
        harness.fulfill_pending();
        assert_eq!(
            harness
                .session
                .torrent_snapshot_hex(&hash)
                .expect("paused scenario torrent")
                .control_state,
            BrowserTorrentControlState::Paused
        );

        key_and_flush(&mut harness.session, KeyCode::Char('p'), KeyModifiers::NONE).await;
        harness.fulfill_pending();
        assert_eq!(
            harness
                .session
                .torrent_snapshot_hex(&hash)
                .expect("resumed scenario torrent")
                .control_state,
            BrowserTorrentControlState::Running
        );

        key_and_flush(&mut harness.session, KeyCode::Char('d'), KeyModifiers::NONE).await;
        key_and_flush(
            &mut harness.session,
            KeyCode::Char('Y'),
            KeyModifiers::SHIFT,
        )
        .await;
        harness.fulfill_pending();
        assert!(harness.session.torrent_snapshot_hex(&hash).is_none());
        assert_eq!(harness.session.torrent_count(), 3);
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
        assert!(matches!(
            harness.fulfill_pending().as_slice(),
            [BrowserCommand::FetchTorrentPreview { .. }]
        ));

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
        assert!(matches!(
            harness.fulfill_pending().as_slice(),
            [BrowserCommand::FetchTorrentPreview { .. }]
        ));
        assert_eq!(harness.session.torrent_preview_state(), "ready");
        assert_eq!(harness.session.torrent_preview_name(), "Incoming Demo Set");
        assert_eq!(harness.session.torrent_preview_file_count(), 3);

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
        let selected_was_paused = session
            .torrent_snapshot_hex(&selected_hash)
            .is_some_and(|snapshot| snapshot.control_state == BrowserTorrentControlState::Paused);

        key_and_flush(&mut session, KeyCode::Char(' '), KeyModifiers::NONE).await;
        key_and_flush(&mut session, KeyCode::Char('p'), KeyModifiers::NONE).await;
        assert!(session.drain_commands().is_empty());
        key_and_flush(&mut session, KeyCode::Char('Y'), KeyModifiers::SHIFT).await;
        assert!(render_plain(&session).contains("Review"));
        key_and_flush(&mut session, KeyCode::Enter, KeyModifiers::NONE).await;

        let expected = if selected_was_paused {
            BrowserCommand::Resume {
                info_hash_hex: selected_hash,
            }
        } else {
            BrowserCommand::Pause {
                info_hash_hex: selected_hash,
            }
        };
        assert_eq!(session.drain_commands(), vec![expected]);
        key_and_flush(&mut session, KeyCode::Char('q'), KeyModifiers::NONE).await;
        assert_eq!(session.screen(), BrowserScreen::Normal);
    }

    #[wasm_bindgen_test]
    fn lifecycle_fixture_exercises_every_simulated_torrent_stage_in_the_production_view() {
        let session = rich_session_at(120, 64);
        let rendered = render_plain_at(&session, 120, 64);
        for name in [
            "Nebula Noodle",
            "Kernel Kettle",
            "Sudo Sandwich",
            "Recursive Raccoon",
            "Packet Yak",
            "Initramfs After Dark",
            "Segfault Sorbet",
            "Bashful Badger",
            "Daemon Dumpling",
            "TTY Tiramisu",
            "Fork Bomb Fondue",
            "Rootless Turnip",
            "Pipe Dream Pudding",
            "Mutex Marmalade",
            "Socket Souffle",
        ] {
            assert!(rendered.contains(name), "normal screen omitted {name}");
        }
    }

    #[wasm_bindgen_test]
    fn wasm_export_renders_from_the_webapp_session() {
        let frame = render_demo_frame_inner(120, 40);
        assert!(frame.starts_with("\x1b[2J"));
        assert!(frame.contains("Nebula Noodle"));
    }
}
