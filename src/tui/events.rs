// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared terminal event buffering, translation, and application-state reduction.

use super::effects::{
    BrowserDialogEffect, BrowserFsEffect, BrowserTransition, ConfigEffect,
    ConfigNetworkInterfaceRefresh, DeleteConfirmEffect, JournalEffect, RssRuntimeEffect,
    RuntimeEffect, RuntimeOutcome, TorrentManagementEffect, UiEffect,
};
use super::input::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use super::state::{AppMode, ConfigItem, ConfigPane, TorrentControlState};
use crate::app::{AppState, RssScreen};
use crate::config::Settings;
use crate::tui::screens::{
    browser, config, delete_confirm, help, journal, normal, peers, power, rss, torrents, welcome,
};
use ratatui::prelude::Rect;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use strum::IntoEnumIterator;
use web_time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub(crate) struct PasteBurstState<const CHARACTER_INTERVAL_MS: u64> {
    queued_keys: Vec<KeyEvent>,
    queued_text: String,
    last_plain_char_at: Option<Instant>,
}

enum PasteBurstFlush {
    None,
    Buffered,
    Text(String),
    Keys(Vec<KeyEvent>),
}

impl<const CHARACTER_INTERVAL_MS: u64> PasteBurstState<CHARACTER_INTERVAL_MS> {
    const CHARACTER_INTERVAL: Duration = Duration::from_millis(CHARACTER_INTERVAL_MS);

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.last_plain_char_at
            .map(|instant| instant + Self::CHARACTER_INTERVAL)
    }

    fn has_pending(&self) -> bool {
        !self.queued_keys.is_empty()
    }

    fn pending_text(&self) -> &str {
        &self.queued_text
    }

    fn is_due(&self, now: Instant) -> bool {
        self.last_plain_char_at
            .is_some_and(|last| now.duration_since(last) > Self::CHARACTER_INTERVAL)
    }

    fn push_key(&mut self, key: KeyEvent, now: Instant) -> PasteBurstFlush {
        let stale_result = if self
            .last_plain_char_at
            .is_some_and(|last| now.duration_since(last) > Self::CHARACTER_INTERVAL)
        {
            self.drain_as_keys()
        } else {
            PasteBurstFlush::None
        };

        if let KeyCode::Char(ch) = key.code {
            self.queued_keys.push(key);
            self.queued_text.push(ch);
            self.last_plain_char_at = Some(now);
        }

        if matches!(stale_result, PasteBurstFlush::None) {
            PasteBurstFlush::Buffered
        } else {
            stale_result
        }
    }

    fn flush_if_due<F>(&mut self, now: Instant, should_treat_as_paste: F) -> PasteBurstFlush
    where
        F: FnOnce(&str) -> bool,
    {
        if !self.is_due(now) {
            return PasteBurstFlush::None;
        }
        self.finish_flush(should_treat_as_paste)
    }

    fn flush_now<F>(&mut self, should_treat_as_paste: F) -> PasteBurstFlush
    where
        F: FnOnce(&str) -> bool,
    {
        self.finish_flush(should_treat_as_paste)
    }

    fn clear(&mut self) {
        self.queued_keys.clear();
        self.queued_text.clear();
        self.last_plain_char_at = None;
    }

    fn finish_flush<F>(&mut self, should_treat_as_paste: F) -> PasteBurstFlush
    where
        F: FnOnce(&str) -> bool,
    {
        if self.queued_keys.is_empty() {
            self.clear();
            return PasteBurstFlush::None;
        }

        if should_treat_as_paste(&self.queued_text) {
            let text = std::mem::take(&mut self.queued_text);
            self.queued_keys.clear();
            self.last_plain_char_at = None;
            return PasteBurstFlush::Text(text);
        }

        self.drain_as_keys()
    }

    fn drain_as_keys(&mut self) -> PasteBurstFlush {
        if self.queued_keys.is_empty() {
            self.clear();
            return PasteBurstFlush::None;
        }

        let keys = std::mem::take(&mut self.queued_keys);
        self.queued_text.clear();
        self.last_plain_char_at = None;
        PasteBurstFlush::Keys(keys)
    }

    #[cfg(test)]
    fn flush_delay() -> Duration {
        Self::CHARACTER_INTERVAL + Duration::from_millis(1)
    }
}

