// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{AppCommand, AppMode, AppState, RssScreen, RssSectionFocus};
use crate::config::RssFilterMode;
use crate::tui::formatters::centered_rect;
use crate::tui::screen_context::ScreenContext;
use chrono::{DateTime, Local, Utc};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use ratatui::crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEventKind};
use ratatui::{prelude::*, widgets::*};
use reqwest::Url;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[derive(Clone, Debug, PartialEq)]
pub enum RssAction {
    ToNormal,
    ToggleHistory,
    FocusNext,
    MoveUp,
    MoveDown,
    TriggerSync,
    InsertChar(char),
    Backspace,
    CommitInput,
    CancelInput,
    AddEntry,
    DeleteEntry,
    ConfirmDeleteEntry,
    CancelDeleteEntry,
    ToggleFeedEnabled,
    StartSearch,
    DownloadSelectedExplorer,
    ToggleFilterMode,
}

#[derive(Default)]
pub struct RssReduceResult {
    pub effects: Vec<RssAction>,
}

fn map_key_to_rss_action(
    key_code: KeyCode,
    key_kind: KeyEventKind,
    app_state: &AppState,
) -> Option<RssAction> {
    if key_kind != KeyEventKind::Press {
        return None;
    }

    if app_state.ui.rss.delete_confirm_armed {
        return match key_code {
            KeyCode::Char('Y') => Some(RssAction::ConfirmDeleteEntry),
            KeyCode::Esc | KeyCode::Char('q') => Some(RssAction::CancelDeleteEntry),
            _ => None,
        };
    }

    if app_state.ui.rss.is_editing || app_state.ui.rss.is_searching {
        return match key_code {
            KeyCode::Esc => Some(RssAction::CancelInput),
            KeyCode::Enter => Some(RssAction::CommitInput),
            KeyCode::Backspace => Some(RssAction::Backspace),
            KeyCode::Tab
                if app_state.ui.rss.is_editing
                    && matches!(app_state.ui.rss.focused_section, RssSectionFocus::Filters) =>
            {
                Some(RssAction::ToggleFilterMode)
            }
            KeyCode::Char(c) => Some(RssAction::InsertChar(c)),
            _ => None,
        };
    }

    match key_code {
        KeyCode::Esc | KeyCode::Char('q') => Some(RssAction::ToNormal),
        KeyCode::Char('h') => Some(RssAction::ToggleHistory),
        KeyCode::Tab => Some(RssAction::FocusNext),
        KeyCode::Char('s') => Some(RssAction::TriggerSync),
        KeyCode::Char('a') => Some(RssAction::AddEntry),
        KeyCode::Char('D') => Some(RssAction::DeleteEntry),
        KeyCode::Char(' ') => Some(RssAction::ToggleFeedEnabled),
        KeyCode::Char('/') => Some(RssAction::StartSearch),
        KeyCode::Char('Y') => Some(RssAction::DownloadSelectedExplorer),
        KeyCode::Up | KeyCode::Char('k') => Some(RssAction::MoveUp),
        KeyCode::Down | KeyCode::Char('j') => Some(RssAction::MoveDown),
        _ => None,
    }
}

fn reduce_rss_action(action: RssAction) -> RssReduceResult {
    RssReduceResult {
        effects: vec![action],
    }
}

fn next_focus(current: RssSectionFocus) -> RssSectionFocus {
    match current {
        RssSectionFocus::Links => RssSectionFocus::Filters,
        RssSectionFocus::Filters => RssSectionFocus::Explorer,
        RssSectionFocus::Explorer => RssSectionFocus::Links,
    }
}

fn selected_index_mut(app_state: &mut AppState) -> &mut usize {
    if matches!(app_state.ui.rss.active_screen, RssScreen::History) {
        return &mut app_state.ui.rss.selected_history_index;
    }

    match app_state.ui.rss.focused_section {
        RssSectionFocus::Links => &mut app_state.ui.rss.selected_feed_index,
        RssSectionFocus::Filters => &mut app_state.ui.rss.selected_filter_index,
        RssSectionFocus::Explorer => &mut app_state.ui.rss.selected_explorer_index,
    }
}

fn current_list_len(app_state: &AppState, settings: &crate::config::Settings) -> usize {
    if matches!(app_state.ui.rss.active_screen, RssScreen::History) {
        return filtered_history_entries(
            &app_state.rss_runtime.history,
            &app_state.ui.rss.search_query,
        )
        .len();
    }

    match app_state.ui.rss.focused_section {
        RssSectionFocus::Links => settings.rss.feeds.len(),
        RssSectionFocus::Filters => settings.rss.filters.len(),
        RssSectionFocus::Explorer => app_state.rss_derived.explorer_items.len(),
    }
}

fn sorted_feed_indices(settings: &crate::config::Settings) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..settings.rss.feeds.len()).collect();
    indices.sort_by(|a, b| {
        settings.rss.feeds[*a]
            .url
            .to_lowercase()
            .cmp(&settings.rss.feeds[*b].url.to_lowercase())
    });
    indices
}

fn selected_feed_actual_idx(
    settings: &crate::config::Settings,
    selected_display_idx: usize,
) -> Option<usize> {
    sorted_feed_indices(settings)
        .get(selected_display_idx)
        .copied()
}

fn sorted_filter_indices(settings: &crate::config::Settings) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..settings.rss.filters.len()).collect();
    indices.sort_by(|a, b| {
        settings.rss.filters[*a]
            .query
            .to_lowercase()
            .cmp(&settings.rss.filters[*b].query.to_lowercase())
    });
    indices
}

fn selected_filter_actual_idx(
    settings: &crate::config::Settings,
    selected_display_idx: usize,
) -> Option<usize> {
    sorted_filter_indices(settings)
        .get(selected_display_idx)
        .copied()
}

#[derive(Clone)]
struct FilterSpec {
    query: String,
    mode: RssFilterMode,
}

struct PreparedFilter {
    mode: RssFilterMode,
    query_lc: String,
    regex: Option<regex::Regex>,
}
fn prepare_filter(query: &str, mode: RssFilterMode) -> Option<PreparedFilter> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return None;
    }

    let regex = if matches!(mode, RssFilterMode::Regex) {
        regex::RegexBuilder::new(trimmed)
            .case_insensitive(true)
            .build()
            .ok()
    } else {
        None
    };

    Some(PreparedFilter {
        mode,
        query_lc: trimmed.to_lowercase(),
        regex,
    })
}

fn prepared_filter_matches(
    title: &str,
    title_lc: &str,
    filter: &PreparedFilter,
    matcher: &SkimMatcherV2,
) -> bool {
    match filter.mode {
        RssFilterMode::Fuzzy => matcher.fuzzy_match(title_lc, &filter.query_lc).is_some(),
        RssFilterMode::Regex => filter.regex.as_ref().is_some_and(|re| re.is_match(title)),
    }
}

fn enabled_filters(settings: &crate::config::Settings) -> Vec<FilterSpec> {
    settings
        .rss
        .filters
        .iter()
        .filter(|f| f.enabled)
        .map(|f| FilterSpec {
            query: f.query.trim().to_string(),
            mode: f.mode,
        })
        .filter(|f| !f.query.is_empty())
        .collect()
}

fn filter_already_exists(
    settings: &crate::config::Settings,
    query: &str,
    mode: RssFilterMode,
) -> bool {
    let normalized = query.trim();
    settings
        .rss
        .filters
        .iter()
        .any(|f| f.mode == mode && f.query.trim().eq_ignore_ascii_case(normalized))
}

fn filter_matches_title(
    title: &str,
    filter_query: &str,
    mode: RssFilterMode,
    matcher: &SkimMatcherV2,
) -> bool {
    let Some(filter) = prepare_filter(filter_query, mode) else {
        return false;
    };
    let title_lc = title.to_lowercase();
    prepared_filter_matches(title, &title_lc, &filter, matcher)
}

fn explorer_should_be_greyed_out(settings: &crate::config::Settings) -> bool {
    settings.rss.filters.iter().all(|f| !f.enabled)
}

fn explorer_effective_greyed_out(app_state: &AppState, settings: &crate::config::Settings) -> bool {
    let is_creating_filter = app_state.ui.rss.is_editing
        && matches!(app_state.ui.rss.focused_section, RssSectionFocus::Filters);
    let has_draft = !app_state.ui.rss.edit_buffer.trim().is_empty();
    explorer_should_be_greyed_out(settings) && !(is_creating_filter && has_draft)
}

fn is_valid_feed_url(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    if let Some(host) = url.host_str() {
        if host.eq_ignore_ascii_case("localhost") {
            return false;
        }
        if let Ok(ip) = host.parse::<IpAddr>() {
            match ip {
                IpAddr::V4(v4) => {
                    if v4.is_private()
                        || v4.is_loopback()
                        || v4.is_link_local()
                        || v4.is_multicast()
                        || v4.is_broadcast()
                        || v4.is_documentation()
                        || v4.is_unspecified()
                    {
                        return false;
                    }
                }
                IpAddr::V6(v6) => {
                    if v6.is_loopback()
                        || v6.is_multicast()
                        || v6.is_unspecified()
                        || v6.is_unique_local()
                        || v6.is_unicast_link_local()
                    {
                        return false;
                    }
                }
            }
        }
    }
    true
}

