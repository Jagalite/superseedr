// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{
    torrent_completion_percent, AppMode, AppState, SearchMode, TorrentControlState,
    TorrentDisplayState, TorrentManagementPendingCommand, TorrentManagementReviewCache,
};
use crate::config::SortDirection;
use crate::integrations::control::ControlRequest;
use crate::terminal_event::{
    Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crate::theme::ThemeContext;
use crate::tui::action_style::{footer_key_style, ActionTone};
pub use crate::tui::effects::TorrentManagementEffect;
use crate::tui::formatters::{
    anonymize_preserving_shape, format_bytes, format_duration, format_speed, sanitize_text,
    speed_to_style, terminal_text_width, truncate_middle_with_ellipsis, truncate_with_ellipsis,
};
use crate::tui::layout::common::{compute_smart_table_layout, SmartCol};
use crate::tui::screen_context::ScreenContext;
use crate::tui::screens::input_panel::draw_prompt_panel;
use chrono::{DateTime, Local};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::prelude::{Color, Frame, Line, Modifier, Span, Style};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Padding, Paragraph, Row, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Table, TableState,
};
use std::cmp::Ordering;
use std::collections::HashSet;
use std::time::{Duration, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TorrentManagementAction {
    ToNormal,
    MoveUp,
    MoveDown,
    MovePageUp,
    MovePageDown,
    MoveFirst,
    MoveLast,
    MoveColumnLeft,
    MoveColumnRight,
    SortBySelectedColumn,
    StartSearch,
    SearchInsert(char),
    SearchBackspace,
    SearchCommit,
    SearchCancel,
    ToggleSearchMode,
    ToggleAnonymizeNames,
    ToggleCurrentSelection,
    SelectAllVisible,
    ClearPendingForTargets,
    OpenHighlightedTorrentFiles,
    TogglePauseTargets,
    StartDelete { delete_files: bool },
    ShowSubmitConfirmation,
    CancelSubmitConfirmation,
    SubmitPendingCommands,
    ReviewScrollUp,
    ReviewScrollDown,
    ReviewPageUp,
    ReviewPageDown,
    ReviewFirst,
    ReviewLast,
}

#[derive(Default)]
pub struct TorrentManagementReduceResult {
    pub consumed: bool,
    pub redraw: bool,
    pub effects: Vec<TorrentManagementEffect>,
}

#[derive(Clone, Debug, PartialEq)]
struct ManagementRow {
    kind: ManagementRowKind,
    label: String,
    info_hashes: Vec<Vec<u8>>,
    depth: usize,
    metrics: RowMetrics,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ManagementRowKind {
    Torrent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManagementColumnId {
    Selection,
    Name,
    Completed,
    State,
    Peers,
    DownSpeed,
    UpSpeed,
    Eta,
    Size,
    DateAdded,
}

#[derive(Clone, Debug)]
struct ManagementColumnDefinition {
    id: ManagementColumnId,
    header: &'static str,
    min_width: u16,
    priority: u8,
    constraint: Constraint,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct RowMetrics {
    count: usize,
    completed: f64,
    state_label: String,
    peer_count: usize,
    download_bps: u64,
    upload_bps: u64,
    eta: Option<Duration>,
    total_size: u64,
    added_at_unix_secs: Option<u64>,
}

#[derive(Default)]
struct PendingManagementSummary {
    pause_count: usize,
    resume_count: usize,
    remove_count: usize,
    purge_count: usize,
}

#[derive(Clone, Copy)]
enum ManagementReviewAction {
    Pause,
    Resume,
    Remove,
    Purge,
}

struct ManagementReviewSection<'a> {
    action: ManagementReviewAction,
    names: &'a [String],
    detail: Option<String>,
}

#[derive(Clone, Copy)]
struct ManagementReviewRegions {
    summary: Rect,
    body: Rect,
    footer: Rect,
    compact: bool,
}

pub fn handle_event(
    event: CrosstermEvent,
    app_state: &mut AppState,
) -> TorrentManagementReduceResult {
    if !matches!(app_state.mode, AppMode::TorrentManagement) {
        return TorrentManagementReduceResult::default();
    }

    let CrosstermEvent::Key(key) = event else {
        return TorrentManagementReduceResult::default();
    };
    let Some(action) = map_key_event_to_management_action_with_latch(key, app_state) else {
        return TorrentManagementReduceResult::default();
    };
    let result = reduce_torrent_management_action(app_state, action);
    if result.redraw {
        app_state.ui.needs_redraw = true;
    }
    result
}

fn map_key_event_to_management_action_with_latch(
    key: KeyEvent,
    app_state: &mut AppState,
) -> Option<TorrentManagementAction> {
    if key.kind == KeyEventKind::Release {
        if app_state.ui.torrent_management.input_latch == Some(key.code) {
            app_state.ui.torrent_management.input_latch = None;
        }
        return None;
    }
    if let Some(latched) = app_state.ui.torrent_management.input_latch {
        if key.code == latched {
            return None;
        }
        app_state.ui.torrent_management.input_latch = None;
    }
    let action = map_key_event_to_management_action(key, app_state)?;
    if management_action_needs_input_latch(&action) {
        app_state.ui.torrent_management.input_latch = Some(key.code);
    }
    Some(action)
}

pub(crate) fn initialize_torrent_management_cursor(app_state: &mut AppState) {
    app_state.ui.torrent_management.selected_index = 0;
    app_state.ui.torrent_management.cursor_hash = None;
    app_state.ui.torrent_management.input_latch = None;
    app_state.ui.torrent_management.review_cache = None;
    normalize_management_cursor(app_state);
}

fn map_key_event_to_management_action(
    key: KeyEvent,
    app_state: &AppState,
) -> Option<TorrentManagementAction> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        || !matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
    {
        return None;
    }

    let action = map_key_to_management_action(key.code, app_state)?;
    if matches!(key.kind, KeyEventKind::Repeat)
        && (!management_action_allows_repeat(&action)
            || matches!(action, TorrentManagementAction::SearchInsert('/')))
    {
        return None;
    }
    Some(action)
}

fn management_action_allows_repeat(action: &TorrentManagementAction) -> bool {
    matches!(
        action,
        TorrentManagementAction::MoveUp
            | TorrentManagementAction::MoveDown
            | TorrentManagementAction::MovePageUp
            | TorrentManagementAction::MovePageDown
            | TorrentManagementAction::MoveColumnLeft
            | TorrentManagementAction::MoveColumnRight
            | TorrentManagementAction::SearchInsert(_)
            | TorrentManagementAction::SearchBackspace
            | TorrentManagementAction::ReviewScrollUp
            | TorrentManagementAction::ReviewScrollDown
            | TorrentManagementAction::ReviewPageUp
            | TorrentManagementAction::ReviewPageDown
    )
}

fn management_action_needs_input_latch(action: &TorrentManagementAction) -> bool {
    // `/` changes from an opener into text input across this boundary. Other shortcuts rely on
    // KeyEventKind::Repeat so distinct Press events remain usable without key-release reporting.
    matches!(action, TorrentManagementAction::StartSearch)
}

fn map_key_to_management_action(
    key_code: KeyCode,
    app_state: &AppState,
) -> Option<TorrentManagementAction> {
    if app_state.ui.torrent_management.confirm_submit {
        return match key_code {
            KeyCode::Enter => Some(TorrentManagementAction::SubmitPendingCommands),
            KeyCode::Char('u') => Some(TorrentManagementAction::ClearPendingForTargets),
            KeyCode::Up | KeyCode::Char('k') => Some(TorrentManagementAction::ReviewScrollUp),
            KeyCode::Down | KeyCode::Char('j') => Some(TorrentManagementAction::ReviewScrollDown),
            KeyCode::PageUp => Some(TorrentManagementAction::ReviewPageUp),
            KeyCode::PageDown => Some(TorrentManagementAction::ReviewPageDown),
            KeyCode::Home => Some(TorrentManagementAction::ReviewFirst),
            KeyCode::End => Some(TorrentManagementAction::ReviewLast),
            KeyCode::Esc | KeyCode::Char('q') => {
                Some(TorrentManagementAction::CancelSubmitConfirmation)
            }
            _ => None,
        };
    }

    if app_state.ui.torrent_management.is_searching {
        return match key_code {
            KeyCode::Esc => Some(TorrentManagementAction::SearchCancel),
            KeyCode::Enter => Some(TorrentManagementAction::SearchCommit),
            KeyCode::Tab => Some(TorrentManagementAction::ToggleSearchMode),
            KeyCode::Backspace => Some(TorrentManagementAction::SearchBackspace),
            KeyCode::Char(c) => Some(TorrentManagementAction::SearchInsert(c)),
            _ => None,
        };
    }

    if management_search_panel_active(app_state) && matches!(key_code, KeyCode::Tab) {
        return Some(TorrentManagementAction::ToggleSearchMode);
    }

    match key_code {
        KeyCode::Esc | KeyCode::Char('q') => Some(TorrentManagementAction::ToNormal),
        KeyCode::Up | KeyCode::Char('k') => Some(TorrentManagementAction::MoveUp),
        KeyCode::Down | KeyCode::Char('j') => Some(TorrentManagementAction::MoveDown),
        KeyCode::PageUp => Some(TorrentManagementAction::MovePageUp),
        KeyCode::PageDown => Some(TorrentManagementAction::MovePageDown),
        KeyCode::Home => Some(TorrentManagementAction::MoveFirst),
        KeyCode::End => Some(TorrentManagementAction::MoveLast),
        KeyCode::Left | KeyCode::Char('h') => Some(TorrentManagementAction::MoveColumnLeft),
        KeyCode::Right | KeyCode::Char('l') => Some(TorrentManagementAction::MoveColumnRight),
        KeyCode::Char('s') => Some(TorrentManagementAction::SortBySelectedColumn),
        KeyCode::Char('/') => Some(TorrentManagementAction::StartSearch),
        KeyCode::Char('x') => Some(TorrentManagementAction::ToggleAnonymizeNames),
        KeyCode::Char('Y') if !app_state.ui.torrent_management.pending_commands.is_empty() => {
            Some(TorrentManagementAction::ShowSubmitConfirmation)
        }
        KeyCode::Char(' ') => Some(TorrentManagementAction::ToggleCurrentSelection),
        KeyCode::Char('A') => Some(TorrentManagementAction::SelectAllVisible),
        KeyCode::Char('u') => Some(TorrentManagementAction::ClearPendingForTargets),
        KeyCode::Char('f') => Some(TorrentManagementAction::OpenHighlightedTorrentFiles),
        KeyCode::Char('p') => Some(TorrentManagementAction::TogglePauseTargets),
        KeyCode::Char('d') => Some(TorrentManagementAction::StartDelete {
            delete_files: false,
        }),
        KeyCode::Char('D') => Some(TorrentManagementAction::StartDelete { delete_files: true }),
        _ => None,
    }
}

pub fn reduce_torrent_management_action(
    app_state: &mut AppState,
    action: TorrentManagementAction,
) -> TorrentManagementReduceResult {
    let mut result = TorrentManagementReduceResult {
        consumed: true,
        redraw: true,
        effects: Vec::new(),
    };
    app_state.ui.torrent_management.status_message = None;
    prune_selected_hashes(app_state);
    normalize_management_cursor(app_state);
    normalize_management_review_state(app_state);

    match action {
        TorrentManagementAction::ToNormal => {
            app_state.ui.torrent_management.is_searching = false;
            app_state.ui.torrent_management.search_query.clear();
            app_state.ui.torrent_management.pending_commands.clear();
            app_state.ui.torrent_management.selected_hashes.clear();
            app_state.ui.torrent_management.confirm_submit = false;
            app_state.ui.torrent_management.cursor_hash = None;
            app_state.ui.torrent_management.review_scroll_offset = 0;
            app_state.ui.torrent_management.input_latch = None;
            result.effects.push(TorrentManagementEffect::ToNormal);
        }
        TorrentManagementAction::MoveUp => {
            app_state.ui.torrent_management.selected_index = app_state
                .ui
                .torrent_management
                .selected_index
                .saturating_sub(1);
            set_management_cursor_hash_from_index(app_state);
        }
        TorrentManagementAction::MoveDown => {
            let row_count = build_management_rows(app_state).len();
            if row_count > 0 {
                app_state.ui.torrent_management.selected_index =
                    (app_state.ui.torrent_management.selected_index + 1).min(row_count - 1);
                set_management_cursor_hash_from_index(app_state);
            }
        }
        TorrentManagementAction::MovePageUp => {
            let page = management_page_rows(app_state);
            app_state.ui.torrent_management.selected_index = app_state
                .ui
                .torrent_management
                .selected_index
                .saturating_sub(page);
            set_management_cursor_hash_from_index(app_state);
        }
        TorrentManagementAction::MovePageDown => {
            let row_count = build_management_rows(app_state).len();
            if row_count > 0 {
                let page = management_page_rows(app_state);
                app_state.ui.torrent_management.selected_index = app_state
                    .ui
                    .torrent_management
                    .selected_index
                    .saturating_add(page)
                    .min(row_count - 1);
                set_management_cursor_hash_from_index(app_state);
            }
        }
        TorrentManagementAction::MoveFirst => {
            app_state.ui.torrent_management.selected_index = 0;
            set_management_cursor_hash_from_index(app_state);
        }
        TorrentManagementAction::MoveLast => {
            let row_count = build_management_rows(app_state).len();
            if row_count > 0 {
                app_state.ui.torrent_management.selected_index = row_count - 1;
                set_management_cursor_hash_from_index(app_state);
            }
        }
        TorrentManagementAction::MoveColumnLeft => {
            move_management_column(app_state, -1);
        }
        TorrentManagementAction::MoveColumnRight => {
            move_management_column(app_state, 1);
        }
        TorrentManagementAction::SortBySelectedColumn => {
            let selected_column_index = normalized_selected_management_column_index(app_state);
            app_state.ui.torrent_management.selected_column_index = selected_column_index;
            if app_state.ui.torrent_management.sort_column_index == Some(selected_column_index) {
                app_state.ui.torrent_management.sort_direction =
                    reverse_sort_direction(app_state.ui.torrent_management.sort_direction);
            } else {
                app_state.ui.torrent_management.sort_column_index = Some(selected_column_index);
                app_state.ui.torrent_management.sort_direction =
                    management_column_default_direction(
                        management_columns()[selected_column_index].id,
                    );
            }
        }
        TorrentManagementAction::StartSearch => {
            app_state.ui.torrent_management.is_searching = true;
            app_state.ui.torrent_management.selected_index = 0;
            app_state.ui.torrent_management.cursor_hash = None;
        }
        TorrentManagementAction::SearchInsert(c) => {
            app_state.ui.torrent_management.search_query.push(c);
            app_state.ui.torrent_management.selected_index = 0;
            app_state.ui.torrent_management.cursor_hash = None;
        }
        TorrentManagementAction::SearchBackspace => {
            app_state.ui.torrent_management.search_query.pop();
            app_state.ui.torrent_management.selected_index = 0;
            app_state.ui.torrent_management.cursor_hash = None;
        }
        TorrentManagementAction::SearchCommit => {
            app_state.ui.torrent_management.is_searching = false;
        }
        TorrentManagementAction::SearchCancel => {
            app_state.ui.torrent_management.is_searching = false;
            app_state.ui.torrent_management.search_query.clear();
            app_state.ui.torrent_management.selected_index = 0;
            app_state.ui.torrent_management.cursor_hash = None;
        }
        TorrentManagementAction::ToggleSearchMode => {
            app_state.ui.torrent_management.search_mode =
                match app_state.ui.torrent_management.search_mode {
                    SearchMode::Fuzzy => SearchMode::Regex,
                    SearchMode::Regex => SearchMode::Fuzzy,
                };
            app_state.ui.torrent_management.selected_index = 0;
            app_state.ui.torrent_management.cursor_hash = None;
        }
        TorrentManagementAction::ToggleAnonymizeNames => {
            app_state.anonymize_torrent_names = !app_state.anonymize_torrent_names;
        }
        TorrentManagementAction::ToggleCurrentSelection => {
            let targets = current_row_targets(app_state);
            toggle_hash_selection(app_state, &targets);
        }
        TorrentManagementAction::SelectAllVisible => {
            app_state.ui.torrent_management.selected_hashes.clear();
            for hash in visible_torrent_hashes(app_state) {
                app_state.ui.torrent_management.selected_hashes.insert(hash);
            }
            let selected_count = app_state.ui.torrent_management.selected_hashes.len();
            app_state.ui.torrent_management.status_message =
                Some(format!("Selected {selected_count} visible torrents"));
        }
        TorrentManagementAction::ClearPendingForTargets => {
            let targets = management_clear_targets(app_state);
            let target_set = targets.into_iter().collect::<HashSet<_>>();
            let cleared = clear_pending_management_commands_for_targets(app_state, &target_set);
            let selected_before = app_state.ui.torrent_management.selected_hashes.len();
            app_state
                .ui
                .torrent_management
                .selected_hashes
                .retain(|hash| !target_set.contains(hash));
            let deselected = selected_before
                .saturating_sub(app_state.ui.torrent_management.selected_hashes.len());
            app_state.ui.torrent_management.status_message =
                Some(management_clear_status(cleared, deselected));
            if app_state.ui.torrent_management.pending_commands.is_empty() {
                app_state.ui.torrent_management.confirm_submit = false;
                app_state.ui.torrent_management.review_scroll_offset = 0;
            }
        }
        TorrentManagementAction::OpenHighlightedTorrentFiles => {
            if let Some(info_hash) = current_row_targets(app_state).into_iter().next() {
                result
                    .effects
                    .push(TorrentManagementEffect::OpenExistingTorrentFileBrowser(
                        info_hash,
                    ));
            } else {
                app_state.ui.torrent_management.status_message =
                    Some("No torrent highlighted".to_string());
            }
        }
        TorrentManagementAction::TogglePauseTargets => {
            let targets = management_targets(app_state);
            if targets.is_empty() {
                app_state.ui.torrent_management.status_message =
                    Some("No torrents selected".to_string());
            } else {
                for info_hash in targets {
                    let should_resume = app_state.torrents.get(&info_hash).is_some_and(|torrent| {
                        torrent.latest_state.torrent_control_state == TorrentControlState::Paused
                    });
                    let state = if should_resume {
                        TorrentControlState::Running
                    } else {
                        TorrentControlState::Paused
                    };
                    let request = if should_resume {
                        ControlRequest::Resume {
                            info_hash_hex: hex::encode(&info_hash),
                        }
                    } else {
                        ControlRequest::Pause {
                            info_hash_hex: hex::encode(&info_hash),
                        }
                    };
                    toggle_pending_management_command(
                        app_state,
                        TorrentManagementPendingCommand {
                            info_hash,
                            request,
                            state,
                            delete_files: false,
                        },
                    );
                }
                app_state.ui.torrent_management.status_message =
                    Some(pending_management_status(app_state));
            }
        }
        TorrentManagementAction::StartDelete { delete_files } => {
            let targets = management_targets(app_state);
            if targets.is_empty() {
                app_state.ui.torrent_management.status_message =
                    Some("No torrents selected".to_string());
            } else {
                for info_hash in targets {
                    toggle_pending_management_command(
                        app_state,
                        TorrentManagementPendingCommand {
                            request: ControlRequest::Delete {
                                info_hash_hex: hex::encode(&info_hash),
                                delete_files,
                            },
                            info_hash: info_hash.clone(),
                            state: TorrentControlState::Deleting,
                            delete_files,
                        },
                    );
                }
                app_state.ui.torrent_management.status_message =
                    Some(pending_management_status(app_state));
            }
        }
        TorrentManagementAction::ShowSubmitConfirmation => {
            if app_state.ui.torrent_management.pending_commands.is_empty() {
                app_state.ui.torrent_management.status_message =
                    Some("No draft commands to submit".to_string());
            } else {
                app_state.ui.torrent_management.confirm_submit = true;
                app_state.ui.torrent_management.review_scroll_offset = 0;
            }
        }
        TorrentManagementAction::CancelSubmitConfirmation => {
            app_state.ui.torrent_management.confirm_submit = false;
            app_state.ui.torrent_management.review_scroll_offset = 0;
        }
        TorrentManagementAction::SubmitPendingCommands => {
            app_state.ui.torrent_management.confirm_submit = false;
            app_state.ui.torrent_management.review_scroll_offset = 0;
            let pending_commands =
                std::mem::take(&mut app_state.ui.torrent_management.pending_commands);
            if pending_commands.is_empty() {
                app_state.ui.torrent_management.status_message =
                    Some("No draft commands to submit".to_string());
            } else {
                for command in pending_commands {
                    result
                        .effects
                        .push(TorrentManagementEffect::SubmitControlRequest(
                            command.request,
                        ));
                    result
                        .effects
                        .push(TorrentManagementEffect::MarkControlState {
                            info_hash: command.info_hash.clone(),
                            state: command.state,
                            delete_files: command.delete_files,
                        });
                }
                app_state.ui.torrent_management.selected_hashes.clear();
                app_state.ui.torrent_management.status_message =
                    Some("Draft commands submitted".to_string());
            }
        }
        TorrentManagementAction::ReviewScrollUp => {
            app_state.ui.torrent_management.review_scroll_offset = app_state
                .ui
                .torrent_management
                .review_scroll_offset
                .saturating_sub(1);
        }
        TorrentManagementAction::ReviewScrollDown => {
            app_state.ui.torrent_management.review_scroll_offset = app_state
                .ui
                .torrent_management
                .review_scroll_offset
                .saturating_add(1);
        }
        TorrentManagementAction::ReviewPageUp => {
            let page = management_review_page_lines(app_state);
            app_state.ui.torrent_management.review_scroll_offset = app_state
                .ui
                .torrent_management
                .review_scroll_offset
                .saturating_sub(page);
        }
        TorrentManagementAction::ReviewPageDown => {
            let page = management_review_page_lines(app_state);
            app_state.ui.torrent_management.review_scroll_offset = app_state
                .ui
                .torrent_management
                .review_scroll_offset
                .saturating_add(page);
        }
        TorrentManagementAction::ReviewFirst => {
            app_state.ui.torrent_management.review_scroll_offset = 0;
        }
        TorrentManagementAction::ReviewLast => {
            app_state.ui.torrent_management.review_scroll_offset =
                max_management_review_scroll_offset(app_state);
        }
    }

    prune_selected_hashes(app_state);
    normalize_management_cursor(app_state);
    clamp_management_column_state(app_state);
    normalize_management_review_state(app_state);
    result
}
pub fn draw(f: &mut Frame, screen: &ScreenContext<'_>) {
    let app_state = screen.app.state;
    let ctx = screen.theme;
    let area = f.area();
    f.render_widget(Clear, area);
    let content_area = management_content_area(area);

    let search_panel_active = management_search_panel_active(app_state);
    let footer_height = management_footer_height();
    let chunks = if search_panel_active {
        Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(footer_height),
        ])
        .split(content_area)
    } else {
        Layout::vertical([Constraint::Min(5), Constraint::Length(footer_height)])
            .split(content_area)
    };

    let (table_area, footer_area) = if search_panel_active {
        draw_management_search_panel(f, app_state, chunks[0], ctx);
        (chunks[1], chunks[2])
    } else {
        (chunks[0], chunks[1])
    };

    draw_management_table(f, app_state, table_area, ctx);
    if !app_state.ui.torrent_management.confirm_submit {
        draw_management_footer(f, app_state, footer_area, ctx);
    }

    if app_state.ui.torrent_management.confirm_submit {
        draw_management_review_panel(f, app_state, ctx);
    }
}