#[cfg(not(windows))]
pub(crate) type PasteBurst = PasteBurstState<8>;
#[cfg(windows)]
pub(crate) type PasteBurst = PasteBurstState<30>;

pub(crate) static GLOBAL_ESC_TIMESTAMP: AtomicU64 = AtomicU64::new(0);

pub(crate) fn pending_paste_text_before_event<'a>(
    event: &Event,
    app_state: &'a AppState,
) -> Option<&'a str> {
    if should_ignore_event_for_paste_burst(event, app_state)
        || matches!(
            event,
            Event::Key(key) if should_buffer_paste_burst_key(app_state, *key)
        )
        || !app_state.ui.normal_paste_burst.has_pending()
    {
        return None;
    }

    Some(app_state.ui.normal_paste_burst.pending_text())
}

pub(crate) fn due_paste_text(app_state: &AppState, now: Instant) -> Option<&str> {
    app_state
        .ui
        .normal_paste_burst
        .is_due(now)
        .then(|| app_state.ui.normal_paste_burst.pending_text())
}

pub(crate) fn translate_event(
    event: Event,
    app_state: &mut AppState,
    now: Instant,
    pending_is_paste: bool,
) -> Vec<Event> {
    let mut translated = Vec::new();
    if should_ignore_event_for_paste_burst(&event, app_state) {
        return translated;
    }

    let buffered_key = match &event {
        Event::Key(key) if should_buffer_paste_burst_key(app_state, *key) => Some(*key),
        _ => None,
    };

    if let Some(key) = buffered_key {
        let flush = app_state.ui.normal_paste_burst.push_key(key, now);
        translated.extend(convert_burst_flush(flush));
        return translated;
    }

    if app_state.ui.normal_paste_burst.has_pending() {
        let flush = app_state
            .ui
            .normal_paste_burst
            .flush_now(|_| pending_is_paste);
        translated.extend(convert_burst_flush(flush));
    }

    translated.push(event);
    translated
}

pub(crate) fn flush_due_events(
    app_state: &mut AppState,
    now: Instant,
    pending_is_paste: bool,
) -> Vec<Event> {
    let flush = app_state
        .ui
        .normal_paste_burst
        .flush_if_due(now, |_| pending_is_paste);
    convert_burst_flush(flush)
}

fn convert_burst_flush(flush: PasteBurstFlush) -> Vec<Event> {
    match flush {
        PasteBurstFlush::None | PasteBurstFlush::Buffered => Vec::new(),
        PasteBurstFlush::Text(text) => vec![Event::Paste(text)],
        PasteBurstFlush::Keys(keys) => keys.into_iter().map(Event::Key).collect(),
    }
}

fn should_buffer_paste_burst_key(app_state: &AppState, key: KeyEvent) -> bool {
    matches!(app_state.mode, AppMode::Normal | AppMode::Welcome)
        && !app_state.ui.is_searching
        && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && matches!(key.code, KeyCode::Char(_))
        && !key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
}

fn should_ignore_event_for_paste_burst(event: &Event, app_state: &AppState) -> bool {
    let Event::Key(KeyEvent {
        code,
        kind: KeyEventKind::Release,
        ..
    }) = event
    else {
        return false;
    };

    !matches!(app_state.mode, AppMode::TorrentManagement)
        || app_state.ui.torrent_management.input_latch != Some(*code)
}

pub fn reduce_event(
    event: Event,
    app_state: &mut AppState,
    settings: &Settings,
    shared_follower: bool,
) -> Vec<RuntimeEffect> {
    if handle_resize_event(&event, app_state)
        || should_quit_on_ctrl_c(&event, app_state)
        || should_debounce_escape(&event)
    {
        return Vec::new();
    }
    dispatch_mode_event(event, app_state, settings, shared_follower)
}

fn should_quit_on_ctrl_c(event: &Event, app_state: &mut AppState) -> bool {
    if let Event::Key(key) = event {
        if key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            app_state.should_quit = true;
            app_state.ui.needs_redraw = true;
            return true;
        }
    }
    false
}

