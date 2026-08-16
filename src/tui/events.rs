// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{App, AppMode, ConfigPane};
use crate::tui::paste_burst::FlushResult as PasteBurstFlushResult;
use crate::tui::screens::{
    browser, config, delete_confirm, help, journal, libraries, normal, peers, power, rss, torrents,
    welcome,
};

use ratatui::crossterm::event::{
    Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::prelude::Rect;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static GLOBAL_ESC_TIMESTAMP: AtomicU64 = AtomicU64::new(0);

pub async fn handle_event(event: CrosstermEvent, app: &mut App) {
    handle_event_at(event, app, Instant::now()).await;
}

pub async fn flush_pending_paste_burst(app: &mut App) {
    flush_pending_paste_burst_at(app, Instant::now()).await;
}

async fn handle_event_at(event: CrosstermEvent, app: &mut App, now: Instant) {
    let translated = translate_event(event, app, now);
    if translated.is_empty() {
        return;
    }

    for event in translated {
        apply_event(event, app).await;
    }
    app.app_state.ui.needs_redraw = true;
}

async fn flush_pending_paste_burst_at(app: &mut App, now: Instant) {
    let translated = flush_due_events(app, now);
    if translated.is_empty() {
        return;
    }

    for event in translated {
        apply_event(event, app).await;
    }
    app.app_state.ui.needs_redraw = true;
}

fn translate_event(event: CrosstermEvent, app: &mut App, now: Instant) -> Vec<CrosstermEvent> {
    let mut translated = Vec::new();
    if should_ignore_event_for_paste_burst(&event, app) {
        return translated;
    }

    let buffered_key = match &event {
        CrosstermEvent::Key(key) if should_buffer_paste_burst_key(app, *key) => Some(*key),
        _ => None,
    };

    if let Some(key) = buffered_key {
        let flush = app.app_state.ui.normal_paste_burst.push_key(key, now);
        translated.extend(convert_burst_flush(flush));
        return translated;
    }

    if app.app_state.ui.normal_paste_burst.has_pending() {
        let flush = app
            .app_state
            .ui
            .normal_paste_burst
            .flush_now(normal::accepts_pasted_text);
        translated.extend(convert_burst_flush(flush));
    }

    translated.push(event);
    translated
}
fn flush_due_events(app: &mut App, now: Instant) -> Vec<CrosstermEvent> {
    let flush = app
        .app_state
        .ui
        .normal_paste_burst
        .flush_if_due(now, normal::accepts_pasted_text);
    convert_burst_flush(flush)
}

fn convert_burst_flush(flush: PasteBurstFlushResult) -> Vec<CrosstermEvent> {
    match flush {
        PasteBurstFlushResult::None | PasteBurstFlushResult::Buffered => Vec::new(),
        PasteBurstFlushResult::Text(text) => vec![CrosstermEvent::Paste(text)],
        PasteBurstFlushResult::Keys(keys) => keys.into_iter().map(CrosstermEvent::Key).collect(),
    }
}

fn should_buffer_paste_burst_key(app: &App, key: KeyEvent) -> bool {
    matches!(app.app_state.mode, AppMode::Normal | AppMode::Welcome)
        && !app.app_state.ui.is_searching
        && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && matches!(key.code, KeyCode::Char(_))
        && !key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
}

fn should_ignore_event_for_paste_burst(event: &CrosstermEvent, app: &App) -> bool {
    let CrosstermEvent::Key(KeyEvent {
        code,
        kind: KeyEventKind::Release,
        ..
    }) = event
    else {
        return false;
    };

    !matches!(app.app_state.mode, AppMode::TorrentManagement)
        || app.app_state.ui.torrent_management.input_latch != Some(*code)
}

async fn apply_event(event: CrosstermEvent, app: &mut App) {
    if handle_resize_event(&event, app) {
        return;
    }

    if should_quit_on_ctrl_c(&event, app) {
        return;
    }

    if should_debounce_escape(&event) {
        return;
    }

    if matches!(app.app_state.mode, AppMode::FileBrowser) {
        browser::handle_event(event, app).await;
        app.sync_torrent_file_preview();
        app.app_state.ui.needs_redraw = true;
        return;
    }

    dispatch_mode_event(event, app).await;
}

fn should_quit_on_ctrl_c(event: &CrosstermEvent, app: &mut App) -> bool {
    if let CrosstermEvent::Key(key) = event {
        if key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            app.app_state.should_quit = true;
            app.app_state.ui.needs_redraw = true;
            return true;
        }
    }
    false
}