fn truncate_with_ellipsis(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let char_count = input.chars().count();
    if char_count <= max_chars {
        return input.to_string();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let mut out = String::new();
    for ch in input.chars().take(max_chars - 3) {
        out.push(ch);
    }
    out.push_str("...");
    out
}

fn execute_rss_effects(
    app_state: &mut AppState,
    settings: &crate::config::Settings,
    app_command_tx: &mpsc::Sender<AppCommand>,
    effects: Vec<RssAction>,
) {
    if app_state.rss_derived.explorer_items.is_empty()
        && !app_state.rss_runtime.preview_items.is_empty()
    {
        // Lazy warm-up to avoid full derived recompute on every key press.
        recompute_rss_derived(app_state, settings);
    }

    fn set_rss_status(app_state: &mut AppState, message: impl Into<String>) {
        app_state.ui.rss.status_message = Some(message.into());
    }
    fn try_update_config(
        app_state: &mut AppState,
        app_command_tx: &mpsc::Sender<AppCommand>,
        new_settings: crate::config::Settings,
        success_message: Option<&str>,
    ) -> bool {
        if app_command_tx
            .try_send(AppCommand::UpdateConfig(new_settings))
            .is_err()
        {
            set_rss_status(app_state, "RSS settings enqueue failed");
            return false;
        }
        if let Some(message) = success_message {
            set_rss_status(app_state, message);
        }
        true
    }

    let mut recompute_needed = false;
    for effect in effects {
        match effect {
            RssAction::ToNormal => app_state.mode = AppMode::Normal,
            RssAction::ToggleHistory => {
                if matches!(app_state.ui.rss.active_screen, RssScreen::History) {
                    app_state.ui.rss.active_screen = RssScreen::Unified;
                    app_state.ui.rss.focused_section = RssSectionFocus::Explorer;
                } else {
                    app_state.ui.rss.active_screen = RssScreen::History;
                }
            }
            RssAction::FocusNext => {
                if matches!(app_state.ui.rss.active_screen, RssScreen::Unified) {
                    app_state.ui.rss.focused_section = next_focus(app_state.ui.rss.focused_section);
                }
            }
            RssAction::ToggleFilterMode => {
                if app_state.ui.rss.is_editing
                    && matches!(app_state.ui.rss.focused_section, RssSectionFocus::Filters)
                {
                    app_state.ui.rss.add_filter_mode = match app_state.ui.rss.add_filter_mode {
                        RssFilterMode::Fuzzy => RssFilterMode::Regex,
                        RssFilterMode::Regex => RssFilterMode::Fuzzy,
                    };
                    recompute_needed = true;
                }
            }
            RssAction::MoveUp => {
                let len = current_list_len(app_state, settings);
                let index = selected_index_mut(app_state);
                if len > 0 {
                    *index = index.saturating_sub(1);
                } else {
                    *index = 0;
                }
            }
            RssAction::MoveDown => {
                let len = current_list_len(app_state, settings);
                let index = selected_index_mut(app_state);
                if len > 0 {
                    *index = (*index + 1).min(len - 1);
                } else {
                    *index = 0;
                }
            }
            RssAction::TriggerSync => {
                let now = Instant::now();
                if let Some(last) = app_state.ui.rss.last_sync_request_at {
                    if now.duration_since(last) < Duration::from_secs(1) {
                        set_rss_status(app_state, "RSS sync throttled");
                        continue;
                    }
                }
                app_state.ui.rss.last_sync_request_at = Some(now);

                if !settings.rss.enabled {
                    let mut new_settings = settings.clone();
                    new_settings.rss.enabled = true;
                    if !try_update_config(app_state, app_command_tx, new_settings, None) {
                        continue;
                    }
                }
                if app_command_tx.try_send(AppCommand::RssSyncNow).is_err() {
                    set_rss_status(app_state, "RSS sync enqueue failed");
                } else {
                    set_rss_status(app_state, "RSS sync requested");
                }
            }
            RssAction::InsertChar(c) => {
                if app_state.ui.rss.is_editing {
                    app_state.ui.rss.edit_buffer.push(c);
                    if matches!(app_state.ui.rss.focused_section, RssSectionFocus::Filters) {
                        app_state.ui.rss.filter_draft = app_state.ui.rss.edit_buffer.clone();
                        recompute_needed = true;
                    }
                } else if app_state.ui.rss.is_searching {
                    app_state.ui.rss.search_query.push(c);
                    recompute_needed = true;
                }
            }
            RssAction::Backspace => {
                if app_state.ui.rss.is_editing {
                    app_state.ui.rss.edit_buffer.pop();
                    if matches!(app_state.ui.rss.focused_section, RssSectionFocus::Filters) {
                        app_state.ui.rss.filter_draft = app_state.ui.rss.edit_buffer.clone();
                        recompute_needed = true;
                    }
                } else if app_state.ui.rss.is_searching {
                    app_state.ui.rss.search_query.pop();
                    recompute_needed = true;
                }
            }
            RssAction::CommitInput => {
                if app_state.ui.rss.is_editing {
                    let value = app_state.ui.rss.edit_buffer.trim().to_string();
                    if !value.is_empty() {
                        let mut new_settings = settings.clone();
                        match app_state.ui.rss.focused_section {
                            RssSectionFocus::Links => {
                                if !is_valid_feed_url(&value) {
                                    set_rss_status(app_state, "Invalid feed URL (use http/https)");
                                    app_state.ui.rss.is_editing = false;
                                    app_state.ui.rss.edit_buffer.clear();
                                    continue;
                                }
                                new_settings.rss.enabled = true;
                                new_settings.rss.feeds.push(crate::config::RssFeed {
                                    url: value,
                                    enabled: true,
                                });
                            }
                            RssSectionFocus::Filters => {
                                if matches!(app_state.ui.rss.add_filter_mode, RssFilterMode::Regex)
                                    && regex::Regex::new(&value).is_err()
                                {
                                    set_rss_status(app_state, "Invalid regex pattern");
                                    continue;
                                }
                                if filter_already_exists(
                                    &new_settings,
                                    &value,
                                    app_state.ui.rss.add_filter_mode,
                                ) {
                                    set_rss_status(app_state, "Filter already exists");
                                    continue;
                                }
                                new_settings.rss.filters.push(crate::config::RssFilter {
                                    query: value,
                                    mode: app_state.ui.rss.add_filter_mode,
                                    enabled: true,
                                });
                                app_state.ui.rss.filter_draft.clear();
                            }
                            RssSectionFocus::Explorer => {}
                        }
                        let success_message = match app_state.ui.rss.focused_section {
                            RssSectionFocus::Links => Some("Link added"),
                            RssSectionFocus::Filters => Some("Filter added"),
                            RssSectionFocus::Explorer => None,
                        };
                        let _ = try_update_config(
                            app_state,
                            app_command_tx,
                            new_settings,
                            success_message,
                        );
                    }
                    app_state.ui.rss.is_editing = false;
                    app_state.ui.rss.edit_buffer.clear();
                    app_state.ui.rss.add_filter_mode = RssFilterMode::Fuzzy;
                    recompute_needed = true;
                } else if app_state.ui.rss.is_searching {
                    if app_state.ui.rss.search_query.trim().is_empty() {
                        app_state.ui.rss.is_searching = false;
                        set_rss_status(app_state, "Search cleared");
                    } else {
                        set_rss_status(app_state, "Search applied");
                    }
                    recompute_needed = true;
                }
            }
            RssAction::CancelInput => {
                if app_state.ui.rss.is_editing {
                    app_state.ui.rss.is_editing = false;
                    app_state.ui.rss.edit_buffer.clear();
                    app_state.ui.rss.filter_draft.clear();
                    app_state.ui.rss.add_filter_mode = RssFilterMode::Fuzzy;
                    set_rss_status(app_state, "Edit cancelled");
                    recompute_needed = true;
                } else if app_state.ui.rss.is_searching {
                    app_state.ui.rss.is_searching = false;
                    app_state.ui.rss.search_query.clear();
                    set_rss_status(app_state, "Search cleared");
                    recompute_needed = true;
                } else {
                    app_state.mode = AppMode::Normal;
                }
            }
            RssAction::AddEntry => {
                if matches!(app_state.ui.rss.active_screen, RssScreen::Unified)
                    && matches!(
                        app_state.ui.rss.focused_section,
                        RssSectionFocus::Links | RssSectionFocus::Filters
                    )
                {
                    app_state.ui.rss.is_editing = true;
                    app_state.ui.rss.edit_buffer.clear();
                    if matches!(app_state.ui.rss.focused_section, RssSectionFocus::Filters) {
                        app_state.ui.rss.add_filter_mode = RssFilterMode::Fuzzy;
                    }
                    set_rss_status(app_state, "Editing new entry");
                    recompute_needed = true;
                }
            }
            RssAction::DeleteEntry => {
                if !matches!(app_state.ui.rss.active_screen, RssScreen::Unified) {
                    continue;
                }
                match app_state.ui.rss.focused_section {
                    RssSectionFocus::Links => {
                        if selected_feed_actual_idx(settings, app_state.ui.rss.selected_feed_index)
                            .is_some()
                        {
                            app_state.ui.rss.delete_confirm_armed = true;
                            set_rss_status(app_state, "Press Y to confirm delete");
                        }
                    }
                    RssSectionFocus::Filters => {
                        if selected_filter_actual_idx(
                            settings,
                            app_state.ui.rss.selected_filter_index,
                        )
                        .is_some()
                        {
                            app_state.ui.rss.delete_confirm_armed = true;
                            set_rss_status(app_state, "Press Y to confirm delete");
                        }
                    }
                    RssSectionFocus::Explorer => {}
                }
            }
            RssAction::ConfirmDeleteEntry => {
                if !app_state.ui.rss.delete_confirm_armed
                    || !matches!(app_state.ui.rss.active_screen, RssScreen::Unified)
                {
                    continue;
                }
                app_state.ui.rss.delete_confirm_armed = false;
                let mut new_settings = settings.clone();
                match app_state.ui.rss.focused_section {
                    RssSectionFocus::Links => {
                        if let Some(idx) = selected_feed_actual_idx(
                            &new_settings,
                            app_state.ui.rss.selected_feed_index,
                        ) {
                            new_settings.rss.feeds.remove(idx);
                            app_state.ui.rss.selected_feed_index =
                                app_state.ui.rss.selected_feed_index.saturating_sub(1);
                            let _ = try_update_config(
                                app_state,
                                app_command_tx,
                                new_settings,
                                Some("Link deleted"),
                            );
                            recompute_needed = true;
                        }
                    }
                    RssSectionFocus::Filters => {
                        if let Some(idx) = selected_filter_actual_idx(
                            &new_settings,
                            app_state.ui.rss.selected_filter_index,
                        ) {
                            new_settings.rss.filters.remove(idx);
                            app_state.ui.rss.selected_filter_index =
                                app_state.ui.rss.selected_filter_index.saturating_sub(1);
                            let _ = try_update_config(
                                app_state,
                                app_command_tx,
                                new_settings,
                                Some("Filter deleted"),
                            );
                            recompute_needed = true;
                        }
                    }
                    RssSectionFocus::Explorer => {}
                }
            }
            RssAction::CancelDeleteEntry => {
                if app_state.ui.rss.delete_confirm_armed {
                    app_state.ui.rss.delete_confirm_armed = false;
                    set_rss_status(app_state, "Delete cancelled");
                }
            }
            RssAction::ToggleFeedEnabled => {
                if !matches!(app_state.ui.rss.active_screen, RssScreen::Unified) {
                    continue;
                }

                let mut new_settings = settings.clone();
                match app_state.ui.rss.focused_section {
                    RssSectionFocus::Links => {
                        if let Some(idx) = selected_feed_actual_idx(
                            &new_settings,
                            app_state.ui.rss.selected_feed_index,
                        ) {
                            new_settings.rss.feeds[idx].enabled =
                                !new_settings.rss.feeds[idx].enabled;
                            let enabled = new_settings.rss.feeds[idx].enabled;
                            let _ = try_update_config(
                                app_state,
                                app_command_tx,
                                new_settings,
                                Some(if enabled {
                                    "Link enabled"
                                } else {
                                    "Link disabled"
                                }),
                            );
                            recompute_needed = true;
                        }
                    }
                    RssSectionFocus::Filters => {
                        if let Some(idx) = selected_filter_actual_idx(
                            &new_settings,
                            app_state.ui.rss.selected_filter_index,
                        ) {
                            new_settings.rss.filters[idx].enabled =
                                !new_settings.rss.filters[idx].enabled;
                            let enabled = new_settings.rss.filters[idx].enabled;
                            let _ = try_update_config(
                                app_state,
                                app_command_tx,
                                new_settings,
                                Some(if enabled {
                                    "Filter enabled"
                                } else {
                                    "Filter disabled"
                                }),
                            );
                            recompute_needed = true;
                        }
                    }
                    RssSectionFocus::Explorer => {}
                }
            }
            RssAction::StartSearch => {
                if (matches!(app_state.ui.rss.active_screen, RssScreen::Unified)
                    && matches!(app_state.ui.rss.focused_section, RssSectionFocus::Explorer))
                    || matches!(app_state.ui.rss.active_screen, RssScreen::History)
                {
                    app_state.ui.rss.is_searching = true;
                    set_rss_status(app_state, "Search mode");
                    recompute_needed = true;
                }
            }
            RssAction::DownloadSelectedExplorer => {
                if matches!(app_state.ui.rss.active_screen, RssScreen::Unified)
                    && matches!(app_state.ui.rss.focused_section, RssSectionFocus::Explorer)
                {
                    let idx = app_state
                        .ui
                        .rss
                        .selected_explorer_index
                        .min(app_state.rss_derived.explorer_items.len().saturating_sub(1));
                    if let Some(item) = app_state.rss_derived.explorer_items.get(idx) {
                        if app_command_tx
                            .try_send(AppCommand::RssDownloadPreview(item.clone()))
                            .is_err()
                        {
                            set_rss_status(app_state, "RSS download enqueue failed");
                        } else {
                            set_rss_status(app_state, "RSS download requested");
                        }
                    }
                }
            }
        }
    }

    if recompute_needed {
        recompute_rss_derived(app_state, settings);
    }
}

fn apply_pasted_text(app_state: &mut AppState, pasted_text: &str) {
    let trimmed = pasted_text.trim();
    if trimmed.is_empty() {
        return;
    }

    if app_state.ui.rss.is_editing {
        app_state.ui.rss.edit_buffer.push_str(trimmed);
        if matches!(app_state.ui.rss.focused_section, RssSectionFocus::Filters) {
            app_state.ui.rss.filter_draft = app_state.ui.rss.edit_buffer.clone();
        }
        app_state.ui.rss.status_message = Some("Pasted input".to_string());
    } else if app_state.ui.rss.is_searching {
        app_state.ui.rss.search_query.push_str(trimmed);
        app_state.ui.rss.status_message = Some("Pasted search".to_string());
    }
}

pub fn handle_event(
    event: CrosstermEvent,
    app_state: &mut AppState,
    settings: &crate::config::Settings,
    app_command_tx: &mpsc::Sender<AppCommand>,
) {
    if !matches!(app_state.mode, AppMode::Rss) {
        return;
    }

    match event {
        CrosstermEvent::Key(key) => {
            if let Some(action) = map_key_to_rss_action(key.code, key.kind, app_state) {
                let result = reduce_rss_action(action);
                execute_rss_effects(app_state, settings, app_command_tx, result.effects);
                app_state.ui.needs_redraw = true;
            }
        }
        CrosstermEvent::Paste(pasted_text) => {
            apply_pasted_text(app_state, pasted_text.as_str());
            recompute_rss_derived(app_state, settings);
            app_state.ui.needs_redraw = true;
        }
        _ => {}
    }
}

fn draw_input_panel(f: &mut Frame, area: Rect, screen: &ScreenContext<'_>) {
    let app_state = screen.app.state;
    let ctx = screen.theme;

    let (title, value) = if app_state.ui.rss.is_searching {
        (
            " RSS Search ".to_string(),
            app_state.ui.rss.search_query.clone(),
        )
    } else {
        let label = match app_state.ui.rss.focused_section {
            RssSectionFocus::Links => "Add Link",
            RssSectionFocus::Filters => "Add Filter",
            RssSectionFocus::Explorer => "Input",
        };
        (
            format!(" RSS {} ", label),
            app_state.ui.rss.edit_buffer.clone(),
        )
    };

    let mut line_spans = vec![
        Span::styled(
            "> ",
            ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
        ),
        Span::raw(value),
        Span::styled("_", ctx.apply(Style::default().fg(ctx.state_warning()))),
    ];
    if app_state.ui.rss.is_editing
        && matches!(app_state.ui.rss.focused_section, RssSectionFocus::Filters)
    {
        let (fuzzy_style, regex_style) = match app_state.ui.rss.add_filter_mode {
            RssFilterMode::Fuzzy => (
                ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
                ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
            ),
            RssFilterMode::Regex => (
                ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
                ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
            ),
        };
        line_spans.push(Span::raw("  "));
        line_spans.push(Span::styled("Fuzzy", fuzzy_style));
        line_spans.push(Span::raw(" / "));
        line_spans.push(Span::styled("Regex", regex_style));
    }
    let line = Line::from(line_spans);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .padding(Padding::horizontal(1))
        .border_style(ctx.apply(Style::default().fg(ctx.state_selected())));
    f.render_widget(Paragraph::new(line).block(block), area);
}

fn draw_shared_footer(f: &mut Frame, area: Rect, screen: &ScreenContext<'_>) {
    let ctx = screen.theme;
    let app_state = screen.app.state;
    let mut footer_spans = vec![];
    let mut push_action = |key: &str, action: &str, key_color: Color| {
        footer_spans.push(Span::styled(
            format!("[{}]", key),
            ctx.apply(Style::default().fg(key_color).bold()),
        ));
        footer_spans.push(Span::styled(
            action.to_string(),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
        ));
        footer_spans.push(Span::styled(
            " | ",
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
        ));
    };

    if app_state.ui.rss.delete_confirm_armed {
        push_action("Y", "confirm-delete", ctx.state_error());
        push_action("Esc", "cancel", ctx.state_selected());
    } else if app_state.ui.rss.is_editing {
        push_action("Enter", "save", ctx.state_success());
        push_action("Esc", "cancel", ctx.state_error());
        if matches!(app_state.ui.rss.focused_section, RssSectionFocus::Filters) {
            push_action("Tab", "mode", ctx.state_selected());
        }
    } else if app_state.ui.rss.is_searching {
        push_action("Enter", "apply", ctx.state_success());
        push_action("Esc", "clear", ctx.state_error());
    } else {
        push_action("Tab", "next-pane", ctx.state_selected());
        push_action("h", "history", ctx.accent_sapphire());
        push_action("s", "ync", ctx.state_warning());
        match app_state.ui.rss.active_screen {
            RssScreen::Unified => match app_state.ui.rss.focused_section {
                RssSectionFocus::Links => {
                    push_action("a", "dd", ctx.state_success());
                    push_action("D", "elete", ctx.state_error());
                    push_action("Space", "toggle", ctx.state_info());
                }
                RssSectionFocus::Filters => {
                    push_action("a", "dd", ctx.state_success());
                    push_action("D", "elete", ctx.state_error());
                    push_action("Space", "toggle", ctx.state_info());
                }
                RssSectionFocus::Explorer => {
                    push_action("/", "search", ctx.accent_sapphire());
                    push_action("Y", "download", ctx.state_success());
                }
            },
            RssScreen::History => {}
        }
        push_action("Esc", "back", ctx.state_error());
    }

    if !footer_spans.is_empty() {
        footer_spans.pop();
    }

    let footer = Line::from(footer_spans);

    let p = Paragraph::new(footer)
        .style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)))
        .alignment(Alignment::Center);
    f.render_widget(p, area);
}