fn handle_resize_event(event: &Event, app_state: &mut AppState) -> bool {
    if let Event::Resize(w, h) = event {
        app_state.screen_area = Rect::new(0, 0, *w, *h);
        app_state.ui.needs_redraw = true;
        return true;
    }
    false
}

pub(crate) fn should_debounce_escape(event: &Event) -> bool {
    if let Event::Key(key) = event {
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

fn dispatch_mode_event(
    event: Event,
    app_state: &mut AppState,
    settings: &Settings,
    shared_follower: bool,
) -> Vec<RuntimeEffect> {
    let mut effects = Vec::new();
    match app_state.mode {
        AppMode::Help => help::handle_event_with_settings(event, app_state, settings),
        AppMode::Journal => {
            let reduced = journal::handle_event(event, app_state);
            for effect in reduced.effects {
                let JournalEffect::ReplaySource(path) = effect;
                effects.push(RuntimeEffect::ReplayJournalSource(path));
            }
        }
        AppMode::TorrentManagement => {
            let reduced = torrents::handle_event(event, app_state);
            if reduced.consumed {
                app_state.ui.needs_redraw = true;
            }
            apply_torrent_management_actions(
                app_state,
                reduced.effects,
                shared_follower,
                &mut effects,
            );
        }
        AppMode::PeerManagement => peers::handle_event(event, app_state),
        AppMode::Welcome => welcome::handle_event(event, app_state),
        AppMode::Normal => {
            let reduced = normal::handle_event(event, app_state, settings.ui_layout_mode);
            if reduced.redraw || reduced.consumed {
                app_state.ui.needs_redraw = true;
            }
            apply_normal_actions(app_state, settings, reduced.effects, &mut effects);
        }
        AppMode::PowerSaving => power::handle_event(event, app_state),
        AppMode::Config => {
            let shared_mode = app_state.runtime_paths.shared_mode;
            let editing_active = app_state.ui.config.editing.is_some();
            let interface_inventory = &app_state.ui.config.network_interface_inventory;
            let network_interfaces = interface_inventory.interfaces.as_slice();
            config::sync_settings_edit_from_applied(
                &mut app_state.ui.config.settings_edit,
                settings,
                editing_active,
                app_state.ui.config.network_interface_selection_pending,
                network_interfaces,
            );
            let config_layout = crate::tui::layout::config::calculate_config_layout(
                app_state.screen_area,
                app_state.ui.config.settings_edit.ui_layout_mode,
            );
            let reduced = config::handle_event(
                event,
                config::ConfigHandleContext {
                    mode: &mut app_state.mode,
                    anonymize: &mut app_state.anonymize_torrent_names,
                    settings_edit: &mut app_state.ui.config.settings_edit,
                    applied_settings: settings,
                    selected_index: &mut app_state.ui.config.selected_index,
                    items: app_state.ui.config.items.as_mut_slice(),
                    active_pane: &mut app_state.ui.config.active_pane,
                    editing: &mut app_state.ui.config.editing,
                    reset_confirmation: &mut app_state.ui.config.reset_confirmation,
                    network_interface_selection_pending: &mut app_state
                        .ui
                        .config
                        .network_interface_selection_pending,
                    network_interfaces,
                    shared_mode,
                    shared_follower,
                    compact: config_layout.kind
                        == crate::tui::layout::config::ConfigLayoutKind::Compact,
                    file_browser_generation: &mut app_state.ui.file_browser.browser_generation,
                },
            );
            if let Some(update) = reduced.settings_update {
                effects.push(RuntimeEffect::ApplyConfig(Box::new(update)));
            }
            apply_config_actions(app_state, reduced.effects, &mut effects);
        }
        AppMode::DeleteConfirm => {
            let reduced = delete_confirm::handle_event(event, app_state);
            if reduced.consumed {
                app_state.ui.needs_redraw = true;
            }
            apply_delete_confirm_actions(app_state, reduced.effects, shared_follower, &mut effects);
        }
        AppMode::Rss => {
            let reduced = rss::handle_event(event, app_state, settings);
            for effect in reduced.effects {
                effects.push(match effect {
                    RssRuntimeEffect::UpdateConfig(settings) => {
                        RuntimeEffect::UpdateRssConfig(settings)
                    }
                    RssRuntimeEffect::SyncNow => RuntimeEffect::SyncRss,
                    RssRuntimeEffect::DownloadPreview(item) => {
                        RuntimeEffect::DownloadRssPreview(item)
                    }
                });
            }
        }
        AppMode::FileBrowser => {
            let browser_generation = app_state.ui.file_browser.browser_generation;
            let reduced = browser::handle_event(event, app_state);
            apply_browser_fs_actions(browser_generation, reduced.fs_effects, &mut effects);
            apply_browser_dialog_actions(app_state, reduced.dialog_effects, &mut effects);
            effects.push(RuntimeEffect::SyncTorrentFilePreview);
            app_state.ui.needs_redraw = true;
        }
    }
    effects
}

pub(crate) fn apply_browser_fs_actions(
    browser_generation: u64,
    actions: Vec<BrowserFsEffect>,
    effects: &mut Vec<RuntimeEffect>,
) {
    effects.extend(actions.into_iter().map(|action| match action {
        BrowserFsEffect::FetchFileTree {
            path,
            browser_mode,
            highlight_path,
        } => RuntimeEffect::FetchFileTree {
            browser_generation,
            path,
            browser_mode,
            preserve_browser_mode: true,
            highlight_path,
        },
    }));
}

pub(crate) fn apply_browser_dialog_actions(
    app_state: &mut AppState,
    actions: Vec<BrowserDialogEffect>,
    effects: &mut Vec<RuntimeEffect>,
) {
    for action in actions {
        match action {
            BrowserDialogEffect::ExecuteConfirmDecision(decision) => {
                effects.push(RuntimeEffect::ConfirmBrowserSelection(decision));
            }
            BrowserDialogEffect::ToConfig(config) => {
                app_state.ui.config = config;
                apply_browser_transition_state(app_state, BrowserTransition::ToConfig);
                effects.push(RuntimeEffect::RefreshConfigNetworkInterfaces(
                    ConfigNetworkInterfaceRefresh::OnOpen,
                ));
            }
            BrowserDialogEffect::CleanupPendingLink => {
                queue_pending_preview_cleanup(app_state, effects);
            }
            BrowserDialogEffect::ToNormalAndClearPending => {
                let clear_preview_marker = !app_state.pending_torrent_link.is_empty();
                apply_browser_transition_state(app_state, BrowserTransition::Close);
                app_state.pending_torrent_path = None;
                app_state.pending_torrent_link.clear();
                if clear_preview_marker {
                    app_state.pending_magnet_preview_info_hash = None;
                }
                app_state.pending_manual_ingest = None;
            }
            BrowserDialogEffect::ClearSearch => {
                app_state.ui.file_browser.search_state = crate::app::BrowserSearchState::Closed;
                app_state.ui.file_browser.search_query.clear();
            }
        }
    }
}

fn queue_pending_preview_cleanup(app_state: &mut AppState, effects: &mut Vec<RuntimeEffect>) {
    if let Some(info_hash) = app_state.pending_magnet_preview_info_hash.take() {
        effects.push(RuntimeEffect::CleanupPendingPreview(info_hash));
    }
}

pub(crate) fn apply_normal_actions(
    app_state: &mut AppState,
    settings: &Settings,
    actions: Vec<UiEffect>,
    effects: &mut Vec<RuntimeEffect>,
) {
    for action in actions {
        match action {
            UiEffect::ToPowerSaving => app_state.mode = AppMode::PowerSaving,
            UiEffect::ToDeleteConfirm => app_state.mode = AppMode::DeleteConfirm,
            UiEffect::OpenAddTorrentFileBrowser => {
                effects.push(RuntimeEffect::OpenAddTorrentFileBrowser);
            }
            UiEffect::OpenExistingTorrentFileBrowser(info_hash) => {
                effects.push(RuntimeEffect::OpenExistingTorrentFileBrowser(info_hash));
            }
            UiEffect::OpenConfigScreen => {
                open_config_screen_state(app_state, settings);
                effects.push(RuntimeEffect::RefreshConfigNetworkInterfaces(
                    ConfigNetworkInterfaceRefresh::OnOpen,
                ));
            }
            UiEffect::OpenRssScreen => open_rss_screen_state(app_state),
            UiEffect::OpenJournalScreen => open_journal_screen_state(app_state),
            UiEffect::OpenPeerManagementScreen => {
                open_peer_management_screen_state(app_state);
                effects.push(RuntimeEffect::RefreshPeerManagement);
            }
            UiEffect::OpenTorrentManagementScreen => {
                open_torrent_management_screen_state(app_state);
            }
            UiEffect::BroadcastManagerDataRate(rate) => {
                effects.push(RuntimeEffect::BroadcastManagerDataRate(rate));
            }
            UiEffect::ApplyThemePrev => effects.push(RuntimeEffect::ApplyThemePrevious),
            UiEffect::ApplyThemeNext => effects.push(RuntimeEffect::ApplyThemeNext),
            UiEffect::PersistVisualizationSelections => {
                effects.push(RuntimeEffect::PersistVisualizationSelections);
            }
            UiEffect::SendPause(info_hash) => {
                effects.push(RuntimeEffect::SubmitControlRequest(
                    crate::integrations::control::ControlRequest::Pause {
                        info_hash_hex: hex::encode(info_hash),
                    },
                ));
            }
            UiEffect::SendResume(info_hash) => {
                effects.push(RuntimeEffect::SubmitControlRequest(
                    crate::integrations::control::ControlRequest::Resume {
                        info_hash_hex: hex::encode(info_hash),
                    },
                ));
            }
            UiEffect::OpenHelpScreen => app_state.mode = AppMode::Help,
            UiEffect::HandlePastedText(text) => {
                effects.push(RuntimeEffect::HandlePastedText(text));
            }
        }
    }
}

fn apply_config_actions(
    app_state: &mut AppState,
    actions: Vec<ConfigEffect>,
    effects: &mut Vec<RuntimeEffect>,
) {
    for action in actions {
        match action {
            ConfigEffect::OpenPathBrowser {
                preferred_path,
                browser_mode,
            } => {
                app_state.ui.file_browser.browser_generation =
                    app_state.ui.file_browser.browser_generation.wrapping_add(1);
                effects.push(RuntimeEffect::OpenConfigPathBrowser {
                    browser_generation: app_state.ui.file_browser.browser_generation,
                    preferred_path,
                    browser_mode: *browser_mode,
                });
            }
            ConfigEffect::RefreshNetworkInterfaces => {
                effects.push(RuntimeEffect::RefreshConfigNetworkInterfaces(
                    ConfigNetworkInterfaceRefresh::Explicit,
                ));
            }
            ConfigEffect::ApplySettings => {}
        }
    }
}

fn apply_delete_confirm_actions(
    app_state: &mut AppState,
    actions: Vec<DeleteConfirmEffect>,
    shared_follower: bool,
    effects: &mut Vec<RuntimeEffect>,
) {
    for action in actions {
        match action {
            DeleteConfirmEffect::SendManagerCommand {
                info_hash,
                with_files,
            } => effects.push(RuntimeEffect::SubmitControlRequest(
                crate::integrations::control::ControlRequest::Delete {
                    info_hash_hex: hex::encode(info_hash),
                    delete_files: with_files,
                },
            )),
            DeleteConfirmEffect::MarkDeleting { info_hash } => {
                mark_torrent_deleting_state(app_state, &info_hash, shared_follower);
            }
            DeleteConfirmEffect::ToNormal => app_state.mode = AppMode::Normal,
        }
    }
}

fn apply_torrent_management_actions(
    app_state: &mut AppState,
    actions: Vec<TorrentManagementEffect>,
    shared_follower: bool,
    effects: &mut Vec<RuntimeEffect>,
) {
    for action in actions {
        match action {
            TorrentManagementEffect::ToNormal => app_state.mode = AppMode::Normal,
            TorrentManagementEffect::SubmitControlRequest(request) => {
                effects.push(RuntimeEffect::SubmitControlRequest(request));
            }
            TorrentManagementEffect::MarkControlState {
                info_hash,
                state,
                delete_files,
            } => mark_torrent_control_state(
                app_state,
                &info_hash,
                state,
                delete_files,
                shared_follower,
            ),
            TorrentManagementEffect::OpenExistingTorrentFileBrowser(info_hash) => {
                effects.push(RuntimeEffect::OpenExistingTorrentFileBrowser(info_hash));
            }
        }
    }
}

pub(crate) fn finish_config_apply_state(app_state: &mut AppState, settings: &Settings) {
    *app_state.ui.config.settings_edit = settings.clone();
    app_state.ui.config.network_interface_selection_pending = false;
}

pub(crate) fn apply_runtime_outcome(
    app_state: &mut AppState,
    outcome: RuntimeOutcome,
) -> Vec<RuntimeEffect> {
    match outcome {
        RuntimeOutcome::BrowserTransition(transition) => {
            apply_browser_transition_state(app_state, transition);
            if transition == BrowserTransition::ToConfig {
                vec![RuntimeEffect::RefreshConfigNetworkInterfaces(
                    ConfigNetworkInterfaceRefresh::OnOpen,
                )]
            } else {
                Vec::new()
            }
        }
        RuntimeOutcome::BrowserConfig(config) => {
            app_state.ui.config = config;
            apply_browser_transition_state(app_state, BrowserTransition::ToConfig);
            vec![RuntimeEffect::RefreshConfigNetworkInterfaces(
                ConfigNetworkInterfaceRefresh::OnOpen,
            )]
        }
        RuntimeOutcome::ConfigApplied(settings) => {
            finish_config_apply_state(app_state, &settings);
            Vec::new()
        }
    }
}

/// Applies the state-only portion of a browser transition.
///
/// Target-specific follow-up is represented as another `RuntimeEffect` by
/// `apply_runtime_outcome`; platform executors never apply this transition.
pub(crate) fn apply_browser_transition_state(
    app_state: &mut AppState,
    transition: BrowserTransition,
) {
    match transition {
        BrowserTransition::ToNormal => {
            app_state.ui.file_browser.invalidate_browser_generation();
            app_state
                .ui
                .file_browser
                .return_to_torrent_management_on_close = false;
            app_state.mode = AppMode::Normal;
        }
        BrowserTransition::ToConfig => {
            app_state.ui.file_browser.invalidate_browser_generation();
            app_state
                .ui
                .file_browser
                .return_to_torrent_management_on_close = false;
            app_state.mode = AppMode::Config;
        }
        BrowserTransition::Close => {
            let return_to_management = app_state
                .ui
                .file_browser
                .return_to_torrent_management_on_close;
            app_state.ui.file_browser.invalidate_browser_generation();
            app_state
                .ui
                .file_browser
                .return_to_torrent_management_on_close = false;
            app_state.mode = if return_to_management {
                AppMode::TorrentManagement
            } else {
                AppMode::Normal
            };
        }
    }
}

pub(crate) fn open_config_screen_state(app_state: &mut AppState, settings: &Settings) {
    *app_state.ui.config.settings_edit = settings.clone();
    app_state.ui.config.selected_index = 0;
    app_state.ui.config.items = ConfigItem::iter().collect();
    app_state.ui.config.active_pane = ConfigPane::Settings;
    app_state.ui.config.editing = None;
    app_state.ui.config.network_interface_selection_pending = false;
    app_state.mode = AppMode::Config;
}

pub(crate) fn open_rss_screen_state(app_state: &mut AppState) {
    app_state.ui.rss.active_screen = RssScreen::Unified;
    app_state.mode = AppMode::Rss;
}

pub(crate) fn open_journal_screen_state(app_state: &mut AppState) {
    app_state.ui.journal.selected_index = 0;
    app_state.ui.journal.scroll_offset = 0;
    app_state.mode = AppMode::Journal;
}

pub(crate) fn open_peer_management_screen_state(app_state: &mut AppState) {
    app_state.ui.peer_management.selected_index = 0;
    app_state.ui.peer_management.show_details = false;
    app_state.ui.peer_management.status_message = None;
    app_state.mode = AppMode::PeerManagement;
}

pub(crate) fn open_torrent_management_screen_state(app_state: &mut AppState) {
    app_state.ui.torrent_management.status_message = None;
    app_state.ui.torrent_management.review_scroll_offset = 0;
    app_state.mode = AppMode::TorrentManagement;
    torrents::initialize_torrent_management_cursor(app_state);
}

pub(crate) fn mark_torrent_deleting_state(
    app_state: &mut AppState,
    info_hash: &[u8],
    shared_follower: bool,
) {
    if shared_follower {
        return;
    }
    if let Some(torrent) = app_state.torrents.get_mut(info_hash) {
        torrent.latest_state.torrent_control_state = TorrentControlState::Deleting;
        torrent.latest_state.delete_files = app_state.ui.delete_confirm.with_files;
    }
}

pub(crate) fn mark_torrent_control_state(
    app_state: &mut AppState,
    info_hash: &[u8],
    state: TorrentControlState,
    delete_files: bool,
    shared_follower: bool,
) {
    if shared_follower {
        return;
    }
    if let Some(torrent) = app_state.torrents.get_mut(info_hash) {
        torrent.latest_state.torrent_control_state = state;
        torrent.latest_state.delete_files = delete_files;
    }
}

#[cfg(test)]
mod paste_burst_tests {
    use super::*;

    type TestPasteBurst = PasteBurstState<8>;

    #[test]
    fn single_key_flushes_as_keys_when_not_paste() {
        let mut burst = TestPasteBurst::default();
        let start = Instant::now();
        let result = burst.push_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), start);
        assert!(matches!(result, PasteBurstFlush::Buffered));

        let result = burst.flush_if_due(start + TestPasteBurst::flush_delay(), |_| false);
        assert!(matches!(result, PasteBurstFlush::Keys(keys) if keys.len() == 1));
    }

    #[test]
    fn magnet_like_burst_flushes_as_text() {
        let mut burst = TestPasteBurst::default();
        let start = Instant::now();
        for (offset, ch) in ['m', 'a', 'g', 'n', 'e', 't', ':'].into_iter().enumerate() {
            let _ = burst.push_key(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                start + Duration::from_millis(offset as u64),
            );
        }

        let result = burst.flush_if_due(
            start + Duration::from_millis(6) + TestPasteBurst::flush_delay(),
            |text| text.starts_with("magnet:"),
        );
        assert!(matches!(result, PasteBurstFlush::Text(text) if text == "magnet:"));
    }

    #[test]
    fn interruption_flushes_pending_keys_without_leaking_state() {
        let mut burst = TestPasteBurst::default();
        let start = Instant::now();
        let _ = burst.push_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), start);
        let _ = burst.push_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            start + Duration::from_millis(1),
        );

        let result = burst.flush_now(|_| false);
        assert!(matches!(result, PasteBurstFlush::Keys(keys) if keys.len() == 2));
        assert!(!burst.has_pending());
    }
}