fn management_content_area(area: Rect) -> Rect {
    if area.width < 90 || area.height < 18 {
        return area;
    }

    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

fn management_page_rows(app_state: &AppState) -> usize {
    let content_area = management_content_area(app_state.screen_area);
    let search_height = if management_search_panel_active(app_state) {
        3
    } else {
        0
    };
    content_area
        .height
        .saturating_sub(search_height)
        .saturating_sub(management_footer_height())
        .saturating_sub(3) // table borders + header
        .max(1) as usize
}

fn management_footer_height() -> u16 {
    1
}

fn management_search_panel_active(app_state: &AppState) -> bool {
    app_state.ui.torrent_management.is_searching
        || !app_state.ui.torrent_management.search_query.is_empty()
}

fn draw_management_search_panel(
    f: &mut Frame,
    app_state: &AppState,
    area: Rect,
    ctx: &ThemeContext,
) {
    draw_prompt_panel(
        f,
        area,
        " Torrent Search ".to_string(),
        sanitize_text(&app_state.ui.torrent_management.search_query),
        management_search_mode_spans(app_state, ctx),
        ctx,
    );
}

fn management_search_mode_spans(app_state: &AppState, ctx: &ThemeContext) -> Vec<Span<'static>> {
    let (fuzzy_style, regex_style) = match app_state.ui.torrent_management.search_mode {
        SearchMode::Fuzzy => (
            ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
        ),
        SearchMode::Regex => (
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
            ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
        ),
    };
    vec![
        Span::raw("  "),
        Span::styled("Fuzzy", fuzzy_style),
        Span::raw(" / "),
        Span::styled("Regex", regex_style),
    ]
}

fn draw_management_table(f: &mut Frame, app_state: &AppState, area: Rect, ctx: &ThemeContext) {
    let rows = build_management_rows(app_state);
    let cursor_index = management_cursor_index_for_rows(app_state, &rows);
    let all_columns = management_columns();
    let (constraints, visible_columns) = compute_visible_management_columns(area.width);
    let mut table_state = TableState::default();
    if let Some(cursor_index) = cursor_index {
        table_state.select(Some(cursor_index));
    }

    let table_rows = rows
        .iter()
        .enumerate()
        .map(|(idx, row)| {
            management_table_row(
                app_state,
                row,
                cursor_index == Some(idx),
                ctx,
                &visible_columns,
            )
        })
        .collect::<Vec<_>>();

    let header = Row::new(
        visible_columns
            .iter()
            .map(|&idx| {
                let column = &all_columns[idx];
                let is_selected = idx
                    == normalized_selected_column_from_visible(
                        app_state.ui.torrent_management.selected_column_index,
                        &visible_columns,
                    );
                let is_sorting = app_state.ui.torrent_management.sort_column_index == Some(idx);
                let mut style =
                    ctx.apply(Style::default().fg(management_column_header_color(column.id, ctx)));
                if is_sorting {
                    style = ctx.apply(style.bold());
                }

                let mut spans = vec![Span::styled(column.header, style)];
                if is_sorting {
                    spans.push(Span::styled(
                        management_sort_arrow(
                            column.id,
                            app_state.ui.torrent_management.sort_direction,
                        ),
                        style,
                    ));
                }
                if is_selected {
                    spans[0] = spans[0].clone().style(
                        ctx.apply(
                            Style::default()
                                .fg(ctx.theme.scale.categorical.lavender)
                                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                        ),
                    );
                }
                Cell::from(Line::from(spans))
            })
            .collect::<Vec<_>>(),
    )
    .style(ctx.apply(Style::default().fg(ctx.state_warning()).bold()));

    let mut table_block = Block::default()
        .title(Span::styled(
            " Torrents ",
            ctx.apply(Style::default().fg(ctx.state_selected())),
        ))
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)))
        .padding(Padding::new(1, 1, 0, 0));
    if let Some(status_message) = app_state.ui.torrent_management.status_message.as_deref() {
        let status_message = sanitize_text(status_message);
        let status_message = truncate_with_ellipsis(
            &status_message,
            area.width.saturating_sub(4).max(1) as usize,
        );
        table_block = table_block.title_bottom(Span::styled(
            format!(" {status_message} "),
            ctx.apply(Style::default().fg(ctx.state_info()).bold()),
        ));
    }
    let table = Table::new(table_rows, constraints)
        .header(header)
        .block(table_block);
    f.render_stateful_widget(table, area, &mut table_state);

    if rows.is_empty() {
        let inner = Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        let message = if app_state.ui.torrent_management.search_query.is_empty() {
            "No torrents"
        } else {
            "No torrents match the search"
        };
        f.render_widget(
            Paragraph::new(message)
                .alignment(Alignment::Center)
                .style(ctx.apply(Style::default().fg(ctx.theme.semantic.surface2))),
            centered_line_rect(inner),
        );
    }
}

fn management_table_row<'a>(
    app_state: &AppState,
    row: &ManagementRow,
    row_is_cursor: bool,
    ctx: &ThemeContext,
    visible_columns: &[usize],
) -> Row<'a> {
    let selected_state = row_selection_state(app_state, row);
    let pending_label = pending_management_label_for_row(app_state, row);
    let reviewing_changes = app_state.ui.torrent_management.confirm_submit;
    let has_pending_action = matches!(row.kind, ManagementRowKind::Torrent)
        && row
            .info_hashes
            .iter()
            .any(|hash| pending_management_command_for_hash(app_state, hash).is_some());
    let pending_action_style = pending_management_review_style_for_row(app_state, row, ctx)
        .unwrap_or_else(|| ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)));
    let affected_by_review = reviewing_changes && has_pending_action;
    let selection_marker = management_selection_marker(selected_state, has_pending_action);

    let row_style = if row_is_cursor && !reviewing_changes {
        ctx.apply(Style::default().fg(ctx.state_warning()).bold())
    } else if !matches!(selected_state, SelectionState::None) {
        ctx.apply(
            Style::default()
                .fg(ctx.theme.scale.categorical.lavender)
                .bold(),
        )
    } else if affected_by_review || has_pending_action {
        pending_action_style
    } else if row.metrics.state_label == "Paused" {
        ctx.apply(Style::default().fg(ctx.theme.semantic.surface1))
    } else if row.metrics.state_label == "Deleting" {
        ctx.apply(Style::default().fg(ctx.state_error()))
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.text))
    };
    let name_prefix = if row.depth > 0 { "  " } else { "" };
    let name = match &row.kind {
        ManagementRowKind::Torrent => format!("{name_prefix}{}", row.label),
    };

    let all_columns = management_columns();
    let cells = visible_columns
        .iter()
        .map(|&idx| match all_columns[idx].id {
            ManagementColumnId::Selection => Cell::from(selection_marker),
            ManagementColumnId::Name => Cell::from(name.clone()),
            ManagementColumnId::DateAdded => {
                Cell::from(format_added_date(row.metrics.added_at_unix_secs))
            }
            ManagementColumnId::Completed => Cell::from(format!("{:.0}%", row.metrics.completed)),
            ManagementColumnId::State => {
                let cell = Cell::from(
                    pending_label
                        .clone()
                        .unwrap_or_else(|| row.metrics.state_label.clone()),
                );
                if pending_label.is_some() {
                    cell.style(pending_action_style)
                } else {
                    cell
                }
            }
            ManagementColumnId::Peers => Cell::from(row.metrics.peer_count.to_string()),
            ManagementColumnId::DownSpeed => management_speed_cell(ctx, row.metrics.download_bps),
            ManagementColumnId::UpSpeed => management_speed_cell(ctx, row.metrics.upload_bps),
            ManagementColumnId::Eta => Cell::from(
                row.metrics
                    .eta
                    .map(format_duration)
                    .unwrap_or_else(|| "-".to_string()),
            ),
            ManagementColumnId::Size => Cell::from(format_bytes(row.metrics.total_size)),
        })
        .collect::<Vec<_>>();

    Row::new(cells).style(row_style)
}

fn management_column_header_color(column: ManagementColumnId, ctx: &ThemeContext) -> Color {
    match column {
        ManagementColumnId::Selection => ctx.theme.semantic.subtext1,
        ManagementColumnId::Name => ctx.accent_sky(),
        ManagementColumnId::Eta => ctx.accent_teal(),
        ManagementColumnId::Completed => ctx.state_success(),
        ManagementColumnId::State => ctx.metric_upload(),
        ManagementColumnId::Peers => ctx.state_info(),
        ManagementColumnId::DownSpeed => ctx.metric_download(),
        ManagementColumnId::UpSpeed => ctx.accent_sapphire(),
        ManagementColumnId::Size => ctx.theme.semantic.text,
        ManagementColumnId::DateAdded => ctx.state_error(),
    }
}

fn management_selection_marker(
    selected_state: SelectionState,
    has_pending_action: bool,
) -> &'static str {
    if has_pending_action {
        return match selected_state {
            SelectionState::None => "!",
            SelectionState::Partial => "~!",
            SelectionState::Full => "x!",
        };
    }

    match selected_state {
        SelectionState::None => "-",
        SelectionState::Partial => "~",
        SelectionState::Full => "x",
    }
}

fn draw_management_footer(f: &mut Frame, app_state: &AppState, area: Rect, ctx: &ThemeContext) {
    if area.height == 0 {
        return;
    }

    let mut footer_spans = Vec::new();
    let mut used_width = 0usize;
    let max_width = area.width as usize;
    let mut push_action = |key: &str, label: &str, tone: ActionTone, key_only_fallback: bool| {
        if !try_push_management_footer_action(
            &mut footer_spans,
            &mut used_width,
            max_width,
            key,
            label,
            tone,
            ctx,
        ) && key_only_fallback
        {
            let _ = try_push_management_footer_action(
                &mut footer_spans,
                &mut used_width,
                max_width,
                key,
                "",
                tone,
                ctx,
            );
        }
    };

    if app_state.ui.torrent_management.is_searching {
        push_action("Esc", "clear", ActionTone::Cancel, true);
        push_action("Enter", "apply", ActionTone::Confirm, true);
        push_action("Tab", "mode", ActionTone::Mode, true);
    } else {
        let has_pending = !app_state.ui.torrent_management.pending_commands.is_empty();
        let compact_core_actions = max_width < if has_pending { 95 } else { 77 };
        let core_label = |label: &'static str| {
            if compact_core_actions {
                ""
            } else {
                label
            }
        };

        push_action("Esc", core_label("back"), ActionTone::Cancel, true);
        if has_pending {
            push_action("Y", core_label("review"), ActionTone::Confirm, true);
        }
        push_action("u", core_label("clear"), ActionTone::Clear, true);
        push_action("Space", core_label("select"), ActionTone::Select, true);
        push_action("A", core_label("all"), ActionTone::Select, true);
        push_action("p", core_label("pause"), ActionTone::Queue, true);
        push_action(
            "d/D",
            core_label("remove/purge"),
            ActionTone::Destructive,
            true,
        );
        push_action("arrows", "nav", ActionTone::Navigate, false);
        push_action("s", "sort", ActionTone::Sort, false);
        push_action("f", "files", ActionTone::Navigate, false);
        push_action("/", "search", ActionTone::Search, false);
        if management_search_panel_active(app_state) {
            push_action("Tab", "mode", ActionTone::Mode, false);
        }
        push_action("x", "names", ActionTone::Toggle, false);
    }

    let footer = Paragraph::new(Line::from(footer_spans))
        .alignment(Alignment::Center)
        .style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)));
    f.render_widget(footer, area);
}

fn try_push_management_footer_action(
    spans: &mut Vec<Span<'static>>,
    used_width: &mut usize,
    max_width: usize,
    key: &str,
    label: &str,
    tone: ActionTone,
    ctx: &ThemeContext,
) -> bool {
    let key_text = format!("[{key}]");
    let separator_text = if label.is_empty() { " " } else { " | " };
    let separator_width = if *used_width == 0 {
        0
    } else {
        separator_text.chars().count()
    };
    let item_width = key_text.chars().count() + label.chars().count();
    if used_width
        .saturating_add(separator_width)
        .saturating_add(item_width)
        > max_width
    {
        return false;
    }

    if separator_width > 0 {
        spans.push(Span::styled(
            separator_text.to_string(),
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
        ));
    }
    spans.push(Span::styled(key_text, footer_key_style(ctx, tone)));
    if !label.is_empty() {
        spans.push(Span::styled(
            label.to_string(),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
        ));
    }
    *used_width += separator_width + item_width;
    true
}

fn draw_management_review_panel(f: &mut Frame, app_state: &AppState, ctx: &ThemeContext) {
    let fallback_groups;
    let groups = if let Some(groups) = app_state.ui.torrent_management.review_cache.as_ref() {
        groups
    } else {
        fallback_groups = pending_management_review_groups(app_state);
        &fallback_groups
    };
    let purge_total_bytes = pending_management_purge_total_bytes(app_state);
    let area = management_review_popup_area(f.area(), groups);
    f.render_widget(Clear, area);

    let horizontal_padding = management_review_horizontal_padding(area.width);
    let block = Block::default()
        .title(Span::styled(
            " Review Queued Changes ",
            ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
        ))
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)))
        .padding(Padding::horizontal(horizontal_padding));
    f.render_widget(block, area);

    let regions = management_review_regions(area);
    let sections = pending_management_review_sections(groups, purge_total_bytes);
    let line_count = pending_management_review_line_count(&sections);
    let body_height = regions.body.height as usize;
    let max_scroll = line_count.saturating_sub(body_height);
    let scroll_offset = app_state
        .ui
        .torrent_management
        .review_scroll_offset
        .min(max_scroll);
    let body_columns = if max_scroll > 0 && regions.body.width > 1 {
        Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).split(regions.body)
    } else {
        Layout::horizontal([Constraint::Min(1), Constraint::Length(0)]).split(regions.body)
    };
    let body_content_area = body_columns[0];
    let body = pending_management_review_visible_lines(
        &sections,
        scroll_offset,
        body_height,
        body_content_area.width as usize,
        ctx,
    );

    f.render_widget(
        Paragraph::new(body).alignment(Alignment::Left),
        body_content_area,
    );
    if max_scroll > 0 && regions.body.width > 1 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .thumb_symbol("█")
            .track_symbol(Some("│"))
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"))
            .thumb_style(ctx.apply(Style::default().fg(ctx.state_selected())))
            .track_style(ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)))
            .begin_style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)))
            .end_style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)));
        let mut scrollbar_state = ScrollbarState::new(max_scroll.saturating_add(1))
            .position(scroll_offset)
            .viewport_content_length(body_height);
        f.render_stateful_widget(scrollbar, regions.body, &mut scrollbar_state);
    }

    draw_management_review_summary(f, groups, purge_total_bytes, regions, ctx);
    draw_management_review_footer(f, groups, regions, scroll_offset, line_count, ctx);
}

fn pending_management_review_sections(
    groups: &TorrentManagementReviewCache,
    purge_total_bytes: u64,
) -> Vec<ManagementReviewSection<'_>> {
    let mut sections = Vec::new();
    if !groups.pause.is_empty() {
        sections.push(ManagementReviewSection {
            action: ManagementReviewAction::Pause,
            names: &groups.pause,
            detail: None,
        });
    }
    if !groups.resume.is_empty() {
        sections.push(ManagementReviewSection {
            action: ManagementReviewAction::Resume,
            names: &groups.resume,
            detail: None,
        });
    }
    if !groups.delete.is_empty() {
        sections.push(ManagementReviewSection {
            action: ManagementReviewAction::Remove,
            names: &groups.delete,
            detail: None,
        });
    }
    if !groups.purge.is_empty() {
        sections.push(ManagementReviewSection {
            action: ManagementReviewAction::Purge,
            names: &groups.purge,
            detail: Some(format_gb(purge_total_bytes)),
        });
    }
    sections
}