fn handle_resize_event(event: &CrosstermEvent, app: &mut App) -> bool {
    if let CrosstermEvent::Resize(w, h) = event {
        app.app_state.screen_area = Rect::new(0, 0, *w, *h);
        app.app_state.ui.needs_redraw = true;
        return true;
    }
    false
}

fn should_debounce_escape(event: &CrosstermEvent) -> bool {
    if let CrosstermEvent::Key(key) = event {
        if key.kind == KeyEventKind::Press
            && key.code == KeyCode::Esc
            && key.modifiers == KeyModifiers::NONE
        {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;

            let last = GLOBAL_ESC_TIMESTAMP.load(Ordering::Relaxed);
            if now.saturating_sub(last) < 200 {
                return true;
            }

            GLOBAL_ESC_TIMESTAMP.store(now, Ordering::Relaxed);
        }
    }
    false
}

async fn dispatch_mode_event(event: CrosstermEvent, app: &mut App) {
    match app.app_state.mode {
        AppMode::Help => {
            help::handle_event_with_settings(event, &mut app.app_state, &app.client_configs);
        }
        AppMode::Journal => {
            journal::handle_event_with_shutdown(
                event,
                &mut app.app_state,
                &app.app_command_tx,
                &app.shutdown_tx,
            );
        }
        AppMode::TorrentManagement => {
            torrents::handle_event(event, app);
        }
        AppMode::PeerManagement => {
            peers::handle_event(event, &mut app.app_state);
        }
        AppMode::Welcome => {
            welcome::handle_event(event, &mut app.app_state);
        }
        AppMode::Normal => normal::handle_event(event, app).await,
        AppMode::PowerSaving => power::handle_event(event, &mut app.app_state),
        AppMode::Config => {
            if app.app_state.ui.config.active_pane == ConfigPane::Libraries {
                libraries::handle_event(event, app);
                return;
            }
            if app.app_state.ui.config.editing.is_none() {
                *app.app_state.ui.config.settings_edit = app.client_configs.clone();
            }
            let applied_settings = app.client_configs.clone();
            let shared_follower = app.is_current_shared_follower();
            let config_layout = crate::tui::layout::config::calculate_config_layout(
                app.app_state.screen_area,
                app.app_state.ui.config.settings_edit.ui_layout_mode,
            );
            let settings_update = config::handle_event(
                event,
                config::ConfigHandleContext {
                    mode: &mut app.app_state.mode,
                    settings_edit: &mut app.app_state.ui.config.settings_edit,
                    applied_settings: &applied_settings,
                    selected_index: &mut app.app_state.ui.config.selected_index,
                    items: app.app_state.ui.config.items.as_mut_slice(),
                    active_pane: &mut app.app_state.ui.config.active_pane,
                    editing: &mut app.app_state.ui.config.editing,
                    reset_confirmation: &mut app.app_state.ui.config.reset_confirmation,
                    shared_follower,
                    compact: config_layout.kind
                        == crate::tui::layout::config::ConfigLayoutKind::Compact,
                    app_command_tx: &app.app_command_tx,
                    shutdown_tx: &app.shutdown_tx,
                    file_browser_generation: &mut app.app_state.ui.file_browser.browser_generation,
                },
            );
            if let Some(settings) = settings_update {
                app.apply_config_update_from_ui(settings).await;
                *app.app_state.ui.config.settings_edit = app.client_configs.clone();
            }
            if app.app_state.ui.config.active_pane == ConfigPane::Libraries {
                app.app_state.ui.libraries.status_message = None;
                app.app_state.ui.libraries.input = None;
                libraries::refresh(app);
            }
        }
        AppMode::DeleteConfirm => {
            let _ = delete_confirm::handle_event(event, app);
        }
        AppMode::Rss => {
            rss::handle_event_with_shutdown(
                event,
                &mut app.app_state,
                &app.client_configs,
                &app.app_command_tx,
                &app.shutdown_tx,
            );
        }
        AppMode::FileBrowser => {}
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{
        AppState, FilePriority, PeerInfo, SelectedHeader, TorrentDisplayState, TorrentMetrics,
        TorrentPreviewPayload,
    };
    use crate::config::Settings;
    use crate::tui::layout::common::{ColumnId, PeerColumnId};
    use crate::tui::paste_burst::PasteBurst;
    use crate::tui::tree::RawNode;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Instant;

    static ESC_DEBOUNCE_TEST_LOCK: Mutex<()> = Mutex::new(());

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