#[cfg(test)]
use super::input::Event as CrosstermEvent;
#[cfg(test)]
use crate::app::App;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{
        AppCommand, AppState, FileBrowserMode, FileMetadata, FilePriority, PeerInfo,
        SelectedHeader, TorrentControlState, TorrentDisplayState, TorrentManagementPendingCommand,
        TorrentMetrics, TorrentPreviewPayload,
    };
    use crate::config::Settings;
    use crate::integrations::control::ControlRequest;
    use crate::tui::input::{KeyCode, KeyEvent, KeyModifiers};
    use crate::tui::layout::common::{ColumnId, PeerColumnId};
    use crate::tui::runtime::{
        flush_pending_paste_burst_at, handle_event as execute_handle_event, handle_event_at,
    };
    use crate::tui::screens::{browser, normal};
    use crate::tui::tree::RawNode;
    use ratatui::prelude::Rect;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{Duration, Instant, UNIX_EPOCH};

    static ESC_DEBOUNCE_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn state_only_normal_actions_do_not_cross_the_runtime_boundary() {
        let mut app_state = AppState::default();
        let mut effects = Vec::new();

        apply_normal_actions(
            &mut app_state,
            &Settings::default(),
            vec![UiEffect::ToPowerSaving],
            &mut effects,
        );

        assert!(matches!(app_state.mode, AppMode::PowerSaving));
        assert!(effects.is_empty());
    }

    #[test]
    fn peer_screen_transition_is_shared_and_only_refresh_crosses_runtime_boundary() {
        let mut app_state = AppState::default();
        app_state.ui.peer_management.selected_index = 9;
        app_state.ui.peer_management.show_details = true;
        let mut effects = Vec::new();

        apply_normal_actions(
            &mut app_state,
            &Settings::default(),
            vec![UiEffect::OpenPeerManagementScreen],
            &mut effects,
        );

        assert!(matches!(app_state.mode, AppMode::PeerManagement));
        assert_eq!(app_state.ui.peer_management.selected_index, 0);
        assert!(!app_state.ui.peer_management.show_details);
        assert!(matches!(
            effects.as_slice(),
            [RuntimeEffect::RefreshPeerManagement]
        ));
    }

    #[test]
    fn delete_confirmation_applies_navigation_before_emitting_control_request() {
        let mut app_state = AppState {
            mode: AppMode::DeleteConfirm,
            ..AppState::default()
        };
        let mut effects = Vec::new();

        apply_delete_confirm_actions(
            &mut app_state,
            vec![
                DeleteConfirmEffect::SendManagerCommand {
                    info_hash: vec![7; 20],
                    with_files: true,
                },
                DeleteConfirmEffect::ToNormal,
            ],
            false,
            &mut effects,
        );

        assert!(matches!(app_state.mode, AppMode::Normal));
        assert!(matches!(
            effects.as_slice(),
            [RuntimeEffect::SubmitControlRequest(
                ControlRequest::Delete {
                    delete_files: true,
                    ..
                }
            )]
        ));
    }

    #[test]
    fn browser_close_clears_shared_state_and_emits_only_preview_cleanup() {
        let mut app_state = AppState {
            mode: AppMode::FileBrowser,
            pending_torrent_link: "magnet:?xt=urn:btih:fixture".to_string(),
            pending_magnet_preview_info_hash: Some(vec![3; 20]),
            ..AppState::default()
        };
        let mut effects = Vec::new();

        apply_browser_dialog_actions(
            &mut app_state,
            vec![
                BrowserDialogEffect::CleanupPendingLink,
                BrowserDialogEffect::ClearSearch,
                BrowserDialogEffect::ToNormalAndClearPending,
            ],
            &mut effects,
        );

        assert!(matches!(app_state.mode, AppMode::Normal));
        assert!(app_state.pending_torrent_link.is_empty());
        assert!(app_state.pending_magnet_preview_info_hash.is_none());
        assert!(matches!(
            effects.as_slice(),
            [RuntimeEffect::CleanupPendingPreview(info_hash)] if info_hash == &vec![3; 20]
        ));
    }

    #[test]
    fn browser_close_preserves_unrelated_pending_preview_marker() {
        let mut app_state = AppState {
            mode: AppMode::FileBrowser,
            pending_magnet_preview_info_hash: Some(vec![5; 20]),
            ..AppState::default()
        };
        let mut effects = Vec::new();

        apply_browser_dialog_actions(
            &mut app_state,
            vec![BrowserDialogEffect::ToNormalAndClearPending],
            &mut effects,
        );

        assert_eq!(
            app_state.pending_magnet_preview_info_hash,
            Some(vec![5; 20])
        );
        assert!(effects.is_empty());
    }

    #[test]
    fn runtime_browser_outcome_reenters_the_shared_reducer() {
        let mut app_state = AppState {
            mode: AppMode::FileBrowser,
            ..AppState::default()
        };

        let follow_up = apply_runtime_outcome(
            &mut app_state,
            RuntimeOutcome::BrowserTransition(BrowserTransition::ToConfig),
        );

        assert!(matches!(app_state.mode, AppMode::Config));
        assert!(matches!(
            follow_up.as_slice(),
            [RuntimeEffect::RefreshConfigNetworkInterfaces(
                ConfigNetworkInterfaceRefresh::OnOpen
            )]
        ));
    }

    #[test]
    fn applied_config_outcome_updates_shared_editor_state() {
        let mut app_state = AppState::default();
        app_state.ui.config.network_interface_selection_pending = true;
        let applied = Settings {
            client_port: 7_373,
            ..Settings::default()
        };

        let follow_up = apply_runtime_outcome(
            &mut app_state,
            RuntimeOutcome::ConfigApplied(applied.clone()),
        );

        assert_eq!(*app_state.ui.config.settings_edit, applied);
        assert!(!app_state.ui.config.network_interface_selection_pending);
        assert!(follow_up.is_empty());
    }

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