fn pending_management_review_line_count(sections: &[ManagementReviewSection<'_>]) -> usize {
    sections
        .iter()
        .map(|section| 1usize.saturating_add(section.names.len()))
        .sum::<usize>()
        .saturating_add(sections.len().saturating_sub(1))
}

fn pending_management_review_summary_line_count(summary: &PendingManagementSummary) -> usize {
    let total = summary
        .pause_count
        .saturating_add(summary.resume_count)
        .saturating_add(summary.remove_count)
        .saturating_add(summary.purge_count);
    let section_count = [
        summary.pause_count,
        summary.resume_count,
        summary.remove_count,
        summary.purge_count,
    ]
    .into_iter()
    .filter(|count| *count > 0)
    .count();
    total
        .saturating_add(section_count)
        .saturating_add(section_count.saturating_sub(1))
}

fn pending_management_review_visible_lines(
    sections: &[ManagementReviewSection<'_>],
    scroll_offset: usize,
    max_lines: usize,
    max_line_width: usize,
    ctx: &ThemeContext,
) -> Vec<Line<'static>> {
    if max_lines == 0 {
        return Vec::new();
    }

    let viewport_end = scroll_offset.saturating_add(max_lines);
    let mut body = Vec::new();
    let mut logical_index = 0usize;
    for (section_index, section) in sections.iter().enumerate() {
        if section_index > 0 {
            if (scroll_offset..viewport_end).contains(&logical_index) {
                body.push(Line::from(""));
            }
            logical_index = logical_index.saturating_add(1);
            if logical_index >= viewport_end {
                break;
            }
        }

        if (scroll_offset..viewport_end).contains(&logical_index) {
            body.push(pending_review_section_header_line(
                section,
                max_line_width,
                ctx,
            ));
        }
        logical_index = logical_index.saturating_add(1);
        if logical_index >= viewport_end {
            break;
        }

        let names_start = logical_index;
        let first_name = scroll_offset
            .saturating_sub(names_start)
            .min(section.names.len());
        let last_name = viewport_end
            .saturating_sub(names_start)
            .min(section.names.len());
        for name in &section.names[first_name..last_name] {
            body.push(pending_review_name_line(name, max_line_width, ctx));
        }
        logical_index = logical_index.saturating_add(section.names.len());
        if logical_index >= viewport_end {
            break;
        }
    }
    body
}

fn management_review_popup_area(frame_area: Rect, groups: &TorrentManagementReviewCache) -> Rect {
    let max_width = frame_area
        .width
        .saturating_mul(86)
        .saturating_div(100)
        .max(frame_area.width.min(72))
        .min(frame_area.width);
    let height = management_review_popup_height(frame_area.height);
    let width = pending_management_review_popup_width(groups, max_width);
    Rect::new(
        frame_area.x + frame_area.width.saturating_sub(width) / 2,
        frame_area.y + frame_area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn management_review_popup_height(frame_height: u16) -> u16 {
    frame_height
        .saturating_mul(88)
        .saturating_div(100)
        .max(frame_height.min(12))
        .min(frame_height)
}

fn management_review_horizontal_padding(popup_width: u16) -> u16 {
    if popup_width > 0 {
        1
    } else {
        0
    }
}

fn management_review_inner_area(popup_area: Rect) -> Rect {
    let horizontal_padding = management_review_horizontal_padding(popup_area.width);
    Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(horizontal_padding))
        .inner(popup_area)
}

fn management_review_regions(popup_area: Rect) -> ManagementReviewRegions {
    let inner = management_review_inner_area(popup_area);
    let footer_target = if inner.height >= 8 { 2 } else { 1 };
    let footer_height = footer_target.min(inner.height.saturating_sub(1));
    let summary_target = if inner.height >= 9 { 3 } else { 2 };
    let summary_height =
        summary_target.min(inner.height.saturating_sub(footer_height).saturating_sub(1));
    let compact = inner.width < 50 || summary_height < 3 || footer_height < 2;
    let regions = Layout::vertical([
        Constraint::Length(summary_height),
        Constraint::Min(1),
        Constraint::Length(footer_height),
    ])
    .split(inner);
    ManagementReviewRegions {
        summary: regions[0],
        body: regions[1],
        footer: regions[2],
        compact,
    }
}

#[cfg(test)]
fn management_review_body_area(frame_area: Rect, groups: &TorrentManagementReviewCache) -> Rect {
    let popup_area = management_review_popup_area(frame_area, groups);
    management_review_regions(popup_area).body
}

fn management_review_body_height(frame_area: Rect) -> u16 {
    management_review_regions(Rect::new(
        0,
        0,
        frame_area.width,
        management_review_popup_height(frame_area.height),
    ))
    .body
    .height
}

fn management_review_page_lines(app_state: &AppState) -> usize {
    management_review_body_height(app_state.screen_area)
        .saturating_sub(1)
        .max(1) as usize
}

fn max_management_review_scroll_offset(app_state: &AppState) -> usize {
    if !app_state.ui.torrent_management.confirm_submit {
        return 0;
    }

    let summary = pending_management_summary(app_state);
    pending_management_review_summary_line_count(&summary)
        .saturating_sub(management_review_body_height(app_state.screen_area) as usize)
}

fn clamp_management_review_scroll(app_state: &mut AppState) {
    if !app_state.ui.torrent_management.confirm_submit {
        app_state.ui.torrent_management.review_scroll_offset = 0;
        return;
    }

    let max_scroll = max_management_review_scroll_offset(app_state);
    app_state.ui.torrent_management.review_scroll_offset = app_state
        .ui
        .torrent_management
        .review_scroll_offset
        .min(max_scroll);
}

fn normalize_management_review_state(app_state: &mut AppState) {
    if app_state.ui.torrent_management.pending_commands.is_empty() {
        app_state.ui.torrent_management.confirm_submit = false;
        app_state.ui.torrent_management.review_scroll_offset = 0;
        app_state.ui.torrent_management.review_cache = None;
    } else if app_state.ui.torrent_management.confirm_submit {
        clamp_management_review_scroll(app_state);
        if app_state.ui.torrent_management.review_cache.is_none() {
            refresh_pending_management_review_cache(app_state);
        }
    } else {
        app_state.ui.torrent_management.review_scroll_offset = 0;
        app_state.ui.torrent_management.review_cache = None;
    }
}

fn refresh_pending_management_review_cache(app_state: &mut AppState) {
    app_state.ui.torrent_management.review_cache =
        Some(pending_management_review_groups(app_state));
}

fn pending_management_review_popup_width(
    groups: &TorrentManagementReviewCache,
    max_width: u16,
) -> u16 {
    if max_width == 0 {
        return 0;
    }
    let longest = if groups.longest_line_width > 0 {
        groups.longest_line_width
    } else {
        pending_management_review_longest_line_width(groups)
    };

    let max_width = max_width as usize;
    let desired = longest.saturating_add(6).min(max_width);
    desired.max(1).max(72.min(max_width)) as u16
}

fn pending_management_review_longest_line_width(groups: &TorrentManagementReviewCache) -> usize {
    let sections = pending_management_review_sections(groups, 0);
    let mut longest = terminal_text_width(" Review Queued Changes ");
    for section in sections {
        longest = longest.max(terminal_text_width(&format!(
            "{}{}",
            section.action.title(),
            section_header_suffix(section.names.len(), section.detail.as_deref())
        )));
        for name in section.names {
            longest = longest.max(terminal_text_width(&format!("• {name}")));
        }
    }
    longest
}

fn pending_management_review_total(groups: &TorrentManagementReviewCache) -> usize {
    groups
        .pause
        .len()
        .saturating_add(groups.resume.len())
        .saturating_add(groups.delete.len())
        .saturating_add(groups.purge.len())
}

fn management_review_visible_range(
    scroll_offset: usize,
    viewport_height: usize,
    line_count: usize,
) -> String {
    if line_count == 0 || viewport_height == 0 {
        return "0 / 0".to_string();
    }
    let first = scroll_offset.min(line_count.saturating_sub(1));
    let last = first
        .saturating_add(viewport_height)
        .min(line_count)
        .max(first.saturating_add(1));
    format!("{}–{last} / {line_count}", first.saturating_add(1))
}

fn management_review_compact_visible_range(
    scroll_offset: usize,
    viewport_height: usize,
    line_count: usize,
) -> String {
    if line_count == 0 || viewport_height == 0 {
        return "↕0/0".to_string();
    }
    let first = scroll_offset.min(line_count.saturating_sub(1));
    let last = first
        .saturating_add(viewport_height)
        .min(line_count)
        .max(first.saturating_add(1));
    format!("↕{}–{last}/{line_count}", first.saturating_add(1))
}

fn management_review_compact_purge_safety_label(
    groups: &TorrentManagementReviewCache,
    purge_total_bytes: u64,
    max_width: usize,
) -> String {
    let size = format_gb(purge_total_bytes);
    let candidates = [
        format!("PURGE {} • DELETE FILES • {size}", groups.purge.len()),
        format!("PURGE {} • FILES • {size}", groups.purge.len()),
        format!("PURGE {} • FILES", groups.purge.len()),
    ];
    candidates
        .iter()
        .find(|candidate| terminal_text_width(candidate) <= max_width)
        .cloned()
        .unwrap_or_else(|| truncate_middle_with_ellipsis(&candidates[2], max_width))
}

fn draw_management_review_summary(
    f: &mut Frame,
    groups: &TorrentManagementReviewCache,
    purge_total_bytes: u64,
    regions: ManagementReviewRegions,
    ctx: &ThemeContext,
) {
    if regions.summary.height == 0 {
        return;
    }

    let summary_content = if regions.summary.height > 1 {
        let summary_block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)));
        let content = summary_block.inner(regions.summary);
        f.render_widget(summary_block, regions.summary);
        content
    } else {
        regions.summary
    };
    if summary_content.height == 0 {
        return;
    }

    let total = pending_management_review_total(groups);
    if regions.compact || summary_content.height == 1 {
        let mut label = if !groups.purge.is_empty() && summary_content.height == 1 {
            management_review_compact_purge_safety_label(
                groups,
                purge_total_bytes,
                summary_content.width as usize,
            )
        } else {
            format!("{total} queued")
        };
        if groups.purge.is_empty() || summary_content.height > 1 {
            if !groups.purge.is_empty() {
                label.push_str(&format!(" • Purge {}", groups.purge.len()));
            } else if !groups.delete.is_empty() {
                label.push_str(&format!(" • Remove {}", groups.delete.len()));
            }
        }
        let label = truncate_middle_with_ellipsis(&label, summary_content.width as usize);
        let tone = if groups.purge.is_empty() {
            ctx.state_selected()
        } else {
            ctx.state_error()
        };
        let count_area = Rect::new(
            summary_content.x,
            summary_content.y,
            summary_content.width,
            1,
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                label,
                ctx.apply(Style::default().fg(tone).bold()),
            ))),
            count_area,
        );
        draw_management_review_safety_detail(f, groups, purge_total_bytes, summary_content, ctx);
        return;
    }

    let count_area = Rect::new(
        summary_content.x,
        summary_content.y,
        summary_content.width,
        1,
    );
    let mut spans = Vec::new();
    let mut used_width = 0usize;
    push_management_review_summary_item(
        &mut spans,
        &mut used_width,
        count_area.width as usize,
        &format!("{total} queued"),
        ctx.state_selected(),
        ctx,
    );
    for (label, count, color) in [
        ("Purge", groups.purge.len(), ctx.state_error()),
        ("Remove", groups.delete.len(), ctx.state_warning()),
        ("Pause", groups.pause.len(), ctx.theme.semantic.surface2),
        ("Resume", groups.resume.len(), ctx.state_success()),
    ] {
        if count > 0 {
            push_management_review_summary_item(
                &mut spans,
                &mut used_width,
                count_area.width as usize,
                &format!("{label} {count}"),
                color,
                ctx,
            );
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), count_area);

    draw_management_review_safety_detail(f, groups, purge_total_bytes, summary_content, ctx);
}

fn draw_management_review_safety_detail(
    f: &mut Frame,
    groups: &TorrentManagementReviewCache,
    purge_total_bytes: u64,
    summary_content: Rect,
    ctx: &ThemeContext,
) {
    if summary_content.height < 2 {
        return;
    }

    let detail_area = Rect::new(
        summary_content.x,
        summary_content.y.saturating_add(1),
        summary_content.width,
        1,
    );
    let (detail, color) = if !groups.purge.is_empty() {
        (
            if detail_area.width >= 52 {
                format!(
                    "Purge permanently removes downloaded files • {}",
                    format_gb(purge_total_bytes)
                )
            } else {
                format!(
                    "PURGE {} • files • {}",
                    groups.purge.len(),
                    format_gb(purge_total_bytes)
                )
            },
            ctx.state_error(),
        )
    } else if !groups.delete.is_empty() {
        (
            "Remove changes the client only • downloaded files stay".to_string(),
            ctx.state_warning(),
        )
    } else {
        (
            "No downloaded files will be deleted".to_string(),
            ctx.state_success(),
        )
    };
    let detail = truncate_middle_with_ellipsis(&detail, detail_area.width as usize);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            detail,
            ctx.apply(Style::default().fg(color)),
        ))),
        detail_area,
    );
}

fn push_management_review_summary_item(
    spans: &mut Vec<Span<'static>>,
    used_width: &mut usize,
    max_width: usize,
    label: &str,
    color: Color,
    ctx: &ThemeContext,
) {
    let separator = if *used_width == 0 { "" } else { "  " };
    let item_width = terminal_text_width(separator).saturating_add(terminal_text_width(label));
    if used_width.saturating_add(item_width) > max_width {
        return;
    }
    if !separator.is_empty() {
        spans.push(Span::styled(
            separator.to_string(),
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
        ));
    }
    spans.push(Span::styled(
        label.to_string(),
        ctx.apply(Style::default().fg(color).bold()),
    ));
    *used_width = used_width.saturating_add(item_width);
}

fn draw_management_review_footer(
    f: &mut Frame,
    groups: &TorrentManagementReviewCache,
    regions: ManagementReviewRegions,
    scroll_offset: usize,
    line_count: usize,
    ctx: &ThemeContext,
) {
    let footer_area = regions.footer;
    if footer_area.height == 0 {
        return;
    }

    let total = pending_management_review_total(groups);
    let overflow = line_count > regions.body.height as usize;
    if footer_area.height > 1 {
        let rows =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(footer_area);
        let navigation = if regions.compact && overflow {
            format!(
                "{}  [j/k] scroll",
                management_review_compact_visible_range(
                    scroll_offset,
                    regions.body.height as usize,
                    line_count,
                )
            )
        } else if regions.compact {
            format!("All {total} changes visible")
        } else if overflow {
            let range = management_review_visible_range(
                scroll_offset,
                regions.body.height as usize,
                line_count,
            );
            format!("{range}  [j/k] Scroll  [PgUp/PgDn] Page  [Home/End] Jump")
        } else {
            format!("All {total} queued changes visible")
        };
        let navigation = truncate_middle_with_ellipsis(&navigation, rows[0].width as usize);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                navigation,
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
            )))
            .alignment(Alignment::Center),
            rows[0],
        );
        draw_management_review_action_footer(f, rows[1], total, regions.compact, None, ctx);
        return;
    }

    let range = overflow.then(|| {
        management_review_compact_visible_range(
            scroll_offset,
            regions.body.height as usize,
            line_count,
        )
    });
    draw_management_review_action_footer(f, footer_area, total, true, range.as_deref(), ctx);
}

fn draw_management_review_action_footer(
    f: &mut Frame,
    footer_area: Rect,
    total: usize,
    compact: bool,
    trailing_label: Option<&str>,
    ctx: &ThemeContext,
) {
    let mut spans = Vec::new();
    let mut used_width = 0usize;
    let max_width = footer_area.width as usize;
    let _ = try_push_management_footer_action(
        &mut spans,
        &mut used_width,
        max_width,
        "Esc",
        if compact { "" } else { "cancel" },
        ActionTone::Cancel,
        ctx,
    );

    if !compact {
        let _ = try_push_management_footer_action(
            &mut spans,
            &mut used_width,
            max_width,
            "u",
            "clear",
            ActionTone::Clear,
            ctx,
        );
    }
    let finalize_label = if compact {
        String::new()
    } else {
        format!("finalize {total}")
    };
    if !try_push_management_footer_action(
        &mut spans,
        &mut used_width,
        max_width,
        "Enter",
        &finalize_label,
        ActionTone::Confirm,
        ctx,
    ) {
        let _ = try_push_management_footer_action(
            &mut spans,
            &mut used_width,
            max_width,
            "↵",
            "",
            ActionTone::Confirm,
            ctx,
        );
    }
    if compact {
        let _ = try_push_management_footer_action(
            &mut spans,
            &mut used_width,
            max_width,
            "u",
            "",
            ActionTone::Clear,
            ctx,
        );
    }

    if let Some(trailing_label) = trailing_label {
        let separator_width = usize::from(used_width > 0);
        let available_width = max_width
            .saturating_sub(used_width)
            .saturating_sub(separator_width);
        if available_width > 0 {
            let trailing_label = truncate_middle_with_ellipsis(trailing_label, available_width);
            if separator_width > 0 {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                trailing_label,
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
            ));
        }
    }

    let footer = Paragraph::new(Line::from(spans))
        .alignment(if compact {
            Alignment::Left
        } else {
            Alignment::Center
        })
        .style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)));
    f.render_widget(footer, footer_area);
}

fn pending_review_section_header_line(
    section: &ManagementReviewSection<'_>,
    max_line_width: usize,
    ctx: &ThemeContext,
) -> Line<'static> {
    let title = section.action.title();
    let color = section.action.color(ctx);
    let (header_title, header_suffix) = management_review_header_parts(
        title,
        section.names.len(),
        section.detail.as_deref(),
        max_line_width,
    );
    Line::from(vec![
        Span::styled(header_title, ctx.apply(Style::default().fg(color).bold())),
        Span::styled(
            header_suffix,
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ),
    ])
}

fn pending_review_name_line(
    name: &str,
    max_line_width: usize,
    ctx: &ThemeContext,
) -> Line<'static> {
    let available_name_width = max_line_width.saturating_sub(2);
    let name = truncate_middle_with_ellipsis(name, available_name_width);
    Line::from(vec![
        Span::styled(
            "• ",
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
        ),
        Span::styled(
            name,
            ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
        ),
    ])
}

impl ManagementReviewAction {
    fn title(self) -> &'static str {
        match self {
            Self::Pause => "PAUSE",
            Self::Resume => "RESUME",
            Self::Remove => "REMOVE",
            Self::Purge => "PURGE",
        }
    }

    fn color(self, ctx: &ThemeContext) -> Color {
        match self {
            Self::Pause => ctx.theme.semantic.surface2,
            Self::Resume => ctx.state_success(),
            Self::Remove => ctx.state_warning(),
            Self::Purge => ctx.state_error(),
        }
    }
}

fn management_review_header_parts(
    title: &str,
    count: usize,
    detail: Option<&str>,
    max_width: usize,
) -> (String, String) {
    let full_suffix = section_header_suffix(count, detail);
    if terminal_text_width(&format!("{title}{full_suffix}")) <= max_width {
        return (title.to_string(), full_suffix);
    }

    let compact_title = title;
    let compact_suffix = match detail {
        Some(detail) => format!(": {count} ({detail})"),
        None => format!(": {count}"),
    };
    let compact = format!("{compact_title}{compact_suffix}");
    if terminal_text_width(&compact) <= max_width {
        return (compact_title.to_string(), compact_suffix);
    }

    (
        truncate_middle_with_ellipsis(&compact, max_width),
        String::new(),
    )
}

fn section_header_suffix(count: usize, detail: Option<&str>) -> String {
    let noun = if count == 1 { "Torrent" } else { "Torrents" };
    match detail {
        Some(detail) => format!(": {count} {noun} ({detail})"),
        None => format!(": {count} {noun}"),
    }
}

fn centered_line_rect(area: Rect) -> Rect {
    Rect::new(
        area.x,
        area.y + area.height.saturating_sub(1) / 2,
        area.width,
        1,
    )
}

fn management_columns() -> Vec<ManagementColumnDefinition> {
    vec![
        ManagementColumnDefinition {
            id: ManagementColumnId::Selection,
            header: "=",
            min_width: 2,
            priority: 0,
            constraint: Constraint::Length(2),
        },
        ManagementColumnDefinition {
            id: ManagementColumnId::Name,
            header: "Name",
            min_width: 20,
            priority: 0,
            constraint: Constraint::Fill(3),
        },
        ManagementColumnDefinition {
            id: ManagementColumnId::Eta,
            header: "ETA",
            min_width: 9,
            priority: 4,
            constraint: Constraint::Length(9),
        },
        ManagementColumnDefinition {
            id: ManagementColumnId::Completed,
            header: "Done",
            min_width: 7,
            priority: 2,
            constraint: Constraint::Length(7),
        },
        ManagementColumnDefinition {
            id: ManagementColumnId::State,
            header: "Action",
            min_width: 8,
            priority: 2,
            constraint: Constraint::Length(8),
        },
        ManagementColumnDefinition {
            id: ManagementColumnId::Peers,
            header: "Peers",
            min_width: 7,
            priority: 3,
            constraint: Constraint::Length(7),
        },
        ManagementColumnDefinition {
            id: ManagementColumnId::DownSpeed,
            header: "DL",
            min_width: 10,
            priority: 1,
            constraint: Constraint::Length(10),
        },
        ManagementColumnDefinition {
            id: ManagementColumnId::UpSpeed,
            header: "UL",
            min_width: 10,
            priority: 1,
            constraint: Constraint::Length(10),
        },
        ManagementColumnDefinition {
            id: ManagementColumnId::Size,
            header: "Size",
            min_width: 10,
            priority: 5,
            constraint: Constraint::Length(10),
        },
        ManagementColumnDefinition {
            id: ManagementColumnId::DateAdded,
            header: "Added",
            min_width: 10,
            priority: 5,
            constraint: Constraint::Length(10),
        },
    ]
}