fn selected_delete_label(
    app_state: &AppState,
    settings: &crate::config::Settings,
) -> Option<String> {
    if !matches!(app_state.ui.rss.active_screen, RssScreen::Unified) {
        return None;
    }
    match app_state.ui.rss.focused_section {
        RssSectionFocus::Links => {
            selected_feed_actual_idx(settings, app_state.ui.rss.selected_feed_index)
                .and_then(|idx| settings.rss.feeds.get(idx))
                .map(|feed| truncate_with_ellipsis(&feed.url, 72))
        }
        RssSectionFocus::Filters => {
            selected_filter_actual_idx(settings, app_state.ui.rss.selected_filter_index)
                .and_then(|idx| settings.rss.filters.get(idx))
                .map(|filter| truncate_with_ellipsis(&filter.query, 72))
        }
        RssSectionFocus::Explorer => None,
    }
}

fn draw_delete_confirm_dialog(f: &mut Frame, area: Rect, screen: &ScreenContext<'_>) {
    let app_state = screen.app.state;
    let settings = screen.settings;
    let ctx = screen.theme;
    let target =
        selected_delete_label(app_state, settings).unwrap_or_else(|| "selected item".to_string());
    let rect_width = if area.width < 60 { 90 } else { 50 };
    let rect_height = if area.height < 20 { 95 } else { 18 };
    let dialog = centered_rect(rect_width, rect_height, area);
    f.render_widget(Clear, dialog);
    let vert_padding = if dialog.height < 10 { 0 } else { 1 };
    let block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::new(2, 2, vert_padding, vert_padding))
        .border_style(ctx.apply(Style::default().fg(ctx.state_error())));
    let inner = block.inner(dialog);
    f.render_widget(block, dialog);

    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);

    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "Delete RSS Entry",
                ctx.apply(Style::default().fg(ctx.state_warning()).bold().underlined()),
            )),
            Line::from(Span::styled(
                target,
                ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
            )),
        ])
        .alignment(Alignment::Center),
        chunks[0],
    );

    if chunks[1].height > 0 {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "This removes the selected RSS link/filter.",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                )),
                Line::from(Span::styled(
                    "This action cannot be undone.",
                    ctx.apply(Style::default().fg(ctx.state_error()).bold()),
                )),
            ])
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
            chunks[1],
        );
    }

    let actions = Line::from(vec![
        Span::styled(
            "[Y]",
            ctx.apply(Style::default().fg(ctx.state_success()).bold()),
        ),
        Span::raw(" Confirm  "),
        Span::styled(
            "[Esc]",
            ctx.apply(Style::default().fg(ctx.state_error()).bold()),
        ),
        Span::raw(" Cancel"),
    ]);
    f.render_widget(
        Paragraph::new(actions).alignment(Alignment::Center),
        chunks[3],
    );
}

fn sync_countdown_label(next_sync_at: &str) -> Option<String> {
    let next_sync = DateTime::parse_from_rfc3339(next_sync_at).ok()?;
    let remaining_secs = next_sync
        .with_timezone(&Utc)
        .signed_duration_since(Utc::now())
        .num_seconds();
    if remaining_secs <= 0 {
        return None;
    }

    let hours = remaining_secs / 3600;
    let minutes = (remaining_secs % 3600) / 60;
    let seconds = remaining_secs % 60;

    let label = if hours > 0 {
        format!("{}h {}m {}s", hours, minutes, seconds)
    } else if minutes > 0 {
        format!("{}m {}s", minutes, seconds)
    } else {
        format!("{}s", seconds)
    };
    Some(label)
}

fn filtered_history_entries<'a>(
    history: &'a [crate::config::RssHistoryEntry],
    search_query: &str,
) -> Vec<&'a crate::config::RssHistoryEntry> {
    let query = search_query.trim().to_lowercase();
    if query.is_empty() {
        return history.iter().collect();
    }

    history
        .iter()
        .filter(|entry| {
            entry.title.to_lowercase().contains(&query)
                || entry
                    .source
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&query)
                || entry.date_iso.to_lowercase().contains(&query)
        })
        .collect()
}

fn human_readable_history_time(date_iso: &str) -> String {
    DateTime::parse_from_rfc3339(date_iso)
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| date_iso.to_string())
}

fn link_matches_selected_explorer_item(feed_url: &str, item: &crate::app::RssPreviewItem) -> bool {
    let feed_url_lc = feed_url.to_lowercase();

    if let Some(link) = &item.link {
        let link_lc = link.to_lowercase();
        if feed_url_lc.contains(&link_lc) || link_lc.contains(&feed_url_lc) {
            return true;
        }

        let feed_host = Url::parse(feed_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_lowercase()));
        let link_host = Url::parse(link)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_lowercase()));
        if let (Some(fh), Some(lh)) = (feed_host.clone(), link_host) {
            if fh == lh {
                return true;
            }
        }

        if let (Some(fh), Some(source)) = (feed_host, item.source.as_ref()) {
            let host_root = fh
                .split('.')
                .next()
                .unwrap_or_default()
                .trim()
                .to_lowercase();
            if !host_root.is_empty() && source.to_lowercase().contains(&host_root) {
                return true;
            }
        }
    }

    false
}

fn pane_block<'a>(title: &'a str, active: bool, ctx: &crate::theme::ThemeContext) -> Block<'a> {
    let border_style = if active {
        ctx.apply(Style::default().fg(ctx.state_selected()))
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.border))
    };

    Block::default()
        .title(format!(" {} ", title))
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .border_style(border_style)
}

fn draw_links(f: &mut Frame, area: Rect, screen: &ScreenContext<'_>, active: bool) {
    let perf_start = Instant::now();
    let app_state = screen.app.state;
    let settings = screen.settings;
    let ctx = screen.theme;
    let selected = app_state.ui.rss.selected_feed_index;
    let selected_item_start = Instant::now();
    let selected_explorer_item = if matches!(app_state.ui.rss.active_screen, RssScreen::Unified)
        && matches!(app_state.ui.rss.focused_section, RssSectionFocus::Explorer)
    {
        if app_state.rss_derived.explorer_items.is_empty() {
            None
        } else {
            let idx = app_state
                .ui
                .rss
                .selected_explorer_index
                .min(app_state.rss_derived.explorer_items.len().saturating_sub(1));
            app_state.rss_derived.explorer_items.get(idx).cloned()
        }
    } else {
        None
    };
    let selected_item_ms = selected_item_start.elapsed().as_millis();

    let sorted_indices = sorted_feed_indices(settings);
    let sync_countdown = app_state
        .rss_runtime
        .next_sync_at
        .as_deref()
        .and_then(sync_countdown_label);
    let lines_start = Instant::now();
    let mut lines: Vec<Line<'static>> = sorted_indices
        .iter()
        .map(|idx| {
            let feed = &settings.rss.feeds[*idx];
            let is_explorer_link_match = selected_explorer_item
                .as_ref()
                .is_some_and(|item| link_matches_selected_explorer_item(&feed.url, item));
            let style = if !feed.enabled {
                ctx.apply(
                    Style::default()
                        .fg(ctx.theme.semantic.overlay0)
                        .add_modifier(Modifier::CROSSED_OUT),
                )
            } else if is_explorer_link_match {
                ctx.apply(Style::default().fg(ctx.state_selected()).bold())
            } else {
                ctx.apply(Style::default().fg(ctx.theme.semantic.text))
            };
            let mut spans = vec![Span::styled(feed.url.clone(), style)];
            if let Some(countdown) = &sync_countdown {
                spans.push(Span::styled(
                    format!(" ({})", countdown),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ));
            }
            Line::from(spans)
        })
        .collect();
    let lines_ms = lines_start.elapsed().as_millis();

    let errors_start = Instant::now();
    let mut feed_error_rows: Vec<_> = app_state
        .rss_runtime
        .feed_errors
        .iter()
        .map(|(url, err)| (url.clone(), err.message.clone()))
        .collect();
    feed_error_rows.sort_by(|a, b| a.0.cmp(&b.0));
    if !feed_error_rows.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "Feed errors:",
            ctx.apply(Style::default().fg(ctx.state_error()).bold()),
        )]));
        let max_line_chars = area.width.saturating_sub(4) as usize;
        for (url, message) in feed_error_rows {
            let max_url_chars = (max_line_chars / 3).max(12);
            let url_text = truncate_with_ellipsis(&url, max_url_chars);
            let prefix = format!("{}: ", url_text);
            let remaining = max_line_chars.saturating_sub(prefix.chars().count());
            let message_text = truncate_with_ellipsis(&message, remaining);
            lines.push(Line::from(vec![
                Span::styled(
                    prefix,
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
                Span::styled(
                    message_text,
                    ctx.apply(Style::default().fg(ctx.state_error())),
                ),
            ]));
        }
    }
    let errors_ms = errors_start.elapsed().as_millis();

    let items: Vec<ListItem<'static>> = lines.into_iter().map(ListItem::new).collect();
    let mut state = ListState::default();
    if !sorted_indices.is_empty() {
        state.select(Some(selected.min(sorted_indices.len() - 1)));
    }
    let highlight_style = if active {
        screen
            .theme
            .apply(Style::default().fg(screen.theme.state_selected()).bold())
    } else {
        screen.theme.apply(Style::default())
    };
    let render_start = Instant::now();
    f.render_stateful_widget(
        List::new(items)
            .block(pane_block("Links", active, screen.theme))
            .highlight_style(highlight_style),
        area,
        &mut state,
    );
    let render_ms = render_start.elapsed().as_millis();

    let _ = perf_start;
    let _ = selected_item_ms;
    let _ = lines_ms;
    let _ = errors_ms;
    let _ = render_ms;
}

fn active_filter_spec(
    app_state: &AppState,
    settings: &crate::config::Settings,
) -> Option<FilterSpec> {
    if app_state.ui.rss.is_editing
        && matches!(app_state.ui.rss.focused_section, RssSectionFocus::Filters)
    {
        return Some(FilterSpec {
            query: app_state.ui.rss.edit_buffer.clone(),
            mode: app_state.ui.rss.add_filter_mode,
        });
    }

    settings
        .rss
        .filters
        .get(
            selected_filter_actual_idx(settings, app_state.ui.rss.selected_filter_index)
                .unwrap_or(app_state.ui.rss.selected_filter_index),
        )
        .filter(|f| f.enabled)
        .map(|f| FilterSpec {
            query: f.query.clone(),
            mode: f.mode,
        })
}

