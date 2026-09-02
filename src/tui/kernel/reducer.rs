// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Top-level terminal-event translation and application-state reduction.

use super::effects::TuiEffect;
use super::paste_burst::FlushResult as PasteBurstFlushResult;
use super::state::AppMode;
use super::terminal_event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crate::app::AppState;
use crate::config::Settings;
use crate::tui::screens::{
    browser, config, delete_confirm, help, journal, normal, peers, power, rss, torrents, welcome,
};
use ratatui::prelude::Rect;
use std::sync::atomic::{AtomicU64, Ordering};
use web_time::{Instant, SystemTime, UNIX_EPOCH};

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

fn convert_burst_flush(flush: PasteBurstFlushResult) -> Vec<Event> {
    match flush {
        PasteBurstFlushResult::None | PasteBurstFlushResult::Buffered => Vec::new(),
        PasteBurstFlushResult::Text(text) => vec![Event::Paste(text)],
        PasteBurstFlushResult::Keys(keys) => keys.into_iter().map(Event::Key).collect(),
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
) -> Vec<TuiEffect> {
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
) -> Vec<TuiEffect> {
    let mut effects = Vec::new();
    match app_state.mode {
        AppMode::Help => help::handle_event_with_settings(event, app_state, settings),
        AppMode::Journal => {
            let reduced = journal::handle_event(event, app_state);
            effects.push(TuiEffect::Journal(reduced.effects));
        }
        AppMode::TorrentManagement => {
            let reduced = torrents::handle_event(event, app_state);
            if reduced.consumed {
                app_state.ui.needs_redraw = true;
            }
            effects.push(TuiEffect::TorrentManagement(reduced.effects));
        }
        AppMode::PeerManagement => peers::handle_event(event, app_state),
        AppMode::Welcome => welcome::handle_event(event, app_state),
        AppMode::Normal => {
            let reduced = normal::handle_event(event, app_state, settings.ui_layout_mode);
            if reduced.redraw || reduced.consumed {
                app_state.ui.needs_redraw = true;
            }
            effects.push(TuiEffect::Normal(reduced.effects));
        }
        AppMode::PowerSaving => power::handle_event(event, app_state),
        AppMode::Config => {
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
                    shared_follower,
                    compact: config_layout.kind
                        == crate::tui::layout::config::ConfigLayoutKind::Compact,
                    file_browser_generation: &mut app_state.ui.file_browser.browser_generation,
                },
            );
            if let Some(update) = reduced.settings_update {
                effects.push(TuiEffect::ApplyConfig(Box::new(update)));
            }
            effects.push(TuiEffect::Config(reduced.effects));
        }
        AppMode::DeleteConfirm => {
            let reduced = delete_confirm::handle_event(event, app_state);
            if reduced.consumed {
                app_state.ui.needs_redraw = true;
            }
            effects.push(TuiEffect::DeleteConfirm(reduced.effects));
        }
        AppMode::Rss => {
            let reduced = rss::handle_event(event, app_state, settings);
            effects.push(TuiEffect::Rss(reduced.effects));
        }
        AppMode::FileBrowser => {
            let browser_generation = app_state.ui.file_browser.browser_generation;
            let reduced = browser::handle_event(event, app_state);
            effects.push(TuiEffect::BrowserFs {
                browser_generation,
                effects: reduced.fs_effects,
            });
            effects.push(TuiEffect::BrowserDialog(reduced.dialog_effects));
            effects.push(TuiEffect::SyncTorrentFilePreview);
            app_state.ui.needs_redraw = true;
        }
    }
    effects
}