fn compute_visible_management_columns(available_width: u16) -> (Vec<Constraint>, Vec<usize>) {
    let columns = management_columns();
    let smart_columns = columns
        .iter()
        .map(|column| SmartCol {
            min_width: column.min_width,
            priority: column.priority,
            constraint: column.constraint,
        })
        .collect::<Vec<_>>();
    compute_smart_table_layout(&smart_columns, available_width.saturating_sub(4), 1)
}

#[cfg(test)]
fn visible_management_column_ids(available_width: u16) -> Vec<ManagementColumnId> {
    let columns = management_columns();
    let (_, visible_indices) = compute_visible_management_columns(available_width);
    visible_indices
        .into_iter()
        .map(|idx| columns[idx].id)
        .collect()
}

fn management_speed_cell<'a>(ctx: &ThemeContext, speed_bps: u64) -> Cell<'a> {
    Cell::from(format_speed(speed_bps)).style(ctx.apply(speed_to_style(ctx, speed_bps)))
}

fn management_table_width_for_state(app_state: &AppState) -> u16 {
    if app_state.screen_area.width > 0 {
        management_content_area(app_state.screen_area).width
    } else {
        140
    }
}

fn visible_management_column_indices_for_state(app_state: &AppState) -> Vec<usize> {
    compute_visible_management_columns(management_table_width_for_state(app_state)).1
}

fn normalized_selected_column_from_visible(
    selected_index: usize,
    visible_columns: &[usize],
) -> usize {
    if visible_columns.is_empty() {
        return management_column_index(ManagementColumnId::Name).unwrap_or(0);
    }
    if visible_columns.contains(&selected_index) {
        return selected_index;
    }
    visible_columns
        .iter()
        .copied()
        .rfind(|idx| *idx <= selected_index)
        .or_else(|| visible_columns.first().copied())
        .unwrap_or(0)
}

fn normalized_selected_management_column_index(app_state: &AppState) -> usize {
    normalized_selected_column_from_visible(
        app_state.ui.torrent_management.selected_column_index,
        &visible_management_column_indices_for_state(app_state),
    )
}

fn move_management_column(app_state: &mut AppState, direction: isize) {
    let visible_columns = visible_management_column_indices_for_state(app_state);
    if visible_columns.is_empty() {
        return;
    }

    let current = normalized_selected_column_from_visible(
        app_state.ui.torrent_management.selected_column_index,
        &visible_columns,
    );
    let current_pos = visible_columns
        .iter()
        .position(|idx| *idx == current)
        .unwrap_or(0);
    let next_pos = if direction < 0 {
        current_pos.saturating_sub(1)
    } else {
        (current_pos + 1).min(visible_columns.len().saturating_sub(1))
    };
    app_state.ui.torrent_management.selected_column_index = visible_columns[next_pos];
}

fn reverse_sort_direction(direction: SortDirection) -> SortDirection {
    match direction {
        SortDirection::Ascending => SortDirection::Descending,
        SortDirection::Descending => SortDirection::Ascending,
    }
}

fn management_column_default_direction(column: ManagementColumnId) -> SortDirection {
    if management_column_is_numeric(column) {
        SortDirection::Descending
    } else {
        SortDirection::Ascending
    }
}

fn management_column_is_numeric(column: ManagementColumnId) -> bool {
    matches!(
        column,
        ManagementColumnId::Completed
            | ManagementColumnId::Peers
            | ManagementColumnId::DownSpeed
            | ManagementColumnId::UpSpeed
            | ManagementColumnId::Eta
            | ManagementColumnId::Size
            | ManagementColumnId::DateAdded
    )
}

fn management_sort_arrow(column: ManagementColumnId, direction: SortDirection) -> &'static str {
    match (management_column_is_numeric(column), direction) {
        (true, SortDirection::Descending) | (false, SortDirection::Ascending) => " ▼",
        (true, SortDirection::Ascending) | (false, SortDirection::Descending) => " ▲",
    }
}

fn management_sort_column(app_state: &AppState) -> Option<ManagementColumnId> {
    let columns = management_columns();
    app_state
        .ui
        .torrent_management
        .sort_column_index
        .and_then(|idx| columns.get(idx))
        .map(|column| column.id)
}

fn management_column_index(column_id: ManagementColumnId) -> Option<usize> {
    management_columns()
        .iter()
        .position(|column| column.id == column_id)
}

fn sort_management_rows(app_state: &AppState, rows: &mut [ManagementRow]) {
    if management_sort_column(app_state).is_some() {
        rows.sort_by(|left, right| compare_management_rows(app_state, left, right));
    }
}

fn compare_management_rows(
    app_state: &AppState,
    left: &ManagementRow,
    right: &ManagementRow,
) -> Ordering {
    let Some(column) = management_sort_column(app_state) else {
        return Ordering::Equal;
    };
    let ordering = match column {
        ManagementColumnId::Selection => {
            selection_sort_rank(app_state, left).cmp(&selection_sort_rank(app_state, right))
        }
        ManagementColumnId::Name => left.label.cmp(&right.label),
        ManagementColumnId::Completed => left.metrics.completed.total_cmp(&right.metrics.completed),
        ManagementColumnId::State => left.metrics.state_label.cmp(&right.metrics.state_label),
        ManagementColumnId::DateAdded => left
            .metrics
            .added_at_unix_secs
            .unwrap_or(0)
            .cmp(&right.metrics.added_at_unix_secs.unwrap_or(0)),
        ManagementColumnId::Peers => left.metrics.peer_count.cmp(&right.metrics.peer_count),
        ManagementColumnId::DownSpeed => left.metrics.download_bps.cmp(&right.metrics.download_bps),
        ManagementColumnId::UpSpeed => left.metrics.upload_bps.cmp(&right.metrics.upload_bps),
        ManagementColumnId::Eta => left.metrics.eta.cmp(&right.metrics.eta),
        ManagementColumnId::Size => left.metrics.total_size.cmp(&right.metrics.total_size),
    };

    apply_sort_direction(ordering, app_state.ui.torrent_management.sort_direction)
        .then_with(|| left.label.cmp(&right.label))
        .then_with(|| left.info_hashes.len().cmp(&right.info_hashes.len()))
}

fn apply_sort_direction(ordering: Ordering, direction: SortDirection) -> Ordering {
    match direction {
        SortDirection::Ascending => ordering,
        SortDirection::Descending => ordering.reverse(),
    }
}

fn selection_sort_rank(app_state: &AppState, row: &ManagementRow) -> usize {
    match row_selection_state(app_state, row) {
        SelectionState::None => 0,
        SelectionState::Partial => 1,
        SelectionState::Full => 2,
    }
}

fn build_management_rows(app_state: &AppState) -> Vec<ManagementRow> {
    let visible = visible_torrent_hashes(app_state);
    let mut rows = visible
        .into_iter()
        .filter_map(|info_hash| torrent_row(app_state, info_hash, 0))
        .collect::<Vec<_>>();
    sort_management_rows(app_state, &mut rows);
    rows
}

fn torrent_row(app_state: &AppState, info_hash: Vec<u8>, depth: usize) -> Option<ManagementRow> {
    let torrent = app_state.torrents.get(&info_hash)?;
    let label = if app_state.anonymize_torrent_names {
        anonymize_preserving_shape(&torrent.latest_state.torrent_name)
    } else {
        sanitize_text(&torrent.latest_state.torrent_name)
    };
    Some(ManagementRow {
        kind: ManagementRowKind::Torrent,
        label,
        info_hashes: vec![info_hash.clone()],
        depth,
        metrics: aggregate_metrics_for_hashes(app_state, vec![info_hash]),
    })
}

fn visible_torrent_hashes(app_state: &AppState) -> Vec<Vec<u8>> {
    let query = app_state.ui.torrent_management.search_query.trim();
    let mode = app_state.ui.torrent_management.search_mode;
    let matcher = SkimMatcherV2::default();
    ordered_torrent_hashes(app_state)
        .into_iter()
        .filter(|info_hash| {
            app_state
                .torrents
                .get(info_hash)
                .is_some_and(|torrent| torrent_matches_query(torrent, query, mode, &matcher))
        })
        .collect()
}

fn ordered_torrent_hashes(app_state: &AppState) -> Vec<Vec<u8>> {
    if !app_state.torrent_list_order.is_empty() {
        let mut hashes = app_state.torrents.keys().cloned().collect::<Vec<_>>();
        hashes.sort_by(|a, b| {
            let a_rank = app_state
                .torrent_list_order
                .iter()
                .position(|hash| hash == a);
            let b_rank = app_state
                .torrent_list_order
                .iter()
                .position(|hash| hash == b);
            match (a_rank, b_rank) {
                (Some(a_rank), Some(b_rank)) => a_rank.cmp(&b_rank),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => torrent_name(app_state, a).cmp(torrent_name(app_state, b)),
            }
        });
        return hashes;
    }

    let mut hashes = app_state.torrents.keys().cloned().collect::<Vec<_>>();
    hashes.sort_by(|a, b| torrent_name(app_state, a).cmp(torrent_name(app_state, b)));
    hashes
}

fn torrent_name<'a>(app_state: &'a AppState, info_hash: &[u8]) -> &'a str {
    app_state
        .torrents
        .get(info_hash)
        .map(|torrent| torrent.latest_state.torrent_name.as_str())
        .unwrap_or_default()
}

fn torrent_matches_query(
    torrent: &TorrentDisplayState,
    query: &str,
    mode: SearchMode,
    matcher: &SkimMatcherV2,
) -> bool {
    if query.is_empty() {
        return true;
    }

    let mut haystack = torrent.latest_state.torrent_name.clone();
    if let Some(path) = &torrent.latest_state.download_path {
        haystack.push(' ');
        haystack.push_str(&path.to_string_lossy());
    }
    if let Some(container) = &torrent.latest_state.container_name {
        haystack.push(' ');
        haystack.push_str(container);
    }
    match mode {
        SearchMode::Fuzzy => matcher
            .fuzzy_match(&haystack.to_lowercase(), &query.to_lowercase())
            .is_some(),
        SearchMode::Regex => regex::RegexBuilder::new(query)
            .case_insensitive(true)
            .build()
            .ok()
            .is_some_and(|re| re.is_match(&haystack)),
    }
}

fn aggregate_metrics_for_hashes<I>(app_state: &AppState, hashes: I) -> RowMetrics
where
    I: IntoIterator<Item = Vec<u8>>,
{
    let mut count = 0usize;
    let mut peer_count = 0usize;
    let mut download_bps = 0u64;
    let mut upload_bps = 0u64;
    let mut total_size = 0u64;
    let mut latest_added_at_unix_secs = None::<u64>;
    let mut weighted_done = 0f64;
    let mut unweighted_done = 0f64;
    let mut weighted_total = 0u64;
    let mut max_eta = Duration::ZERO;
    let mut any_incomplete = false;
    let mut states = HashSet::new();

    for info_hash in hashes {
        let Some(torrent) = app_state.torrents.get(&info_hash) else {
            continue;
        };
        let state = &torrent.latest_state;
        count += 1;
        peer_count += state
            .number_of_successfully_connected_peers
            .max(state.peers.len());
        download_bps = download_bps.saturating_add(torrent.smoothed_download_speed_bps);
        upload_bps = upload_bps.saturating_add(torrent.smoothed_upload_speed_bps);
        total_size = total_size.saturating_add(state.total_size);
        latest_added_at_unix_secs = latest_added_at_unix_secs.max(torrent.added_at_unix_secs);
        states.insert(state.torrent_control_state.clone());

        let pct = torrent_completion_percent(state).clamp(0.0, 100.0);
        unweighted_done += pct;
        if state.total_size > 0 {
            weighted_done += pct * state.total_size as f64;
            weighted_total = weighted_total.saturating_add(state.total_size);
        }
        if pct < 100.0 {
            any_incomplete = true;
            max_eta = max_eta.max(state.eta);
        }
    }

    let completed = if weighted_total > 0 {
        (weighted_done / weighted_total as f64).clamp(0.0, 100.0)
    } else if count > 0 {
        (unweighted_done / count as f64).clamp(0.0, 100.0)
    } else {
        0.0
    };

    RowMetrics {
        count,
        completed,
        state_label: aggregate_state_label(&states, count),
        peer_count,
        download_bps,
        upload_bps,
        eta: (any_incomplete && !max_eta.is_zero()).then_some(max_eta),
        total_size,
        added_at_unix_secs: latest_added_at_unix_secs,
    }
}

fn format_added_date(added_at_unix_secs: Option<u64>) -> String {
    let Some(added_at_unix_secs) = added_at_unix_secs else {
        return "-".to_string();
    };
    let system_time = UNIX_EPOCH + Duration::from_secs(added_at_unix_secs);
    let datetime: DateTime<Local> = system_time.into();
    datetime.format("%Y-%m-%d").to_string()
}

fn aggregate_state_label(states: &HashSet<TorrentControlState>, count: usize) -> String {
    if count == 0 {
        return "-".to_string();
    }
    if states.contains(&TorrentControlState::Deleting) {
        return "Deleting".to_string();
    }
    if states.len() > 1 {
        return "Mixed".to_string();
    }
    if states.contains(&TorrentControlState::Paused) {
        "Paused".to_string()
    } else {
        "Running".to_string()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionState {
    None,
    Partial,
    Full,
}

fn row_selection_state(app_state: &AppState, row: &ManagementRow) -> SelectionState {
    let selected = &app_state.ui.torrent_management.selected_hashes;
    let selected_count = row
        .info_hashes
        .iter()
        .filter(|hash| selected.contains(*hash))
        .count();
    match selected_count {
        0 => SelectionState::None,
        count if count == row.info_hashes.len() => SelectionState::Full,
        _ => SelectionState::Partial,
    }
}

fn toggle_hash_selection(app_state: &mut AppState, targets: &[Vec<u8>]) {
    if targets.is_empty() {
        return;
    }
    let selected = &mut app_state.ui.torrent_management.selected_hashes;
    if targets.iter().all(|hash| selected.contains(hash)) {
        for hash in targets {
            selected.remove(hash);
        }
    } else {
        for hash in targets {
            selected.insert(hash.clone());
        }
    }
}

fn current_row_targets(app_state: &AppState) -> Vec<Vec<u8>> {
    let rows = build_management_rows(app_state);
    management_cursor_index_for_rows(app_state, &rows)
        .and_then(|index| rows.get(index))
        .map(|row| row.info_hashes.clone())
        .unwrap_or_default()
}

fn management_targets(app_state: &AppState) -> Vec<Vec<u8>> {
    if !app_state.ui.torrent_management.selected_hashes.is_empty() {
        let selected_visible = visible_torrent_hashes(app_state)
            .into_iter()
            .filter(|hash| {
                app_state
                    .ui
                    .torrent_management
                    .selected_hashes
                    .contains(hash)
            })
            .collect::<Vec<_>>();
        if !selected_visible.is_empty() {
            return selected_visible;
        }
    }

    current_row_targets(app_state)
}

fn management_clear_targets(app_state: &AppState) -> Vec<Vec<u8>> {
    if !app_state.ui.torrent_management.confirm_submit {
        return management_targets(app_state);
    }

    let selected_hashes = &app_state.ui.torrent_management.selected_hashes;
    if !selected_hashes.is_empty() {
        return selected_hashes.iter().cloned().collect();
    }

    let mut seen = HashSet::new();
    app_state
        .ui
        .torrent_management
        .pending_commands
        .iter()
        .filter(|command| seen.insert(command.info_hash.clone()))
        .map(|command| command.info_hash.clone())
        .collect()
}

fn toggle_pending_management_command(
    app_state: &mut AppState,
    command: TorrentManagementPendingCommand,
) {
    app_state.ui.torrent_management.review_cache = None;
    if app_state
        .ui
        .torrent_management
        .pending_commands
        .iter()
        .any(|pending| pending.info_hash == command.info_hash && pending.request == command.request)
    {
        app_state
            .ui
            .torrent_management
            .pending_commands
            .retain(|pending| pending.info_hash != command.info_hash);
        return;
    }

    app_state
        .ui
        .torrent_management
        .pending_commands
        .retain(|pending| pending.info_hash != command.info_hash);
    app_state
        .ui
        .torrent_management
        .pending_commands
        .push(command);
}

fn clear_pending_management_commands_for_targets(
    app_state: &mut AppState,
    targets: &HashSet<Vec<u8>>,
) -> usize {
    let before = app_state.ui.torrent_management.pending_commands.len();
    app_state
        .ui
        .torrent_management
        .pending_commands
        .retain(|pending| !targets.contains(&pending.info_hash));
    let cleared = before.saturating_sub(app_state.ui.torrent_management.pending_commands.len());
    if cleared > 0 {
        app_state.ui.torrent_management.review_cache = None;
    }
    cleared
}

fn management_clear_status(cleared: usize, deselected: usize) -> String {
    match (cleared, deselected) {
        (0, 0) => "No selection or draft commands to clear".to_string(),
        (0, deselected) => format!(
            "Cleared {deselected} {}",
            if deselected == 1 {
                "selection"
            } else {
                "selections"
            }
        ),
        (cleared, 0) => format!(
            "Cleared {cleared} draft {}",
            if cleared == 1 { "command" } else { "commands" }
        ),
        (cleared, deselected) => format!(
            "Cleared {cleared} draft {} and {deselected} {}",
            if cleared == 1 { "command" } else { "commands" },
            if deselected == 1 {
                "selection"
            } else {
                "selections"
            }
        ),
    }
}

fn pending_management_status(app_state: &AppState) -> String {
    let pending_count = app_state.ui.torrent_management.pending_commands.len();
    format!("{pending_count} draft commands pending")
}

fn pending_management_summary(app_state: &AppState) -> PendingManagementSummary {
    let mut summary = PendingManagementSummary::default();
    for command in &app_state.ui.torrent_management.pending_commands {
        match &command.request {
            ControlRequest::Pause { .. } => summary.pause_count += 1,
            ControlRequest::Resume { .. } => summary.resume_count += 1,
            ControlRequest::Delete {
                delete_files: true, ..
            } => summary.purge_count += 1,
            ControlRequest::Delete {
                delete_files: false,
                ..
            } => summary.remove_count += 1,
            _ => {}
        }
    }
    summary
}

fn pending_management_review_groups(app_state: &AppState) -> TorrentManagementReviewCache {
    let mut groups = TorrentManagementReviewCache::default();
    for command in &app_state.ui.torrent_management.pending_commands {
        let name = pending_management_command_display_name(app_state, command);
        match &command.request {
            ControlRequest::Pause { .. } => groups.pause.push(name),
            ControlRequest::Resume { .. } => groups.resume.push(name),
            ControlRequest::Delete {
                delete_files: true, ..
            } => {
                groups.purge.push(name);
            }
            ControlRequest::Delete {
                delete_files: false,
                ..
            } => groups.delete.push(name),
            _ => {}
        }
    }
    groups.pause.sort();
    groups.resume.sort();
    groups.delete.sort();
    groups.purge.sort();
    groups.longest_line_width = pending_management_review_longest_line_width(&groups);
    groups
}

fn pending_management_purge_total_bytes(app_state: &AppState) -> u64 {
    app_state
        .ui
        .torrent_management
        .pending_commands
        .iter()
        .filter(|command| {
            matches!(
                &command.request,
                ControlRequest::Delete {
                    delete_files: true,
                    ..
                }
            )
        })
        .fold(0, |total, command| {
            total.saturating_add(pending_management_command_total_size(app_state, command))
        })
}

fn pending_management_command_total_size(
    app_state: &AppState,
    command: &TorrentManagementPendingCommand,
) -> u64 {
    app_state
        .torrents
        .get(&command.info_hash)
        .map(|torrent| torrent.latest_state.total_size)
        .unwrap_or(0)
}

fn format_gb(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / 1_000_000_000.0)
}

fn pending_management_command_display_name(
    app_state: &AppState,
    command: &TorrentManagementPendingCommand,
) -> String {
    if let Some(torrent) = app_state.torrents.get(&command.info_hash) {
        if app_state.anonymize_torrent_names {
            anonymize_preserving_shape(&torrent.latest_state.torrent_name)
        } else {
            sanitize_text(&torrent.latest_state.torrent_name)
        }
    } else {
        let hash = hex::encode(&command.info_hash);
        format!(
            "unknown torrent {}",
            hash.chars().take(8).collect::<String>()
        )
    }
}

fn pending_management_command_for_hash<'a>(
    app_state: &'a AppState,
    info_hash: &[u8],
) -> Option<&'a TorrentManagementPendingCommand> {
    app_state
        .ui
        .torrent_management
        .pending_commands
        .iter()
        .find(|command| command.info_hash == info_hash)
}