fn focused_filter_query(
    app_state: &AppState,
    settings: &crate::config::Settings,
) -> Option<FilterSpec> {
    if !matches!(app_state.ui.rss.active_screen, RssScreen::Unified)
        || !matches!(app_state.ui.rss.focused_section, RssSectionFocus::Filters)
        || app_state.ui.rss.is_editing
    {
        return None;
    }

    settings
        .rss
        .filters
        .get(
            selected_filter_actual_idx(settings, app_state.ui.rss.selected_filter_index)
                .unwrap_or(app_state.ui.rss.selected_filter_index),
        )
        .filter(|f| f.enabled)
        .map(|f| FilterSpec {
            query: f.query.trim().to_string(),
            mode: f.mode,
        })
        .filter(|f| !f.query.is_empty())
}

#[cfg(test)]
fn compute_filter_preview_items(
    preview_items: &[crate::app::RssPreviewItem],
    draft: &str,
) -> Vec<(crate::app::RssPreviewItem, bool)> {
    let draft = draft.trim();
    if draft.is_empty() {
        return preview_items
            .iter()
            .cloned()
            .map(|item| (item, true))
            .collect();
    }

    let matcher = SkimMatcherV2::default();
    let draft_lc = draft.to_lowercase();

    let mut ranked: Vec<(crate::app::RssPreviewItem, bool)> = preview_items
        .iter()
        .map(|item| {
            let is_match = matcher
                .fuzzy_match(&item.title.to_lowercase(), &draft_lc)
                .is_some();
            (item.clone(), is_match)
        })
        .collect();

    ranked.sort_by_key(|item| std::cmp::Reverse(item.1));
    ranked
}

#[cfg(test)]
fn compute_filter_match_counts(
    app_state: &AppState,
    filter_text: &str,
    filter_mode: RssFilterMode,
    matcher: &SkimMatcherV2,
) -> (usize, usize) {
    let filter = filter_text.trim();
    if filter.is_empty() {
        return (0, 0);
    }

    let matched_items: Vec<&crate::app::RssPreviewItem> = app_state
        .rss_runtime
        .preview_items
        .iter()
        .filter(|item| filter_matches_title(&item.title, filter, filter_mode, matcher))
        .collect();

    let feed_matches = matched_items.len();

    let downloaded_from_torrents = app_state
        .torrents
        .values()
        .filter(|torrent| {
            filter_matches_title(
                &torrent.latest_state.torrent_name,
                filter,
                filter_mode,
                matcher,
            )
        })
        .count();
    let app_hashes: HashSet<Vec<u8>> = app_state.torrents.keys().cloned().collect();
    let app_titles: HashSet<String> = app_state
        .torrents
        .values()
        .map(|torrent| normalize_title(&torrent.latest_state.torrent_name))
        .collect();

    let history_missing_from_app = app_state
        .rss_runtime
        .history
        .iter()
        .filter(|entry| filter_matches_title(&entry.title, filter, filter_mode, matcher))
        .filter(|entry| {
            let hash_in_app = entry
                .info_hash
                .as_deref()
                .and_then(|hash| hex::decode(hash).ok())
                .is_some_and(|hash| app_hashes.contains(&hash));
            let title_in_app = app_titles.contains(&normalize_title(&entry.title));
            !hash_in_app && !title_in_app
        })
        .count();

    let downloaded_matches = downloaded_from_torrents + history_missing_from_app;

    (feed_matches, downloaded_matches)
}

fn compute_filter_downloaded_matches(
    app_state: &AppState,
    filter_text: &str,
    filter_mode: RssFilterMode,
    matcher: &SkimMatcherV2,
) -> usize {
    let filter = filter_text.trim();
    if filter.is_empty() {
        return 0;
    }

    let downloaded_from_torrents = app_state
        .torrents
        .values()
        .filter(|torrent| {
            filter_matches_title(
                &torrent.latest_state.torrent_name,
                filter,
                filter_mode,
                matcher,
            )
        })
        .count();
    let app_hashes: HashSet<Vec<u8>> = app_state.torrents.keys().cloned().collect();
    let app_titles: HashSet<String> = app_state
        .torrents
        .values()
        .map(|torrent| normalize_title(&torrent.latest_state.torrent_name))
        .collect();

    let history_missing_from_app = app_state
        .rss_runtime
        .history
        .iter()
        .filter(|entry| filter_matches_title(&entry.title, filter, filter_mode, matcher))
        .filter(|entry| {
            let hash_in_app = entry
                .info_hash
                .as_deref()
                .and_then(|hash| hex::decode(hash).ok())
                .is_some_and(|hash| app_hashes.contains(&hash));
            let title_in_app = app_titles.contains(&normalize_title(&entry.title));
            !hash_in_app && !title_in_app
        })
        .count();

    downloaded_from_torrents + history_missing_from_app
}

fn filter_history_age_label(
    app_state: &AppState,
    filter_text: &str,
    filter_mode: RssFilterMode,
    matcher: &SkimMatcherV2,
) -> String {
    let latest = app_state
        .rss_runtime
        .history
        .iter()
        .filter(|entry| filter_matches_title(&entry.title, filter_text, filter_mode, matcher))
        .filter_map(|entry| DateTime::parse_from_rfc3339(&entry.date_iso).ok())
        .max();

    let Some(latest) = latest else {
        return "today".to_string();
    };

    let now = Utc::now();
    let days = now
        .signed_duration_since(latest.with_timezone(&Utc))
        .num_days();
    if days <= 0 {
        "today".to_string()
    } else if days == 1 {
        "1 day ago".to_string()
    } else {
        format!("{} days ago", days)
    }
}

fn compute_filter_runtime_stats(
    app_state: &AppState,
    settings: &crate::config::Settings,
) -> HashMap<usize, crate::app::RssFilterRuntimeStat> {
    let matcher = SkimMatcherV2::default();
    settings
        .rss
        .filters
        .iter()
        .enumerate()
        .map(|(idx, filter)| {
            let downloaded_matches =
                compute_filter_downloaded_matches(app_state, &filter.query, filter.mode, &matcher);
            let history_age =
                filter_history_age_label(app_state, &filter.query, filter.mode, &matcher);
            (
                idx,
                crate::app::RssFilterRuntimeStat {
                    downloaded_matches,
                    history_age,
                },
            )
        })
        .collect::<HashMap<_, _>>()
}

fn build_history_hash_by_dedupe(
    history: &[crate::config::RssHistoryEntry],
) -> HashMap<String, Vec<u8>> {
    history
        .iter()
        .filter_map(|entry| {
            entry
                .info_hash
                .as_deref()
                .and_then(|hash| hex::decode(hash).ok())
                .map(|decoded| (entry.dedupe_key.clone(), decoded))
        })
        .collect()
}

pub fn recompute_rss_derived(app_state: &mut AppState, settings: &crate::config::Settings) {
    let enabled_filters = enabled_filters(settings);
    let is_creating_filter = app_state.ui.rss.is_editing
        && matches!(app_state.ui.rss.focused_section, RssSectionFocus::Filters);
    let active_filter = active_filter_spec(app_state, settings);
    let active_filter_query = active_filter
        .as_ref()
        .map(|f| f.query.as_str())
        .unwrap_or("");
    let active_filter_mode = active_filter
        .as_ref()
        .map(|f| f.mode)
        .unwrap_or(RssFilterMode::Fuzzy);
    let (items, combined_match, prioritise_matches) = compute_explorer_items(
        &app_state.rss_runtime.preview_items,
        &app_state.ui.rss.search_query,
        &enabled_filters,
        active_filter_query,
        active_filter_mode,
        is_creating_filter,
    );

    app_state.rss_derived.explorer_items = items;
    app_state.rss_derived.explorer_combined_match = combined_match;
    app_state.rss_derived.explorer_prioritise_matches = prioritise_matches;
    app_state.rss_derived.history_hash_by_dedupe =
        build_history_hash_by_dedupe(&app_state.rss_runtime.history);
    app_state.rss_derived.filter_runtime_stats = compute_filter_runtime_stats(app_state, settings);
}

fn normalize_title(input: &str) -> String {
    input
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn draw_filters(f: &mut Frame, area: Rect, screen: &ScreenContext<'_>, active: bool) {
    let perf_start = Instant::now();
    let app_state = screen.app.state;
    let settings = screen.settings;
    let ctx = screen.theme;
    let matcher = SkimMatcherV2::default();
    let selected = app_state.ui.rss.selected_filter_index;
    let is_creating_filter = app_state.ui.rss.is_editing
        && matches!(app_state.ui.rss.focused_section, RssSectionFocus::Filters);
    let draft_lc = app_state.ui.rss.edit_buffer.trim().to_lowercase();
    let crosswire_start = Instant::now();
    let explorer_selected_title_lc = if matches!(app_state.ui.rss.active_screen, RssScreen::Unified)
        && matches!(app_state.ui.rss.focused_section, RssSectionFocus::Explorer)
    {
        if app_state.rss_derived.explorer_items.is_empty() {
            None
        } else {
            let idx = app_state
                .ui
                .rss
                .selected_explorer_index
                .min(app_state.rss_derived.explorer_items.len().saturating_sub(1));
            app_state
                .rss_derived
                .explorer_items
                .get(idx)
                .map(|item| item.title.to_lowercase())
        }
    } else {
        None
    };
    let crosswire_ms = crosswire_start.elapsed().as_millis();

    let sort_start = Instant::now();
    let mut sorted_indices = sorted_filter_indices(settings);
    if is_creating_filter && !draft_lc.is_empty() {
        sorted_indices.sort_by(|a, b| {
            let a_query = settings.rss.filters[*a].query.to_lowercase();
            let b_query = settings.rss.filters[*b].query.to_lowercase();
            let a_match = a_query.contains(&draft_lc);
            let b_match = b_query.contains(&draft_lc);
            b_match.cmp(&a_match).then_with(|| a_query.cmp(&b_query))
        });
    }
    let sort_ms = sort_start.elapsed().as_millis();
    let stats_start = Instant::now();
    let filter_runtime_stats = &app_state.rss_derived.filter_runtime_stats;
    let stats_ms = stats_start.elapsed().as_millis();
    let rows_start = Instant::now();
    let mut items: Vec<ListItem<'static>> = Vec::with_capacity(sorted_indices.len());
    for idx in &sorted_indices {
        let filter = &settings.rss.filters[*idx];
        let filter_text = filter.query.clone();
        let filter_lc = filter_text.trim().to_lowercase();
        let is_matching_existing = is_creating_filter
            && !draft_lc.is_empty()
            && matches!(app_state.ui.rss.add_filter_mode, RssFilterMode::Fuzzy)
            && filter_lc.contains(&draft_lc);
        let matches_explorer_selection = filter.enabled
            && !filter_lc.is_empty()
            && explorer_selected_title_lc.as_ref().is_some_and(|title| {
                filter_matches_title(title, &filter_text, filter.mode, &matcher)
            });
        let style = if !filter.enabled {
            ctx.apply(
                Style::default()
                    .fg(ctx.theme.semantic.overlay0)
                    .add_modifier(Modifier::CROSSED_OUT),
            )
        } else if matches_explorer_selection {
            ctx.apply(Style::default().fg(ctx.state_selected()).bold())
        } else if is_matching_existing {
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0))
        } else {
            ctx.apply(Style::default().fg(ctx.theme.semantic.text))
        };
        let stats = filter_runtime_stats.get(idx);
        let downloaded_matches = stats.map(|s| s.downloaded_matches).unwrap_or(0);
        let history_age = stats.map(|s| s.history_age.as_str()).unwrap_or("today");
        let mode_label = match filter.mode {
            RssFilterMode::Fuzzy => "[fuzzy]",
            RssFilterMode::Regex => "[regex]",
        };

        items.push(ListItem::new(Line::from(vec![Span::styled(
            format!(
                "{} [downloaded {}] \"{}\" - {}",
                mode_label, downloaded_matches, filter_text, history_age
            ),
            style,
        )])));
    }
    let rows_ms = rows_start.elapsed().as_millis();
    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(selected.min(items.len() - 1)));
    }
    let highlight_style = if active {
        screen
            .theme
            .apply(Style::default().fg(screen.theme.state_selected()).bold())
    } else {
        screen.theme.apply(Style::default())
    };
    let render_start = Instant::now();
    f.render_stateful_widget(
        List::new(items)
            .block(pane_block("Filters", active, screen.theme))
            .highlight_style(highlight_style),
        area,
        &mut state,
    );
    let render_ms = render_start.elapsed().as_millis();
    let _ = perf_start;
    let _ = crosswire_ms;
    let _ = sort_ms;
    let _ = stats_ms;
    let _ = rows_ms;
    let _ = render_ms;
}

