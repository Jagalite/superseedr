// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

#[cfg(test)]
use crate::app::{App, AppMode};
#[cfg(test)]
use crate::terminal_event::{Event as CrosstermEvent, KeyEventKind};
#[cfg(test)]
use std::sync::atomic::Ordering;

pub(crate) use crate::tui::kernel::reducer::{
    due_paste_text, flush_due_events, pending_paste_text_before_event, translate_event,
};
#[cfg(test)]
pub(crate) use crate::tui::kernel::reducer::{should_debounce_escape, GLOBAL_ESC_TIMESTAMP};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tui_effect_executor::{
        flush_pending_paste_burst_at, handle_event as execute_handle_event, handle_event_at,
    };
    use crate::app::{
        AppCommand, AppState, FileBrowserMode, FileMetadata, FilePriority, PeerInfo,
        SelectedHeader, TorrentControlState, TorrentDisplayState, TorrentManagementPendingCommand,
        TorrentMetrics, TorrentPreviewPayload,
    };
    use crate::config::Settings;
    use crate::integrations::control::ControlRequest;
    use crate::terminal_event::{KeyCode, KeyEvent, KeyModifiers};
    use crate::tui::layout::common::{ColumnId, PeerColumnId};
    use crate::tui::paste_burst::PasteBurst;
    use crate::tui::screens::{browser, normal};
    use crate::tui::tree::RawNode;
    use ratatui::prelude::Rect;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{Duration, Instant, UNIX_EPOCH};

    static ESC_DEBOUNCE_TEST_LOCK: Mutex<()> = Mutex::new(());

    async fn handle_event(event: CrosstermEvent, app: &mut App) {
        execute_handle_event(app, event).await;
    }

    fn translate_event(event: CrosstermEvent, app: &mut App, now: Instant) -> Vec<CrosstermEvent> {
        let pending_text =
            super::pending_paste_text_before_event(&event, &app.app_state).map(str::to_owned);
        let pending_is_paste = pending_text
            .as_deref()
            .is_some_and(|text| app.accepts_pasted_text(text));
        super::translate_event(event, &mut app.app_state, now, pending_is_paste)
    }

    fn flush_due_events(app: &mut App, now: Instant) -> Vec<CrosstermEvent> {
        let pending_text = super::due_paste_text(&app.app_state, now).map(str::to_owned);
        let pending_is_paste = pending_text
            .as_deref()
            .is_some_and(|text| app.accepts_pasted_text(text));
        super::flush_due_events(&mut app.app_state, now, pending_is_paste)
    }

    /// Creates a mock TorrentMetrics with a specific number of peers.
    fn create_mock_metrics(peer_count: usize) -> TorrentMetrics {
        let mut metrics = TorrentMetrics::default();
        let mut peers = Vec::new();
        for i in 0..peer_count {
            peers.push(PeerInfo {
                address: format!("127.0.0.1:{}", 6881 + i),
                ..Default::default()
            });
        }
        metrics.peers = peers;
        metrics
    }

    /// Creates a mock TorrentDisplayState for testing.
    fn create_mock_display_state(peer_count: usize) -> TorrentDisplayState {
        TorrentDisplayState {
            latest_state: create_mock_metrics(peer_count),
            ..Default::default()
        }
    }

    /// Creates a mock AppState for testing navigation.
    fn create_test_app_state() -> AppState {
        let mut app_state = AppState {
            screen_area: ratatui::layout::Rect::new(0, 0, 200, 100),
            ..Default::default()
        };

        let torrent_a = create_mock_display_state(2); // Has 2 peers
        let torrent_b = create_mock_display_state(0); // Has 0 peers

        app_state
            .torrents
            .insert("hash_a".as_bytes().to_vec(), torrent_a);
        app_state
            .torrents
            .insert("hash_b".as_bytes().to_vec(), torrent_b);

        app_state.torrent_list_order =
            vec!["hash_a".as_bytes().to_vec(), "hash_b".as_bytes().to_vec()];

        app_state
    }

    fn create_test_app_state_with_torrent_count(count: usize) -> AppState {
        let mut app_state = AppState {
            screen_area: ratatui::layout::Rect::new(0, 0, 200, 100),
            ..Default::default()
        };
        for i in 0..count {
            let info_hash = format!("hash_{i:02}").into_bytes();
            app_state
                .torrents
                .insert(info_hash.clone(), create_mock_display_state(0));
            app_state.torrent_list_order.push(info_hash);
        }
        app_state
    }

    // --- NAVIGATION TESTS ---

    async fn build_test_app() -> App {
        let settings = Settings {
            client_port: 0,
            ..Settings::default()
        };
        let mut app = App::new(settings, crate::app::AppRuntimeMode::Normal)
            .await
            .expect("build app");
        app.app_state.mode = AppMode::Normal;
        app
    }

    fn drain_app_commands(app: &mut App) {
        while app.app_command_rx.try_recv().is_ok() {}
    }

    async fn next_control_request(app: &mut App) -> ControlRequest {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let command = app
                    .app_command_rx
                    .recv()
                    .await
                    .expect("application command channel remains open");
                if let AppCommand::SubmitControlRequest(request) = command {
                    return request;
                }
            }
        })
        .await
        .expect("production event path emits a control request")
    }

    async fn assert_no_control_request(app: &mut App) {
        tokio::task::yield_now().await;
        while let Ok(command) = app.app_command_rx.try_recv() {
            assert!(
                !matches!(command, AppCommand::SubmitControlRequest(_)),
                "production event path emitted a control request before confirmation"
            );
        }
    }

    fn install_characterization_torrent(app: &mut App, info_hash: Vec<u8>) {
        let mut display = TorrentDisplayState::default();
        display.latest_state.info_hash = info_hash.clone();
        display.latest_state.torrent_name = "Geometry Packet".to_string();
        display.latest_state.torrent_control_state = TorrentControlState::Running;
        app.app_state.torrents.insert(info_hash.clone(), display);
        app.app_state.torrent_list_order = vec![info_hash];
        app.app_state.ui.selected_torrent_index = 0;
    }

    async fn press_and_flush(app: &mut App, key: char, start: Instant) {
        handle_event_at(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)),
            app,
            start,
        )
        .await;
        flush_pending_paste_burst_at(app, start + PasteBurst::flush_delay()).await;
    }

    async fn press_key(app: &mut App, key: KeyCode) {
        handle_event(
            CrosstermEvent::Key(KeyEvent::new(key, KeyModifiers::NONE)),
            app,
        )
        .await;
    }

    #[tokio::test]
    async fn native_characterization_explicit_paste_emits_add_magnet_request() {
        let temp_dir = tempfile::tempdir().expect("create download root");
        let mut app = build_test_app().await;
        app.client_configs.default_download_folder = Some(temp_dir.path().to_path_buf());
        app.client_configs.always_show_add_location_prompt = false;
        drain_app_commands(&mut app);
        let magnet = "magnet:?xt=urn:btih:1010101010101010101010101010101010101010";

        handle_event(CrosstermEvent::Paste(magnet.to_string()), &mut app).await;

        let ControlRequest::AddMagnet {
            magnet_link,
            download_path,
            container_name,
            ..
        } = next_control_request(&mut app).await
        else {
            panic!("expected add magnet request");
        };
        assert_eq!(magnet_link, magnet);
        assert_eq!(download_path.as_deref(), Some(temp_dir.path()));
        assert!(container_name.is_none());
        assert!(matches!(app.app_state.mode, AppMode::Normal));
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn native_characterization_paste_burst_uses_the_same_add_path() {
        let temp_dir = tempfile::tempdir().expect("create download root");
        let mut app = build_test_app().await;
        app.client_configs.default_download_folder = Some(temp_dir.path().to_path_buf());
        app.client_configs.always_show_add_location_prompt = false;
        drain_app_commands(&mut app);
        let magnet = "magnet:?xt=urn:btih:2020202020202020202020202020202020202020";
        let start = Instant::now();

        for (offset, character) in magnet.chars().enumerate() {
            handle_event_at(
                CrosstermEvent::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
                &mut app,
                start + Duration::from_millis(offset as u64),
            )
            .await;
        }
        flush_pending_paste_burst_at(
            &mut app,
            start + Duration::from_millis((magnet.len() - 1) as u64) + PasteBurst::flush_delay(),
        )
        .await;

        let ControlRequest::AddMagnet { magnet_link, .. } = next_control_request(&mut app).await
        else {
            panic!("expected add magnet request");
        };
        assert_eq!(magnet_link, magnet);
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn native_characterization_pause_resume_preserves_state_and_command_order() {
        let mut app = build_test_app().await;
        let info_hash = vec![0x2a; 20];
        install_characterization_torrent(&mut app, info_hash.clone());
        drain_app_commands(&mut app);
        let start = Instant::now();

        press_and_flush(&mut app, 'p', start).await;
        press_and_flush(
            &mut app,
            'p',
            start + PasteBurst::flush_delay() + Duration::from_millis(1),
        )
        .await;

        assert!(matches!(
            next_control_request(&mut app).await,
            ControlRequest::Pause { ref info_hash_hex } if info_hash_hex == &hex::encode(&info_hash)
        ));
        assert!(matches!(
            next_control_request(&mut app).await,
            ControlRequest::Resume { ref info_hash_hex } if info_hash_hex == &hex::encode(&info_hash)
        ));
        assert_eq!(
            app.app_state
                .torrents
                .get(&info_hash)
                .expect("characterization torrent remains selected")
                .latest_state
                .torrent_control_state,
            TorrentControlState::Running
        );
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn native_characterization_delete_requires_confirmation_and_cancel_is_safe() {
        let mut app = build_test_app().await;
        let info_hash = vec![0x3b; 20];
        install_characterization_torrent(&mut app, info_hash.clone());
        drain_app_commands(&mut app);
        let start = Instant::now();

        press_and_flush(&mut app, 'd', start).await;
        assert!(matches!(app.app_state.mode, AppMode::DeleteConfirm));
        assert_eq!(app.app_state.ui.delete_confirm.info_hash, info_hash);
        assert!(!app.app_state.ui.delete_confirm.with_files);
        assert_no_control_request(&mut app).await;

        GLOBAL_ESC_TIMESTAMP.store(0, Ordering::Relaxed);
        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &mut app,
        )
        .await;
        assert!(matches!(app.app_state.mode, AppMode::Normal));
        assert_no_control_request(&mut app).await;
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn native_characterization_confirm_delete_marks_selected_torrent_and_emits_once() {
        let mut app = build_test_app().await;
        let info_hash = vec![0x4c; 20];
        install_characterization_torrent(&mut app, info_hash.clone());
        drain_app_commands(&mut app);
        let start = Instant::now();

        press_and_flush(&mut app, 'D', start).await;
        assert!(matches!(app.app_state.mode, AppMode::DeleteConfirm));
        assert_no_control_request(&mut app).await;

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::NONE)),
            &mut app,
        )
        .await;

        assert!(matches!(
            next_control_request(&mut app).await,
            ControlRequest::Delete {
                ref info_hash_hex,
                delete_files: true,
            } if info_hash_hex == &hex::encode(&info_hash)
        ));
        assert_eq!(
            app.app_state
                .torrents
                .get(&info_hash)
                .expect("selected torrent remains available for deleting state")
                .latest_state
                .torrent_control_state,
            TorrentControlState::Deleting
        );
        assert!(
            app.app_state
                .torrents
                .get(&info_hash)
                .expect("selected torrent remains available for delete-files state")
                .latest_state
                .delete_files
        );
        assert!(matches!(app.app_state.mode, AppMode::Normal));
        assert_no_control_request(&mut app).await;
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn native_characterization_missing_selection_emits_no_control_request() {
        let mut app = build_test_app().await;
        app.app_state.torrents.clear();
        app.app_state.torrent_list_order.clear();
        app.app_state.ui.selected_torrent_index = 0;
        drain_app_commands(&mut app);

        press_and_flush(&mut app, 'p', Instant::now()).await;
        press_and_flush(&mut app, 'd', Instant::now() + PasteBurst::flush_delay()).await;

        assert!(matches!(app.app_state.mode, AppMode::Normal));
        assert_no_control_request(&mut app).await;
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn native_characterization_resize_updates_shared_screen_area_without_command() {
        let mut app = build_test_app().await;
        drain_app_commands(&mut app);

        handle_event(CrosstermEvent::Resize(91, 27), &mut app).await;

        assert_eq!(app.app_state.screen_area, Rect::new(0, 0, 91, 27));
        assert!(app.app_state.ui.needs_redraw);
        assert_no_control_request(&mut app).await;
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn native_characterization_file_browser_left_queues_parent_fetch_through_top_dispatcher()
    {
        let directory = tempfile::tempdir().expect("create browser root");
        let child = directory.path().join("child");
        std::fs::create_dir(&child).expect("create child directory");
        let mut app = build_test_app().await;
        drain_app_commands(&mut app);
        app.app_state.mode = AppMode::FileBrowser;
        app.app_state.ui.file_browser.browser_generation = 17;
        app.app_state.ui.file_browser.browser_mode = FileBrowserMode::Directory;
        app.app_state.ui.file_browser.state.current_path = child.clone();

        press_key(&mut app, KeyCode::Left).await;

        let command = tokio::time::timeout(Duration::from_secs(1), app.app_command_rx.recv())
            .await
            .expect("top dispatcher should queue parent fetch")
            .expect("application command channel remains open");
        assert!(matches!(
            command,
            AppCommand::FetchFileTree {
                browser_generation: 17,
                path,
                preserve_browser_mode: true,
                highlight_path: Some(highlight_path),
                ..
            } if path == directory.path() && highlight_path == child
        ));
        assert!(matches!(app.app_state.mode, AppMode::FileBrowser));
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn native_characterization_torrent_file_confirm_queues_add_through_top_dispatcher() {
        let directory = tempfile::tempdir().expect("create browser root");
        let path = directory.path().join("fixture.TORRENT");
        std::fs::write(&path, []).expect("create selected torrent file");
        let mut app = build_test_app().await;
        drain_app_commands(&mut app);
        app.app_state.mode = AppMode::FileBrowser;
        app.app_state.screen_area = Rect::new(0, 0, 120, 40);
        app.app_state.ui.file_browser.browser_mode =
            FileBrowserMode::File(vec![".torrent".to_string()]);
        app.app_state.ui.file_browser.state.current_path = directory.path().to_path_buf();
        app.app_state.ui.file_browser.state.cursor_path = Some(path.clone());
        app.app_state.ui.file_browser.data = vec![RawNode {
            name: "fixture.TORRENT".to_string(),
            full_path: path.clone(),
            children: Vec::new(),
            payload: FileMetadata {
                size: 0,
                modified: UNIX_EPOCH,
            },
            is_dir: false,
        }];
        app.app_state.ui.file_browser.fetch_pending = false;

        press_key(&mut app, KeyCode::Char('Y')).await;

        let command = tokio::time::timeout(Duration::from_secs(1), app.app_command_rx.recv())
            .await
            .expect("top dispatcher should queue torrent file add")
            .expect("application command channel remains open");
        assert!(matches!(
            command,
            AppCommand::AddTorrentFromFile(queued_path) if queued_path == path
        ));
        assert!(matches!(app.app_state.mode, AppMode::Normal));
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn native_characterization_stale_torrent_file_stays_open_and_queues_nothing() {
        let directory = tempfile::tempdir().expect("create browser root");
        let path = directory.path().join("stale-fixture.torrent");
        std::fs::write(&path, []).expect("create selected torrent file");
        let mut app = build_test_app().await;
        drain_app_commands(&mut app);
        app.app_state.mode = AppMode::FileBrowser;
        app.app_state.screen_area = Rect::new(0, 0, 120, 40);
        app.app_state.ui.file_browser.browser_mode =
            FileBrowserMode::File(vec![".torrent".to_string()]);
        app.app_state.ui.file_browser.state.current_path = directory.path().to_path_buf();
        app.app_state.ui.file_browser.state.cursor_path = Some(path.clone());
        app.app_state.ui.file_browser.data = vec![RawNode {
            name: "stale-fixture.torrent".to_string(),
            full_path: path.clone(),
            children: Vec::new(),
            payload: FileMetadata {
                size: 0,
                modified: UNIX_EPOCH,
            },
            is_dir: false,
        }];
        app.app_state.ui.file_browser.fetch_pending = false;
        std::fs::remove_file(&path).expect("remove selected torrent after metadata load");

        press_key(&mut app, KeyCode::Char('Y')).await;

        assert!(matches!(app.app_state.mode, AppMode::FileBrowser));
        tokio::time::sleep(Duration::from_millis(50)).await;
        while let Ok(command) = app.app_command_rx.try_recv() {
            assert!(
                !matches!(command, AppCommand::AddTorrentFromFile(_)),
                "stale selection must not queue an add command"
            );
        }
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn native_characterization_all_production_modes_use_the_shared_event_dispatcher() {
        let mut app = build_test_app().await;
        install_characterization_torrent(&mut app, vec![0x5d; 20]);
        drain_app_commands(&mut app);
        let mut now = Instant::now();

        press_and_flush(&mut app, 'm', now).await;
        assert!(matches!(app.app_state.mode, AppMode::Help));
        GLOBAL_ESC_TIMESTAMP.store(0, Ordering::Relaxed);
        press_key(&mut app, KeyCode::Esc).await;
        assert!(matches!(app.app_state.mode, AppMode::Normal));
        now += PasteBurst::flush_delay() + Duration::from_millis(1);

        press_and_flush(&mut app, 'c', now).await;
        assert!(matches!(app.app_state.mode, AppMode::Config));
        GLOBAL_ESC_TIMESTAMP.store(0, Ordering::Relaxed);
        press_key(&mut app, KeyCode::Esc).await;
        assert!(matches!(app.app_state.mode, AppMode::Normal));
        now += PasteBurst::flush_delay() + Duration::from_millis(1);

        press_and_flush(&mut app, 'r', now).await;
        assert!(matches!(app.app_state.mode, AppMode::Rss));
        GLOBAL_ESC_TIMESTAMP.store(0, Ordering::Relaxed);
        press_key(&mut app, KeyCode::Esc).await;
        assert!(matches!(app.app_state.mode, AppMode::Normal));
        now += PasteBurst::flush_delay() + Duration::from_millis(1);

        press_and_flush(&mut app, 'J', now).await;
        assert!(matches!(app.app_state.mode, AppMode::Journal));
        GLOBAL_ESC_TIMESTAMP.store(0, Ordering::Relaxed);
        press_key(&mut app, KeyCode::Esc).await;
        assert!(matches!(app.app_state.mode, AppMode::Normal));
        now += PasteBurst::flush_delay() + Duration::from_millis(1);

        press_and_flush(&mut app, 'P', now).await;
        assert!(matches!(app.app_state.mode, AppMode::PeerManagement));
        GLOBAL_ESC_TIMESTAMP.store(0, Ordering::Relaxed);
        press_key(&mut app, KeyCode::Esc).await;
        assert!(matches!(app.app_state.mode, AppMode::Normal));
        now += PasteBurst::flush_delay() + Duration::from_millis(1);

        press_and_flush(&mut app, 'M', now).await;
        assert!(matches!(app.app_state.mode, AppMode::TorrentManagement));
        GLOBAL_ESC_TIMESTAMP.store(0, Ordering::Relaxed);
        press_key(&mut app, KeyCode::Esc).await;
        assert!(matches!(app.app_state.mode, AppMode::Normal));
        now += PasteBurst::flush_delay() + Duration::from_millis(1);

        press_and_flush(&mut app, 'a', now).await;
        assert!(matches!(app.app_state.mode, AppMode::FileBrowser));
        GLOBAL_ESC_TIMESTAMP.store(0, Ordering::Relaxed);
        press_key(&mut app, KeyCode::Esc).await;
        assert!(matches!(app.app_state.mode, AppMode::Normal));
        now += PasteBurst::flush_delay() + Duration::from_millis(1);

        press_and_flush(&mut app, 'z', now).await;
        assert!(matches!(app.app_state.mode, AppMode::PowerSaving));
        press_and_flush(
            &mut app,
            'z',
            now + PasteBurst::flush_delay() + Duration::from_millis(1),
        )
        .await;
        assert!(matches!(app.app_state.mode, AppMode::Normal));

        app.app_state.mode = AppMode::Welcome;
        GLOBAL_ESC_TIMESTAMP.store(0, Ordering::Relaxed);
        press_key(&mut app, KeyCode::Esc).await;
        assert!(matches!(app.app_state.mode, AppMode::Normal));

        assert_no_control_request(&mut app).await;
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn native_characterization_management_submit_preserves_effect_order() {
        let mut app = build_test_app().await;
        let first_hash = vec![0x61; 20];
        let second_hash = vec![0x62; 20];
        install_characterization_torrent(&mut app, first_hash.clone());
        install_characterization_torrent(&mut app, second_hash.clone());
        app.app_state.mode = AppMode::TorrentManagement;
        app.app_state.ui.torrent_management.pending_commands = vec![
            TorrentManagementPendingCommand {
                info_hash: first_hash.clone(),
                request: ControlRequest::Pause {
                    info_hash_hex: hex::encode(&first_hash),
                },
                state: TorrentControlState::Paused,
                delete_files: false,
            },
            TorrentManagementPendingCommand {
                info_hash: second_hash.clone(),
                request: ControlRequest::Resume {
                    info_hash_hex: hex::encode(&second_hash),
                },
                state: TorrentControlState::Running,
                delete_files: false,
            },
        ];
        app.app_state.ui.torrent_management.confirm_submit = true;
        drain_app_commands(&mut app);

        press_key(&mut app, KeyCode::Enter).await;

        assert!(matches!(
            next_control_request(&mut app).await,
            ControlRequest::Pause { ref info_hash_hex }
                if info_hash_hex == &hex::encode(&first_hash)
        ));
        assert!(matches!(
            next_control_request(&mut app).await,
            ControlRequest::Resume { ref info_hash_hex }
                if info_hash_hex == &hex::encode(&second_hash)
        ));
        assert_eq!(
            app.app_state
                .torrents
                .get(&first_hash)
                .expect("first characterization torrent remains")
                .latest_state
                .torrent_control_state,
            TorrentControlState::Paused
        );
        assert_eq!(
            app.app_state
                .torrents
                .get(&second_hash)
                .expect("second characterization torrent remains")
                .latest_state
                .torrent_control_state,
            TorrentControlState::Running
        );
        assert!(app
            .app_state
            .ui
            .torrent_management
            .pending_commands
            .is_empty());
        assert!(!app.app_state.ui.torrent_management.confirm_submit);
        let _ = app.shutdown_tx.send(());
    }

    #[test]
    fn test_nav_down_torrents() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 0;
        app_state.ui.selected_header = SelectedHeader::Torrent(ColumnId::Name);

        normal::handle_navigation(&mut app_state, KeyCode::Down);

        assert_eq!(app_state.ui.selected_torrent_index, 1);
        assert_eq!(app_state.ui.selected_peer_index, 0); // Should reset
    }

    #[test]
    fn test_nav_up_torrents() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 1;
        app_state.ui.selected_header = SelectedHeader::Torrent(ColumnId::Name);

        normal::handle_navigation(&mut app_state, KeyCode::Up);

        assert_eq!(app_state.ui.selected_torrent_index, 0);
        assert_eq!(app_state.ui.selected_peer_index, 0); // Should reset
    }

    #[test]
    fn test_nav_down_peers() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 0; // "hash_a" has 2 peers
        app_state.ui.selected_peer_index = 0;
        app_state.ui.selected_header = SelectedHeader::Peer(PeerColumnId::Flags);

        normal::handle_navigation(&mut app_state, KeyCode::Down);

        assert_eq!(app_state.ui.selected_torrent_index, 0); // Stays on same torrent
        assert_eq!(app_state.ui.selected_peer_index, 1); // Moves down peer list
    }

    #[test]
    fn test_nav_right_to_peers_when_peers_exist() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 0; // "hash_a" has peers
        app_state.ui.selected_header = SelectedHeader::Torrent(ColumnId::Name);

        normal::handle_navigation(&mut app_state, KeyCode::Right);

        assert_eq!(
            app_state.ui.selected_header,
            SelectedHeader::Peer(PeerColumnId::Flags)
        );
    }

    #[test]
    fn test_nav_right_to_peers_when_no_peers() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 1; // "hash_b" has 0 peers
        app_state.ui.selected_header = SelectedHeader::Torrent(ColumnId::Name);

        normal::handle_navigation(&mut app_state, KeyCode::Right);

        assert_eq!(
            app_state.ui.selected_header,
            SelectedHeader::Torrent(ColumnId::Name)
        );
    }

    #[test]
    fn test_nav_left_from_peers() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 0;
        app_state.ui.selected_header = SelectedHeader::Peer(PeerColumnId::Flags);

        normal::handle_navigation(&mut app_state, KeyCode::Left);

        assert_eq!(
            app_state.ui.selected_header,
            SelectedHeader::Torrent(ColumnId::Name)
        );
    }

    #[test]
    fn test_nav_up_peers() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 0; // "hash_a" has 2 peers
        app_state.ui.selected_peer_index = 1;
        app_state.ui.selected_header = SelectedHeader::Peer(PeerColumnId::Flags);

        normal::handle_navigation(&mut app_state, KeyCode::Up);

        assert_eq!(app_state.ui.selected_torrent_index, 0); // Stays on same torrent
        assert_eq!(app_state.ui.selected_peer_index, 0); // Moves up peer list
    }

    #[test]
    fn test_nav_up_at_top_of_list() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 0; // At the top
        app_state.ui.selected_header = SelectedHeader::Torrent(ColumnId::Name);

        normal::handle_navigation(&mut app_state, KeyCode::Up);

        // Should stay at 0, thanks to saturating_sub
        assert_eq!(app_state.ui.selected_torrent_index, 0);
    }

    #[test]
    fn test_nav_down_at_bottom_of_list() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 1; // At the bottom (index 1 of 2)
        app_state.ui.selected_header = SelectedHeader::Torrent(ColumnId::Name);

        normal::handle_navigation(&mut app_state, KeyCode::Down);

        // Should stay at 1, as it's the last index
        assert_eq!(app_state.ui.selected_torrent_index, 1);
    }

    #[test]
    fn test_nav_page_down_and_page_up_torrents() {
        let mut app_state = create_test_app_state_with_torrent_count(12);
        app_state.ui.selected_torrent_index = 0;
        app_state.ui.selected_header = SelectedHeader::Torrent(ColumnId::Name);

        normal::handle_navigation(&mut app_state, KeyCode::PageDown);

        assert_eq!(app_state.ui.selected_torrent_index, 11);
        assert_eq!(app_state.ui.selected_peer_index, 0);

        normal::handle_navigation(&mut app_state, KeyCode::PageUp);

        assert_eq!(app_state.ui.selected_torrent_index, 0);
        assert_eq!(app_state.ui.selected_peer_index, 0);
    }

    #[test]
    fn test_nav_home_and_end_torrents() {
        let mut app_state = create_test_app_state_with_torrent_count(12);
        app_state.ui.selected_torrent_index = 5;
        app_state.ui.selected_peer_index = 1;
        app_state.ui.selected_header = SelectedHeader::Torrent(ColumnId::Name);

        normal::handle_navigation(&mut app_state, KeyCode::End);

        assert_eq!(app_state.ui.selected_torrent_index, 11);
        assert_eq!(app_state.ui.selected_peer_index, 0);

        normal::handle_navigation(&mut app_state, KeyCode::Home);

        assert_eq!(app_state.ui.selected_torrent_index, 0);
        assert_eq!(app_state.ui.selected_peer_index, 0);
    }

    #[test]
    fn test_nav_up_peers_at_top_of_list() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 0; // "hash_a" has 2 peers
        app_state.ui.selected_peer_index = 0; // At the top
        app_state.ui.selected_header = SelectedHeader::Peer(PeerColumnId::Flags);

        normal::handle_navigation(&mut app_state, KeyCode::Up);

        // Should stay at 0
        assert_eq!(app_state.ui.selected_peer_index, 0);
    }

    #[test]
    fn test_nav_down_peers_at_bottom_of_list() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 0; // "hash_a" has 2 peers
        app_state.ui.selected_peer_index = 1; // At the bottom (index 1 of 2)
        app_state.ui.selected_header = SelectedHeader::Peer(PeerColumnId::Flags);

        normal::handle_navigation(&mut app_state, KeyCode::Down);

        // Should stay at 1
        assert_eq!(app_state.ui.selected_peer_index, 1);
    }

    #[test]
    fn test_nav_right_jumps_to_peers_when_only_name_column_visible() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 0;
        app_state.ui.selected_header = SelectedHeader::Torrent(ColumnId::Name);

        if let Some(torrent) = app_state.torrents.get_mut("hash_a".as_bytes()) {
            torrent.latest_state.activity_message = "Seeding".to_string();
            torrent.latest_state.number_of_pieces_total = 100;
            torrent.latest_state.number_of_pieces_completed = 100;
        }

        for torrent in app_state.torrents.values_mut() {
            torrent.smoothed_download_speed_bps = 0;
            torrent.smoothed_upload_speed_bps = 0;
        }

        normal::handle_navigation(&mut app_state, KeyCode::Right);

        assert_eq!(
            app_state.ui.selected_header,
            SelectedHeader::Peer(PeerColumnId::Flags)
        );
    }

    #[test]
    fn test_apply_priority_action_cycles_target_and_children() {
        let mut nodes = vec![RawNode {
            name: "root".to_string(),
            full_path: PathBuf::from("root"),
            is_dir: true,
            payload: TorrentPreviewPayload::default(),
            children: vec![RawNode {
                name: "leaf.bin".to_string(),
                full_path: PathBuf::from("root/leaf.bin"),
                is_dir: false,
                payload: TorrentPreviewPayload::default(),
                children: vec![],
            }],
        }];

        let changed = browser::apply_priority_cycle(&mut nodes, &PathBuf::from("root"));

        assert!(changed);
        assert_eq!(nodes[0].payload.priority, FilePriority::Skip);
        assert_eq!(nodes[0].children[0].payload.priority, FilePriority::Skip);
    }

    #[test]
    fn test_apply_priority_action_returns_false_for_missing_path() {
        let mut nodes = vec![RawNode {
            name: "root".to_string(),
            full_path: PathBuf::from("root"),
            is_dir: true,
            payload: TorrentPreviewPayload::default(),
            children: vec![],
        }];

        let changed = browser::apply_priority_cycle(&mut nodes, &PathBuf::from("missing"));

        assert!(!changed);
        assert_eq!(nodes[0].payload.priority, FilePriority::Normal);
    }

    #[test]
    fn test_escape_debounce_ignores_non_escape_keys() {
        let _guard = ESC_DEBOUNCE_TEST_LOCK
            .lock()
            .expect("escape debounce test lock poisoned");
        GLOBAL_ESC_TIMESTAMP.store(0, Ordering::Relaxed);
        let event = CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(!should_debounce_escape(&event));
    }

    #[test]
    fn test_escape_debounce_blocks_rapid_second_escape() {
        let _guard = ESC_DEBOUNCE_TEST_LOCK
            .lock()
            .expect("escape debounce test lock poisoned");
        GLOBAL_ESC_TIMESTAMP.store(0, Ordering::Relaxed);
        let event = CrosstermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(!should_debounce_escape(&event));
        assert!(should_debounce_escape(&event));
    }

    #[test]
    fn test_escape_debounce_modified_escape_does_not_block_next_plain_escape() {
        let _guard = ESC_DEBOUNCE_TEST_LOCK
            .lock()
            .expect("escape debounce test lock poisoned");
        GLOBAL_ESC_TIMESTAMP.store(0, Ordering::Relaxed);
        let modified = CrosstermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::ALT));
        let plain = CrosstermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(!should_debounce_escape(&modified));
        assert!(!should_debounce_escape(&plain));
    }

    #[tokio::test]
    async fn single_shortcut_replays_after_burst_timeout() {
        let mut app = build_test_app().await;
        let start = Instant::now();

        handle_event_at(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            &mut app,
            start,
        )
        .await;
        assert!(matches!(app.app_state.mode, AppMode::Normal));

        let translated = flush_due_events(&mut app, start + PasteBurst::flush_delay());
        assert!(matches!(translated.as_slice(), [CrosstermEvent::Key(_)]));
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn supported_burst_flushes_as_synthetic_paste() {
        let mut app = build_test_app().await;
        let start = Instant::now();
        let magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567";

        for (offset, ch) in magnet.chars().enumerate() {
            handle_event_at(
                CrosstermEvent::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
                &mut app,
                start + std::time::Duration::from_millis(offset as u64),
            )
            .await;
        }

        let translated = flush_due_events(
            &mut app,
            start
                + std::time::Duration::from_millis((magnet.len() - 1) as u64)
                + PasteBurst::flush_delay(),
        );
        assert!(matches!(translated.as_slice(), [CrosstermEvent::Paste(text)] if text == magnet));
        assert!(matches!(app.app_state.mode, AppMode::Normal));
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn welcome_screen_paste_burst_flushes_as_synthetic_paste() {
        let mut app = build_test_app().await;
        app.app_state.mode = AppMode::Welcome;
        let start = Instant::now();
        let magnet = "magnet:?xt=urn:btih:fedcba9876543210fedcba9876543210fedcba98";

        for (offset, ch) in magnet.chars().enumerate() {
            handle_event_at(
                CrosstermEvent::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
                &mut app,
                start + std::time::Duration::from_millis(offset as u64),
            )
            .await;
        }

        let translated = flush_due_events(
            &mut app,
            start
                + std::time::Duration::from_millis((magnet.len() - 1) as u64)
                + PasteBurst::flush_delay(),
        );
        assert!(matches!(translated.as_slice(), [CrosstermEvent::Paste(text)] if text == magnet));
        assert!(matches!(app.app_state.mode, AppMode::Welcome));
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn unsupported_burst_replays_original_keys() {
        let mut app = build_test_app().await;
        let start = Instant::now();

        for (offset, ch) in ['j', 'j'].into_iter().enumerate() {
            handle_event_at(
                CrosstermEvent::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
                &mut app,
                start + std::time::Duration::from_millis(offset as u64),
            )
            .await;
        }

        let translated = flush_due_events(
            &mut app,
            start + std::time::Duration::from_millis(1) + PasteBurst::flush_delay(),
        );
        assert!(matches!(
            translated.as_slice(),
            [CrosstermEvent::Key(_), CrosstermEvent::Key(_)]
        ));
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn explicit_paste_bypasses_pending_burst() {
        let mut app = build_test_app().await;
        let start = Instant::now();

        handle_event_at(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            &mut app,
            start,
        )
        .await;

        let translated = translate_event(
            CrosstermEvent::Paste(
                "magnet:?xt=urn:btih:fedcba9876543210fedcba9876543210fedcba98".to_string(),
            ),
            &mut app,
            start + std::time::Duration::from_millis(1),
        );
        assert!(matches!(
            translated.as_slice(),
            [CrosstermEvent::Key(_), CrosstermEvent::Paste(_)]
        ));
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn explicit_paste_on_welcome_screen_is_ignored() {
        let mut app = build_test_app().await;
        app.app_state.mode = AppMode::Welcome;
        let magnet = "magnet:?xt=urn:btih:00112233445566778899aabbccddeeff00112233";

        handle_event_at(
            CrosstermEvent::Paste(magnet.to_string()),
            &mut app,
            Instant::now(),
        )
        .await;

        assert!(matches!(app.app_state.mode, AppMode::Welcome));
        assert!(app.app_state.pending_torrent_link.is_empty());
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn release_events_are_forwarded_only_for_the_management_latch() {
        let mut app = build_test_app().await;
        app.app_state.mode = AppMode::Help;

        let translated = translate_event(
            CrosstermEvent::Key(KeyEvent::new_with_kind(
                KeyCode::Char('m'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )),
            &mut app,
            Instant::now(),
        );

        assert!(translated.is_empty());

        app.app_state.mode = AppMode::TorrentManagement;
        app.app_state.ui.torrent_management.input_latch = Some(KeyCode::Char('/'));
        let translated = translate_event(
            CrosstermEvent::Key(KeyEvent::new_with_kind(
                KeyCode::Char('m'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )),
            &mut app,
            Instant::now(),
        );
        assert!(translated.is_empty());

        let translated = translate_event(
            CrosstermEvent::Key(KeyEvent::new_with_kind(
                KeyCode::Char('/'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )),
            &mut app,
            Instant::now(),
        );
        assert!(matches!(
            translated.as_slice(),
            [CrosstermEvent::Key(KeyEvent {
                code: KeyCode::Char('/'),
                kind: KeyEventKind::Release,
                ..
            })]
        ));
        let _ = app.shutdown_tx.send(());
    }
}