fn pending_management_review_style_for_row(
    app_state: &AppState,
    row: &ManagementRow,
    ctx: &ThemeContext,
) -> Option<Style> {
    let mut style = None;
    for hash in &row.info_hashes {
        let Some(command) = pending_management_command_for_hash(app_state, hash) else {
            continue;
        };
        let next = match &command.request {
            ControlRequest::Pause { .. } => {
                ctx.apply(Style::default().fg(ctx.theme.semantic.surface2))
            }
            ControlRequest::Resume { .. } => ctx.apply(Style::default().fg(ctx.state_success())),
            ControlRequest::Delete {
                delete_files: false,
                ..
            } => ctx.apply(Style::default().fg(ctx.state_warning())),
            ControlRequest::Delete {
                delete_files: true, ..
            } => ctx.apply(Style::default().fg(ctx.state_error())),
            _ => continue,
        };

        style = Some(next);
        if matches!(
            command.request,
            ControlRequest::Delete {
                delete_files: true,
                ..
            }
        ) {
            break;
        }
    }
    style
}

fn pending_management_label_for_row(app_state: &AppState, row: &ManagementRow) -> Option<String> {
    let mut matching_commands = row
        .info_hashes
        .iter()
        .filter_map(|hash| pending_management_command_for_hash(app_state, hash));
    matching_commands.next().map(|_| "Review".to_string())
}

fn prune_selected_hashes(app_state: &mut AppState) {
    let live_hashes: HashSet<Vec<u8>> = app_state.torrents.keys().cloned().collect();
    app_state
        .ui
        .torrent_management
        .selected_hashes
        .retain(|hash| live_hashes.contains(hash));
    let pending_before = app_state.ui.torrent_management.pending_commands.len();
    app_state
        .ui
        .torrent_management
        .pending_commands
        .retain(|command| live_hashes.contains(&command.info_hash));
    if app_state.ui.torrent_management.pending_commands.len() != pending_before {
        app_state.ui.torrent_management.review_cache = None;
    }
}

fn management_cursor_index_for_rows(app_state: &AppState, rows: &[ManagementRow]) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }

    app_state
        .ui
        .torrent_management
        .cursor_hash
        .as_ref()
        .and_then(|cursor_hash| {
            rows.iter()
                .position(|row| row.info_hashes.iter().any(|hash| hash == cursor_hash))
        })
        .or_else(|| {
            Some(
                app_state
                    .ui
                    .torrent_management
                    .selected_index
                    .min(rows.len().saturating_sub(1)),
            )
        })
}

fn normalize_management_cursor(app_state: &mut AppState) {
    let rows = build_management_rows(app_state);
    let Some(index) = management_cursor_index_for_rows(app_state, &rows) else {
        app_state.ui.torrent_management.selected_index = 0;
        app_state.ui.torrent_management.cursor_hash = None;
        return;
    };

    app_state.ui.torrent_management.selected_index = index;
    app_state.ui.torrent_management.cursor_hash = rows[index].info_hashes.first().cloned();
}

fn set_management_cursor_hash_from_index(app_state: &mut AppState) {
    let rows = build_management_rows(app_state);
    if rows.is_empty() {
        app_state.ui.torrent_management.selected_index = 0;
        app_state.ui.torrent_management.cursor_hash = None;
        return;
    }

    let index = app_state
        .ui
        .torrent_management
        .selected_index
        .min(rows.len().saturating_sub(1));
    app_state.ui.torrent_management.selected_index = index;
    app_state.ui.torrent_management.cursor_hash = rows[index].info_hashes.first().cloned();
}