fn compute_explorer_items(
    preview_items: &[crate::app::RssPreviewItem],
    search_query: &str,
    enabled_filters: &[FilterSpec],
    draft_filter_query: &str,
    draft_filter_mode: RssFilterMode,
    prefer_draft_sort: bool,
) -> (Vec<crate::app::RssPreviewItem>, Vec<bool>, bool) {
    let search = search_query.to_lowercase();
    let has_search = !search.is_empty();
    let draft_filter = prepare_filter(draft_filter_query, draft_filter_mode);
    let has_draft_query = draft_filter.is_some();
    let matcher = SkimMatcherV2::default();
    let enabled_prepared: Vec<PreparedFilter> = enabled_filters
        .iter()
        .filter_map(|f| prepare_filter(&f.query, f.mode))
        .collect();

    let has_enabled_filters = !enabled_prepared.is_empty();
    let prioritise_matches = has_search || has_enabled_filters || has_draft_query;
    let prepared_items: Vec<(crate::app::RssPreviewItem, String)> = preview_items
        .iter()
        .cloned()
        .map(|item| {
            let title_lc = item.title.to_lowercase();
            (item, title_lc)
        })
        .collect();

    let mut combined_match: Vec<bool> = prepared_items
        .iter()
        .map(|(item, title_lc)| {
            let search_hit = has_search && title_lc.contains(&search);
            let enabled_filter_hit = enabled_prepared
                .iter()
                .any(|f| prepared_filter_matches(&item.title, title_lc, f, &matcher));
            let draft_hit = draft_filter
                .as_ref()
                .is_some_and(|f| prepared_filter_matches(&item.title, title_lc, f, &matcher));
            enabled_filter_hit || search_hit || draft_hit
        })
        .collect();

    if has_search {
        let filtered: Vec<(crate::app::RssPreviewItem, String, bool)> = prepared_items
            .into_iter()
            .zip(combined_match)
            .filter_map(|((item, title_lc), is_match)| is_match.then_some((item, title_lc, true)))
            .collect();
        let mut filtered = filtered;
        filtered.sort_by(|a, b| a.1.cmp(&b.1));
        combined_match = filtered.iter().map(|p| p.2).collect();
        let items = filtered.into_iter().map(|p| p.0).collect();
        return (items, combined_match, prioritise_matches);
    }

    let mut paired: Vec<(crate::app::RssPreviewItem, String, bool, bool, Option<i64>)> =
        prepared_items
            .into_iter()
            .zip(combined_match)
            .map(|((item, title_lc), is_match)| {
                let draft_hit = draft_filter
                    .as_ref()
                    .is_some_and(|f| prepared_filter_matches(&item.title, &title_lc, f, &matcher));
                let draft_score = draft_filter.as_ref().and_then(|f| {
                    if matches!(f.mode, RssFilterMode::Fuzzy) {
                        matcher.fuzzy_match(&title_lc, &f.query_lc)
                    } else {
                        None
                    }
                });
                (item, title_lc, is_match, draft_hit, draft_score)
            })
            .collect();
    if prioritise_matches {
        if prefer_draft_sort && has_draft_query {
            paired.sort_by(|a, b| {
                b.3.cmp(&a.3)
                    .then_with(|| b.4.unwrap_or(0).cmp(&a.4.unwrap_or(0)))
                    .then_with(|| b.2.cmp(&a.2))
                    .then_with(|| a.1.cmp(&b.1))
            });
        } else {
            paired.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
        }
    } else {
        paired.sort_by(|a, b| a.1.cmp(&b.1));
    }
    combined_match = paired.iter().map(|p| p.2).collect();
    let items = paired.into_iter().map(|p| p.0).collect();

    (items, combined_match, prioritise_matches)
}

fn rss_item_completion_percent(
    item: &crate::app::RssPreviewItem,
    app_state: &AppState,
    history_hash_map: &HashMap<String, Vec<u8>>,
    completion_by_title: &HashMap<String, f64>,
) -> Option<f64> {
    if app_state.torrents.is_empty() {
        return None;
    }

    if let Some(link) = &item.link {
        if link.starts_with("magnet:") {
            let (v1_hash, v2_hash) = crate::app::parse_hybrid_hashes(link);
            for hash in [v1_hash, v2_hash].into_iter().flatten() {
                if let Some(torrent) = app_state.torrents.get(&hash) {
                    return Some(crate::app::torrent_completion_percent(
                        &torrent.latest_state,
                    ));
                }
            }
        }
    }

    if let Some(history_hash) = history_hash_map.get(&item.dedupe_key) {
        if let Some(torrent) = app_state.torrents.get(history_hash) {
            return Some(crate::app::torrent_completion_percent(
                &torrent.latest_state,
            ));
        }
    }

    let normalized_title = normalize_title(&item.title);
    completion_by_title.get(&normalized_title).copied()
}

fn build_completion_by_normalized_title(app_state: &AppState) -> HashMap<String, f64> {
    let mut completion_by_title: HashMap<String, f64> = HashMap::new();
    for torrent in app_state.torrents.values() {
        let normalized = normalize_title(&torrent.latest_state.torrent_name);
        let completion = crate::app::torrent_completion_percent(&torrent.latest_state);
        completion_by_title
            .entry(normalized)
            .and_modify(|existing| *existing = existing.max(completion))
            .or_insert(completion);
    }
    completion_by_title
}

fn format_completion_prefix(pct: f64) -> String {
    if (pct - 100.0).abs() < f64::EPSILON {
        "100.0% ".to_string()
    } else {
        format!("{:.1}% ", pct)
    }
}

fn completion_color_for_pct(ctx: &crate::theme::ThemeContext, pct: f64) -> ratatui::style::Color {
    if pct >= 100.0 {
        ctx.state_success()
    } else {
        ctx.state_selected()
    }
}

fn draw_explorer(f: &mut Frame, area: Rect, screen: &ScreenContext<'_>, active: bool) {
    let perf_start = Instant::now();
    let app_state = screen.app.state;
    let settings = screen.settings;
    let ctx = screen.theme;
    let matcher = SkimMatcherV2::default();
    let selected = app_state
        .ui
        .rss
        .selected_explorer_index
        .min(app_state.rss_derived.explorer_items.len().saturating_sub(1));

    let explorer_greyed_out = explorer_effective_greyed_out(app_state, settings);
    let is_creating_filter = app_state.ui.rss.is_editing
        && matches!(app_state.ui.rss.focused_section, RssSectionFocus::Filters);
    let focused_filter_query = focused_filter_query(app_state, settings);
    let compute_start = Instant::now();
    let items = &app_state.rss_derived.explorer_items;
    let combined_match = &app_state.rss_derived.explorer_combined_match;
    let prioritise_matches = app_state.rss_derived.explorer_prioritise_matches;
    let compute_ms = compute_start.elapsed().as_millis();
    let history_hash_map = &app_state.rss_derived.history_hash_by_dedupe;
    let completion_by_title = build_completion_by_normalized_title(app_state);
    let draft_filter = prepare_filter(
        &app_state.ui.rss.edit_buffer,
        app_state.ui.rss.add_filter_mode,
    );
    let enabled_prepared: Vec<PreparedFilter> = settings
        .rss
        .filters
        .iter()
        .filter(|f| f.enabled)
        .filter_map(|f| prepare_filter(&f.query, f.mode))
        .collect();
    let focused_filter = focused_filter_query
        .as_ref()
        .and_then(|f| prepare_filter(&f.query, f.mode));

    let rows_start = Instant::now();
    let list_items: Vec<ListItem<'static>> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let is_combined_match = combined_match.get(i).copied().unwrap_or(item.is_match);
            let title_lc = item.title.to_lowercase();
            let draft_hit = draft_filter
                .as_ref()
                .is_some_and(|f| prepared_filter_matches(&item.title, &title_lc, f, &matcher));
            let existing_filter_hit = enabled_prepared
                .iter()
                .any(|f| prepared_filter_matches(&item.title, &title_lc, f, &matcher));
            let focused_filter_hit = focused_filter
                .as_ref()
                .is_none_or(|f| prepared_filter_matches(&item.title, &title_lc, f, &matcher));

            let dim_as_other_filter_match = is_creating_filter && existing_filter_hit && !draft_hit;
            let style = if explorer_greyed_out || dim_as_other_filter_match {
                ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0))
            } else if focused_filter_query.is_some() && focused_filter_hit {
                ctx.apply(Style::default().fg(ctx.state_selected()).bold())
            } else if prioritise_matches && !is_combined_match {
                ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0))
            } else {
                ctx.apply(Style::default().fg(ctx.theme.semantic.text))
            };

            let completion_pct = rss_item_completion_percent(
                item,
                app_state,
                history_hash_map,
                &completion_by_title,
            );
            if let Some(pct) = completion_pct {
                let completion_style =
                    style.patch(Style::default().fg(completion_color_for_pct(ctx, pct)));
                ListItem::new(Line::from(vec![
                    Span::styled(format_completion_prefix(pct), completion_style),
                    Span::styled(item.title.clone(), style),
                ]))
            } else {
                ListItem::new(Line::from(vec![Span::styled(item.title.clone(), style)]))
            }
        })
        .collect();
    let rows_ms = rows_start.elapsed().as_millis();

    let mut state = ListState::default();
    if active && !items.is_empty() {
        state.select(Some(selected.min(items.len() - 1)));
    }
    let suppress_selection_highlight = app_state.ui.rss.is_editing
        && matches!(app_state.ui.rss.focused_section, RssSectionFocus::Filters);
    let highlight_style = if suppress_selection_highlight || explorer_greyed_out {
        screen.theme.apply(Style::default())
    } else if active {
        screen
            .theme
            .apply(Style::default().fg(screen.theme.state_selected()).bold())
    } else {
        screen
            .theme
            .apply(Style::default().fg(screen.theme.theme.semantic.text).bold())
    };
    let render_start = Instant::now();
    f.render_stateful_widget(
        List::new(list_items)
            .block(pane_block("Explorer", active, screen.theme))
            .highlight_style(highlight_style),
        area,
        &mut state,
    );
    let render_ms = render_start.elapsed().as_millis();
    let _ = perf_start;
    let _ = compute_ms;
    let _ = rows_ms;
    let _ = render_ms;
}

fn draw_history(f: &mut Frame, area: Rect, screen: &ScreenContext<'_>) {
    let app_state = screen.app.state;
    let ctx = screen.theme;
    let selected = app_state.ui.rss.selected_history_index;

    let filtered = filtered_history_entries(
        &app_state.rss_runtime.history,
        &app_state.ui.rss.search_query,
    );

    let lines: Vec<Line<'static>> = filtered
        .iter()
        .map(|entry| {
            let src = entry
                .source
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            Line::from(format!(
                "{} | {} | {}",
                human_readable_history_time(&entry.date_iso),
                entry.title,
                src
            ))
        })
        .collect();

    let items: Vec<ListItem<'static>> = lines.into_iter().map(ListItem::new).collect();
    let mut state = ListState::default();
    if !filtered.is_empty() {
        state.select(Some(selected.min(filtered.len() - 1)));
    }
    f.render_stateful_widget(
        List::new(items)
            .block(pane_block("History", true, ctx))
            .highlight_style(ctx.apply(Style::default().fg(ctx.state_selected()).bold())),
        area,
        &mut state,
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnifiedLayout {
    Wide,
    Narrow,
}

fn unified_layout_for_width(width: u16) -> UnifiedLayout {
    if width >= 140 {
        UnifiedLayout::Wide
    } else {
        UnifiedLayout::Narrow
    }
}

fn draw_unified_body(f: &mut Frame, area: Rect, screen: &ScreenContext<'_>, show_history: bool) {
    let app_state = screen.app.state;
    if matches!(unified_layout_for_width(area.width), UnifiedLayout::Wide) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(area);

        let right_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(cols[1]);

        if show_history {
            draw_history(f, cols[0], screen);
        } else {
            draw_explorer(
                f,
                cols[0],
                screen,
                matches!(app_state.ui.rss.focused_section, RssSectionFocus::Explorer),
            );
        }
        draw_links(
            f,
            right_rows[0],
            screen,
            !show_history && matches!(app_state.ui.rss.focused_section, RssSectionFocus::Links),
        );
        draw_filters(
            f,
            right_rows[1],
            screen,
            !show_history && matches!(app_state.ui.rss.focused_section, RssSectionFocus::Filters),
        );
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(50),
                Constraint::Percentage(20),
                Constraint::Percentage(30),
            ])
            .split(area);

        if show_history {
            draw_history(f, rows[0], screen);
        } else {
            draw_explorer(
                f,
                rows[0],
                screen,
                matches!(app_state.ui.rss.focused_section, RssSectionFocus::Explorer),
            );
        }
        draw_filters(
            f,
            rows[1],
            screen,
            !show_history && matches!(app_state.ui.rss.focused_section, RssSectionFocus::Filters),
        );
        draw_links(
            f,
            rows[2],
            screen,
            !show_history && matches!(app_state.ui.rss.focused_section, RssSectionFocus::Links),
        );
    }
}