fn clamp_management_column_state(app_state: &mut AppState) {
    let columns_len = management_columns().len();
    if columns_len == 0 {
        app_state.ui.torrent_management.selected_column_index = 0;
        app_state.ui.torrent_management.sort_column_index = None;
        return;
    }

    if app_state.ui.torrent_management.selected_column_index >= columns_len {
        app_state.ui.torrent_management.selected_column_index = columns_len - 1;
    }
    if app_state
        .ui
        .torrent_management
        .sort_column_index
        .is_some_and(|idx| idx >= columns_len)
    {
        app_state.ui.torrent_management.sort_column_index = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{TorrentMetrics, UiState};
    use crate::config::Settings;
    use crate::dht_service::{DhtStatus, DhtWaveTelemetry};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn hash(byte: u8) -> Vec<u8> {
        vec![byte; 20]
    }

    fn pause_command(byte: u8) -> TorrentManagementPendingCommand {
        let info_hash = hash(byte);
        TorrentManagementPendingCommand {
            info_hash: info_hash.clone(),
            request: ControlRequest::Pause {
                info_hash_hex: hex::encode(&info_hash),
            },
            state: TorrentControlState::Paused,
            delete_files: false,
        }
    }

    fn delete_command(byte: u8, delete_files: bool) -> TorrentManagementPendingCommand {
        let info_hash = hash(byte);
        TorrentManagementPendingCommand {
            info_hash: info_hash.clone(),
            request: ControlRequest::Delete {
                info_hash_hex: hex::encode(&info_hash),
                delete_files,
            },
            state: TorrentControlState::Deleting,
            delete_files,
        }
    }

    fn app_state_with_torrents(torrents: Vec<(Vec<u8>, &str, u64, u64, usize)>) -> AppState {
        let mut app_state = AppState {
            mode: AppMode::TorrentManagement,
            ui: UiState::default(),
            ..Default::default()
        };

        for (idx, (info_hash, name, download_bps, upload_bps, peers)) in
            torrents.into_iter().enumerate()
        {
            let mut metrics = TorrentMetrics {
                info_hash: info_hash.clone(),
                torrent_name: name.to_string(),
                number_of_pieces_total: 100,
                number_of_pieces_completed: 50,
                number_of_successfully_connected_peers: peers,
                total_size: 1_000 + idx as u64,
                eta: Duration::from_secs(30 + idx as u64),
                ..Default::default()
            };
            metrics.peers = Vec::new();
            app_state.torrents.insert(
                info_hash.clone(),
                TorrentDisplayState {
                    latest_state: metrics,
                    added_at_unix_secs: Some(1_700_000_000 + idx as u64 * 86_400),
                    smoothed_download_speed_bps: download_bps,
                    smoothed_upload_speed_bps: upload_bps,
                    ..Default::default()
                },
            );
            app_state.torrent_list_order.push(info_hash);
        }

        app_state
    }

    fn render_management_screen(app_state: &mut AppState, width: u16, height: u16) -> String {
        app_state.screen_area = Rect::new(0, 0, width, height);
        normalize_management_review_state(app_state);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let dht_status = DhtStatus::default();
        let dht_wave_telemetry = DhtWaveTelemetry::default();
        let settings = Settings::default();
        let ctx = ThemeContext::new(app_state.theme, 0.0);

        terminal
            .draw(|frame| {
                let screen = ScreenContext::new(
                    app_state,
                    &dht_status,
                    &dht_wave_telemetry,
                    &settings,
                    &ctx,
                );
                draw(frame, &screen);
            })
            .expect("render management screen");

        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn management_columns_keep_core_identity_on_tiny_widths() {
        let visible = visible_management_column_ids(45);

        assert_eq!(
            visible,
            vec![ManagementColumnId::Selection, ManagementColumnId::Name]
        );
    }

    #[test]
    fn management_columns_prioritize_speeds_on_medium_widths() {
        let visible = visible_management_column_ids(80);

        assert!(visible.contains(&ManagementColumnId::Selection));
        assert!(visible.contains(&ManagementColumnId::Name));
        assert!(visible.contains(&ManagementColumnId::DownSpeed));
        assert!(visible.contains(&ManagementColumnId::UpSpeed));
        assert!(!visible.contains(&ManagementColumnId::Eta));
        assert!(!visible.contains(&ManagementColumnId::Size));
    }

    #[test]
    fn management_columns_restore_all_metrics_on_wide_widths() {
        let visible = visible_management_column_ids(150);

        assert_eq!(
            visible,
            vec![
                ManagementColumnId::Selection,
                ManagementColumnId::Name,
                ManagementColumnId::Eta,
                ManagementColumnId::Completed,
                ManagementColumnId::State,
                ManagementColumnId::Peers,
                ManagementColumnId::DownSpeed,
                ManagementColumnId::UpSpeed,
                ManagementColumnId::Size,
                ManagementColumnId::DateAdded,
            ]
        );
    }

    #[test]
    fn management_content_area_insets_roomy_viewports() {
        let area = Rect::new(0, 0, 120, 32);

        assert_eq!(management_content_area(area), Rect::new(1, 1, 118, 30));
    }

    #[test]
    fn management_content_area_keeps_compact_viewports_full_width() {
        let area = Rect::new(0, 0, 78, 16);

        assert_eq!(management_content_area(area), area);
    }

    #[test]
    fn management_column_navigation_uses_the_rendered_table_width() {
        let mut app_state = app_state_with_torrents(vec![(hash(1), "Sample Packet", 50, 5, 1)]);

        for (width, height) in [
            (91, 32),
            (92, 32),
            (99, 32),
            (100, 32),
            (109, 32),
            (110, 32),
            (120, 32),
            (121, 32),
            (131, 32),
            (132, 32),
            (120, 17),
        ] {
            app_state.screen_area = Rect::new(0, 0, width, height);
            let rendered_width = management_content_area(app_state.screen_area).width;
            assert_eq!(
                visible_management_column_indices_for_state(&app_state),
                compute_visible_management_columns(rendered_width).1,
                "width={width}, height={height}"
            );
        }
    }

    #[test]
    fn management_sort_uses_the_column_that_is_actually_rendered() {
        let mut app_state = app_state_with_torrents(vec![(hash(1), "Sample Packet", 50, 5, 1)]);
        let size_column = management_column_index(ManagementColumnId::Size).expect("size column");
        let up_speed_column =
            management_column_index(ManagementColumnId::UpSpeed).expect("upload column");
        app_state.ui.torrent_management.selected_column_index = size_column;

        let roomy = render_management_screen(&mut app_state, 120, 32);
        assert!(roomy.contains("ETA"));
        assert!(!roomy.contains("Size"));
        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::SortBySelectedColumn,
        );
        assert_eq!(
            app_state.ui.torrent_management.sort_column_index,
            Some(up_speed_column)
        );

        app_state.ui.torrent_management.selected_column_index = size_column;
        let compact = render_management_screen(&mut app_state, 120, 17);
        assert!(compact.contains("Size"));
        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::SortBySelectedColumn,
        );
        assert_eq!(
            app_state.ui.torrent_management.sort_column_index,
            Some(size_column)
        );
    }

    #[test]
    fn management_keymap_moves_columns_and_sorts_selected_column() {
        let app_state = app_state_with_torrents(vec![(hash(1), "Mock Release S01E01", 50, 5, 1)]);

        assert_eq!(
            map_key_to_management_action(KeyCode::Left, &app_state),
            Some(TorrentManagementAction::MoveColumnLeft)
        );
        assert_eq!(
            map_key_to_management_action(KeyCode::Right, &app_state),
            Some(TorrentManagementAction::MoveColumnRight)
        );
        assert_eq!(
            map_key_to_management_action(KeyCode::Char('s'), &app_state),
            Some(TorrentManagementAction::SortBySelectedColumn)
        );
    }

    #[test]
    fn management_keymap_maps_page_home_end_vertical_movement() {
        let app_state = app_state_with_torrents(vec![(hash(1), "Mock Release S01E01", 50, 5, 1)]);

        assert_eq!(
            map_key_to_management_action(KeyCode::PageUp, &app_state),
            Some(TorrentManagementAction::MovePageUp)
        );
        assert_eq!(
            map_key_to_management_action(KeyCode::PageDown, &app_state),
            Some(TorrentManagementAction::MovePageDown)
        );
        assert_eq!(
            map_key_to_management_action(KeyCode::Home, &app_state),
            Some(TorrentManagementAction::MoveFirst)
        );
        assert_eq!(
            map_key_to_management_action(KeyCode::End, &app_state),
            Some(TorrentManagementAction::MoveLast)
        );
    }

    #[test]
    fn repeat_events_cannot_cross_management_state_boundaries() {
        let mut app_state = app_state_with_torrents(vec![(hash(1), "Sample Packet", 50, 5, 1)]);
        app_state
            .ui
            .torrent_management
            .pending_commands
            .push(pause_command(1));

        let submit_press = KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT);
        assert_eq!(
            map_key_event_to_management_action(submit_press, &app_state),
            Some(TorrentManagementAction::ShowSubmitConfirmation)
        );
        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::ShowSubmitConfirmation,
        );

        let submit_repeat = KeyEvent::new_with_kind(
            KeyCode::Char('Y'),
            KeyModifiers::SHIFT,
            KeyEventKind::Repeat,
        );
        assert_eq!(
            map_key_event_to_management_action(submit_repeat, &app_state),
            None
        );
        assert!(app_state.ui.torrent_management.confirm_submit);
        assert_eq!(app_state.ui.torrent_management.pending_commands.len(), 1);
        assert_eq!(
            map_key_event_to_management_action(submit_press, &app_state),
            None
        );
        assert_eq!(
            map_key_event_to_management_action(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &app_state,
            ),
            Some(TorrentManagementAction::SubmitPendingCommands)
        );

        app_state.ui.torrent_management.confirm_submit = false;
        for code in [
            KeyCode::Char('p'),
            KeyCode::Char('d'),
            KeyCode::Char('D'),
            KeyCode::Char('u'),
            KeyCode::Char(' '),
            KeyCode::Char('x'),
            KeyCode::Char('s'),
            KeyCode::Tab,
            KeyCode::Esc,
        ] {
            let repeat = KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Repeat);
            assert_eq!(
                map_key_event_to_management_action(repeat, &app_state),
                None,
                "repeat should be ignored for {code:?}"
            );
        }

        let navigation_repeat =
            KeyEvent::new_with_kind(KeyCode::Down, KeyModifiers::NONE, KeyEventKind::Repeat);
        assert_eq!(
            map_key_event_to_management_action(navigation_repeat, &app_state),
            Some(TorrentManagementAction::MoveDown)
        );

        app_state.ui.torrent_management.confirm_submit = true;
        assert_eq!(
            map_key_event_to_management_action(navigation_repeat, &app_state),
            Some(TorrentManagementAction::ReviewScrollDown)
        );
    }

    #[test]
    fn modified_management_shortcuts_are_ignored() {
        let app_state = app_state_with_torrents(vec![(hash(1), "Sample Packet", 50, 5, 1)]);

        for (code, modifiers) in [
            (KeyCode::Char('p'), KeyModifiers::CONTROL),
            (KeyCode::Char('d'), KeyModifiers::ALT),
            (KeyCode::Char('u'), KeyModifiers::SUPER),
        ] {
            let event = KeyEvent::new(code, modifiers);
            assert_eq!(map_key_event_to_management_action(event, &app_state), None);
        }
    }

    #[test]
    fn repeated_search_shortcut_does_not_insert_a_slash() {
        let mut app_state = app_state_with_torrents(vec![(hash(1), "Sample Packet", 50, 5, 1)]);
        let slash_press = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE);
        assert_eq!(
            map_key_event_to_management_action(slash_press, &app_state),
            Some(TorrentManagementAction::StartSearch)
        );
        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::StartSearch);

        let slash_repeat =
            KeyEvent::new_with_kind(KeyCode::Char('/'), KeyModifiers::NONE, KeyEventKind::Repeat);
        assert_eq!(
            map_key_event_to_management_action(slash_repeat, &app_state),
            None
        );
        assert!(app_state.ui.torrent_management.search_query.is_empty());
    }

    #[test]
    fn held_submit_key_does_not_submit_through_the_input_boundary() {
        let mut app_state = app_state_with_torrents(vec![(hash(1), "Sample Packet", 50, 5, 1)]);
        app_state
            .ui
            .torrent_management
            .pending_commands
            .push(pause_command(1));

        let submit_press = KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT);
        let action = map_key_event_to_management_action_with_latch(submit_press, &mut app_state)
            .expect("open review");
        assert_eq!(action, TorrentManagementAction::ShowSubmitConfirmation);
        reduce_torrent_management_action(&mut app_state, action);
        assert!(app_state.ui.torrent_management.confirm_submit);

        assert_eq!(
            map_key_event_to_management_action_with_latch(
                KeyEvent::new_with_kind(
                    KeyCode::Char('Y'),
                    KeyModifiers::SHIFT,
                    KeyEventKind::Repeat,
                ),
                &mut app_state,
            ),
            None
        );
        assert!(app_state.ui.torrent_management.confirm_submit);
        assert_eq!(app_state.ui.torrent_management.pending_commands.len(), 1);
        assert_eq!(
            map_key_event_to_management_action_with_latch(submit_press, &mut app_state),
            None
        );
        assert!(app_state.ui.torrent_management.confirm_submit);
        assert_eq!(app_state.ui.torrent_management.pending_commands.len(), 1);
    }

    #[test]
    fn press_only_terminal_keeps_search_opener_latched_but_allows_escape_reuse() {
        let mut app_state = app_state_with_torrents(vec![(hash(1), "Sample Packet", 50, 5, 1)]);
        let slash = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE);

        let action = map_key_event_to_management_action_with_latch(slash, &mut app_state)
            .expect("start search");
        assert_eq!(action, TorrentManagementAction::StartSearch);
        reduce_torrent_management_action(&mut app_state, action);
        assert!(app_state.ui.torrent_management.is_searching);
        assert_eq!(
            map_key_event_to_management_action_with_latch(slash, &mut app_state),
            None
        );
        assert!(app_state.ui.torrent_management.search_query.is_empty());

        let action = map_key_event_to_management_action_with_latch(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            &mut app_state,
        )
        .expect("insert search text");
        reduce_torrent_management_action(&mut app_state, action);
        assert_eq!(app_state.ui.torrent_management.search_query, "a");
        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let action = map_key_event_to_management_action_with_latch(escape, &mut app_state)
            .expect("cancel search");
        assert_eq!(action, TorrentManagementAction::SearchCancel);
        reduce_torrent_management_action(&mut app_state, action);
        assert!(!app_state.ui.torrent_management.is_searching);
        assert_eq!(
            map_key_event_to_management_action_with_latch(escape, &mut app_state),
            Some(TorrentManagementAction::ToNormal)
        );
    }

    #[test]
    fn key_release_clears_the_management_input_latch() {
        let mut app_state = app_state_with_torrents(vec![(hash(1), "Sample Packet", 50, 5, 1)]);
        let slash = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE);
        let action = map_key_event_to_management_action_with_latch(slash, &mut app_state)
            .expect("start search");
        reduce_torrent_management_action(&mut app_state, action);

        assert_eq!(
            map_key_event_to_management_action_with_latch(
                KeyEvent::new_with_kind(
                    KeyCode::Char('/'),
                    KeyModifiers::NONE,
                    KeyEventKind::Release,
                ),
                &mut app_state,
            ),
            None
        );
        let action = map_key_event_to_management_action_with_latch(slash, &mut app_state)
            .expect("insert slash after release");
        assert_eq!(action, TorrentManagementAction::SearchInsert('/'));
        reduce_torrent_management_action(&mut app_state, action);
        assert_eq!(app_state.ui.torrent_management.search_query, "/");
    }

    #[test]
    fn distinct_press_can_toggle_a_staged_action_back_off_without_release_events() {
        let mut app_state = app_state_with_torrents(vec![(hash(1), "Sample Packet", 50, 5, 1)]);
        let pause = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE);

        let action = map_key_event_to_management_action_with_latch(pause, &mut app_state)
            .expect("stage pause");
        reduce_torrent_management_action(&mut app_state, action);
        assert_eq!(app_state.ui.torrent_management.pending_commands.len(), 1);

        let action = map_key_event_to_management_action_with_latch(pause, &mut app_state)
            .expect("reuse pause without a release event");
        reduce_torrent_management_action(&mut app_state, action);
        assert!(app_state.ui.torrent_management.pending_commands.is_empty());
    }

    #[test]
    fn press_only_terminals_can_reuse_reversible_shortcuts() {
        for (code, modifiers) in [
            (KeyCode::Char('p'), KeyModifiers::NONE),
            (KeyCode::Char('d'), KeyModifiers::NONE),
            (KeyCode::Char('D'), KeyModifiers::SHIFT),
            (KeyCode::Char(' '), KeyModifiers::NONE),
            (KeyCode::Char('x'), KeyModifiers::NONE),
            (KeyCode::Char('s'), KeyModifiers::NONE),
            (KeyCode::Tab, KeyModifiers::NONE),
        ] {
            let mut app_state = app_state_with_torrents(vec![(hash(1), "Sample Packet", 50, 5, 1)]);
            app_state.ui.torrent_management.search_query = "Sample".to_string();
            let key = KeyEvent::new(code, modifiers);

            assert!(
                map_key_event_to_management_action_with_latch(key, &mut app_state).is_some(),
                "first press should map for {code:?}"
            );
            assert!(
                map_key_event_to_management_action_with_latch(key, &mut app_state).is_some(),
                "second Press should remain usable for {code:?}"
            );
        }
    }

    #[test]
    fn review_enter_emits_ordered_runtime_effects() {
        let mut app_state = app_state_with_torrents(vec![(hash(1), "Sample Packet", 50, 5, 1)]);
        app_state
            .ui
            .torrent_management
            .pending_commands
            .push(pause_command(1));

        let review = handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT)),
            &mut app_state,
        );
        assert!(review.consumed);
        assert!(review.effects.is_empty());
        assert!(app_state.ui.torrent_management.confirm_submit);

        let submit = handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &mut app_state,
        );
        assert!(submit.consumed);
        assert_eq!(
            submit.effects,
            vec![
                TorrentManagementEffect::SubmitControlRequest(ControlRequest::Pause {
                    info_hash_hex: hex::encode(hash(1)),
                }),
                TorrentManagementEffect::MarkControlState {
                    info_hash: hash(1),
                    state: TorrentControlState::Paused,
                    delete_files: false,
                },
            ]
        );
    }

    #[test]
    fn management_page_home_end_move_selected_row_vertically() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(0), "Mock Release S01E00", 50, 5, 1),
            (hash(1), "Mock Release S01E01", 50, 5, 1),
            (hash(2), "Mock Release S01E02", 50, 5, 1),
            (hash(3), "Mock Release S01E03", 50, 5, 1),
            (hash(4), "Mock Release S01E04", 50, 5, 1),
            (hash(5), "Mock Release S01E05", 50, 5, 1),
            (hash(6), "Mock Release S01E06", 50, 5, 1),
            (hash(7), "Mock Release S01E07", 50, 5, 1),
            (hash(8), "Mock Release S01E08", 50, 5, 1),
            (hash(9), "Mock Release S01E09", 50, 5, 1),
            (hash(10), "Mock Release S01E10", 50, 5, 1),
            (hash(11), "Mock Release S01E11", 50, 5, 1),
        ]);
        app_state.screen_area = Rect::new(0, 0, 200, 100);

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::MovePageDown);
        assert_eq!(app_state.ui.torrent_management.selected_index, 11);

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::MovePageUp);
        assert_eq!(app_state.ui.torrent_management.selected_index, 0);

        app_state.ui.torrent_management.selected_index = 5;
        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::MoveLast);
        assert_eq!(app_state.ui.torrent_management.selected_index, 11);

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::MoveFirst);
        assert_eq!(app_state.ui.torrent_management.selected_index, 0);
    }

    #[test]
    fn management_keymap_opens_highlighted_torrent_files() {
        let app_state = app_state_with_torrents(vec![(hash(1), "Mock Release S01E01", 50, 5, 1)]);

        assert_eq!(
            map_key_to_management_action(KeyCode::Char('f'), &app_state),
            Some(TorrentManagementAction::OpenHighlightedTorrentFiles)
        );
    }

    #[test]
    fn open_highlighted_torrent_files_ignores_multi_select_targets() {
        let first_hash = hash(1);
        let second_hash = hash(2);
        let mut app_state = app_state_with_torrents(vec![
            (first_hash.clone(), "Mock Release S01E01", 50, 5, 1),
            (second_hash.clone(), "Mock Release S01E02", 60, 6, 2),
        ]);
        app_state.ui.torrent_management.selected_index = 1;
        app_state
            .ui
            .torrent_management
            .selected_hashes
            .insert(first_hash);

        let result = reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::OpenHighlightedTorrentFiles,
        );

        assert_eq!(
            result.effects,
            vec![TorrentManagementEffect::OpenExistingTorrentFileBrowser(
                second_hash
            )]
        );
    }

    #[test]
    fn management_column_movement_stays_on_visible_columns() {
        let mut app_state =
            app_state_with_torrents(vec![(hash(1), "Mock Release S01E01", 50, 5, 1)]);
        app_state.screen_area = Rect::new(0, 0, 80, 24);
        app_state.ui.torrent_management.selected_column_index =
            management_column_index(ManagementColumnId::Name).expect("name column");

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::MoveColumnRight);

        let visible = visible_management_column_ids(app_state.screen_area.width);
        assert!(visible.contains(
            &management_columns()[app_state.ui.torrent_management.selected_column_index].id
        ));
    }

    #[test]
    fn default_management_sort_is_name_ascending() {
        let app_state = app_state_with_torrents(vec![
            (hash(1), "Zephyr Archive", 100, 10, 2),
            (hash(2), "Aurora Archive", 100, 10, 2),
        ]);
        let rows = build_management_rows(&app_state);

        assert_eq!(rows[0].label, "Aurora Archive");
        assert_eq!(
            app_state.ui.torrent_management.sort_column_index,
            management_column_index(ManagementColumnId::Name)
        );
        assert_eq!(
            app_state.ui.torrent_management.sort_direction,
            SortDirection::Ascending
        );
    }

    #[test]
    fn sorting_by_download_speed_orders_rows_descending_then_toggles() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Slower Seed", 100, 10, 2),
            (hash(2), "Faster Seed", 900, 20, 3),
        ]);
        app_state.ui.torrent_management.selected_column_index =
            management_column_index(ManagementColumnId::DownSpeed).expect("download column");

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::SortBySelectedColumn,
        );
        let rows = build_management_rows(&app_state);
        assert_eq!(rows[0].label, "Faster Seed");
        assert_eq!(
            app_state.ui.torrent_management.sort_direction,
            SortDirection::Descending
        );

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::SortBySelectedColumn,
        );
        let rows = build_management_rows(&app_state);
        assert_eq!(rows[0].label, "Slower Seed");
        assert_eq!(
            app_state.ui.torrent_management.sort_direction,
            SortDirection::Ascending
        );
    }

    #[test]
    fn sorting_unsorted_numeric_column_starts_highest_first_with_down_arrow() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Fewer Peers", 100, 10, 2),
            (hash(2), "More Peers", 100, 10, 7),
        ]);
        app_state.ui.torrent_management.sort_column_index = None;
        app_state.ui.torrent_management.selected_column_index =
            management_column_index(ManagementColumnId::Peers).expect("peers column");

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::SortBySelectedColumn,
        );

        let rows = build_management_rows(&app_state);
        assert_eq!(rows[0].label, "More Peers");
        assert_eq!(
            app_state.ui.torrent_management.sort_direction,
            SortDirection::Descending
        );
        assert_eq!(
            management_sort_arrow(ManagementColumnId::Peers, SortDirection::Descending),
            " ▼"
        );
    }

    #[test]
    fn sorting_by_date_added_orders_newest_first_then_toggles() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Older Seed", 100, 10, 2),
            (hash(2), "Newer Seed", 100, 10, 2),
        ]);
        app_state.ui.torrent_management.selected_column_index =
            management_column_index(ManagementColumnId::DateAdded).expect("date added column");

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::SortBySelectedColumn,
        );
        let rows = build_management_rows(&app_state);
        assert_eq!(rows[0].label, "Newer Seed");
        assert_eq!(
            app_state.ui.torrent_management.sort_direction,
            SortDirection::Descending
        );

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::SortBySelectedColumn,
        );
        let rows = build_management_rows(&app_state);
        assert_eq!(rows[0].label, "Older Seed");
        assert_eq!(
            app_state.ui.torrent_management.sort_direction,
            SortDirection::Ascending
        );
    }

    #[test]
    fn date_added_formats_as_local_calendar_date_or_dash() {
        assert_eq!(format_added_date(None), "-");

        let rendered = format_added_date(Some(1_700_000_000));

        assert_eq!(rendered.len(), 10);
        assert_eq!(rendered.chars().nth(4), Some('-'));
        assert_eq!(rendered.chars().nth(7), Some('-'));
    }

    #[test]
    fn pending_action_marker_preserves_selection_state() {
        assert_eq!(management_selection_marker(SelectionState::None, true), "!");
        assert_eq!(
            management_selection_marker(SelectionState::Partial, true),
            "~!"
        );
        assert_eq!(
            management_selection_marker(SelectionState::Full, true),
            "x!"
        );
        assert_eq!(
            management_selection_marker(SelectionState::Full, false),
            "x"
        );
    }

    #[test]
    fn selection_marker_column_uses_equals_header_and_compact_values() {
        let selection_column = management_columns()
            .into_iter()
            .find(|column| column.id == ManagementColumnId::Selection)
            .expect("selection column");

        assert_eq!(selection_column.header, "=");
        assert_eq!(selection_column.min_width, 2);
        assert_eq!(selection_column.constraint, Constraint::Length(2));
        assert_eq!(
            management_selection_marker(SelectionState::None, false),
            "-"
        );
        assert_eq!(
            management_selection_marker(SelectionState::Partial, false),
            "~"
        );
        assert_eq!(
            management_selection_marker(SelectionState::Full, false),
            "x"
        );
    }

    #[test]
    fn management_speed_cells_use_shared_speed_palette() {
        let ctx = ThemeContext::new(Default::default(), 0.0);
        let cell = management_speed_cell(&ctx, 2_100_000);

        assert_eq!(
            ratatui::style::Styled::style(&cell).fg,
            Some(ctx.theme.scale.speed[3])
        );
    }

    #[test]
    fn management_zero_speed_cells_inherit_row_style() {
        let ctx = ThemeContext::new(Default::default(), 0.0);
        let cell = management_speed_cell(&ctx, 0);

        assert_eq!(ratatui::style::Styled::style(&cell).fg, None);
    }

    #[test]
    fn search_filters_torrent_rows_without_mutating_dashboard_search() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Meadow Saga S01E01", 100, 10, 2),
            (hash(2), "Mock Release S01E01", 50, 5, 1),
        ]);
        app_state.ui.search_query = "normal".to_string();
        app_state.ui.torrent_management.search_query = "mock".to_string();

        let rows = build_management_rows(&app_state);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Mock Release S01E01");
        assert_eq!(app_state.ui.search_query, "normal");
    }

    #[test]
    fn empty_management_search_ignores_cached_normal_search_subset() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Meadow Saga S01E01", 100, 10, 2),
            (hash(2), "Mock Release S01E01", 50, 5, 1),
            (hash(3), "Orchard Notes S01E01", 75, 8, 1),
        ]);
        app_state.ui.search_query = "mock".to_string();
        app_state.torrent_list_order = vec![hash(2)];
        app_state.ui.torrent_management.search_query.clear();

        let rows = build_management_rows(&app_state);

        assert_eq!(rows.len(), 3);
        assert!(rows.iter().any(|row| row.info_hashes == vec![hash(1)]));
        assert!(rows.iter().any(|row| row.info_hashes == vec![hash(2)]));
        assert!(rows.iter().any(|row| row.info_hashes == vec![hash(3)]));
    }

    #[test]
    fn committed_management_search_keeps_search_panel_visible() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Meadow Saga S01E01", 100, 10, 2),
            (hash(2), "Mock Release S01E01", 50, 5, 1),
        ]);
        app_state.ui.torrent_management.is_searching = true;
        app_state.ui.torrent_management.search_query = "mock".to_string();

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::SearchCommit);

        assert!(!app_state.ui.torrent_management.is_searching);
        assert_eq!(app_state.ui.torrent_management.search_query, "mock");
        assert!(management_search_panel_active(&app_state));
    }

    #[test]
    fn empty_management_search_panel_stays_hidden_outside_search_mode() {
        let mut app_state =
            app_state_with_torrents(vec![(hash(1), "Mock Release S01E01", 50, 5, 1)]);
        app_state.ui.torrent_management.is_searching = false;
        app_state.ui.torrent_management.search_query.clear();

        assert!(!management_search_panel_active(&app_state));
    }

    #[test]
    fn tab_toggles_management_search_mode_while_searching() {
        let mut app_state =
            app_state_with_torrents(vec![(hash(1), "Mock Release S01E01", 50, 5, 1)]);
        app_state.ui.torrent_management.is_searching = true;
        assert!(matches!(
            app_state.ui.torrent_management.search_mode,
            SearchMode::Regex
        ));
        assert_eq!(
            map_key_to_management_action(KeyCode::Tab, &app_state),
            Some(TorrentManagementAction::ToggleSearchMode)
        );

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::ToggleSearchMode);

        assert!(matches!(
            app_state.ui.torrent_management.search_mode,
            SearchMode::Fuzzy
        ));
    }

    #[test]
    fn tab_toggles_management_search_mode_for_committed_search() {
        let mut app_state =
            app_state_with_torrents(vec![(hash(1), "Mock Release S01E01", 50, 5, 1)]);
        app_state.ui.torrent_management.is_searching = false;
        app_state.ui.torrent_management.search_query = "Mock".to_string();

        assert_eq!(
            map_key_to_management_action(KeyCode::Tab, &app_state),
            Some(TorrentManagementAction::ToggleSearchMode)
        );
    }

    #[test]
    fn regex_management_search_filters_torrent_rows() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Meadow Saga S01E01", 100, 10, 2),
            (hash(2), "Mock Release S01E02", 50, 5, 1),
        ]);
        app_state.ui.torrent_management.search_mode = SearchMode::Regex;
        app_state.ui.torrent_management.search_query = r"S01E0[12]".to_string();

        let rows = build_management_rows(&app_state);

        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn invalid_regex_management_search_matches_no_rows() {
        let mut app_state =
            app_state_with_torrents(vec![(hash(1), "Mock Release S01E01", 50, 5, 1)]);
        app_state.ui.torrent_management.search_mode = SearchMode::Regex;
        app_state.ui.torrent_management.search_query = "[".to_string();

        let rows = build_management_rows(&app_state);

        assert!(rows.is_empty());
    }

    #[test]
    fn management_search_matches_download_path_and_container() {
        let mut app_state = app_state_with_torrents(vec![(hash(1), "Sample Packet", 50, 5, 1)]);
        let torrent = app_state.torrents.get_mut(&hash(1)).expect("torrent");
        torrent.latest_state.download_path =
            Some(std::path::PathBuf::from("/archive/needle-folder"));
        torrent.latest_state.container_name = Some("sample-container".to_string());

        app_state.ui.torrent_management.search_query = "needle-folder".to_string();
        assert_eq!(build_management_rows(&app_state).len(), 1);

        app_state.ui.torrent_management.search_query = "sample-container".to_string();
        assert_eq!(build_management_rows(&app_state).len(), 1);
    }

    #[test]
    fn management_search_edit_sequence_updates_and_cancels_cleanly() {
        let mut app_state = app_state_with_torrents(vec![(hash(1), "Sample Packet", 50, 5, 1)]);

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::StartSearch);
        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::SearchInsert('x'),
        );
        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::SearchBackspace);

        assert!(app_state.ui.torrent_management.is_searching);
        assert!(app_state.ui.torrent_management.search_query.is_empty());

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::SearchCancel);
        assert!(!app_state.ui.torrent_management.is_searching);
        assert!(app_state.ui.torrent_management.search_query.is_empty());
        assert_eq!(
            app_state.ui.torrent_management.cursor_hash.as_deref(),
            Some(hash(1).as_slice())
        );
    }

    #[test]
    fn search_cancel_preserves_the_selected_target_set() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Amber Packet", 100, 10, 2),
            (hash(2), "Blue Packet", 300, 20, 3),
        ]);
        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::SelectAllVisible);

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::StartSearch);
        for character in "Blue".chars() {
            reduce_torrent_management_action(
                &mut app_state,
                TorrentManagementAction::SearchInsert(character),
            );
        }

        assert_eq!(build_management_rows(&app_state).len(), 1);
        assert_eq!(app_state.ui.torrent_management.selected_hashes.len(), 2);
        assert!(app_state
            .ui
            .torrent_management
            .selected_hashes
            .contains(&hash(1)));
        assert!(app_state
            .ui
            .torrent_management
            .selected_hashes
            .contains(&hash(2)));

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::SearchCancel);

        assert_eq!(build_management_rows(&app_state).len(), 2);
        assert_eq!(app_state.ui.torrent_management.selected_hashes.len(), 2);
    }

    #[test]
    fn invalid_regex_does_not_clear_the_selected_target_set() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Amber Packet", 100, 10, 2),
            (hash(2), "Blue Packet", 300, 20, 3),
        ]);
        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::SelectAllVisible);
        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::StartSearch);
        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::SearchInsert('['),
        );

        assert!(build_management_rows(&app_state).is_empty());
        assert_eq!(app_state.ui.torrent_management.selected_hashes.len(), 2);

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::SearchCancel);

        assert_eq!(build_management_rows(&app_state).len(), 2);
        assert_eq!(app_state.ui.torrent_management.selected_hashes.len(), 2);
    }

    #[test]
    fn anonymized_torrent_rows_hide_release_markers() {
        let mut app_state =
            app_state_with_torrents(vec![(hash(1), "Mock.Release S01E01", 50, 5, 1)]);
        app_state.anonymize_torrent_names = true;

        let rows = build_management_rows(&app_state);
        let anonymized = &rows[0].label;

        assert_ne!(anonymized, "Mock.Release S01E01");
        assert_ne!(anonymized, "Torrent 1");
        assert!(!anonymized.contains('.'));
        assert!(!anonymized.chars().any(|ch| ch.is_ascii_digit()));
        assert!(!anonymized.contains("  "));
        assert!(anonymized.matches(' ').count() >= 1);
    }

    #[test]
    fn anonymized_rows_hide_numbered_episode_markers() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Meadow Saga S01E01", 100, 10, 2),
            (hash(2), "Meadow Saga S01E02", 300, 20, 3),
        ]);
        app_state.anonymize_torrent_names = true;

        let rows = build_management_rows(&app_state);
        let anonymized = &rows[0].label;

        assert_ne!(anonymized, "Meadow Saga S01E01");
        assert!(!anonymized.chars().any(|ch| ch.is_ascii_digit()));
        assert!(!anonymized.contains("  "));
        assert!(anonymized.matches(' ').count() >= 2);
    }

    #[test]
    fn x_toggles_anonymized_names_in_management_screen() {
        let mut app_state =
            app_state_with_torrents(vec![(hash(1), "Mock Release S01E01", 50, 5, 1)]);
        assert!(!app_state.anonymize_torrent_names);
        assert_eq!(
            map_key_to_management_action(KeyCode::Char('x'), &app_state),
            Some(TorrentManagementAction::ToggleAnonymizeNames)
        );

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::ToggleAnonymizeNames,
        );
        assert!(app_state.anonymize_torrent_names);

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::ToggleAnonymizeNames,
        );
        assert!(!app_state.anonymize_torrent_names);
    }

    #[test]
    fn x_still_types_into_management_search() {
        let mut app_state =
            app_state_with_torrents(vec![(hash(1), "Mock Release S01E01", 50, 5, 1)]);
        app_state.ui.torrent_management.is_searching = true;

        assert_eq!(
            map_key_to_management_action(KeyCode::Char('x'), &app_state),
            Some(TorrentManagementAction::SearchInsert('x'))
        );
    }

    #[test]
    fn toggle_current_selection_selects_current_torrent_row() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Meadow Saga S01E01", 100, 10, 2),
            (hash(2), "Meadow Saga S01E02", 300, 20, 3),
        ]);

        let result = reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::ToggleCurrentSelection,
        );

        assert!(result.consumed);
        assert!(app_state
            .ui
            .torrent_management
            .selected_hashes
            .contains(&hash(1)));
        assert_eq!(app_state.ui.torrent_management.selected_hashes.len(), 1);
    }

    #[test]
    fn pause_action_stages_batch_pause_requests_for_selected_torrents() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Meadow Saga S01E01", 100, 10, 2),
            (hash(2), "Meadow Saga S01E02", 300, 20, 3),
        ]);
        app_state
            .ui
            .torrent_management
            .selected_hashes
            .insert(hash(1));
        app_state
            .ui
            .torrent_management
            .selected_hashes
            .insert(hash(2));

        let result = reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::TogglePauseTargets,
        );

        assert!(result.effects.is_empty());
        assert_eq!(app_state.ui.torrent_management.pending_commands.len(), 2);
        assert!(matches!(
            app_state.ui.torrent_management.pending_commands[0].request,
            ControlRequest::Pause { .. }
        ));
        assert!(matches!(
            app_state.ui.torrent_management.pending_commands[1].request,
            ControlRequest::Pause { .. }
        ));
        assert_eq!(app_state.ui.torrent_management.selected_hashes.len(), 2);
        assert_eq!(
            map_key_to_management_action(KeyCode::Char('Y'), &app_state),
            Some(TorrentManagementAction::ShowSubmitConfirmation)
        );
        assert_eq!(
            map_key_to_management_action(KeyCode::Enter, &app_state),
            None
        );
    }

    #[test]
    fn pause_action_toggles_each_selected_torrent_independently() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Meadow Saga S01E01", 100, 10, 2),
            (hash(2), "Meadow Saga S01E02", 300, 20, 3),
        ]);
        app_state
            .torrents
            .get_mut(&hash(1))
            .expect("paused torrent")
            .latest_state
            .torrent_control_state = TorrentControlState::Paused;
        app_state
            .ui
            .torrent_management
            .selected_hashes
            .insert(hash(1));
        app_state
            .ui
            .torrent_management
            .selected_hashes
            .insert(hash(2));

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::TogglePauseTargets,
        );

        assert_eq!(app_state.ui.torrent_management.pending_commands.len(), 2);
        assert!(app_state
            .ui
            .torrent_management
            .pending_commands
            .iter()
            .any(|command| command.info_hash == hash(1)
                && matches!(command.request, ControlRequest::Resume { .. })));
        assert!(app_state
            .ui
            .torrent_management
            .pending_commands
            .iter()
            .any(|command| command.info_hash == hash(2)
                && matches!(command.request, ControlRequest::Pause { .. })));
    }

    #[test]
    fn select_all_pause_then_clear_removes_the_original_batch() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Meadow Saga S01E01", 100, 10, 2),
            (hash(2), "Meadow Saga S01E02", 300, 20, 3),
        ]);

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::SelectAllVisible);
        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::TogglePauseTargets,
        );
        assert_eq!(app_state.ui.torrent_management.selected_hashes.len(), 2);
        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::ClearPendingForTargets,
        );

        assert!(app_state.ui.torrent_management.pending_commands.is_empty());
        assert!(app_state.ui.torrent_management.selected_hashes.is_empty());
    }

    #[test]
    fn select_all_then_u_clears_selection_without_drafts() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Sample Packet One", 100, 10, 2),
            (hash(2), "Sample Packet Two", 300, 20, 3),
        ]);

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::SelectAllVisible);
        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::ClearPendingForTargets,
        );

        assert!(app_state.ui.torrent_management.selected_hashes.is_empty());
        assert!(app_state.ui.torrent_management.pending_commands.is_empty());
        assert_eq!(
            app_state.ui.torrent_management.status_message.as_deref(),
            Some("Cleared 2 selections")
        );
    }

    #[test]
    fn selected_batch_can_replace_pause_with_remove_without_reselecting() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Sample Packet One", 100, 10, 2),
            (hash(2), "Sample Packet Two", 300, 20, 3),
        ]);

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::SelectAllVisible);
        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::TogglePauseTargets,
        );
        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::StartDelete {
                delete_files: false,
            },
        );

        assert_eq!(app_state.ui.torrent_management.selected_hashes.len(), 2);
        assert_eq!(app_state.ui.torrent_management.pending_commands.len(), 2);
        assert!(app_state
            .ui
            .torrent_management
            .pending_commands
            .iter()
            .all(|command| matches!(
                command.request,
                ControlRequest::Delete {
                    delete_files: false,
                    ..
                }
            )));
    }

    #[test]
    fn filtered_out_selection_does_not_block_the_highlighted_visible_torrent() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Amber Packet", 100, 10, 2),
            (hash(2), "Blue Packet", 300, 20, 3),
        ]);
        app_state
            .ui
            .torrent_management
            .selected_hashes
            .insert(hash(1));
        app_state.ui.torrent_management.search_query = "Blue".to_string();

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::TogglePauseTargets,
        );

        assert_eq!(app_state.ui.torrent_management.pending_commands.len(), 1);
        assert_eq!(
            app_state.ui.torrent_management.pending_commands[0].info_hash,
            hash(2)
        );
        assert_eq!(
            app_state.ui.torrent_management.selected_hashes,
            HashSet::from([hash(1)])
        );
    }

    #[test]
    fn sorting_preserves_the_highlighted_torrent_identity() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Amber Packet", 100, 10, 2),
            (hash(2), "Blue Packet", 900, 20, 3),
        ]);
        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::MoveFirst);
        app_state.ui.torrent_management.selected_column_index =
            management_column_index(ManagementColumnId::DownSpeed).expect("download column");

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::SortBySelectedColumn,
        );

        assert_eq!(current_row_targets(&app_state), vec![hash(1)]);
        assert_eq!(app_state.ui.torrent_management.selected_index, 1);
        assert_eq!(
            app_state.ui.torrent_management.cursor_hash.as_deref(),
            Some(hash(1).as_slice())
        );
    }

    #[test]
    fn entry_anchors_the_highlight_before_live_sort_values_change() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Amber Packet", 100, 10, 2),
            (hash(2), "Blue Packet", 900, 20, 3),
        ]);
        let speed_column =
            management_column_index(ManagementColumnId::DownSpeed).expect("download column");
        app_state.ui.torrent_management.sort_column_index = Some(speed_column);
        app_state.ui.torrent_management.sort_direction = SortDirection::Descending;

        initialize_torrent_management_cursor(&mut app_state);
        assert_eq!(
            app_state.ui.torrent_management.cursor_hash.as_deref(),
            Some(hash(2).as_slice())
        );
        app_state
            .torrents
            .get_mut(&hash(1))
            .expect("torrent")
            .smoothed_download_speed_bps = 1_200;

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::ToggleCurrentSelection,
        );

        assert_eq!(
            app_state.ui.torrent_management.selected_hashes,
            HashSet::from([hash(2)])
        );
    }

    #[test]
    fn first_action_after_highlighted_row_removal_uses_the_visible_fallback() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Amber Packet", 100, 10, 2),
            (hash(2), "Blue Packet", 300, 20, 3),
        ]);
        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::MoveLast);
        app_state.torrents.remove(&hash(2));

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::ToggleCurrentSelection,
        );

        assert!(app_state
            .ui
            .torrent_management
            .selected_hashes
            .contains(&hash(1)));
        assert_eq!(app_state.ui.torrent_management.selected_index, 0);
        assert_eq!(
            app_state.ui.torrent_management.cursor_hash.as_deref(),
            Some(hash(1).as_slice())
        );
    }

    #[test]
    fn submit_confirmation_enter_emits_staged_requests_and_marks_state() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Meadow Saga S01E01", 100, 10, 2),
            (hash(2), "Meadow Saga S01E02", 300, 20, 3),
        ]);
        app_state
            .ui
            .torrent_management
            .pending_commands
            .push(TorrentManagementPendingCommand {
                info_hash: hash(1),
                request: ControlRequest::Pause {
                    info_hash_hex: hex::encode(hash(1)),
                },
                state: TorrentControlState::Paused,
                delete_files: false,
            });
        app_state
            .ui
            .torrent_management
            .pending_commands
            .push(TorrentManagementPendingCommand {
                info_hash: hash(2),
                request: ControlRequest::Delete {
                    info_hash_hex: hex::encode(hash(2)),
                    delete_files: true,
                },
                state: TorrentControlState::Deleting,
                delete_files: true,
            });
        app_state
            .ui
            .torrent_management
            .selected_hashes
            .insert(hash(2));

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::ShowSubmitConfirmation,
        );
        assert!(app_state.ui.torrent_management.confirm_submit);
        assert_eq!(
            map_key_to_management_action(KeyCode::Char('Y'), &app_state),
            None
        );
        assert_eq!(
            map_key_to_management_action(KeyCode::Enter, &app_state),
            Some(TorrentManagementAction::SubmitPendingCommands)
        );

        let result = reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::SubmitPendingCommands,
        );

        assert!(!app_state.ui.torrent_management.confirm_submit);
        assert!(app_state.ui.torrent_management.pending_commands.is_empty());
        assert_eq!(result.effects.len(), 4);
        assert!(matches!(
            result.effects[0],
            TorrentManagementEffect::SubmitControlRequest(ControlRequest::Pause { .. })
        ));
        assert!(matches!(
            result.effects[2],
            TorrentManagementEffect::SubmitControlRequest(ControlRequest::Delete { .. })
        ));
        assert!(app_state.ui.torrent_management.selected_hashes.is_empty());
    }

    #[test]
    fn exiting_management_clears_pending_draft_and_filter() {
        let mut app_state =
            app_state_with_torrents(vec![(hash(1), "Meadow Saga S01E01", 100, 10, 2)]);
        app_state
            .ui
            .torrent_management
            .pending_commands
            .push(TorrentManagementPendingCommand {
                info_hash: hash(1),
                request: ControlRequest::Pause {
                    info_hash_hex: hex::encode(hash(1)),
                },
                state: TorrentControlState::Paused,
                delete_files: false,
            });
        app_state.ui.torrent_management.confirm_submit = true;
        app_state.ui.torrent_management.is_searching = true;
        app_state.ui.torrent_management.search_query = "meadow".to_string();
        app_state
            .ui
            .torrent_management
            .selected_hashes
            .insert(hash(1));

        let result =
            reduce_torrent_management_action(&mut app_state, TorrentManagementAction::ToNormal);

        assert!(matches!(
            result.effects.as_slice(),
            [TorrentManagementEffect::ToNormal]
        ));
        assert!(app_state.ui.torrent_management.pending_commands.is_empty());
        assert!(!app_state.ui.torrent_management.confirm_submit);
        assert!(!app_state.ui.torrent_management.is_searching);
        assert!(app_state.ui.torrent_management.search_query.is_empty());
        assert!(app_state.ui.torrent_management.selected_hashes.is_empty());
    }

    #[test]
    fn u_clears_pending_drafts_for_selected_rows() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Meadow Saga S01E01", 100, 10, 2),
            (hash(2), "Meadow Saga S01E02", 300, 20, 3),
        ]);
        app_state
            .ui
            .torrent_management
            .selected_hashes
            .insert(hash(2));
        app_state.ui.torrent_management.pending_commands = vec![
            TorrentManagementPendingCommand {
                info_hash: hash(1),
                request: ControlRequest::Pause {
                    info_hash_hex: hex::encode(hash(1)),
                },
                state: TorrentControlState::Paused,
                delete_files: false,
            },
            TorrentManagementPendingCommand {
                info_hash: hash(2),
                request: ControlRequest::Pause {
                    info_hash_hex: hex::encode(hash(2)),
                },
                state: TorrentControlState::Paused,
                delete_files: false,
            },
        ];
        app_state.ui.torrent_management.confirm_submit = true;

        assert_eq!(
            map_key_to_management_action(KeyCode::Char('u'), &app_state),
            Some(TorrentManagementAction::ClearPendingForTargets)
        );

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::ClearPendingForTargets,
        );

        assert_eq!(app_state.ui.torrent_management.pending_commands.len(), 1);
        assert_eq!(
            app_state.ui.torrent_management.pending_commands[0].info_hash,
            hash(1)
        );
        assert!(!app_state
            .ui
            .torrent_management
            .selected_hashes
            .contains(&hash(2)));
        assert!(app_state.ui.torrent_management.confirm_submit);
        assert_eq!(
            app_state
                .ui
                .torrent_management
                .review_cache
                .as_ref()
                .expect("refreshed review cache")
                .pause,
            vec!["Meadow Saga S01E01"]
        );
    }

    #[test]
    fn clearing_the_final_review_draft_closes_the_modal() {
        let mut app_state = app_state_with_torrents(vec![(hash(1), "Sample Packet", 100, 10, 2)]);
        app_state
            .ui
            .torrent_management
            .pending_commands
            .push(pause_command(1));
        app_state.ui.torrent_management.confirm_submit = true;

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::ClearPendingForTargets,
        );

        assert!(app_state.ui.torrent_management.pending_commands.is_empty());
        assert!(!app_state.ui.torrent_management.confirm_submit);
        assert_eq!(app_state.ui.torrent_management.review_scroll_offset, 0);
    }

    #[test]
    fn review_u_without_selection_clears_all_drafts() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Sample Packet One", 100, 10, 2),
            (hash(2), "Sample Packet Two", 300, 20, 3),
        ]);
        app_state.ui.torrent_management.pending_commands = vec![pause_command(1), pause_command(2)];
        app_state.ui.torrent_management.confirm_submit = true;

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::ClearPendingForTargets,
        );

        assert!(app_state.ui.torrent_management.pending_commands.is_empty());
        assert!(!app_state.ui.torrent_management.confirm_submit);
    }

    #[test]
    fn review_u_with_undrafted_selection_preserves_other_drafts() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Sample Packet One", 100, 10, 2),
            (hash(2), "Sample Packet Two", 300, 20, 3),
        ]);
        app_state.ui.torrent_management.pending_commands = vec![pause_command(1)];
        app_state
            .ui
            .torrent_management
            .selected_hashes
            .insert(hash(2));
        app_state.ui.torrent_management.confirm_submit = true;

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::ClearPendingForTargets,
        );

        assert_eq!(
            app_state.ui.torrent_management.pending_commands,
            vec![pause_command(1)]
        );
        assert!(app_state.ui.torrent_management.selected_hashes.is_empty());
        assert!(app_state.ui.torrent_management.confirm_submit);
        assert_eq!(
            app_state.ui.torrent_management.status_message.as_deref(),
            Some("Cleared 1 selection")
        );
    }

    #[test]
    fn review_u_clears_a_selected_draft_hidden_by_the_filter() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Amber Packet", 100, 10, 2),
            (hash(2), "Blue Packet", 300, 20, 3),
        ]);
        app_state.ui.torrent_management.pending_commands = vec![pause_command(1)];
        app_state
            .ui
            .torrent_management
            .selected_hashes
            .insert(hash(1));
        app_state.ui.torrent_management.search_query = "Blue".to_string();
        app_state.ui.torrent_management.confirm_submit = true;

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::ClearPendingForTargets,
        );

        assert!(app_state.ui.torrent_management.pending_commands.is_empty());
        assert!(app_state.ui.torrent_management.selected_hashes.is_empty());
        assert!(!app_state.ui.torrent_management.confirm_submit);
    }

    #[test]
    fn review_closes_when_its_last_live_draft_disappears() {
        let mut app_state = app_state_with_torrents(vec![(hash(1), "Sample Packet", 100, 10, 2)]);
        app_state.ui.torrent_management.pending_commands = vec![pause_command(1)];
        app_state.ui.torrent_management.confirm_submit = true;
        app_state.ui.torrent_management.review_scroll_offset = 4;
        app_state.torrents.remove(&hash(1));

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::ReviewScrollDown);

        assert!(app_state.ui.torrent_management.pending_commands.is_empty());
        assert!(!app_state.ui.torrent_management.confirm_submit);
        assert_eq!(app_state.ui.torrent_management.review_scroll_offset, 0);
    }

    #[test]
    fn space_toggles_selection_without_clearing_pending_draft() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Meadow Saga S01E01", 100, 10, 2),
            (hash(2), "Meadow Saga S01E02", 300, 20, 3),
        ]);
        app_state.ui.torrent_management.pending_commands = vec![TorrentManagementPendingCommand {
            info_hash: hash(1),
            request: ControlRequest::Pause {
                info_hash_hex: hex::encode(hash(1)),
            },
            state: TorrentControlState::Paused,
            delete_files: false,
        }];

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::ToggleCurrentSelection,
        );

        assert_eq!(app_state.ui.torrent_management.pending_commands.len(), 1);
        assert!(app_state
            .ui
            .torrent_management
            .selected_hashes
            .contains(&hash(1)));
    }

    #[test]
    fn repeated_same_management_action_clears_pending_draft_for_target() {
        let mut app_state =
            app_state_with_torrents(vec![(hash(1), "Meadow Saga S01E01", 100, 10, 2)]);
        app_state
            .ui
            .torrent_management
            .selected_hashes
            .insert(hash(1));

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::TogglePauseTargets,
        );
        assert_eq!(app_state.ui.torrent_management.pending_commands.len(), 1);

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::TogglePauseTargets,
        );

        assert!(app_state.ui.torrent_management.pending_commands.is_empty());
    }

    #[test]
    fn different_management_action_replaces_pending_draft_for_target() {
        let mut app_state =
            app_state_with_torrents(vec![(hash(1), "Meadow Saga S01E01", 100, 10, 2)]);
        app_state
            .ui
            .torrent_management
            .selected_hashes
            .insert(hash(1));

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::TogglePauseTargets,
        );
        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::StartDelete {
                delete_files: false,
            },
        );

        assert_eq!(app_state.ui.torrent_management.pending_commands.len(), 1);
        assert!(matches!(
            app_state.ui.torrent_management.pending_commands[0].request,
            ControlRequest::Delete {
                delete_files: false,
                ..
            }
        ));
    }

    #[test]
    fn pending_management_review_groups_split_commands_by_action() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Cinder Trails S01E01", 100, 10, 2),
            (hash(2), "Cinder Trails S01E02", 300, 20, 3),
            (hash(3), "Meadow Saga S01E01", 400, 30, 4),
        ]);
        app_state
            .torrents
            .get_mut(&hash(3))
            .expect("purge torrent")
            .latest_state
            .total_size = 2_500_000_000;
        app_state.ui.torrent_management.pending_commands = vec![
            TorrentManagementPendingCommand {
                request: ControlRequest::Pause {
                    info_hash_hex: hex::encode(hash(1)),
                },
                info_hash: hash(1),
                state: TorrentControlState::Paused,
                delete_files: false,
            },
            TorrentManagementPendingCommand {
                request: ControlRequest::Delete {
                    info_hash_hex: hex::encode(hash(2)),
                    delete_files: false,
                },
                info_hash: hash(2),
                state: TorrentControlState::Deleting,
                delete_files: false,
            },
            TorrentManagementPendingCommand {
                request: ControlRequest::Delete {
                    info_hash_hex: hex::encode(hash(3)),
                    delete_files: true,
                },
                info_hash: hash(3),
                state: TorrentControlState::Deleting,
                delete_files: true,
            },
        ];

        let groups = pending_management_review_groups(&app_state);

        assert_eq!(groups.pause, vec!["Cinder Trails S01E01"]);
        assert_eq!(groups.delete, vec!["Cinder Trails S01E02"]);
        assert_eq!(groups.purge, vec!["Meadow Saga S01E01"]);
        assert_eq!(
            pending_management_purge_total_bytes(&app_state),
            2_500_000_000
        );
        assert_eq!(
            format_gb(pending_management_purge_total_bytes(&app_state)),
            "2.50 GB"
        );
        assert!(groups.resume.is_empty());
    }

    #[test]
    fn delete_action_stages_delete_request_without_confirmation() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Cinder Trails S01E01", 100, 10, 2),
            (hash(2), "Cinder Trails S01E02", 300, 20, 3),
        ]);
        app_state
            .ui
            .torrent_management
            .selected_hashes
            .insert(hash(2));

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::StartDelete { delete_files: true },
        );

        assert!(!app_state.ui.torrent_management.confirm_submit);
        assert_eq!(app_state.ui.torrent_management.pending_commands.len(), 1);
        assert!(matches!(
            app_state.ui.torrent_management.pending_commands[0].request,
            ControlRequest::Delete {
                delete_files: true,
                ..
            }
        ));
        assert!(app_state
            .ui
            .torrent_management
            .selected_hashes
            .contains(&hash(2)));
    }

    #[test]
    fn management_status_and_compact_footer_are_rendered() {
        let mut app_state = app_state_with_torrents(vec![(hash(1), "Sample Packet", 100, 10, 2)]);
        app_state.ui.torrent_management.status_message = Some("Cleared 2 selections".to_string());

        let rendered = render_management_screen(&mut app_state, 45, 16);

        assert!(rendered.contains("Cleared 2 selections"));
        assert!(rendered.contains("[Esc]"));
        assert!(rendered.contains("[u]"));

        app_state.ui.torrent_management.status_message =
            Some("No selection or draft commands to clear".to_string());
        let long_status = render_management_screen(&mut app_state, 40, 16);
        assert!(long_status.contains("No selection"));
        assert!(long_status.contains("..."));

        app_state
            .ui
            .torrent_management
            .pending_commands
            .push(pause_command(1));
        let pending_footer = render_management_screen(&mut app_state, 80, 24);
        assert!(pending_footer.contains("[Y]"));
        assert!(pending_footer.contains("[d/D]"));

        let narrow_pending_footer = render_management_screen(&mut app_state, 40, 16);
        for key in ["[Esc]", "[Y]", "[u]", "[Space]", "[A]", "[p]", "[d/D]"] {
            assert!(
                narrow_pending_footer.contains(key),
                "missing compact action {key}"
            );
        }
    }

    #[test]
    fn compact_review_footer_keeps_clear_and_cancel_visible() {
        let mut app_state = app_state_with_torrents(vec![(hash(1), "Sample Packet", 100, 10, 2)]);
        app_state
            .ui
            .torrent_management
            .pending_commands
            .push(pause_command(1));
        app_state.ui.torrent_management.confirm_submit = true;

        let rendered = render_management_screen(&mut app_state, 45, 16);

        assert!(rendered.contains("[Esc]"));
        assert!(rendered.contains("[Enter]"));
        assert!(rendered.contains("[u]"));
        assert!(rendered.contains("1 queued"));
    }

    #[test]
    fn review_middle_truncation_preserves_distinct_name_suffixes() {
        let alpha = "Shared Review Prefix with Extra Words alpha.bin";
        let omega = "Shared Review Prefix with Extra Words omega.bin";
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), alpha, 100, 10, 2),
            (hash(2), omega, 300, 20, 3),
        ]);
        app_state.ui.torrent_management.pending_commands = vec![pause_command(1), pause_command(2)];
        app_state.ui.torrent_management.confirm_submit = true;

        let rendered = render_management_screen(&mut app_state, 40, 20);

        assert!(rendered.contains("alpha.bin"));
        assert!(rendered.contains("omega.bin"));
        assert!(rendered.contains('…'));
    }

    #[test]
    fn compact_review_preserves_purge_count_and_size() {
        let mut app_state = app_state_with_torrents(vec![(hash(1), "Sample Packet", 100, 10, 2)]);
        app_state
            .torrents
            .get_mut(&hash(1))
            .expect("torrent")
            .latest_state
            .total_size = 2_500_000_000;
        app_state.ui.torrent_management.pending_commands = vec![TorrentManagementPendingCommand {
            request: ControlRequest::Delete {
                info_hash_hex: hex::encode(hash(1)),
                delete_files: true,
            },
            info_hash: hash(1),
            state: TorrentControlState::Deleting,
            delete_files: true,
        }];
        app_state.ui.torrent_management.confirm_submit = true;

        let rendered = render_management_screen(&mut app_state, 40, 20);

        assert!(rendered.contains("PURGE: 1 Torrent (2.50 GB)"));
    }

    #[test]
    fn open_review_refreshes_purge_size_when_metadata_arrives() {
        let mut app_state =
            app_state_with_torrents(vec![(hash(1), "Pending Metadata Packet", 100, 10, 2)]);
        app_state.ui.torrent_management.pending_commands = vec![TorrentManagementPendingCommand {
            request: ControlRequest::Delete {
                info_hash_hex: hex::encode(hash(1)),
                delete_files: true,
            },
            info_hash: hash(1),
            state: TorrentControlState::Deleting,
            delete_files: true,
        }];
        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::ShowSubmitConfirmation,
        );
        assert!(app_state.ui.torrent_management.review_cache.is_some());

        app_state
            .torrents
            .get_mut(&hash(1))
            .expect("torrent")
            .latest_state
            .total_size = 2_500_000_000;

        let rendered = render_management_screen(&mut app_state, 40, 20);

        assert!(rendered.contains("PURGE: 1 Torrent (2.50 GB)"));
        assert!(app_state.ui.torrent_management.review_cache.is_some());
    }

    #[test]
    fn review_truncates_wide_unicode_by_terminal_width() {
        let wide_name = format!("{}alpha.bin", "界".repeat(24));
        let mut app_state =
            app_state_with_torrents(vec![(hash(1), wide_name.as_str(), 100, 10, 2)]);
        app_state.ui.torrent_management.pending_commands = vec![pause_command(1)];
        app_state.ui.torrent_management.confirm_submit = true;

        let truncated = truncate_middle_with_ellipsis(&wide_name, 24);
        assert!(terminal_text_width(&truncated) <= 24);
        assert!(truncated.ends_with("alpha.bin"));
        let rendered = render_management_screen(&mut app_state, 40, 20);
        assert!(rendered.contains("alpha.bin"));
        assert!(rendered.contains('…'));
    }

    #[test]
    fn review_omits_empty_groups_from_the_scroll_range() {
        let mut app_state = app_state_with_torrents(vec![(hash(1), "Sample Packet", 100, 10, 2)]);
        app_state.ui.torrent_management.pending_commands = vec![pause_command(1)];
        app_state.ui.torrent_management.confirm_submit = true;
        app_state.screen_area = Rect::new(0, 0, 45, 18);

        let groups = pending_management_review_groups(&app_state);
        let sections = pending_management_review_sections(&groups, 0);

        assert_eq!(sections.len(), 1);
        assert_eq!(pending_management_review_line_count(&sections), 2);
        assert_eq!(max_management_review_scroll_offset(&app_state), 0);
    }

    #[test]
    fn review_geometry_grows_monotonically_across_responsive_breakpoints() {
        let groups = TorrentManagementReviewCache {
            pause: vec!["Geometry Packet".to_string()],
            ..Default::default()
        };

        let mut previous_popup_width = 0;
        let mut previous_body_width = 0;
        for width in 1..=120 {
            let popup = management_review_popup_area(Rect::new(0, 0, width, 30), &groups);
            let body = management_review_regions(popup).body;
            assert!(popup.width >= previous_popup_width, "frame width {width}");
            assert!(body.width >= previous_body_width, "frame width {width}");
            previous_popup_width = popup.width;
            previous_body_width = body.width;
        }

        let mut previous_popup_height = 0;
        let mut previous_body_height = 0;
        for height in 1..=40 {
            let popup = management_review_popup_area(Rect::new(0, 0, 80, height), &groups);
            let body = management_review_regions(popup).body;
            assert!(
                popup.height >= previous_popup_height,
                "frame height {height}"
            );
            assert!(body.height >= previous_body_height, "frame height {height}");
            previous_popup_height = popup.height;
            previous_body_height = body.height;
        }
    }

    #[test]
    fn pause_only_review_avoids_early_overflow() {
        let names = (0..13)
            .map(|index| format!("Quiet Packet {index:02}"))
            .collect::<Vec<_>>();
        let torrents = names
            .iter()
            .enumerate()
            .map(|(index, name)| (hash(index as u8 + 1), name.as_str(), 100, 10, 2))
            .collect::<Vec<_>>();
        let mut app_state = app_state_with_torrents(torrents);
        app_state.ui.torrent_management.pending_commands = (1..=13).map(pause_command).collect();
        app_state.ui.torrent_management.confirm_submit = true;

        let rendered = render_management_screen(&mut app_state, 100, 30);

        assert_eq!(max_management_review_scroll_offset(&app_state), 0);
        assert!(rendered.contains("13 queued"));
        assert!(rendered.contains("PAUSE: 13 Torrents"));
        assert!(!rendered.contains("RESUME:"));
        assert!(!rendered.contains("REMOVE:"));
        assert!(!rendered.contains("PURGE:"));
        assert!(rendered.contains("All 13 queued changes visible"));
    }

    #[test]
    fn review_scrolling_reuses_the_cached_grouped_names() {
        let names = (0..20)
            .map(|index| format!("Cached Packet {index:02}"))
            .collect::<Vec<_>>();
        let torrents = names
            .iter()
            .enumerate()
            .map(|(index, name)| (hash(index as u8 + 1), name.as_str(), 100, 10, 2))
            .collect::<Vec<_>>();
        let mut app_state = app_state_with_torrents(torrents);
        app_state.ui.torrent_management.pending_commands = (1..=20).map(pause_command).collect();
        app_state.screen_area = Rect::new(0, 0, 80, 16);

        reduce_torrent_management_action(
            &mut app_state,
            TorrentManagementAction::ShowSubmitConfirmation,
        );
        app_state
            .ui
            .torrent_management
            .review_cache
            .as_mut()
            .expect("review cache")
            .longest_line_width = usize::MAX;

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::ReviewScrollDown);

        assert_eq!(
            app_state
                .ui
                .torrent_management
                .review_cache
                .as_ref()
                .expect("review cache")
                .longest_line_width,
            usize::MAX
        );
    }

    #[test]
    fn large_review_batches_scroll_to_the_last_item_and_back() {
        let names = (0..20)
            .map(|index| format!("Batch Packet {index:02}"))
            .collect::<Vec<_>>();
        let torrents = names
            .iter()
            .enumerate()
            .map(|(index, name)| (hash(index as u8 + 1), name.as_str(), 100, 10, 2))
            .collect::<Vec<_>>();
        let mut app_state = app_state_with_torrents(torrents);
        app_state.ui.torrent_management.pending_commands = (1..=20).map(pause_command).collect();
        app_state.ui.torrent_management.confirm_submit = true;

        let first_page = render_management_screen(&mut app_state, 100, 30);
        assert!(max_management_review_scroll_offset(&app_state) > 0);
        assert!(first_page.contains("Batch Packet 00"));
        assert!(!first_page.contains("Batch Packet 19"));
        assert!(first_page.contains("[j/k]"));

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::ReviewLast);
        let last_page = render_management_screen(&mut app_state, 100, 30);
        assert!(last_page.contains("Batch Packet 19"));
        assert!(app_state.ui.torrent_management.review_scroll_offset > 0);
        let groups = pending_management_review_groups(&app_state);
        let body = management_review_body_area(app_state.screen_area, &groups);
        let scrollbar_x = body.x.saturating_add(body.width.saturating_sub(1)) as usize;
        let scrollbar_y = body.y.saturating_add(body.height.saturating_sub(2)) as usize;
        assert_eq!(
            last_page
                .lines()
                .nth(scrollbar_y)
                .and_then(|line| line.chars().nth(scrollbar_x)),
            Some('█')
        );

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::ReviewFirst);
        assert_eq!(app_state.ui.torrent_management.review_scroll_offset, 0);
    }

    #[test]
    fn mixed_large_review_keeps_summary_and_last_destructive_item_visible() {
        let names = (0..40)
            .map(|index| format!("Review Packet {index:02}"))
            .collect::<Vec<_>>();
        let torrents = names
            .iter()
            .enumerate()
            .map(|(index, name)| (hash(index as u8 + 1), name.as_str(), 100, 10, 2))
            .collect::<Vec<_>>();
        let mut app_state = app_state_with_torrents(torrents);
        for byte in 31..=40 {
            app_state
                .torrents
                .get_mut(&hash(byte))
                .expect("purge torrent")
                .latest_state
                .total_size = 250_000_000;
        }
        app_state.ui.torrent_management.pending_commands = (1..=20)
            .map(pause_command)
            .chain((21..=30).map(|byte| delete_command(byte, false)))
            .chain((31..=40).map(|byte| delete_command(byte, true)))
            .collect();
        app_state.ui.torrent_management.confirm_submit = true;

        let first_page = render_management_screen(&mut app_state, 80, 24);
        assert!(first_page.contains("40 queued"));
        assert!(first_page.contains("Purge 10"));
        assert!(first_page.contains("Review Packet 00"));
        assert!(!first_page.contains("Review Packet 39"));

        let compact_first_page = render_management_screen(&mut app_state, 32, 16);
        assert!(compact_first_page.contains("PURGE 10"));
        assert!(compact_first_page.contains("files"));
        assert!(compact_first_page.contains("2.50 GB"));
        assert!(compact_first_page.contains("[Enter]"));

        let short_first_page = render_management_screen(&mut app_state, 32, 7);
        assert!(short_first_page.contains("PURGE 10"));
        assert!(short_first_page.contains("FILES"));
        assert!(short_first_page.contains("2.50 GB"));
        assert!(short_first_page.contains("[Enter]"));

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::ReviewLast);
        let last_page = render_management_screen(&mut app_state, 80, 24);
        assert!(last_page.contains("Review Packet 39"));
        assert!(last_page.contains("PURGE: 10 Torrents"));
        assert!(last_page.contains("[Enter]"));
        assert!(last_page.contains("[Esc]"));
    }

    #[test]
    fn narrow_review_keeps_confirmation_and_scroll_position_visible() {
        let names = (0..200)
            .map(|index| format!("Narrow Packet {index:02}"))
            .collect::<Vec<_>>();
        let torrents = names
            .iter()
            .enumerate()
            .map(|(index, name)| (hash(index as u8 + 1), name.as_str(), 100, 10, 2))
            .collect::<Vec<_>>();
        let mut app_state = app_state_with_torrents(torrents);
        app_state.ui.torrent_management.pending_commands = (1..=200).map(pause_command).collect();
        app_state.ui.torrent_management.confirm_submit = true;

        let rendered = render_management_screen(&mut app_state, 32, 16);

        assert!(rendered.contains("200 queued"));
        assert!(rendered.contains("[Esc]"));
        assert!(rendered.contains("[u]"));
        assert!(rendered.contains("[Enter]"));
        assert!(rendered.contains('↕'));
    }

    #[test]
    fn short_review_reserves_at_least_one_body_row() {
        let names = (0..20)
            .map(|index| format!("Short Packet {index:02}"))
            .collect::<Vec<_>>();
        let torrents = names
            .iter()
            .enumerate()
            .map(|(index, name)| (hash(index as u8 + 1), name.as_str(), 100, 10, 2))
            .collect::<Vec<_>>();
        let mut app_state = app_state_with_torrents(torrents);
        app_state.ui.torrent_management.pending_commands = (1..=20).map(pause_command).collect();
        app_state.ui.torrent_management.confirm_submit = true;
        let groups = pending_management_review_groups(&app_state);

        assert!(management_review_body_area(Rect::new(0, 0, 80, 7), &groups).height >= 1);
        assert_eq!(
            management_review_body_area(Rect::new(0, 0, 80, 7), &groups).height,
            management_review_body_height(Rect::new(0, 0, 80, 7))
        );
        let rendered = render_management_screen(&mut app_state, 80, 7);
        assert!(rendered.contains("20 queued"));
        assert!(rendered.contains("PAUSE: 20 Torrents"));
        assert!(rendered.contains("[Enter]"));
    }

    #[test]
    fn very_narrow_review_truncates_unicode_without_losing_the_suffix() {
        let wide_name = format!("{}final.part", "界".repeat(24));
        let mut app_state =
            app_state_with_torrents(vec![(hash(1), wide_name.as_str(), 100, 10, 2)]);
        app_state.ui.torrent_management.pending_commands = vec![pause_command(1)];
        app_state.ui.torrent_management.confirm_submit = true;

        let rendered = render_management_screen(&mut app_state, 32, 16);

        assert!(rendered.contains("final.part"));
        assert!(rendered.contains('…'));
        assert!(rendered.contains("[Enter]"));
    }

    #[test]
    fn review_viewport_supports_offsets_beyond_u16() {
        let names = (0..65_540)
            .map(|index| format!("Deep Queue Packet {index:05}"))
            .collect::<Vec<_>>();
        let groups = TorrentManagementReviewCache {
            pause: names,
            ..Default::default()
        };
        let sections = pending_management_review_sections(&groups, 0);
        let ctx = ThemeContext::new(Default::default(), 0.0);

        let visible = pending_management_review_visible_lines(&sections, 65_540, 1, 40, &ctx);
        assert_eq!(visible.len(), 1);
        let rendered = visible[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(rendered.contains("Deep Queue Packet 65539"));
    }

    #[test]
    fn review_scroll_up_uses_the_resized_viewport_range() {
        let names = (0..20)
            .map(|index| format!("Resize Packet {index:02}"))
            .collect::<Vec<_>>();
        let torrents = names
            .iter()
            .enumerate()
            .map(|(index, name)| (hash(index as u8 + 1), name.as_str(), 100, 10, 2))
            .collect::<Vec<_>>();
        let mut app_state = app_state_with_torrents(torrents);
        app_state.ui.torrent_management.pending_commands = (1..=20).map(pause_command).collect();
        app_state.ui.torrent_management.confirm_submit = true;
        app_state.screen_area = Rect::new(0, 0, 100, 16);
        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::ReviewLast);
        let old_max = app_state.ui.torrent_management.review_scroll_offset;

        app_state.screen_area = Rect::new(0, 0, 100, 24);
        let resized_max = max_management_review_scroll_offset(&app_state);
        assert!(resized_max > 0);
        assert!(resized_max < old_max);

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::ReviewScrollUp);

        assert_eq!(
            app_state.ui.torrent_management.review_scroll_offset,
            resized_max - 1
        );

        reduce_torrent_management_action(&mut app_state, TorrentManagementAction::ReviewLast);
        let resized_last_page = render_management_screen(&mut app_state, 100, 24);
        assert!(resized_last_page.contains("Resize Packet 19"));
    }

    #[test]
    fn pending_management_summary_counts_draft_actions() {
        let mut app_state = app_state_with_torrents(vec![
            (hash(1), "Cinder Trails S01E01", 100, 10, 2),
            (hash(2), "Cinder Trails S01E02", 300, 20, 3),
            (hash(3), "Meadow Saga S01E01", 100, 10, 2),
            (hash(4), "Meadow Saga S01E02", 300, 20, 3),
        ]);
        app_state.ui.torrent_management.pending_commands = vec![
            TorrentManagementPendingCommand {
                info_hash: hash(1),
                request: ControlRequest::Pause {
                    info_hash_hex: hex::encode(hash(1)),
                },
                state: TorrentControlState::Paused,
                delete_files: false,
            },
            TorrentManagementPendingCommand {
                info_hash: hash(2),
                request: ControlRequest::Resume {
                    info_hash_hex: hex::encode(hash(2)),
                },
                state: TorrentControlState::Running,
                delete_files: false,
            },
            TorrentManagementPendingCommand {
                info_hash: hash(3),
                request: ControlRequest::Delete {
                    info_hash_hex: hex::encode(hash(3)),
                    delete_files: false,
                },
                state: TorrentControlState::Deleting,
                delete_files: false,
            },
            TorrentManagementPendingCommand {
                info_hash: hash(4),
                request: ControlRequest::Delete {
                    info_hash_hex: hex::encode(hash(4)),
                    delete_files: true,
                },
                state: TorrentControlState::Deleting,
                delete_files: true,
            },
        ];

        let summary = pending_management_summary(&app_state);

        assert_eq!(summary.pause_count, 1);
        assert_eq!(summary.resume_count, 1);
        assert_eq!(summary.remove_count, 1);
        assert_eq!(summary.purge_count, 1);
        let groups = pending_management_review_groups(&app_state);
        let sections = pending_management_review_sections(&groups, 0);
        assert_eq!(
            pending_management_review_summary_line_count(&summary),
            pending_management_review_line_count(&sections)
        );
    }
}