pub fn draw(f: &mut Frame, screen: &ScreenContext<'_>) {
    let area = centered_rect(88, 86, f.area());
    let app_state = screen.app.state;

    f.render_widget(Clear, area);

    let show_input_panel = app_state.ui.rss.is_editing || app_state.ui.rss.is_searching;
    let constraints = if show_input_panel {
        vec![
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(1),
        ]
    } else {
        vec![Constraint::Min(5), Constraint::Length(1)]
    };

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    if show_input_panel {
        draw_input_panel(f, inner[0], screen);
    }
    let body_idx = if show_input_panel { 1 } else { 0 };
    let footer_idx = if show_input_panel { 2 } else { 1 };
    draw_unified_body(
        f,
        inner[body_idx],
        screen,
        matches!(app_state.ui.rss.active_screen, RssScreen::History),
    );
    draw_shared_footer(f, inner[footer_idx], screen);
    if app_state.ui.rss.delete_confirm_armed {
        draw_delete_confirm_dialog(f, area, screen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::RssPreviewItem;

    fn base_state() -> AppState {
        AppState {
            mode: AppMode::Rss,
            ..Default::default()
        }
    }

    #[test]
    fn esc_returns_to_normal_mode() {
        let mut app_state = base_state();
        let settings = crate::config::Settings::default();
        let (tx, _rx) = mpsc::channel(2);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Esc,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        assert!(matches!(app_state.mode, AppMode::Normal));
    }

    #[test]
    fn tab_cycles_focus_sections() {
        let mut app_state = base_state();
        let settings = crate::config::Settings::default();
        let (tx, _rx) = mpsc::channel(2);

        assert!(matches!(
            app_state.ui.rss.focused_section,
            RssSectionFocus::Explorer
        ));

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Tab,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        assert!(matches!(
            app_state.ui.rss.focused_section,
            RssSectionFocus::Links
        ));

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Tab,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        assert!(matches!(
            app_state.ui.rss.focused_section,
            RssSectionFocus::Filters
        ));

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Tab,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        assert!(matches!(
            app_state.ui.rss.focused_section,
            RssSectionFocus::Explorer
        ));
    }

    #[test]
    fn h_toggles_history_and_returns_to_explorer_focus() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Links;
        let settings = crate::config::Settings::default();
        let (tx, _rx) = mpsc::channel(2);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('h'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        assert!(matches!(app_state.ui.rss.active_screen, RssScreen::History));

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('h'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        assert!(matches!(app_state.ui.rss.active_screen, RssScreen::Unified));
        assert!(matches!(
            app_state.ui.rss.focused_section,
            RssSectionFocus::Explorer
        ));
    }

    #[test]
    fn down_moves_rows_and_left_right_do_not_change_focus() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Explorer;
        app_state.rss_runtime.preview_items.push(RssPreviewItem {
            title: "A".to_string(),
            ..Default::default()
        });
        app_state.rss_runtime.preview_items.push(RssPreviewItem {
            title: "B".to_string(),
            ..Default::default()
        });
        let settings = crate::config::Settings::default();
        let (tx, _rx) = mpsc::channel(2);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Down,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        assert_eq!(app_state.ui.rss.selected_explorer_index, 1);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Left,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        assert!(matches!(
            app_state.ui.rss.focused_section,
            RssSectionFocus::Explorer
        ));

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Right,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        assert!(matches!(
            app_state.ui.rss.focused_section,
            RssSectionFocus::Explorer
        ));
    }

    #[test]
    fn sync_key_enqueues_command() {
        let mut app_state = base_state();
        let settings = crate::config::Settings::default();
        let (tx, mut rx) = mpsc::channel(2);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('s'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        let cmd = rx.try_recv().expect("expected rss sync command");
        assert!(matches!(cmd, AppCommand::RssSyncNow));
        assert_eq!(
            app_state.ui.rss.status_message.as_deref(),
            Some("RSS sync requested")
        );
    }

    #[test]
    fn sync_key_auto_enables_rss_when_disabled() {
        let mut app_state = base_state();
        let mut settings = crate::config::Settings::default();
        settings.rss.enabled = false;
        let (tx, mut rx) = mpsc::channel(4);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('s'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        let first = rx.try_recv().expect("expected first command");
        match first {
            AppCommand::UpdateConfig(s) => assert!(s.rss.enabled),
            _ => panic!("unexpected first command"),
        }

        let second = rx.try_recv().expect("expected second command");
        assert!(matches!(second, AppCommand::RssSyncNow));
    }

    #[test]
    fn sync_key_is_throttled_when_spammed() {
        let mut app_state = base_state();
        let settings = crate::config::Settings::default();
        let (tx, mut rx) = mpsc::channel(4);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('s'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        assert!(matches!(
            rx.try_recv().expect("expected first sync command"),
            AppCommand::RssSyncNow
        ));

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('s'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(
            app_state.ui.rss.status_message.as_deref(),
            Some("RSS sync throttled")
        );
    }

    #[test]
    fn add_link_dispatches_update_config() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Links;
        app_state.ui.rss.is_editing = true;
        let settings = crate::config::Settings::default();
        let (tx, mut rx) = mpsc::channel(8);

        for c in "https://example.com/rss.xml".chars() {
            handle_event(
                CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                    KeyCode::Char(c),
                    ratatui::crossterm::event::KeyModifiers::NONE,
                )),
                &mut app_state,
                &settings,
                &tx,
            );
        }

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Enter,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        let cmd = rx.try_recv().expect("expected UpdateConfig dispatch");
        match cmd {
            AppCommand::UpdateConfig(s) => {
                assert_eq!(s.rss.feeds.len(), 1);
                assert_eq!(s.rss.feeds[0].url, "https://example.com/rss.xml");
                assert!(s.rss.feeds[0].enabled);
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn add_entry_from_explorer_does_not_start_editing() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Explorer;
        app_state.ui.rss.add_filter_mode = RssFilterMode::Fuzzy;
        app_state.ui.rss.edit_buffer.clear();
        let settings = crate::config::Settings::default();
        let (tx, _rx) = mpsc::channel(8);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('a'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        assert!(matches!(
            app_state.ui.rss.focused_section,
            RssSectionFocus::Explorer
        ));
        assert!(!app_state.ui.rss.is_editing);
        assert!(app_state.ui.rss.edit_buffer.is_empty());
        assert!(matches!(
            app_state.ui.rss.add_filter_mode,
            RssFilterMode::Fuzzy
        ));
        assert!(app_state.ui.rss.status_message.is_none());
    }

    #[test]
    fn add_entry_from_links_starts_editing() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Links;
        let settings = crate::config::Settings::default();
        let (tx, _rx) = mpsc::channel(8);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('a'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        assert!(app_state.ui.rss.is_editing);
        assert_eq!(
            app_state.ui.rss.status_message.as_deref(),
            Some("Editing new entry")
        );
    }

    #[test]
    fn add_entry_from_filters_starts_editing() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Filters;
        app_state.ui.rss.add_filter_mode = RssFilterMode::Regex;
        let settings = crate::config::Settings::default();
        let (tx, _rx) = mpsc::channel(8);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('a'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        assert!(app_state.ui.rss.is_editing);
        assert!(app_state.ui.rss.edit_buffer.is_empty());
        assert!(matches!(
            app_state.ui.rss.add_filter_mode,
            RssFilterMode::Fuzzy
        ));
        assert_eq!(
            app_state.ui.rss.status_message.as_deref(),
            Some("Editing new entry")
        );
    }

    #[test]
    fn add_link_reports_failure_when_update_config_enqueue_fails() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Links;
        app_state.ui.rss.is_editing = true;
        let settings = crate::config::Settings::default();
        let (tx, mut rx) = mpsc::channel(1);

        tx.try_send(AppCommand::RssSyncNow)
            .expect("prefill channel to force enqueue failure");

        for c in "https://example.com/rss.xml".chars() {
            handle_event(
                CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                    KeyCode::Char(c),
                    ratatui::crossterm::event::KeyModifiers::NONE,
                )),
                &mut app_state,
                &settings,
                &tx,
            );
        }

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Enter,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        assert_eq!(
            app_state.ui.rss.status_message.as_deref(),
            Some("RSS settings enqueue failed")
        );
        assert!(matches!(rx.try_recv(), Ok(AppCommand::RssSyncNow)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn invalid_feed_url_is_rejected() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Links;
        app_state.ui.rss.is_editing = true;
        let settings = crate::config::Settings::default();
        let (tx, mut rx) = mpsc::channel(8);
        for c in "javascript:alert(1)".chars() {
            handle_event(
                CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                    KeyCode::Char(c),
                    ratatui::crossterm::event::KeyModifiers::NONE,
                )),
                &mut app_state,
                &settings,
                &tx,
            );
        }
        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Enter,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        assert!(rx.try_recv().is_err());
        assert_eq!(
            app_state.ui.rss.status_message.as_deref(),
            Some("Invalid feed URL (use http/https)")
        );
        assert!(!app_state.ui.rss.is_editing);
    }

    #[test]
    fn paste_link_in_edit_mode_dispatches_update_config() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Links;
        app_state.ui.rss.is_editing = true;
        let settings = crate::config::Settings::default();
        let (tx, mut rx) = mpsc::channel(8);

        handle_event(
            CrosstermEvent::Paste("https://example.test/rss/?t&r=1080".to_string()),
            &mut app_state,
            &settings,
            &tx,
        );

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Enter,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        let cmd = rx.try_recv().expect("expected UpdateConfig dispatch");
        match cmd {
            AppCommand::UpdateConfig(s) => {
                assert_eq!(s.rss.feeds.len(), 1);
                assert_eq!(s.rss.feeds[0].url, "https://example.test/rss/?t&r=1080");
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn delete_link_requires_confirmation_then_dispatches_update_config() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Links;
        let mut settings = crate::config::Settings::default();
        settings.rss.feeds.push(crate::config::RssFeed {
            url: "https://a.test/rss".to_string(),
            enabled: true,
        });
        let (tx, mut rx) = mpsc::channel(8);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('D'),
                ratatui::crossterm::event::KeyModifiers::SHIFT,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        assert!(app_state.ui.rss.delete_confirm_armed);
        assert!(rx.try_recv().is_err());

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('Y'),
                ratatui::crossterm::event::KeyModifiers::SHIFT,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        assert!(!app_state.ui.rss.delete_confirm_armed);
        let cmd = rx.try_recv().expect("expected UpdateConfig dispatch");
        match cmd {
            AppCommand::UpdateConfig(s) => assert!(s.rss.feeds.is_empty()),
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn delete_link_confirmation_can_be_cancelled_with_escape() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Links;
        let mut settings = crate::config::Settings::default();
        settings.rss.feeds.push(crate::config::RssFeed {
            url: "https://a.test/rss".to_string(),
            enabled: true,
        });
        let (tx, mut rx) = mpsc::channel(8);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('D'),
                ratatui::crossterm::event::KeyModifiers::SHIFT,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        assert!(app_state.ui.rss.delete_confirm_armed);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Esc,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        assert!(!app_state.ui.rss.delete_confirm_armed);
        assert_eq!(
            app_state.ui.rss.status_message.as_deref(),
            Some("Delete cancelled")
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn toggle_link_dispatches_update_config() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Links;
        let mut settings = crate::config::Settings::default();
        settings.rss.feeds.push(crate::config::RssFeed {
            url: "https://a.test/rss".to_string(),
            enabled: true,
        });
        let (tx, mut rx) = mpsc::channel(8);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char(' '),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        let cmd = rx.try_recv().expect("expected UpdateConfig dispatch");
        match cmd {
            AppCommand::UpdateConfig(s) => assert!(!s.rss.feeds[0].enabled),
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn toggle_filter_dispatches_update_config() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Filters;
        let mut settings = crate::config::Settings::default();
        settings.rss.filters.push(crate::config::RssFilter {
            query: "samplealpha".to_string(),
            mode: RssFilterMode::Fuzzy,
            enabled: true,
        });
        let (tx, mut rx) = mpsc::channel(8);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char(' '),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        let cmd = rx.try_recv().expect("expected UpdateConfig dispatch");
        match cmd {
            AppCommand::UpdateConfig(s) => assert!(!s.rss.filters[0].enabled),
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn tab_toggles_filter_mode_while_editing_filter() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Filters;
        let settings = crate::config::Settings::default();
        let (tx, _rx) = mpsc::channel(8);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('a'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        assert!(app_state.ui.rss.is_editing);
        assert!(matches!(
            app_state.ui.rss.add_filter_mode,
            RssFilterMode::Fuzzy
        ));

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Tab,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        assert!(matches!(
            app_state.ui.rss.add_filter_mode,
            RssFilterMode::Regex
        ));

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Tab,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        assert!(matches!(
            app_state.ui.rss.add_filter_mode,
            RssFilterMode::Fuzzy
        ));
    }

    #[test]
    fn add_filter_rejects_invalid_regex_pattern() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Filters;
        let settings = crate::config::Settings::default();
        let (tx, mut rx) = mpsc::channel(8);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('a'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        app_state.ui.rss.add_filter_mode = RssFilterMode::Regex;

        for c in "(invalid".chars() {
            handle_event(
                CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                    KeyCode::Char(c),
                    ratatui::crossterm::event::KeyModifiers::NONE,
                )),
                &mut app_state,
                &settings,
                &tx,
            );
        }
        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Enter,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        assert!(rx.try_recv().is_err());
        assert!(app_state.ui.rss.is_editing);
        assert_eq!(
            app_state.ui.rss.status_message.as_deref(),
            Some("Invalid regex pattern")
        );
    }

    #[test]
    fn add_filter_rejects_duplicate_filter() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Filters;
        let mut settings = crate::config::Settings::default();
        settings.rss.filters.push(crate::config::RssFilter {
            query: "samplealpha".to_string(),
            mode: RssFilterMode::Fuzzy,
            enabled: true,
        });
        let (tx, mut rx) = mpsc::channel(8);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('a'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        for c in "samplealpha".chars() {
            handle_event(
                CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                    KeyCode::Char(c),
                    ratatui::crossterm::event::KeyModifiers::NONE,
                )),
                &mut app_state,
                &settings,
                &tx,
            );
        }
        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Enter,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        assert!(rx.try_recv().is_err());
        assert!(app_state.ui.rss.is_editing);
        assert_eq!(
            app_state.ui.rss.status_message.as_deref(),
            Some("Filter already exists")
        );
    }

    #[test]
    fn add_filter_rejects_duplicate_filter_with_case_and_whitespace() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Filters;
        let mut settings = crate::config::Settings::default();
        settings.rss.filters.push(crate::config::RssFilter {
            query: "  SampleAlpha  ".to_string(),
            mode: RssFilterMode::Fuzzy,
            enabled: true,
        });
        let (tx, mut rx) = mpsc::channel(8);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('a'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        for c in "samplealpha".chars() {
            handle_event(
                CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                    KeyCode::Char(c),
                    ratatui::crossterm::event::KeyModifiers::NONE,
                )),
                &mut app_state,
                &settings,
                &tx,
            );
        }
        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Enter,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        assert!(rx.try_recv().is_err());
        assert!(app_state.ui.rss.is_editing);
        assert_eq!(
            app_state.ui.rss.status_message.as_deref(),
            Some("Filter already exists")
        );
    }

    #[test]
    fn shift_y_downloads_selected_explorer_item_when_not_downloaded() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Explorer;
        app_state.rss_runtime.preview_items.push(RssPreviewItem {
            title: "SampleAlpha ISO".to_string(),
            link: Some("magnet:?xt=urn:btih:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
            dedupe_key: "guid:samplealpha".to_string(),
            is_downloaded: false,
            ..Default::default()
        });

        let settings = crate::config::Settings::default();
        let (tx, mut rx) = mpsc::channel(8);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('Y'),
                ratatui::crossterm::event::KeyModifiers::SHIFT,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        let cmd = rx.try_recv().expect("expected RSS download command");
        match cmd {
            AppCommand::RssDownloadPreview(item) => {
                assert_eq!(item.title, "SampleAlpha ISO");
                assert_eq!(item.dedupe_key, "guid:samplealpha");
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn shift_y_allows_selected_explorer_item_when_already_downloaded() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Explorer;
        app_state.rss_runtime.preview_items.push(RssPreviewItem {
            title: "SampleAlpha ISO".to_string(),
            link: Some("magnet:?xt=urn:btih:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
            dedupe_key: "guid:samplealpha".to_string(),
            is_downloaded: true,
            ..Default::default()
        });

        let settings = crate::config::Settings::default();
        let (tx, mut rx) = mpsc::channel(8);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('Y'),
                ratatui::crossterm::event::KeyModifiers::SHIFT,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        let cmd = rx.try_recv().expect("expected RSS download command");
        match cmd {
            AppCommand::RssDownloadPreview(item) => {
                assert_eq!(item.title, "SampleAlpha ISO");
                assert_eq!(item.dedupe_key, "guid:samplealpha");
            }
            _ => panic!("unexpected command"),
        }
        assert_eq!(
            app_state.ui.rss.status_message.as_deref(),
            Some("RSS download requested")
        );
    }

    #[test]
    fn explorer_search_mode_sets_and_clears_status() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Explorer;
        let settings = crate::config::Settings::default();
        let (tx, _rx) = mpsc::channel(8);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('/'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        assert!(app_state.ui.rss.is_searching);
        assert_eq!(
            app_state.ui.rss.status_message.as_deref(),
            Some("Search mode")
        );

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Esc,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        assert!(!app_state.ui.rss.is_searching);
        assert_eq!(
            app_state.ui.rss.status_message.as_deref(),
            Some("Search cleared")
        );
    }

    #[test]
    fn history_search_mode_sets_and_clears_status() {
        let mut app_state = base_state();
        app_state.ui.rss.active_screen = RssScreen::History;
        let settings = crate::config::Settings::default();
        let (tx, _rx) = mpsc::channel(8);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('/'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        assert!(app_state.ui.rss.is_searching);
        assert_eq!(
            app_state.ui.rss.status_message.as_deref(),
            Some("Search mode")
        );

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Esc,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        assert!(!app_state.ui.rss.is_searching);
        assert_eq!(
            app_state.ui.rss.status_message.as_deref(),
            Some("Search cleared")
        );
    }

    #[test]
    fn backspace_does_not_exit_search_mode_when_query_becomes_empty() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Explorer;
        let settings = crate::config::Settings::default();
        let (tx, _rx) = mpsc::channel(8);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('/'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('x'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );
        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Backspace,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        assert!(app_state.ui.rss.is_searching);
        assert!(app_state.ui.rss.search_query.is_empty());
    }

    #[test]
    fn explorer_compute_filters_out_non_matches_when_search_active() {
        let items = vec![
            RssPreviewItem {
                title: "SampleAlpha LTS".to_string(),
                is_match: true,
                ..Default::default()
            },
            RssPreviewItem {
                title: "SampleBeta".to_string(),
                is_match: false,
                ..Default::default()
            },
        ];

        let (sorted, combined, prioritise) =
            compute_explorer_items(&items, "samplealpha", &[], "", RssFilterMode::Fuzzy, false);
        assert!(prioritise);
        assert_eq!(sorted.len(), 1);
        assert_eq!(combined.len(), 1);
        assert_eq!(sorted[0].title, "SampleAlpha LTS");
    }

    #[test]
    fn search_enter_keeps_mode_active_when_query_non_empty() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Explorer;
        let settings = crate::config::Settings::default();
        let (tx, _rx) = mpsc::channel(8);

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('/'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char('f'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Enter,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
            &mut app_state,
            &settings,
            &tx,
        );

        assert!(app_state.ui.rss.is_searching);
    }

    #[test]
    fn explorer_compute_sorts_matches_first_only_when_active() {
        let items = vec![
            RssPreviewItem {
                title: "Non match".to_string(),
                is_match: false,
                ..Default::default()
            },
            RssPreviewItem {
                title: "Match".to_string(),
                is_match: true,
                ..Default::default()
            },
        ];

        let (inactive_sorted, _, inactive_prioritise) =
            compute_explorer_items(&items, "", &[], "", RssFilterMode::Fuzzy, false);
        assert!(!inactive_prioritise);
        assert_eq!(inactive_sorted[0].title, "Match");

        let enabled = vec![FilterSpec {
            query: "match".to_string(),
            mode: RssFilterMode::Fuzzy,
        }];
        let (active_sorted, _, active_prioritise) =
            compute_explorer_items(&items, "", &enabled, "", RssFilterMode::Fuzzy, false);
        assert!(active_prioritise);
        assert_eq!(active_sorted[0].title, "Match");
    }

    #[test]
    fn explorer_compute_prefers_draft_matches_while_creating_filter() {
        let items = vec![
            RssPreviewItem {
                title: "Series Beta".to_string(),
                ..Default::default()
            },
            RssPreviewItem {
                title: "Series Alpha".to_string(),
                ..Default::default()
            },
        ];

        let enabled = vec![FilterSpec {
            query: "series beta".to_string(),
            mode: RssFilterMode::Fuzzy,
        }];
        let (sorted, _, prioritise) =
            compute_explorer_items(&items, "", &enabled, "alpha", RssFilterMode::Fuzzy, true);
        assert!(prioritise);
        assert_eq!(sorted[0].title, "Series Alpha");
    }

    #[test]
    fn explorer_compute_supports_regex_draft_matching() {
        let items = vec![
            RssPreviewItem {
                title: "Series Beta".to_string(),
                ..Default::default()
            },
            RssPreviewItem {
                title: "Series Alpha".to_string(),
                ..Default::default()
            },
        ];

        let (sorted, _, prioritise) = compute_explorer_items(
            &items,
            "",
            &[],
            "series\\s+alpha",
            RssFilterMode::Regex,
            true,
        );
        assert!(prioritise);
        assert_eq!(sorted[0].title, "Series Alpha");
    }

    #[test]
    fn explorer_compute_prefers_regex_draft_matches_over_existing_filter_matches() {
        let items = vec![
            RssPreviewItem {
                title: "Series Beta".to_string(),
                ..Default::default()
            },
            RssPreviewItem {
                title: "Series Alpha".to_string(),
                ..Default::default()
            },
        ];
        let enabled = vec![FilterSpec {
            query: "series beta".to_string(),
            mode: RssFilterMode::Fuzzy,
        }];

        let (sorted, _, prioritise) = compute_explorer_items(
            &items,
            "",
            &enabled,
            "series\\s+alpha",
            RssFilterMode::Regex,
            true,
        );
        assert!(prioritise);
        assert_eq!(sorted[0].title, "Series Alpha");
    }

    #[test]
    fn filter_preview_keeps_all_items_and_sorts_matches_first() {
        let items = vec![
            RssPreviewItem {
                title: "SampleBeta".to_string(),
                ..Default::default()
            },
            RssPreviewItem {
                title: "SampleAlpha LTS".to_string(),
                ..Default::default()
            },
        ];

        let ranked = compute_filter_preview_items(&items, "samplealpha");
        assert_eq!(ranked.len(), 2);
        assert!(ranked[0].1);
        assert_eq!(ranked[0].0.title, "SampleAlpha LTS");
        assert!(!ranked[1].1);
        assert_eq!(ranked[1].0.title, "SampleBeta");
    }

    #[test]
    fn filter_preview_with_empty_draft_still_shows_full_list() {
        let items = vec![
            RssPreviewItem {
                title: "SampleBeta".to_string(),
                ..Default::default()
            },
            RssPreviewItem {
                title: "SampleAlpha".to_string(),
                ..Default::default()
            },
        ];

        let ranked = compute_filter_preview_items(&items, "");
        assert_eq!(ranked.len(), 2);
        assert!(ranked.iter().all(|(_, is_match)| *is_match));
    }

    #[test]
    fn compute_filter_match_counts_counts_feed_and_downloaded_from_torrent_hash() {
        let mut app_state = base_state();
        app_state.rss_runtime.preview_items.push(RssPreviewItem {
            title: "Series Alpha Episode 1".to_string(),
            link: Some("magnet:?xt=urn:btih:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
            ..Default::default()
        });
        app_state.rss_runtime.preview_items.push(RssPreviewItem {
            title: "Series Beta Episode 1".to_string(),
            ..Default::default()
        });

        let mut torrent = crate::app::TorrentDisplayState::default();
        torrent.latest_state.torrent_name = "Series Alpha Batch".to_string();
        app_state.torrents.insert(vec![0xaa; 20], torrent);

        let matcher = SkimMatcherV2::default();
        let (feed, downloaded) =
            compute_filter_match_counts(&app_state, "alpha", RssFilterMode::Fuzzy, &matcher);
        assert_eq!(feed, 1);
        assert_eq!(downloaded, 1);
    }

    #[test]
    fn compute_filter_match_counts_falls_back_to_history_when_no_torrent_hash_match() {
        let mut app_state = base_state();
        app_state.rss_runtime.preview_items.push(RssPreviewItem {
            dedupe_key: "guid:series-alpha-1".to_string(),
            title: "Series Alpha Episode 1".to_string(),
            link: Some("https://example.test/series-alpha-1.torrent".to_string()),
            ..Default::default()
        });
        app_state
            .rss_runtime
            .history
            .push(crate::config::RssHistoryEntry {
                dedupe_key: "guid:series-alpha-1".to_string(),
                title: "Series Alpha Episode 1".to_string(),
                ..Default::default()
            });

        let matcher = SkimMatcherV2::default();
        let (feed, downloaded) =
            compute_filter_match_counts(&app_state, "alpha", RssFilterMode::Fuzzy, &matcher);
        assert_eq!(feed, 1);
        assert_eq!(downloaded, 1);
    }

    #[test]
    fn compute_filter_match_counts_uses_history_when_no_feed_matches() {
        let mut app_state = base_state();
        app_state
            .rss_runtime
            .history
            .push(crate::config::RssHistoryEntry {
                dedupe_key: "guid:series-seriessigma-1".to_string(),
                title: "Series Sigma Episode 54".to_string(),
                ..Default::default()
            });
        app_state
            .rss_runtime
            .history
            .push(crate::config::RssHistoryEntry {
                dedupe_key: "guid:series-seriessigma-2".to_string(),
                title: "Series Sigma Episode 55".to_string(),
                ..Default::default()
            });

        let matcher = SkimMatcherV2::default();
        let (feed, downloaded) =
            compute_filter_match_counts(&app_state, "seriessigma", RssFilterMode::Fuzzy, &matcher);
        assert_eq!(feed, 0);
        assert_eq!(downloaded, 2);
    }

    #[test]
    fn compute_filter_match_counts_counts_app_state_and_missing_history_entries() {
        let mut app_state = base_state();

        let mut torrent_one = crate::app::TorrentDisplayState::default();
        torrent_one.latest_state.torrent_name = "Series Sigma Episode 1".to_string();
        app_state.torrents.insert(vec![1; 20], torrent_one);
        let mut torrent_two = crate::app::TorrentDisplayState::default();
        torrent_two.latest_state.torrent_name = "Series Sigma Episode 2".to_string();
        app_state.torrents.insert(vec![2; 20], torrent_two);

        app_state
            .rss_runtime
            .history
            .push(crate::config::RssHistoryEntry {
                dedupe_key: "guid:series-seriessigma-1".to_string(),
                info_hash: Some(hex::encode(vec![1; 20])),
                title: "Series Sigma Episode 1".to_string(),
                ..Default::default()
            });
        app_state
            .rss_runtime
            .history
            .push(crate::config::RssHistoryEntry {
                dedupe_key: "guid:series-seriessigma-2".to_string(),
                info_hash: Some(hex::encode(vec![2; 20])),
                title: "Series Sigma Episode 2".to_string(),
                ..Default::default()
            });
        app_state
            .rss_runtime
            .history
            .push(crate::config::RssHistoryEntry {
                dedupe_key: "guid:series-seriessigma-3".to_string(),
                title: "Series Sigma Episode 3".to_string(),
                ..Default::default()
            });

        let matcher = SkimMatcherV2::default();
        let (feed, downloaded) =
            compute_filter_match_counts(&app_state, "seriessigma", RssFilterMode::Fuzzy, &matcher);
        assert_eq!(feed, 0);
        assert_eq!(downloaded, 3);
    }

    #[test]
    fn active_filter_spec_uses_selected_filter_in_nav_mode() {
        let mut app_state = base_state();
        app_state.ui.rss.active_screen = RssScreen::Unified;
        app_state.ui.rss.focused_section = RssSectionFocus::Filters;
        app_state.ui.rss.is_editing = false;
        app_state.ui.rss.filter_draft.clear();
        app_state.ui.rss.selected_filter_index = 1;

        let mut settings = crate::config::Settings::default();
        settings.rss.filters.push(crate::config::RssFilter {
            query: "samplealpha".to_string(),
            mode: RssFilterMode::Fuzzy,
            enabled: true,
        });
        settings.rss.filters.push(crate::config::RssFilter {
            query: "samplebeta".to_string(),
            mode: RssFilterMode::Regex,
            enabled: true,
        });

        let spec = active_filter_spec(&app_state, &settings).expect("expected active filter");
        assert_eq!(spec.query, "samplebeta");
        assert!(matches!(spec.mode, RssFilterMode::Regex));
    }

    #[test]
    fn active_filter_spec_ignores_disabled_selected_filter() {
        let mut app_state = base_state();
        app_state.ui.rss.active_screen = RssScreen::Unified;
        app_state.ui.rss.focused_section = RssSectionFocus::Filters;
        app_state.ui.rss.is_editing = false;
        app_state.ui.rss.filter_draft.clear();
        app_state.ui.rss.selected_filter_index = 0;

        let mut settings = crate::config::Settings::default();
        settings.rss.filters.push(crate::config::RssFilter {
            query: "seriesdelta".to_string(),
            mode: RssFilterMode::Fuzzy,
            enabled: false,
        });

        assert!(active_filter_spec(&app_state, &settings).is_none());
    }

    #[test]
    fn active_filter_spec_ignores_stale_draft_when_not_editing() {
        let mut app_state = base_state();
        app_state.ui.rss.active_screen = RssScreen::Unified;
        app_state.ui.rss.focused_section = RssSectionFocus::Explorer;
        app_state.ui.rss.is_editing = false;
        app_state.ui.rss.edit_buffer.clear();
        app_state.ui.rss.filter_draft = "seriesomega".to_string();

        let settings = crate::config::Settings::default();
        assert!(active_filter_spec(&app_state, &settings).is_none());
    }

    #[test]
    fn focused_filter_query_uses_selected_filter_in_filters_focus() {
        let mut app_state = base_state();
        app_state.ui.rss.active_screen = RssScreen::Unified;
        app_state.ui.rss.focused_section = RssSectionFocus::Filters;
        app_state.ui.rss.is_editing = false;

        let mut settings = crate::config::Settings::default();
        settings.rss.filters.push(crate::config::RssFilter {
            query: "series alpha".to_string(),
            mode: RssFilterMode::Fuzzy,
            enabled: true,
        });

        let focused = focused_filter_query(&app_state, &settings).expect("focused filter");
        assert_eq!(focused.query, "series alpha");
        assert!(matches!(focused.mode, RssFilterMode::Fuzzy));
    }

    #[test]
    fn focused_filter_query_is_none_when_not_on_filters_focus() {
        let mut app_state = base_state();
        app_state.ui.rss.active_screen = RssScreen::Unified;
        app_state.ui.rss.focused_section = RssSectionFocus::Explorer;
        app_state.ui.rss.is_editing = false;

        let mut settings = crate::config::Settings::default();
        settings.rss.filters.push(crate::config::RssFilter {
            query: "series alpha".to_string(),
            mode: RssFilterMode::Fuzzy,
            enabled: true,
        });

        assert!(focused_filter_query(&app_state, &settings).is_none());
    }

    #[test]
    fn explorer_greyed_out_when_no_filters_exist() {
        let settings = crate::config::Settings::default();
        assert!(explorer_should_be_greyed_out(&settings));
    }

    #[test]
    fn explorer_greyed_out_when_all_filters_disabled() {
        let mut settings = crate::config::Settings::default();
        settings.rss.filters.push(crate::config::RssFilter {
            query: "samplealpha".to_string(),
            mode: RssFilterMode::Fuzzy,
            enabled: false,
        });
        settings.rss.filters.push(crate::config::RssFilter {
            query: "samplebeta".to_string(),
            mode: RssFilterMode::Fuzzy,
            enabled: false,
        });
        assert!(explorer_should_be_greyed_out(&settings));
    }

    #[test]
    fn explorer_not_greyed_out_when_any_filter_enabled() {
        let mut settings = crate::config::Settings::default();
        settings.rss.filters.push(crate::config::RssFilter {
            query: "samplealpha".to_string(),
            mode: RssFilterMode::Fuzzy,
            enabled: false,
        });
        settings.rss.filters.push(crate::config::RssFilter {
            query: "samplebeta".to_string(),
            mode: RssFilterMode::Fuzzy,
            enabled: true,
        });
        assert!(!explorer_should_be_greyed_out(&settings));
    }

    #[test]
    fn explorer_effective_greyed_out_is_false_while_creating_first_filter_with_draft() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Filters;
        app_state.ui.rss.is_editing = true;
        app_state.ui.rss.edit_buffer = "sample draft".to_string();

        let settings = crate::config::Settings::default();
        assert!(!explorer_effective_greyed_out(&app_state, &settings));
    }

    #[test]
    fn explorer_effective_greyed_out_is_true_while_creating_first_filter_without_draft() {
        let mut app_state = base_state();
        app_state.ui.rss.focused_section = RssSectionFocus::Filters;
        app_state.ui.rss.is_editing = true;
        app_state.ui.rss.edit_buffer.clear();

        let settings = crate::config::Settings::default();
        assert!(explorer_effective_greyed_out(&app_state, &settings));
    }

    #[test]
    fn sync_countdown_label_formats_minutes_and_seconds() {
        let future = (Utc::now() + chrono::Duration::seconds(274)).to_rfc3339();
        let label = sync_countdown_label(&future).expect("expected countdown");
        assert!(label.ends_with('s'));
        assert!(label.contains('m'));
    }

    #[test]
    fn sync_countdown_label_returns_none_for_past_timestamp() {
        let past = (Utc::now() - chrono::Duration::seconds(5)).to_rfc3339();
        assert!(sync_countdown_label(&past).is_none());
    }

    #[test]
    fn is_valid_feed_url_rejects_localhost_and_private_ips() {
        assert!(!is_valid_feed_url("http://localhost/rss"));
        assert!(!is_valid_feed_url("https://127.0.0.1/feed.xml"));
        assert!(!is_valid_feed_url("https://192.168.1.20/rss"));
    }

    #[test]
    fn is_valid_feed_url_accepts_public_https_feed() {
        assert!(is_valid_feed_url("https://example.com/rss.xml"));
    }

    #[test]
    fn truncate_with_ellipsis_shortens_long_text() {
        assert_eq!(truncate_with_ellipsis("abcdefghij", 6), "abc...");
    }

    #[test]
    fn filtered_history_entries_respects_search_query() {
        let entries = vec![
            crate::config::RssHistoryEntry {
                title: "Series Alpha".to_string(),
                source: Some("ExampleFeed".to_string()),
                date_iso: "2026-02-17T10:00:00Z".to_string(),
                ..Default::default()
            },
            crate::config::RssHistoryEntry {
                title: "Series Gamma".to_string(),
                source: Some("ExampleFeed".to_string()),
                date_iso: "2026-02-16T10:00:00Z".to_string(),
                ..Default::default()
            },
        ];

        let filtered = filtered_history_entries(&entries, "alpha");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].title, "Series Alpha");
    }

    #[test]
    fn human_readable_history_time_formats_rfc3339() {
        let ts = "2026-02-17T10:05:00Z";
        assert_eq!(human_readable_history_time(ts).len(), 16);
    }

    #[test]
    fn link_matches_selected_explorer_item_matches_by_host() {
        let item = RssPreviewItem {
            link: Some("https://example.test/item/abc".to_string()),
            ..Default::default()
        };
        assert!(link_matches_selected_explorer_item(
            "https://example.test/rss/?t&r=1080",
            &item
        ));
    }

    #[test]
    fn link_matches_selected_explorer_item_matches_by_source_hint() {
        let item = RssPreviewItem {
            link: Some("magnet:?xt=urn:btih:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
            source: Some("ExampleFeed RSS".to_string()),
            ..Default::default()
        };
        assert!(link_matches_selected_explorer_item(
            "https://example.test/rss/?t&r=1080",
            &item
        ));
    }

    #[test]
    fn rss_item_completion_percent_is_none_without_live_torrent_metrics() {
        let app_state = base_state();
        let history_hash_map = build_history_hash_by_dedupe(&app_state.rss_runtime.history);
        let item = RssPreviewItem {
            title: "Series Alpha".to_string(),
            is_downloaded: true,
            link: Some("magnet:?xt=urn:btih:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
            ..Default::default()
        };

        let completion_by_title = HashMap::new();
        assert!(rss_item_completion_percent(
            &item,
            &app_state,
            &history_hash_map,
            &completion_by_title
        )
        .is_none());
    }

    #[test]
    fn rss_item_completion_percent_uses_history_info_hash_fallback() {
        let mut app_state = base_state();
        let info_hash = vec![0xaa; 20];

        let mut torrent = crate::app::TorrentDisplayState::default();
        torrent.latest_state.number_of_pieces_total = 10;
        torrent.latest_state.number_of_pieces_completed = 10;
        app_state.torrents.insert(info_hash.clone(), torrent);

        app_state
            .rss_runtime
            .history
            .push(crate::config::RssHistoryEntry {
                dedupe_key: "guid:series-alpha".to_string(),
                info_hash: Some(hex::encode(&info_hash)),
                title: "Series Alpha".to_string(),
                ..Default::default()
            });

        let item = RssPreviewItem {
            dedupe_key: "guid:series-alpha".to_string(),
            title: "Series Alpha".to_string(),
            link: Some("https://example.test/series-alpha.torrent".to_string()),
            is_downloaded: true,
            ..Default::default()
        };

        let history_hash_map = build_history_hash_by_dedupe(&app_state.rss_runtime.history);
        let completion_by_title = HashMap::new();
        assert_eq!(
            rss_item_completion_percent(&item, &app_state, &history_hash_map, &completion_by_title),
            Some(100.0)
        );
    }

    #[test]
    fn rss_item_completion_percent_does_not_require_downloaded_flag() {
        let mut app_state = base_state();
        let info_hash = vec![0xbb; 20];

        let mut torrent = crate::app::TorrentDisplayState::default();
        torrent.latest_state.number_of_pieces_total = 10;
        torrent.latest_state.number_of_pieces_completed = 10;
        app_state.torrents.insert(info_hash.clone(), torrent);

        app_state
            .rss_runtime
            .history
            .push(crate::config::RssHistoryEntry {
                dedupe_key: "guid:series-beta".to_string(),
                info_hash: Some(hex::encode(&info_hash)),
                title: "Series Beta".to_string(),
                ..Default::default()
            });

        let item = RssPreviewItem {
            dedupe_key: "guid:series-beta".to_string(),
            title: "Series Beta".to_string(),
            is_downloaded: false,
            ..Default::default()
        };

        let history_hash_map = build_history_hash_by_dedupe(&app_state.rss_runtime.history);
        let completion_by_title = HashMap::new();
        assert_eq!(
            rss_item_completion_percent(&item, &app_state, &history_hash_map, &completion_by_title),
            Some(100.0)
        );
    }

    #[test]
    fn unified_layout_is_narrow_below_boundary() {
        assert!(matches!(
            unified_layout_for_width(139),
            UnifiedLayout::Narrow
        ));
    }

    #[test]
    fn unified_layout_is_wide_at_boundary() {
        assert!(matches!(unified_layout_for_width(140), UnifiedLayout::Wide));
    }
}
