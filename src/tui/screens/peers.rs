// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{AppMode, AppState, PeerManagementFilter, SearchMode};
use crate::config::SortDirection;
use crate::peer_manager::{PeerManagerTrackedPeer, PeerRestriction, PeerRestrictionReason};
use crate::theme::ThemeContext;
use crate::tui::action_style::{footer_key_style, ActionTone};
use crate::tui::formatters::{
    anonymize_preserving_shape, format_bytes, sanitize_text, truncate_with_ellipsis,
};
use crate::tui::layout::common::{compute_smart_table_layout, SmartCol};
use crate::tui::layout::peers::{calculate_peer_screen_layout, PeerBodyLayout};
use crate::tui::screen_context::ScreenContext;
use crate::tui::screens::input_panel::draw_prompt_panel;
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use ratatui::crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::prelude::{Color, Frame, Line, Modifier, Span, Style};
use ratatui::widgets::{Block, Borders, Cell, Clear, Padding, Paragraph, Row, Table, TableState};
use regex::RegexBuilder;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::time::{Duration, SystemTime};

#[cfg(test)]
std::thread_local! {
    static PEER_DERIVED_RECOMPUTE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerManagementAction {
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
    FilterNext,
    FilterPrev,
    StartSearch,
    SearchInsert(char),
    SearchBackspace,
    SearchCommit,
    SearchCancel,
    ToggleSearchMode,
    TogglePrivacy,
    ToggleDetails,
    CloseDetails,
    ScrollDetailsUp,
    ScrollDetailsDown,
    ScrollDetailsPageUp,
    ScrollDetailsPageDown,
    StartDetailsSearch,
    DetailsSearchInsert(char),
    DetailsSearchBackspace,
    DetailsSearchCommit,
    DetailsSearchCancel,
    ToggleDetailsSearchMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerManagementEffect {
    ToNormal,
}

#[derive(Default)]
pub struct PeerManagementReduceResult {
    pub redraw: bool,
    pub effects: Vec<PeerManagementEffect>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerColumnId {
    State,
    Address,
    Torrents,
    Client,
    Connects,
    Disconnects,
    Downloaded,
    Uploaded,
    Evidence,
    LastSeen,
}

const STATE_COLUMN_WIDTH: u16 = 18;
const TORRENTS_COLUMN_WIDTH: u16 = 10;
const CONNECTS_COLUMN_WIDTH: u16 = 10;
const DISCONNECTS_COLUMN_WIDTH: u16 = 13;
const TRANSFER_COLUMN_WIDTH: u16 = 11;
const EVIDENCE_COLUMN_WIDTH: u16 = 18;
const EVIDENCE_CONTENT_WIDTH: u16 = EVIDENCE_COLUMN_WIDTH - 1;
const LAST_SEEN_COLUMN_WIDTH: u16 = 12;
const RESTRICTION_REMAINING_WIDTH: u16 = 12;

#[derive(Clone, Debug)]
struct PeerColumnDefinition {
    id: PeerColumnId,
    header: &'static str,
    min_width: u16,
    priority: u8,
    constraint: Constraint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum EvidenceKind {
    Upload,
    Download,
    Reconnect,
    Manual,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PeerEvidence {
    kind: EvidenceKind,
    observed: u64,
    threshold: u64,
    from_policy: bool,
}

impl PeerEvidence {
    fn compact_label(&self) -> String {
        let label = match self.kind {
            EvidenceKind::Upload => format!("UL {:.0}%", self.percent()),
            EvidenceKind::Download => format!("DL {:.0}%", self.percent()),
            EvidenceKind::Reconnect => {
                format!("Reconnect {}/{}", self.observed, self.threshold)
            }
            EvidenceKind::Manual => "MANUAL".to_string(),
        };
        if fits_column(&label, EVIDENCE_CONTENT_WIDTH) {
            return label;
        }

        let compact = match self.kind {
            EvidenceKind::Upload => format!("UL {}%", compact_magnitude(self.percent())),
            EvidenceKind::Download => format!("DL {}%", compact_magnitude(self.percent())),
            EvidenceKind::Reconnect => format!(
                "R {}/{}",
                compact_count(self.observed),
                compact_count(self.threshold)
            ),
            EvidenceKind::Manual => label,
        };
        truncate_with_ellipsis(&compact, usize::from(EVIDENCE_CONTENT_WIDTH))
    }

    fn percent(&self) -> f64 {
        if self.threshold == 0 {
            0.0
        } else {
            self.observed as f64 * 100.0 / self.threshold as f64
        }
    }
}

fn fits_column(label: &str, width: u16) -> bool {
    label.chars().count() <= usize::from(width)
}

fn compact_count(value: u64) -> String {
    const UNITS: [(u64, &str); 6] = [
        (1_000_000_000_000_000_000, "E"),
        (1_000_000_000_000_000, "P"),
        (1_000_000_000_000, "T"),
        (1_000_000_000, "G"),
        (1_000_000, "M"),
        (1_000, "K"),
    ];
    for (threshold, unit) in UNITS {
        if value >= threshold {
            return format_compact_scaled(value as f64 / threshold as f64, unit);
        }
    }
    value.to_string()
}

fn compact_transfer_bytes(bytes: u64) -> String {
    const UNITS: [(u64, &str); 6] = [
        (1 << 60, "EB"),
        (1 << 50, "PB"),
        (1 << 40, "TB"),
        (1 << 30, "GB"),
        (1 << 20, "MB"),
        (1 << 10, "KB"),
    ];
    for (threshold, unit) in UNITS {
        if bytes >= threshold {
            return format!("{:.2} {unit}", bytes as f64 / threshold as f64);
        }
    }
    format!("{bytes} B")
}

fn compact_magnitude(value: f64) -> String {
    const UNITS: [(f64, &str); 8] = [
        (1e24, "Y"),
        (1e21, "Z"),
        (1e18, "E"),
        (1e15, "P"),
        (1e12, "T"),
        (1e9, "G"),
        (1e6, "M"),
        (1e3, "K"),
    ];
    for (threshold, unit) in UNITS {
        if value >= threshold {
            return format_compact_scaled(value / threshold, unit);
        }
    }
    format!("{value:.0}")
}

fn format_compact_scaled(value: f64, unit: &str) -> String {
    if value < 10.0 {
        format!("{value:.1}{unit}")
    } else {
        format!("{value:.0}{unit}")
    }
}

#[derive(Clone, Debug)]
struct PeerRowModel {
    ip: IpAddr,
    tracked_indices: Vec<usize>,
    restriction: Option<PeerRestriction>,
    torrent_count: usize,
    is_active: bool,
    last_seen: Option<SystemTime>,
    strongest_evidence: PeerEvidence,
    client_label: String,
    connection_count: u64,
    disconnect_count: u64,
    total_downloaded_bytes: u64,
    total_uploaded_bytes: u64,
}

impl PeerRowModel {
    fn is_active(&self) -> bool {
        self.is_active
    }

    fn is_restricted(&self) -> bool {
        self.restriction.is_some()
    }

    fn state_label(&self) -> &'static str {
        if self.is_restricted() {
            "BLOCKED"
        } else if self.is_active() {
            "ACTIVE"
        } else {
            "RECENT"
        }
    }

    fn state_column_label(&self, now: SystemTime) -> String {
        let Some(restriction) = &self.restriction else {
            return self.state_label().to_string();
        };
        let remaining = restriction
            .blocked_until
            .duration_since(now)
            .unwrap_or_default();
        if remaining.is_zero() {
            return "BLOCKED expired".to_string();
        }
        let compact = compact_duration(remaining);
        let label = format!("BLOCKED {compact}");
        if fits_column(&label, STATE_COLUMN_WIDTH) {
            label
        } else {
            "BLOCKED >999d".to_string()
        }
    }

    fn state_sort_rank(&self) -> u8 {
        if self.is_restricted() {
            2
        } else if self.is_active() {
            1
        } else {
            0
        }
    }

    fn last_seen(&self) -> Option<SystemTime> {
        self.last_seen
    }

    fn tracked<'a>(&self, app_state: &'a AppState) -> Vec<&'a PeerManagerTrackedPeer> {
        self.tracked_indices
            .iter()
            .filter_map(|index| app_state.peer_manager_view.tracked_peers.get(*index))
            .collect()
    }

    fn torrent_count(&self, tracked: &[&PeerManagerTrackedPeer]) -> usize {
        let mut hashes = tracked
            .iter()
            .map(|peer| peer.torrent_info_hash.as_slice())
            .collect::<BTreeSet<_>>();
        if let Some(hash) = self
            .restriction
            .as_ref()
            .and_then(|restriction| restriction.torrent_info_hash.as_deref())
        {
            hashes.insert(hash);
        }
        hashes.len()
    }

    fn endpoint_count(&self, app_state: &AppState) -> usize {
        self.tracked(app_state)
            .into_iter()
            .flat_map(|peer| peer.endpoints.iter().map(|endpoint| &endpoint.address))
            .collect::<BTreeSet<_>>()
            .len()
    }

    fn strongest_evidence(&self) -> &PeerEvidence {
        &self.strongest_evidence
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PeerManagementDerivedState {
    rows: Vec<PeerRowModel>,
    next_restriction_expiry: Option<SystemTime>,
}

fn strongest_peer_evidence(
    tracked: &[&PeerManagerTrackedPeer],
    restriction: Option<&PeerRestriction>,
) -> PeerEvidence {
    tracked
        .iter()
        .flat_map(|peer| tracked_peer_evidence(peer))
        .chain(restriction.map(|restriction| restriction_evidence(&restriction.reason)))
        .max_by(compare_evidence)
        .unwrap_or(PeerEvidence {
            kind: EvidenceKind::Reconnect,
            observed: 0,
            threshold: 0,
            from_policy: false,
        })
}

fn peer_client_label(tracked: &[&PeerManagerTrackedPeer]) -> String {
    preferred_client_label(
        tracked
            .iter()
            .flat_map(|peer| peer.clients.iter().map(String::as_str)),
    )
}

fn preferred_client_label<'a>(clients: impl Iterator<Item = &'a str>) -> String {
    let clients = clients.collect::<BTreeSet<_>>();
    if clients.is_empty() {
        return "Unknown".to_string();
    }

    let resolved = clients
        .iter()
        .copied()
        .filter(|client| !is_unknown_client_label(client))
        .collect::<Vec<_>>();
    if resolved.is_empty() {
        clients.into_iter().collect::<Vec<_>>().join(", ")
    } else {
        resolved.join(", ")
    }
}

fn is_unknown_client_label(client: &str) -> bool {
    client == "Unknown" || client.starts_with("Unknown (")
}

pub fn handle_event(event: CrosstermEvent, app_state: &mut AppState) {
    if !matches!(app_state.mode, AppMode::PeerManagement) {
        return;
    }

    let CrosstermEvent::Key(key) = event else {
        return;
    };
    let Some(action) = map_key_event_to_peer_management_action(key, app_state) else {
        return;
    };
    let result = reduce_peer_management_action(app_state, action);
    if result.redraw {
        app_state.ui.needs_redraw = true;
    }
    execute_peer_management_effects(app_state, result.effects);
}

fn map_key_event_to_peer_management_action(
    key: KeyEvent,
    app_state: &AppState,
) -> Option<PeerManagementAction> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }

    let action = map_key_to_peer_management_action(key.code, app_state)?;
    if matches!(key.kind, KeyEventKind::Repeat)
        && (!peer_management_action_allows_repeat(&action)
            || matches!(
                action,
                PeerManagementAction::SearchInsert('/')
                    | PeerManagementAction::DetailsSearchInsert('/')
            ))
    {
        return None;
    }
    Some(action)
}

fn peer_management_action_allows_repeat(action: &PeerManagementAction) -> bool {
    matches!(
        action,
        PeerManagementAction::MoveUp
            | PeerManagementAction::MoveDown
            | PeerManagementAction::MovePageUp
            | PeerManagementAction::MovePageDown
            | PeerManagementAction::MoveFirst
            | PeerManagementAction::MoveLast
            | PeerManagementAction::MoveColumnLeft
            | PeerManagementAction::MoveColumnRight
            | PeerManagementAction::SearchInsert(_)
            | PeerManagementAction::SearchBackspace
            | PeerManagementAction::ScrollDetailsUp
            | PeerManagementAction::ScrollDetailsDown
            | PeerManagementAction::ScrollDetailsPageUp
            | PeerManagementAction::ScrollDetailsPageDown
            | PeerManagementAction::DetailsSearchInsert(_)
            | PeerManagementAction::DetailsSearchBackspace
    )
}

fn map_key_to_peer_management_action(
    key_code: KeyCode,
    app_state: &AppState,
) -> Option<PeerManagementAction> {
    if app_state.ui.peer_management.details_is_searching {
        return match key_code {
            KeyCode::Esc => Some(PeerManagementAction::DetailsSearchCancel),
            KeyCode::Enter => Some(PeerManagementAction::DetailsSearchCommit),
            KeyCode::Tab => Some(PeerManagementAction::ToggleDetailsSearchMode),
            KeyCode::Backspace => Some(PeerManagementAction::DetailsSearchBackspace),
            KeyCode::Char(c) => Some(PeerManagementAction::DetailsSearchInsert(c)),
            _ => None,
        };
    }

    if app_state.ui.peer_management.is_searching {
        return match key_code {
            KeyCode::Esc => Some(PeerManagementAction::SearchCancel),
            KeyCode::Enter => Some(PeerManagementAction::SearchCommit),
            KeyCode::Tab => Some(PeerManagementAction::ToggleSearchMode),
            KeyCode::Backspace => Some(PeerManagementAction::SearchBackspace),
            KeyCode::Char(c) => Some(PeerManagementAction::SearchInsert(c)),
            _ => None,
        };
    }

    if peer_details_overlay_active(app_state)
        && peer_details_search_panel_active(app_state)
        && matches!(key_code, KeyCode::Esc)
    {
        return Some(PeerManagementAction::DetailsSearchCancel);
    }

    if peer_details_overlay_active(app_state)
        && peer_details_search_panel_active(app_state)
        && matches!(key_code, KeyCode::Tab)
    {
        return Some(PeerManagementAction::ToggleDetailsSearchMode);
    }

    if peer_table_search_panel_active(app_state) && matches!(key_code, KeyCode::Esc) {
        return Some(PeerManagementAction::SearchCancel);
    }

    if peer_table_search_panel_active(app_state) && matches!(key_code, KeyCode::Tab) {
        return Some(PeerManagementAction::ToggleSearchMode);
    }

    if peer_details_overlay_active(app_state) {
        return match key_code {
            KeyCode::Esc | KeyCode::Enter => Some(PeerManagementAction::CloseDetails),
            KeyCode::Char('q') => Some(PeerManagementAction::ToNormal),
            KeyCode::Up | KeyCode::Char('k') => Some(PeerManagementAction::ScrollDetailsUp),
            KeyCode::Down | KeyCode::Char('j') => Some(PeerManagementAction::ScrollDetailsDown),
            KeyCode::PageUp => Some(PeerManagementAction::ScrollDetailsPageUp),
            KeyCode::PageDown => Some(PeerManagementAction::ScrollDetailsPageDown),
            KeyCode::Char('/') => Some(PeerManagementAction::StartDetailsSearch),
            _ => None,
        };
    }

    match key_code {
        KeyCode::Char('q') => Some(PeerManagementAction::ToNormal),
        KeyCode::Esc => Some(PeerManagementAction::ToNormal),
        KeyCode::Up | KeyCode::Char('k') => Some(PeerManagementAction::MoveUp),
        KeyCode::Down | KeyCode::Char('j') => Some(PeerManagementAction::MoveDown),
        KeyCode::PageUp => Some(PeerManagementAction::MovePageUp),
        KeyCode::PageDown => Some(PeerManagementAction::MovePageDown),
        KeyCode::Home => Some(PeerManagementAction::MoveFirst),
        KeyCode::End => Some(PeerManagementAction::MoveLast),
        KeyCode::Left | KeyCode::Char('h') => Some(PeerManagementAction::MoveColumnLeft),
        KeyCode::Right | KeyCode::Char('l') => Some(PeerManagementAction::MoveColumnRight),
        KeyCode::Char('s') => Some(PeerManagementAction::SortBySelectedColumn),
        KeyCode::Tab => Some(PeerManagementAction::FilterNext),
        KeyCode::BackTab => Some(PeerManagementAction::FilterPrev),
        KeyCode::Char('/') => Some(PeerManagementAction::StartSearch),
        KeyCode::Char('x') => Some(PeerManagementAction::TogglePrivacy),
        KeyCode::Enter if peer_uses_details_overlay(app_state) => {
            Some(PeerManagementAction::ToggleDetails)
        }
        _ => None,
    }
}

pub fn reduce_peer_management_action(
    app_state: &mut AppState,
    action: PeerManagementAction,
) -> PeerManagementReduceResult {
    let now = SystemTime::now();
    app_state.ui.peer_management.status_message = None;
    let mut result = PeerManagementReduceResult {
        redraw: true,
        effects: Vec::new(),
    };
    let mut recompute_derived = false;

    match action {
        PeerManagementAction::ToNormal => {
            app_state.ui.peer_management.is_searching = false;
            app_state.ui.peer_management.search_query.clear();
            app_state.ui.peer_management.show_details = false;
            app_state.ui.peer_management.details_peer_ip = None;
            app_state.ui.peer_management.details_scroll_offset = 0;
            app_state.ui.peer_management.details_is_searching = false;
            app_state.ui.peer_management.details_search_query.clear();
            result.effects.push(PeerManagementEffect::ToNormal);
        }
        PeerManagementAction::MoveUp => {
            let row_count = app_state.peer_management_derived.rows.len();
            move_peer_selection(app_state, row_count, -1);
        }
        PeerManagementAction::MoveDown => {
            let row_count = app_state.peer_management_derived.rows.len();
            move_peer_selection(app_state, row_count, 1);
        }
        PeerManagementAction::MovePageUp => {
            let row_count = app_state.peer_management_derived.rows.len();
            move_peer_selection(app_state, row_count, -(peer_page_rows(app_state) as isize));
        }
        PeerManagementAction::MovePageDown => {
            let row_count = app_state.peer_management_derived.rows.len();
            move_peer_selection(app_state, row_count, peer_page_rows(app_state) as isize);
        }
        PeerManagementAction::MoveFirst => {
            let row_count = app_state.peer_management_derived.rows.len();
            select_peer_index(app_state, row_count, 0);
        }
        PeerManagementAction::MoveLast => {
            let row_count = app_state.peer_management_derived.rows.len();
            select_peer_index(app_state, row_count, row_count.saturating_sub(1));
        }
        PeerManagementAction::MoveColumnLeft => move_peer_column(app_state, -1),
        PeerManagementAction::MoveColumnRight => move_peer_column(app_state, 1),
        PeerManagementAction::SortBySelectedColumn => {
            let selected = normalized_selected_peer_column_index(app_state);
            app_state.ui.peer_management.selected_column_index = selected;
            if app_state.ui.peer_management.sort_column_index == Some(selected) {
                app_state.ui.peer_management.sort_direction =
                    reverse_sort_direction(app_state.ui.peer_management.sort_direction);
            } else {
                app_state.ui.peer_management.sort_column_index = Some(selected);
                app_state.ui.peer_management.sort_direction =
                    peer_column_default_direction(peer_columns()[selected].id);
            }
            recompute_derived = true;
        }
        PeerManagementAction::FilterNext => {
            app_state.ui.peer_management.filter = app_state.ui.peer_management.filter.next();
            reset_peer_selection(app_state);
            recompute_derived = true;
        }
        PeerManagementAction::FilterPrev => {
            app_state.ui.peer_management.filter = app_state.ui.peer_management.filter.prev();
            reset_peer_selection(app_state);
            recompute_derived = true;
        }
        PeerManagementAction::StartSearch => {
            app_state.ui.peer_management.is_searching = true;
        }
        PeerManagementAction::SearchInsert(c) => {
            app_state.ui.peer_management.search_query.push(c);
            reset_peer_selection(app_state);
            recompute_derived = true;
        }
        PeerManagementAction::SearchBackspace => {
            app_state.ui.peer_management.search_query.pop();
            reset_peer_selection(app_state);
            recompute_derived = true;
        }
        PeerManagementAction::SearchCommit => {
            app_state.ui.peer_management.is_searching = false;
        }
        PeerManagementAction::SearchCancel => {
            app_state.ui.peer_management.is_searching = false;
            app_state.ui.peer_management.search_query.clear();
            reset_peer_selection(app_state);
            recompute_derived = true;
        }
        PeerManagementAction::ToggleSearchMode => {
            app_state.ui.peer_management.search_mode =
                match app_state.ui.peer_management.search_mode {
                    SearchMode::Fuzzy => SearchMode::Regex,
                    SearchMode::Regex => SearchMode::Fuzzy,
                };
            reset_peer_selection(app_state);
            recompute_derived = true;
        }
        PeerManagementAction::TogglePrivacy => {
            let enabling_privacy = !app_state.anonymize_torrent_names;
            app_state.anonymize_torrent_names = enabling_privacy;
            if enabling_privacy {
                app_state.ui.peer_management.is_searching = false;
                app_state.ui.peer_management.search_query.clear();
                reset_peer_selection(app_state);
            }
            recompute_derived = true;
        }
        PeerManagementAction::ToggleDetails => {
            if app_state.ui.peer_management.show_details {
                app_state.ui.peer_management.show_details = false;
                app_state.ui.peer_management.details_peer_ip = None;
            } else {
                app_state.ui.peer_management.details_peer_ip =
                    selected_peer_row(app_state, &app_state.peer_management_derived.rows)
                        .map(|row| row.ip);
                app_state.ui.peer_management.show_details =
                    app_state.ui.peer_management.details_peer_ip.is_some();
            }
            app_state.ui.peer_management.details_scroll_offset = 0;
            app_state.ui.peer_management.details_is_searching = false;
            app_state.ui.peer_management.details_search_query.clear();
        }
        PeerManagementAction::CloseDetails => {
            app_state.ui.peer_management.show_details = false;
            app_state.ui.peer_management.details_peer_ip = None;
            app_state.ui.peer_management.details_scroll_offset = 0;
            app_state.ui.peer_management.details_is_searching = false;
            app_state.ui.peer_management.details_search_query.clear();
        }
        PeerManagementAction::ScrollDetailsUp => {
            let max_scroll = peer_details_max_scroll_at(app_state, now);
            app_state.ui.peer_management.details_scroll_offset = app_state
                .ui
                .peer_management
                .details_scroll_offset
                .min(max_scroll)
                .saturating_sub(1);
        }
        PeerManagementAction::ScrollDetailsDown => {
            let max_scroll = peer_details_max_scroll_at(app_state, now);
            app_state.ui.peer_management.details_scroll_offset = app_state
                .ui
                .peer_management
                .details_scroll_offset
                .min(max_scroll)
                .saturating_add(1)
                .min(max_scroll);
        }
        PeerManagementAction::ScrollDetailsPageUp => {
            let page = peer_details_page_rows(app_state);
            let max_scroll = peer_details_max_scroll_at(app_state, now);
            app_state.ui.peer_management.details_scroll_offset = app_state
                .ui
                .peer_management
                .details_scroll_offset
                .min(max_scroll)
                .saturating_sub(page);
        }
        PeerManagementAction::ScrollDetailsPageDown => {
            let page = peer_details_page_rows(app_state);
            let max_scroll = peer_details_max_scroll_at(app_state, now);
            app_state.ui.peer_management.details_scroll_offset = app_state
                .ui
                .peer_management
                .details_scroll_offset
                .min(max_scroll)
                .saturating_add(page)
                .min(max_scroll);
        }
        PeerManagementAction::StartDetailsSearch => {
            app_state.ui.peer_management.details_is_searching = true;
        }
        PeerManagementAction::DetailsSearchInsert(c) => {
            app_state.ui.peer_management.details_search_query.push(c);
            app_state.ui.peer_management.details_scroll_offset = 0;
        }
        PeerManagementAction::DetailsSearchBackspace => {
            app_state.ui.peer_management.details_search_query.pop();
            app_state.ui.peer_management.details_scroll_offset = 0;
        }
        PeerManagementAction::DetailsSearchCommit => {
            app_state.ui.peer_management.details_is_searching = false;
        }
        PeerManagementAction::DetailsSearchCancel => {
            app_state.ui.peer_management.details_is_searching = false;
            app_state.ui.peer_management.details_search_query.clear();
            app_state.ui.peer_management.details_scroll_offset = 0;
        }
        PeerManagementAction::ToggleDetailsSearchMode => {
            app_state.ui.peer_management.details_search_mode =
                match app_state.ui.peer_management.details_search_mode {
                    SearchMode::Fuzzy => SearchMode::Regex,
                    SearchMode::Regex => SearchMode::Fuzzy,
                };
            app_state.ui.peer_management.details_scroll_offset = 0;
        }
    }

    if recompute_derived {
        recompute_peer_management_derived(app_state, now);
    }
    clamp_peer_column_state(app_state);
    result
}

fn execute_peer_management_effects(app_state: &mut AppState, effects: Vec<PeerManagementEffect>) {
    for effect in effects {
        match effect {
            PeerManagementEffect::ToNormal => app_state.mode = AppMode::Normal,
        }
    }
}

pub fn draw(f: &mut Frame, screen: &ScreenContext<'_>) {
    let app_state = screen.app.state;
    let ctx = screen.theme;
    let area = f.area();
    let now = SystemTime::now();
    let search_visible = active_peer_search_panel_active(app_state);
    let layout = calculate_peer_screen_layout(
        area,
        search_visible,
        app_state.ui.peer_management.show_details,
    );
    let rows = &app_state.peer_management_derived.rows;

    f.render_widget(Clear, area);
    if let Some(search_area) = layout.search {
        draw_peer_search_panel(f, app_state, search_area, ctx);
    }
    draw_peer_summary(f, app_state, layout.summary, rows.len(), now, ctx);

    match layout.body {
        PeerBodyLayout::TableOnly { table } => {
            draw_peer_table(f, app_state, rows, table, now, ctx);
        }
        PeerBodyLayout::DetailsOnly { details } => {
            draw_peer_details(
                f,
                app_state,
                pinned_peer_detail_row(app_state, rows),
                details,
                now,
                ctx,
            );
        }
    }
    draw_peer_footer(f, app_state, layout.footer, ctx);
}

fn build_peer_rows_at(app_state: &AppState, now: SystemTime) -> Vec<PeerRowModel> {
    #[cfg(test)]
    PEER_DERIVED_RECOMPUTE_COUNT.with(|count| count.set(count.get().saturating_add(1)));

    let mut by_ip = BTreeMap::<IpAddr, PeerRowModel>::new();
    for (tracked_index, tracked) in app_state.peer_manager_view.tracked_peers.iter().enumerate() {
        let ip = normalize_peer_ip(tracked.ip);
        by_ip
            .entry(ip)
            .or_insert_with(|| PeerRowModel {
                ip,
                tracked_indices: Vec::new(),
                restriction: None,
                torrent_count: 0,
                is_active: false,
                last_seen: None,
                strongest_evidence: strongest_peer_evidence(&[], None),
                client_label: String::new(),
                connection_count: 0,
                disconnect_count: 0,
                total_downloaded_bytes: 0,
                total_uploaded_bytes: 0,
            })
            .tracked_indices
            .push(tracked_index);
    }

    for (policy_ip, restriction) in &app_state.peer_policy.restrictions {
        if restriction.blocked_until <= now {
            continue;
        }
        let ip = normalize_peer_ip(*policy_ip);
        let row = by_ip.entry(ip).or_insert_with(|| PeerRowModel {
            ip,
            tracked_indices: Vec::new(),
            restriction: None,
            torrent_count: 0,
            is_active: false,
            last_seen: None,
            strongest_evidence: strongest_peer_evidence(&[], None),
            client_label: String::new(),
            connection_count: 0,
            disconnect_count: 0,
            total_downloaded_bytes: 0,
            total_uploaded_bytes: 0,
        });
        if row
            .restriction
            .as_ref()
            .is_none_or(|current| restriction.blocked_until > current.blocked_until)
        {
            row.restriction = Some(restriction.clone());
        }
    }

    let mut rows = by_ip.into_values().collect::<Vec<_>>();
    for row in &mut rows {
        row.tracked_indices.sort_by(|left, right| {
            let left = &app_state.peer_manager_view.tracked_peers[*left];
            let right = &app_state.peer_manager_view.tracked_peers[*right];
            left.torrent_name
                .cmp(&right.torrent_name)
                .then_with(|| left.torrent_info_hash.cmp(&right.torrent_info_hash))
        });
        let tracked = row.tracked(app_state);
        row.torrent_count = row.torrent_count(&tracked);
        row.is_active = tracked.iter().any(|peer| peer.is_active);
        row.last_seen = tracked.iter().filter_map(|peer| peer.last_seen).max();
        row.strongest_evidence = strongest_peer_evidence(&tracked, row.restriction.as_ref());
        row.client_label = peer_client_label(&tracked);
        let (connections, disconnects, downloaded, uploaded) = tracked.iter().fold(
            (0u64, 0u64, 0u64, 0u64),
            |(connections, disconnects, downloaded, uploaded), peer| {
                (
                    connections.saturating_add(peer.connection_count),
                    disconnects.saturating_add(peer.disconnect_count),
                    downloaded.saturating_add(peer.total_downloaded_bytes),
                    uploaded.saturating_add(peer.total_uploaded_bytes),
                )
            },
        );
        row.connection_count = connections;
        row.disconnect_count = disconnects;
        row.total_downloaded_bytes = downloaded;
        row.total_uploaded_bytes = uploaded;
    }

    rows.retain(|row| peer_matches_filter(row, app_state.ui.peer_management.filter));

    let search = PeerSearchMatcher::new(app_state);
    if !matches!(&search, PeerSearchMatcher::MatchAll) {
        rows.retain(|row| search.matches(&peer_search_text(row, app_state)));
    }

    sort_peer_rows(app_state, &mut rows);
    rows
}

pub(crate) fn recompute_peer_management_derived(app_state: &mut AppState, now: SystemTime) {
    let rows = build_peer_rows_at(app_state, now);
    let next_restriction_expiry = app_state
        .peer_policy
        .restrictions
        .values()
        .filter_map(|restriction| {
            (restriction.blocked_until > now).then_some(restriction.blocked_until)
        })
        .min();
    reconcile_peer_selection(app_state, rows.len());
    app_state.peer_management_derived = PeerManagementDerivedState {
        rows,
        next_restriction_expiry,
    };
}

pub(crate) fn refresh_peer_management_expiries(app_state: &mut AppState, now: SystemTime) {
    if app_state
        .peer_management_derived
        .next_restriction_expiry
        .is_some_and(|expiry| expiry <= now)
    {
        recompute_peer_management_derived(app_state, now);
    }
}

fn normalize_peer_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ipv6) => ipv6.to_ipv4_mapped().map_or(IpAddr::V6(ipv6), IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

fn peer_matches_filter(row: &PeerRowModel, filter: PeerManagementFilter) -> bool {
    match filter {
        PeerManagementFilter::All => true,
        PeerManagementFilter::Active => row.is_active() && !row.is_restricted(),
        PeerManagementFilter::Recent => !row.is_active() && !row.is_restricted(),
        PeerManagementFilter::Restricted => row.is_restricted(),
    }
}

enum PeerSearchMatcher {
    MatchAll,
    Fuzzy {
        query: String,
        matcher: Box<SkimMatcherV2>,
    },
    Regex(regex::Regex),
    Invalid,
}

impl PeerSearchMatcher {
    fn new(app_state: &AppState) -> Self {
        Self::from_query(
            &app_state.ui.peer_management.search_query,
            app_state.ui.peer_management.search_mode,
        )
    }

    fn details(app_state: &AppState) -> Self {
        Self::from_query(
            &app_state.ui.peer_management.details_search_query,
            app_state.ui.peer_management.details_search_mode,
        )
    }

    fn from_query(query: &str, mode: SearchMode) -> Self {
        let query = query.trim();
        if query.is_empty() {
            return Self::MatchAll;
        }
        match mode {
            SearchMode::Fuzzy => Self::Fuzzy {
                query: query.to_string(),
                matcher: Box::new(SkimMatcherV2::default().ignore_case()),
            },
            SearchMode::Regex => RegexBuilder::new(query)
                .case_insensitive(true)
                .build()
                .map_or(Self::Invalid, Self::Regex),
        }
    }

    fn matches(&self, haystack: &str) -> bool {
        match self {
            Self::MatchAll => true,
            Self::Fuzzy { query, matcher } => matcher.fuzzy_match(haystack, query).is_some(),
            Self::Regex(regex) => regex.is_match(haystack),
            Self::Invalid => false,
        }
    }
}

fn peer_search_text(row: &PeerRowModel, app_state: &AppState) -> String {
    let privacy = app_state.anonymize_torrent_names;
    let mut fields = vec![
        display_ip(row.ip, privacy),
        row.state_label().to_string(),
        row.client_label.clone(),
    ];
    for tracked in row.tracked(app_state) {
        fields.push(if privacy {
            display_torrent_name(&tracked.torrent_name, true)
        } else {
            tracked.torrent_name.clone()
        });
        fields.push(if privacy {
            short_info_hash(&tracked.torrent_info_hash, true)
        } else {
            hex::encode(&tracked.torrent_info_hash)
        });
        fields.extend(tracked.endpoints.iter().map(|endpoint| {
            if privacy {
                display_endpoint(&endpoint.address, true)
            } else {
                endpoint.address.clone()
            }
        }));
    }
    if let Some(restriction) = &row.restriction {
        fields.push(restriction_reason_search_text(&restriction.reason).to_string());
        if let Some(hash) = &restriction.torrent_info_hash {
            if privacy {
                fields.push(short_info_hash(hash, true));
                fields.push(torrent_label_for_hash(app_state, row, hash));
            } else {
                fields.push(hex::encode(hash));
                if let Some(torrent) = app_state.torrents.get(hash) {
                    fields.push(torrent.latest_state.torrent_name.clone());
                }
            }
        }
    }
    fields.join("\n")
}

fn restriction_reason_search_text(reason: &PeerRestrictionReason) -> &'static str {
    match reason {
        PeerRestrictionReason::ExcessiveUpload { .. } => "excessive upload",
        PeerRestrictionReason::ExcessiveDownload { .. } => "excessive download",
        PeerRestrictionReason::ReconnectChurn { .. } => "reconnect churn",
        PeerRestrictionReason::Manual => "manual",
    }
}

fn tracked_peer_evidence(peer: &PeerManagerTrackedPeer) -> [PeerEvidence; 3] {
    [
        PeerEvidence {
            kind: EvidenceKind::Upload,
            observed: peer.uploaded_evidence_bytes,
            threshold: peer.transfer_threshold_bytes,
            from_policy: false,
        },
        PeerEvidence {
            kind: EvidenceKind::Download,
            observed: peer.downloaded_evidence_bytes,
            threshold: peer.transfer_threshold_bytes,
            from_policy: false,
        },
        PeerEvidence {
            kind: EvidenceKind::Reconnect,
            observed: u64::from(peer.reconnect_count),
            threshold: u64::from(peer.reconnect_limit),
            from_policy: false,
        },
    ]
}

fn restriction_evidence(reason: &PeerRestrictionReason) -> PeerEvidence {
    match reason {
        PeerRestrictionReason::ExcessiveUpload {
            uploaded_bytes,
            threshold_bytes,
        } => PeerEvidence {
            kind: EvidenceKind::Upload,
            observed: *uploaded_bytes,
            threshold: *threshold_bytes,
            from_policy: true,
        },
        PeerRestrictionReason::ExcessiveDownload {
            downloaded_bytes,
            threshold_bytes,
        } => PeerEvidence {
            kind: EvidenceKind::Download,
            observed: *downloaded_bytes,
            threshold: *threshold_bytes,
            from_policy: true,
        },
        PeerRestrictionReason::ReconnectChurn {
            reconnects,
            threshold,
            ..
        } => PeerEvidence {
            kind: EvidenceKind::Reconnect,
            observed: u64::from(*reconnects),
            threshold: u64::from(*threshold),
            from_policy: true,
        },
        PeerRestrictionReason::Manual => PeerEvidence {
            kind: EvidenceKind::Manual,
            observed: 1,
            threshold: 1,
            from_policy: true,
        },
    }
}

fn compare_evidence(left: &PeerEvidence, right: &PeerEvidence) -> Ordering {
    compare_evidence_ratio(left, right)
        .then_with(|| left.from_policy.cmp(&right.from_policy))
        .then_with(|| left.kind.cmp(&right.kind))
}

fn compare_evidence_ratio(left: &PeerEvidence, right: &PeerEvidence) -> Ordering {
    let (left_numerator, left_denominator) = evidence_ratio_parts(left);
    let (right_numerator, right_denominator) = evidence_ratio_parts(right);
    left_numerator
        .saturating_mul(right_denominator)
        .cmp(&right_numerator.saturating_mul(left_denominator))
}

fn evidence_ratio_parts(evidence: &PeerEvidence) -> (u128, u128) {
    if evidence.threshold == 0 {
        (0, 1)
    } else {
        (
            u128::from(evidence.observed),
            u128::from(evidence.threshold),
        )
    }
}

fn peer_columns() -> &'static [PeerColumnDefinition] {
    static COLUMNS: [PeerColumnDefinition; 10] = [
        PeerColumnDefinition {
            id: PeerColumnId::State,
            header: "State",
            min_width: STATE_COLUMN_WIDTH,
            priority: 0,
            constraint: Constraint::Length(STATE_COLUMN_WIDTH),
        },
        PeerColumnDefinition {
            id: PeerColumnId::Address,
            header: "Address",
            min_width: 20,
            priority: 0,
            constraint: Constraint::Fill(1),
        },
        PeerColumnDefinition {
            id: PeerColumnId::Torrents,
            header: "Torrents",
            min_width: TORRENTS_COLUMN_WIDTH,
            priority: 2,
            constraint: Constraint::Length(TORRENTS_COLUMN_WIDTH),
        },
        PeerColumnDefinition {
            id: PeerColumnId::Client,
            header: "Client",
            min_width: 18,
            priority: 2,
            constraint: Constraint::Fill(1),
        },
        PeerColumnDefinition {
            id: PeerColumnId::Connects,
            header: "Connects",
            min_width: CONNECTS_COLUMN_WIDTH,
            priority: 1,
            constraint: Constraint::Length(CONNECTS_COLUMN_WIDTH),
        },
        PeerColumnDefinition {
            id: PeerColumnId::Disconnects,
            header: "Disconnects",
            min_width: DISCONNECTS_COLUMN_WIDTH,
            priority: 1,
            constraint: Constraint::Length(DISCONNECTS_COLUMN_WIDTH),
        },
        PeerColumnDefinition {
            id: PeerColumnId::Downloaded,
            header: "DL",
            min_width: TRANSFER_COLUMN_WIDTH,
            priority: 1,
            constraint: Constraint::Length(TRANSFER_COLUMN_WIDTH),
        },
        PeerColumnDefinition {
            id: PeerColumnId::Uploaded,
            header: "UL",
            min_width: TRANSFER_COLUMN_WIDTH,
            priority: 1,
            constraint: Constraint::Length(TRANSFER_COLUMN_WIDTH),
        },
        PeerColumnDefinition {
            id: PeerColumnId::Evidence,
            header: "Evidence",
            min_width: EVIDENCE_COLUMN_WIDTH,
            priority: 0,
            constraint: Constraint::Length(EVIDENCE_COLUMN_WIDTH),
        },
        PeerColumnDefinition {
            id: PeerColumnId::LastSeen,
            header: "Last Seen",
            min_width: LAST_SEEN_COLUMN_WIDTH,
            priority: 1,
            constraint: Constraint::Length(LAST_SEEN_COLUMN_WIDTH),
        },
    ];
    &COLUMNS
}

fn compute_visible_peer_management_columns(available_width: u16) -> (Vec<Constraint>, Vec<usize>) {
    let columns = peer_columns();
    let smart = columns
        .iter()
        .map(|column| SmartCol {
            min_width: column.min_width,
            priority: column.priority,
            constraint: column.constraint,
        })
        .collect::<Vec<_>>();
    compute_smart_table_layout(&smart, available_width.saturating_sub(4), 1)
}

fn peer_table_width_for_state(app_state: &AppState) -> u16 {
    let area = if app_state.screen_area.width == 0 {
        Rect::new(0, 0, 140, 36)
    } else {
        app_state.screen_area
    };
    let layout = calculate_peer_screen_layout(
        area,
        active_peer_search_panel_active(app_state),
        app_state.ui.peer_management.show_details,
    );
    match layout.body {
        PeerBodyLayout::TableOnly { table } => table.width,
        PeerBodyLayout::DetailsOnly { details } => details.width,
    }
}

fn peer_uses_details_overlay(_app_state: &AppState) -> bool {
    true
}

fn peer_details_overlay_active(app_state: &AppState) -> bool {
    app_state.ui.peer_management.show_details && peer_uses_details_overlay(app_state)
}

fn visible_peer_column_indices_for_state(app_state: &AppState) -> Vec<usize> {
    compute_visible_peer_management_columns(peer_table_width_for_state(app_state)).1
}

fn normalized_selected_column_from_visible(selected: usize, visible: &[usize]) -> usize {
    if visible.is_empty() {
        return 0;
    }
    if visible.contains(&selected) {
        return selected;
    }
    visible
        .iter()
        .copied()
        .rfind(|index| *index <= selected)
        .or_else(|| visible.first().copied())
        .unwrap_or(0)
}

fn normalized_selected_peer_column_index(app_state: &AppState) -> usize {
    normalized_selected_column_from_visible(
        app_state.ui.peer_management.selected_column_index,
        &visible_peer_column_indices_for_state(app_state),
    )
}

fn move_peer_column(app_state: &mut AppState, direction: isize) {
    let visible = visible_peer_column_indices_for_state(app_state);
    if visible.is_empty() {
        return;
    }
    let current = normalized_selected_column_from_visible(
        app_state.ui.peer_management.selected_column_index,
        &visible,
    );
    let position = visible
        .iter()
        .position(|index| *index == current)
        .unwrap_or(0);
    let next = if direction < 0 {
        position.saturating_sub(1)
    } else {
        (position + 1).min(visible.len() - 1)
    };
    app_state.ui.peer_management.selected_column_index = visible[next];
}

fn clamp_peer_column_state(app_state: &mut AppState) {
    let columns_len = peer_columns().len();
    if columns_len == 0 {
        return;
    }
    app_state.ui.peer_management.selected_column_index = app_state
        .ui
        .peer_management
        .selected_column_index
        .min(columns_len - 1);
    if app_state
        .ui
        .peer_management
        .sort_column_index
        .is_some_and(|index| index >= columns_len)
    {
        app_state.ui.peer_management.sort_column_index = None;
    }
}

fn reverse_sort_direction(direction: SortDirection) -> SortDirection {
    match direction {
        SortDirection::Ascending => SortDirection::Descending,
        SortDirection::Descending => SortDirection::Ascending,
    }
}

fn peer_column_default_direction(column: PeerColumnId) -> SortDirection {
    match column {
        PeerColumnId::Address | PeerColumnId::Client => SortDirection::Ascending,
        PeerColumnId::State
        | PeerColumnId::Torrents
        | PeerColumnId::Connects
        | PeerColumnId::Disconnects
        | PeerColumnId::Downloaded
        | PeerColumnId::Uploaded
        | PeerColumnId::Evidence
        | PeerColumnId::LastSeen => SortDirection::Descending,
    }
}

fn peer_sort_column(app_state: &AppState) -> Option<PeerColumnId> {
    app_state
        .ui
        .peer_management
        .sort_column_index
        .and_then(|index| peer_columns().get(index).map(|column| column.id))
}

fn sort_peer_rows(app_state: &AppState, rows: &mut [PeerRowModel]) {
    let column = peer_sort_column(app_state);
    let direction = app_state.ui.peer_management.sort_direction;
    rows.sort_by(|left, right| compare_peer_rows(column, direction, left, right));
}

fn compare_peer_rows(
    column: Option<PeerColumnId>,
    direction: SortDirection,
    left: &PeerRowModel,
    right: &PeerRowModel,
) -> Ordering {
    let Some(column) = column else {
        return default_peer_order(left, right);
    };
    let column_ordering = match column {
        PeerColumnId::State => left.state_sort_rank().cmp(&right.state_sort_rank()),
        PeerColumnId::Address => left.ip.cmp(&right.ip),
        PeerColumnId::Torrents => left.torrent_count.cmp(&right.torrent_count),
        PeerColumnId::Client => left.client_label.cmp(&right.client_label),
        PeerColumnId::Connects => left.connection_count.cmp(&right.connection_count),
        PeerColumnId::Disconnects => left.disconnect_count.cmp(&right.disconnect_count),
        PeerColumnId::Downloaded => left
            .total_downloaded_bytes
            .cmp(&right.total_downloaded_bytes),
        PeerColumnId::Uploaded => left.total_uploaded_bytes.cmp(&right.total_uploaded_bytes),
        PeerColumnId::Evidence => {
            compare_evidence_ratio(left.strongest_evidence(), right.strongest_evidence())
        }
        PeerColumnId::LastSeen => left.last_seen().cmp(&right.last_seen()),
    };
    let ordering = if matches!(column, PeerColumnId::State) {
        right
            .is_restricted()
            .cmp(&left.is_restricted())
            .then_with(|| apply_sort_direction(column_ordering, direction))
    } else {
        apply_sort_direction(column_ordering, direction)
    };
    ordering
        .then_with(|| {
            if matches!(column, PeerColumnId::LastSeen) {
                Ordering::Equal
            } else {
                right.last_seen().cmp(&left.last_seen())
            }
        })
        .then_with(|| left.ip.cmp(&right.ip))
}

fn default_peer_order(left: &PeerRowModel, right: &PeerRowModel) -> Ordering {
    default_peer_order_values(left, right)
        .reverse()
        .then_with(|| left.ip.cmp(&right.ip))
}

fn default_peer_order_values(left: &PeerRowModel, right: &PeerRowModel) -> Ordering {
    left.is_restricted()
        .cmp(&right.is_restricted())
        .then_with(|| compare_evidence_ratio(left.strongest_evidence(), right.strongest_evidence()))
        .then_with(|| left.is_active().cmp(&right.is_active()))
        .then_with(|| left.last_seen().cmp(&right.last_seen()))
}

fn apply_sort_direction(ordering: Ordering, direction: SortDirection) -> Ordering {
    match direction {
        SortDirection::Ascending => ordering,
        SortDirection::Descending => ordering.reverse(),
    }
}

fn reset_peer_selection(app_state: &mut AppState) {
    app_state.ui.peer_management.selected_index = 0;
    app_state.ui.peer_management.show_details = false;
    app_state.ui.peer_management.details_peer_ip = None;
    app_state.ui.peer_management.details_scroll_offset = 0;
    app_state.ui.peer_management.details_is_searching = false;
    app_state.ui.peer_management.details_search_query.clear();
}

fn reconcile_peer_selection(app_state: &mut AppState, row_count: usize) {
    if row_count == 0 {
        app_state.ui.peer_management.selected_index = 0;
        app_state.ui.peer_management.show_details = false;
        app_state.ui.peer_management.details_peer_ip = None;
        app_state.ui.peer_management.details_scroll_offset = 0;
        app_state.ui.peer_management.details_is_searching = false;
        app_state.ui.peer_management.details_search_query.clear();
        return;
    }
    app_state.ui.peer_management.selected_index = app_state
        .ui
        .peer_management
        .selected_index
        .min(row_count - 1);
}

fn move_peer_selection(app_state: &mut AppState, row_count: usize, delta: isize) {
    if row_count == 0 {
        reconcile_peer_selection(app_state, row_count);
        return;
    }
    reconcile_peer_selection(app_state, row_count);
    let current = app_state.ui.peer_management.selected_index;
    let next = current
        .saturating_add_signed(delta)
        .min(row_count.saturating_sub(1));
    if next != current {
        app_state.ui.peer_management.details_scroll_offset = 0;
    }
    app_state.ui.peer_management.selected_index = next;
}

fn select_peer_index(app_state: &mut AppState, row_count: usize, index: usize) {
    if row_count == 0 {
        reconcile_peer_selection(app_state, row_count);
        return;
    }
    let next = index.min(row_count - 1);
    if next != app_state.ui.peer_management.selected_index {
        app_state.ui.peer_management.details_scroll_offset = 0;
    }
    app_state.ui.peer_management.selected_index = next;
}

fn selected_peer_index(app_state: &AppState, rows: &[PeerRowModel]) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }
    Some(
        app_state
            .ui
            .peer_management
            .selected_index
            .min(rows.len() - 1),
    )
}

fn selected_peer_row<'a>(
    app_state: &AppState,
    rows: &'a [PeerRowModel],
) -> Option<&'a PeerRowModel> {
    selected_peer_index(app_state, rows).and_then(|index| rows.get(index))
}

fn pinned_peer_detail_row<'a>(
    app_state: &AppState,
    rows: &'a [PeerRowModel],
) -> Option<&'a PeerRowModel> {
    let ip = app_state.ui.peer_management.details_peer_ip?;
    rows.iter().find(|row| row.ip == ip)
}

fn peer_page_rows(app_state: &AppState) -> usize {
    let area = if app_state.screen_area.height == 0 {
        Rect::new(0, 0, 140, 36)
    } else {
        app_state.screen_area
    };
    let layout = calculate_peer_screen_layout(
        area,
        active_peer_search_panel_active(app_state),
        app_state.ui.peer_management.show_details,
    );
    let table_height = match layout.body {
        PeerBodyLayout::TableOnly { table } => table.height,
        PeerBodyLayout::DetailsOnly { details } => details.height,
    };
    table_height.saturating_sub(3).max(1) as usize
}

fn peer_details_area_for_state(app_state: &AppState) -> Option<Rect> {
    let area = if app_state.screen_area.width == 0 || app_state.screen_area.height == 0 {
        Rect::new(0, 0, 140, 36)
    } else {
        app_state.screen_area
    };
    match calculate_peer_screen_layout(
        area,
        active_peer_search_panel_active(app_state),
        app_state.ui.peer_management.show_details,
    )
    .body
    {
        PeerBodyLayout::DetailsOnly { details } => Some(details),
        PeerBodyLayout::TableOnly { .. } => None,
    }
}

fn peer_details_inner_area(area: Rect) -> Rect {
    Block::default()
        .borders(Borders::ALL)
        .padding(Padding::new(1, 1, 0, 0))
        .inner(area)
}

fn peer_details_content_height(line_count: usize, inner_height: u16) -> usize {
    let height = usize::from(inner_height);
    if line_count > height {
        height.saturating_sub(1)
    } else {
        height
    }
}

fn tracked_peer_detail_line_count(peer: &PeerManagerTrackedPeer) -> usize {
    if peer.endpoints.is_empty() {
        7
    } else {
        7 + peer.endpoints.len()
    }
}

fn matching_detail_torrents<'a>(
    app_state: &'a AppState,
    row: &PeerRowModel,
) -> Vec<&'a PeerManagerTrackedPeer> {
    let matcher = PeerSearchMatcher::details(app_state);
    row.tracked(app_state)
        .into_iter()
        .filter(|peer| {
            matcher.matches(&display_torrent_name(
                &peer.torrent_name,
                app_state.anonymize_torrent_names,
            ))
        })
        .collect()
}

fn peer_detail_line_count(app_state: &AppState, row: &PeerRowModel) -> usize {
    let header_and_policy = if row.restriction.is_some() { 8 } else { 4 };
    if row.tracked_indices.is_empty() {
        return header_and_policy + 2;
    }
    let matching = matching_detail_torrents(app_state, row);
    if matching.is_empty() {
        return header_and_policy + 3;
    }
    header_and_policy
        + 2
        + matching
            .iter()
            .map(|peer| tracked_peer_detail_line_count(peer))
            .sum::<usize>()
        + matching.len().saturating_sub(1)
}

fn peer_details_max_scroll_at(app_state: &AppState, _now: SystemTime) -> usize {
    let Some(area) = peer_details_area_for_state(app_state) else {
        return 0;
    };
    let Some(row) = pinned_peer_detail_row(app_state, &app_state.peer_management_derived.rows)
    else {
        return 0;
    };
    let line_count = peer_detail_line_count(app_state, row);
    let inner = peer_details_inner_area(area);
    line_count.saturating_sub(peer_details_content_height(line_count, inner.height))
}

fn peer_details_page_rows(app_state: &AppState) -> usize {
    let Some(area) = peer_details_area_for_state(app_state) else {
        return 1;
    };
    usize::from(peer_details_inner_area(area).height)
        .saturating_sub(2)
        .max(1)
}

fn peer_table_search_panel_active(app_state: &AppState) -> bool {
    app_state.ui.peer_management.is_searching
        || !app_state.ui.peer_management.search_query.is_empty()
}

fn peer_details_search_panel_active(app_state: &AppState) -> bool {
    app_state.ui.peer_management.details_is_searching
        || !app_state.ui.peer_management.details_search_query.is_empty()
}

fn active_peer_search_panel_active(app_state: &AppState) -> bool {
    if peer_details_overlay_active(app_state) {
        peer_details_search_panel_active(app_state)
    } else {
        peer_table_search_panel_active(app_state)
    }
}

fn draw_peer_search_panel(f: &mut Frame, app_state: &AppState, area: Rect, ctx: &ThemeContext) {
    let (title, query, mode) = if peer_details_overlay_active(app_state) {
        (
            " Torrent Search ",
            app_state.ui.peer_management.details_search_query.as_str(),
            app_state.ui.peer_management.details_search_mode,
        )
    } else {
        (
            " Peer Search ",
            app_state.ui.peer_management.search_query.as_str(),
            app_state.ui.peer_management.search_mode,
        )
    };
    draw_prompt_panel(
        f,
        area,
        title.to_string(),
        sanitize_text(query),
        peer_search_mode_spans(mode, ctx),
        ctx,
    );
}

fn peer_search_mode_spans(mode: SearchMode, ctx: &ThemeContext) -> Vec<Span<'static>> {
    let selected = ctx.apply(Style::default().fg(ctx.state_selected()).bold());
    let idle = ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0));
    let (fuzzy, regex) = match mode {
        SearchMode::Fuzzy => (selected, idle),
        SearchMode::Regex => (idle, selected),
    };
    vec![
        Span::raw("  "),
        Span::styled("Fuzzy", fuzzy),
        Span::raw(" / "),
        Span::styled("Regex", regex),
    ]
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PeerSummaryCounts {
    tracked: usize,
    active: usize,
    restricted: usize,
}

fn peer_summary_counts_at(app_state: &AppState, now: SystemTime) -> PeerSummaryCounts {
    let tracked = app_state
        .peer_manager_view
        .tracked_peers
        .iter()
        .map(|peer| normalize_peer_ip(peer.ip))
        .collect::<BTreeSet<_>>();
    let active = app_state
        .peer_manager_view
        .tracked_peers
        .iter()
        .filter(|peer| peer.is_active)
        .map(|peer| normalize_peer_ip(peer.ip))
        .collect::<BTreeSet<_>>();
    let restricted = app_state
        .peer_policy
        .restrictions
        .iter()
        .filter(|(_, restriction)| restriction.blocked_until > now)
        .map(|(ip, _)| normalize_peer_ip(*ip))
        .collect::<BTreeSet<_>>();
    PeerSummaryCounts {
        tracked: tracked.len(),
        active: active.len(),
        restricted: restricted.len(),
    }
}

fn draw_peer_summary(
    f: &mut Frame,
    app_state: &AppState,
    area: Rect,
    visible_count: usize,
    now: SystemTime,
    ctx: &ThemeContext,
) {
    if area.height == 0 {
        return;
    }
    let counts = peer_summary_counts_at(app_state, now);
    let mut count_spans = vec![
        Span::styled(
            "Tracked ",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ),
        Span::styled(
            counts.tracked.to_string(),
            ctx.apply(Style::default().fg(ctx.accent_sky()).bold()),
        ),
        Span::styled(
            "  Live ",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ),
        Span::styled(
            counts.active.to_string(),
            ctx.apply(Style::default().fg(ctx.state_success()).bold()),
        ),
        Span::styled(
            "  Restricted ",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ),
        Span::styled(
            counts.restricted.to_string(),
            ctx.apply(Style::default().fg(ctx.state_error()).bold()),
        ),
        Span::styled(
            format!(
                "  Torrents {}  Showing {}",
                app_state.peer_manager_view.registered_torrents, visible_count
            ),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ),
    ];
    if app_state.anonymize_torrent_names {
        count_spans.push(Span::styled(
            "  PRIVACY",
            ctx.apply(Style::default().fg(ctx.state_warning()).bold()),
        ));
    }
    if let Some(message) = active_peer_search_error(app_state)
        .or_else(|| app_state.ui.peer_management.status_message.clone())
    {
        count_spans.push(Span::styled(
            format!("  |  {}", sanitize_text(&message)),
            ctx.apply(Style::default().fg(ctx.state_warning())),
        ));
    }

    let filters = [
        PeerManagementFilter::All,
        PeerManagementFilter::Active,
        PeerManagementFilter::Recent,
        PeerManagementFilter::Restricted,
    ];
    let filter_spans = filters
        .iter()
        .enumerate()
        .flat_map(|(index, filter)| {
            let style = if *filter == app_state.ui.peer_management.filter {
                ctx.apply(Style::default().fg(ctx.state_selected()).bold())
            } else {
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1))
            };
            let mut spans = vec![Span::styled(filter.label(), style)];
            if index + 1 < filters.len() {
                spans.push(Span::styled(
                    "  ",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
                ));
            }
            spans
        })
        .collect::<Vec<_>>();

    f.render_widget(
        Paragraph::new(vec![Line::from(count_spans), Line::from(filter_spans)]),
        area,
    );
}

fn peer_search_error(app_state: &AppState) -> Option<String> {
    search_error(
        &app_state.ui.peer_management.search_query,
        app_state.ui.peer_management.search_mode,
    )
}

fn peer_details_search_error(app_state: &AppState) -> Option<String> {
    search_error(
        &app_state.ui.peer_management.details_search_query,
        app_state.ui.peer_management.details_search_mode,
    )
}

fn active_peer_search_error(app_state: &AppState) -> Option<String> {
    if peer_details_overlay_active(app_state) {
        peer_details_search_error(app_state)
    } else {
        peer_search_error(app_state)
    }
}

fn search_error(query: &str, mode: SearchMode) -> Option<String> {
    let query = query.trim();
    if query.is_empty() || !matches!(mode, SearchMode::Regex) {
        return None;
    }
    RegexBuilder::new(query)
        .case_insensitive(true)
        .build()
        .err()
        .map(|_| "Invalid regular expression".to_string())
}

fn draw_peer_table(
    f: &mut Frame,
    app_state: &AppState,
    rows: &[PeerRowModel],
    area: Rect,
    now: SystemTime,
    ctx: &ThemeContext,
) {
    let columns = peer_columns();
    let (constraints, visible) = compute_visible_peer_management_columns(area.width);
    let selected_column = normalized_selected_column_from_visible(
        app_state.ui.peer_management.selected_column_index,
        &visible,
    );
    let header = Row::new(
        visible
            .iter()
            .map(|index| {
                let column = &columns[*index];
                let is_selected = *index == selected_column;
                let is_sorting = app_state.ui.peer_management.sort_column_index == Some(*index);
                Cell::from(Line::from(peer_column_header_spans(
                    column,
                    is_selected,
                    is_sorting,
                    app_state.ui.peer_management.sort_direction,
                    ctx,
                )))
            })
            .collect::<Vec<_>>(),
    );
    let selected_index = selected_peer_index(app_state, rows);
    let viewport = peer_table_viewport(rows.len(), selected_index, area.height);
    let table_rows = rows[viewport.clone()]
        .iter()
        .map(|row| peer_table_row(app_state, row, &visible, now, ctx))
        .collect::<Vec<_>>();
    let table = Table::new(table_rows, constraints)
        .header(header)
        .row_highlight_style(peer_row_highlight_style(ctx))
        .block(
            Block::default()
                .title(Span::styled(
                    " Peer Manager ",
                    ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
                ))
                .borders(Borders::ALL)
                .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)))
                .padding(Padding::horizontal(1)),
        );
    let mut table_state = TableState::default();
    table_state.select(selected_index.map(|selected| selected.saturating_sub(viewport.start)));
    f.render_stateful_widget(table, area, &mut table_state);

    if rows.is_empty() {
        let message = if peer_search_error(app_state).is_some() {
            "Fix the search expression to see peers"
        } else if app_state.ui.peer_management.search_query.is_empty()
            && matches!(
                app_state.ui.peer_management.filter,
                PeerManagementFilter::All
            )
        {
            "No peers tracked yet"
        } else {
            "No peers match this view"
        };
        f.render_widget(
            Paragraph::new(message)
                .alignment(Alignment::Center)
                .style(ctx.apply(Style::default().fg(ctx.theme.semantic.surface2))),
            centered_line_rect(inner_rect(area)),
        );
    }
}

fn peer_column_header_spans(
    column: &PeerColumnDefinition,
    is_selected: bool,
    is_sorting: bool,
    sort_direction: SortDirection,
    ctx: &ThemeContext,
) -> Vec<Span<'static>> {
    let mut style = ctx.apply(Style::default().fg(peer_column_header_color(column.id, ctx)));
    if is_sorting {
        style = style.bold();
    }
    let label_style = if is_selected {
        ctx.apply(style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED))
    } else {
        style
    };
    let mut spans = vec![Span::styled(column.header, label_style)];
    if is_sorting {
        spans.push(Span::styled(peer_sort_arrow(sort_direction), style));
    }
    spans
}

fn peer_row_highlight_style(ctx: &ThemeContext) -> Style {
    ctx.apply(Style::default().fg(ctx.state_warning()).bold())
}

fn peer_table_viewport(
    row_count: usize,
    selected_index: Option<usize>,
    area_height: u16,
) -> std::ops::Range<usize> {
    if row_count == 0 {
        return 0..0;
    }

    let capacity = usize::from(area_height.saturating_sub(3).max(1));
    let selected = selected_index.unwrap_or(0).min(row_count - 1);
    let start = selected
        .saturating_sub(capacity.saturating_sub(1))
        .min(row_count.saturating_sub(capacity));
    start..start.saturating_add(capacity).min(row_count)
}

fn peer_table_row<'a>(
    app_state: &AppState,
    row: &PeerRowModel,
    visible: &[usize],
    now: SystemTime,
    ctx: &ThemeContext,
) -> Row<'a> {
    let columns = peer_columns();
    let cells = visible
        .iter()
        .map(|index| match columns[*index].id {
            PeerColumnId::State => {
                Cell::from(row.state_column_label(now)).style(peer_state_style(row, ctx))
            }
            PeerColumnId::Address => {
                Cell::from(display_ip(row.ip, app_state.anonymize_torrent_names))
            }
            PeerColumnId::Torrents => Cell::from(peer_torrents_label(row)),
            PeerColumnId::Client => Cell::from(sanitize_text(&row.client_label)),
            PeerColumnId::Connects => Cell::from(compact_count(row.connection_count)),
            PeerColumnId::Disconnects => Cell::from(compact_count(row.disconnect_count)),
            PeerColumnId::Downloaded => {
                Cell::from(compact_transfer_bytes(row.total_downloaded_bytes))
            }
            PeerColumnId::Uploaded => Cell::from(compact_transfer_bytes(row.total_uploaded_bytes)),
            PeerColumnId::Evidence => Cell::from(row.strongest_evidence().compact_label()),
            PeerColumnId::LastSeen => Cell::from(
                row.last_seen()
                    .map(|last_seen| format_elapsed(now, last_seen))
                    .unwrap_or_else(|| "policy only".to_string()),
            ),
        })
        .collect::<Vec<_>>();
    Row::new(cells).style(if row.is_restricted() {
        ctx.apply(Style::default().fg(ctx.state_error()))
    } else if row.is_active() {
        ctx.apply(Style::default().fg(ctx.theme.semantic.text))
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1))
    })
}

fn peer_state_style(row: &PeerRowModel, ctx: &ThemeContext) -> Style {
    if row.is_restricted() {
        ctx.apply(Style::default().fg(ctx.state_error()).bold())
    } else if row.is_active() {
        ctx.apply(Style::default().fg(ctx.state_success()).bold())
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1))
    }
}

fn peer_column_header_color(column: PeerColumnId, ctx: &ThemeContext) -> Color {
    match column {
        PeerColumnId::State => ctx.state_success(),
        PeerColumnId::Address => ctx.accent_sky(),
        PeerColumnId::Torrents => ctx.accent_teal(),
        PeerColumnId::Client => ctx.accent_sapphire(),
        PeerColumnId::Connects => ctx.state_success(),
        PeerColumnId::Disconnects => ctx.state_warning(),
        PeerColumnId::Downloaded => ctx.accent_sky(),
        PeerColumnId::Uploaded => ctx.accent_teal(),
        PeerColumnId::Evidence => ctx.state_warning(),
        PeerColumnId::LastSeen => ctx.state_info(),
    }
}

fn peer_sort_arrow(direction: SortDirection) -> &'static str {
    match direction {
        SortDirection::Ascending => " ▲",
        SortDirection::Descending => " ▼",
    }
}

fn peer_torrents_label(row: &PeerRowModel) -> String {
    let label = row.torrent_count.to_string();
    if fits_column(&label, TORRENTS_COLUMN_WIDTH) {
        label
    } else {
        format!(
            "{}+",
            "9".repeat(usize::from(TORRENTS_COLUMN_WIDTH.saturating_sub(1)))
        )
    }
}

fn torrent_name_for_hash<'a>(
    app_state: &'a AppState,
    row: &PeerRowModel,
    hash: &[u8],
) -> Option<&'a str> {
    row.tracked(app_state)
        .into_iter()
        .find(|tracked| tracked.torrent_info_hash == hash)
        .map(|tracked| tracked.torrent_name.as_str())
        .or_else(|| {
            app_state
                .torrents
                .get(hash)
                .map(|torrent| torrent.latest_state.torrent_name.as_str())
        })
}

fn torrent_label_for_hash(app_state: &AppState, row: &PeerRowModel, hash: &[u8]) -> String {
    let name = torrent_name_for_hash(app_state, row, hash);
    let Some(name) = name else {
        return short_info_hash(hash, app_state.anonymize_torrent_names);
    };
    display_torrent_name(name, app_state.anonymize_torrent_names)
}

fn draw_peer_details(
    f: &mut Frame,
    app_state: &AppState,
    row: Option<&PeerRowModel>,
    area: Rect,
    now: SystemTime,
    ctx: &ThemeContext,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    f.render_widget(Clear, area);
    let block = Block::default()
        .title(Span::styled(
            " Peer Details ",
            ctx.apply(Style::default().fg(ctx.accent_sapphire()).bold()),
        ))
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(row) = row else {
        f.render_widget(
            Paragraph::new("No peer selected")
                .alignment(Alignment::Center)
                .style(ctx.apply(Style::default().fg(ctx.theme.semantic.surface2))),
            centered_line_rect(inner),
        );
        return;
    };

    let lines = peer_detail_lines(app_state, row, now, inner.width, ctx);
    let line_count = lines.len();
    let content_height = peer_details_content_height(line_count, inner.height);
    let max_scroll = line_count.saturating_sub(content_height);
    let scroll = app_state
        .ui
        .peer_management
        .details_scroll_offset
        .min(max_scroll);
    let content_area = Rect::new(inner.x, inner.y, inner.width, content_height as u16);
    f.render_widget(
        Paragraph::new(lines)
            .scroll((scroll.min(u16::MAX as usize) as u16, 0))
            .style(ctx.apply(Style::default().fg(ctx.theme.semantic.text))),
        content_area,
    );

    if max_scroll > 0 {
        let status_area = Rect::new(
            inner.x,
            inner.y.saturating_add(content_height as u16),
            inner.width,
            1,
        );
        let end = scroll.saturating_add(content_height).min(line_count);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "↑/↓ scroll",
                    ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
                ),
                Span::styled(
                    format!("  lines {}–{} of {}", scroll + 1, end, line_count),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
                ),
            ]))
            .alignment(Alignment::Right),
            status_area,
        );
    }
}

fn peer_detail_lines(
    app_state: &AppState,
    row: &PeerRowModel,
    now: SystemTime,
    available_width: u16,
    ctx: &ThemeContext,
) -> Vec<Line<'static>> {
    let privacy = app_state.anonymize_torrent_names;
    let endpoint_count = row.endpoint_count(app_state);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            display_ip(row.ip, privacy),
            ctx.apply(Style::default().fg(ctx.accent_sky()).bold()),
        ),
        Span::styled(
            format!("  {}", row.state_label()),
            peer_state_style(row, ctx),
        ),
        Span::styled(
            format!(
                "  •  {} torrent{}  •  {} endpoint{}",
                row.torrent_count,
                plural_suffix(row.torrent_count),
                endpoint_count,
                plural_suffix(endpoint_count)
            ),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ),
    ])];
    lines.push(key_value_line(
        if row.client_label.contains(", ") {
            "Clients"
        } else {
            "Client"
        },
        row.client_label.clone(),
        ctx,
    ));

    if let Some(restriction) = &row.restriction {
        lines.push(Line::from(""));
        lines.push(section_line("Policy", ctx));
        lines.push(key_value_line(
            "Reason",
            restriction_reason_detail(&restriction.reason),
            ctx,
        ));
        lines.push(key_value_line(
            "Detected",
            format_elapsed(now, restriction.detected_at),
            ctx,
        ));
        lines.push(key_value_line(
            "Expires",
            format_remaining(now, restriction.blocked_until),
            ctx,
        ));
        let origin = restriction
            .torrent_info_hash
            .as_ref()
            .map(|hash| torrent_label_for_hash(app_state, row, hash))
            .unwrap_or_else(|| "global/manual".to_string());
        lines.push(key_value_line("Origin", origin, ctx));
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "No active restriction",
            ctx.apply(Style::default().fg(ctx.state_success())),
        )));
    }

    if row.tracked_indices.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Tracking history is unavailable for this restored policy entry.",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1).italic()),
        )));
    } else {
        let matching = matching_detail_torrents(app_state, row);
        lines.push(Line::from(""));
        lines.push(section_line(
            &format!(
                "Per-torrent evidence  •  {} of {}",
                matching.len(),
                row.tracked_indices.len()
            ),
            ctx,
        ));
        if matching.is_empty() {
            lines.push(Line::from(Span::styled(
                if peer_details_search_error(app_state).is_some() {
                    "Fix the search expression to see torrents."
                } else {
                    "No torrent names match this search."
                },
                ctx.apply(Style::default().fg(ctx.state_warning())),
            )));
        }
        for (index, tracked) in matching.iter().enumerate() {
            if index > 0 {
                lines.push(Line::from(""));
            }
            lines.extend(tracked_peer_detail_lines(
                tracked,
                index + 1,
                privacy,
                available_width,
                ctx,
            ));
        }
    }
    lines
}

fn tracked_peer_detail_lines(
    peer: &PeerManagerTrackedPeer,
    index: usize,
    privacy: bool,
    available_width: u16,
    ctx: &ThemeContext,
) -> Vec<Line<'static>> {
    let torrent_name = display_torrent_name(&peer.torrent_name, privacy);
    let hash = short_info_hash(&peer.torrent_info_hash, privacy);
    let client_label = tracked_peer_client_label(peer);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!("{:02}  ", index),
            ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
        ),
        Span::styled(
            truncate_with_ellipsis(
                &sanitize_text(&torrent_name),
                usize::from(available_width).saturating_sub(4).max(1),
            ),
            ctx.apply(Style::default().fg(ctx.accent_teal()).bold()),
        ),
    ])];
    lines.push(Line::from(vec![
        detail_label_span("  Hash ", ctx),
        Span::styled(
            hash,
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
        ),
    ]));
    lines.push(Line::from(vec![
        detail_label_span("  Client ", ctx),
        Span::styled(
            sanitize_text(&client_label),
            ctx.apply(Style::default().fg(ctx.accent_sapphire())),
        ),
    ]));
    lines.push(Line::from(vec![
        detail_label_span("  Upload    ", ctx),
        Span::styled(
            format!(
                "{} / {} ({:.0}%)",
                format_bytes(peer.uploaded_evidence_bytes),
                format_bytes(peer.transfer_threshold_bytes),
                evidence_percent(peer.uploaded_evidence_bytes, peer.transfer_threshold_bytes),
            ),
            ctx.apply(Style::default().fg(ctx.accent_teal())),
        ),
    ]));
    lines.push(Line::from(vec![
        detail_label_span("  Download  ", ctx),
        Span::styled(
            format!(
                "{} / {} ({:.0}%)",
                format_bytes(peer.downloaded_evidence_bytes),
                format_bytes(peer.transfer_threshold_bytes),
                evidence_percent(
                    peer.downloaded_evidence_bytes,
                    peer.transfer_threshold_bytes
                ),
            ),
            ctx.apply(Style::default().fg(ctx.accent_sky())),
        ),
    ]));
    lines.push(Line::from(vec![
        detail_label_span("  Reconnects  ", ctx),
        Span::styled(
            peer.reconnect_count.to_string(),
            ctx.apply(Style::default().fg(if peer.reconnect_count == 0 {
                ctx.state_success()
            } else {
                ctx.state_warning()
            })),
        ),
        Span::styled(
            format!(
                " / {}  •  {} window",
                peer.reconnect_limit,
                compact_duration(Duration::from_secs(peer.reconnect_window_secs))
            ),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ),
    ]));
    if peer.endpoints.is_empty() {
        lines.push(Line::from(vec![
            detail_label_span("  Endpoints  ", ctx),
            Span::styled(
                "none",
                ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0).italic()),
            ),
        ]));
    } else {
        lines.push(Line::from(detail_label_span("  Endpoints", ctx)));
        for (index, endpoint) in peer.endpoints.iter().enumerate() {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("    {}  ", index + 1),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
                ),
                Span::styled(
                    display_endpoint(&endpoint.address, privacy),
                    ctx.apply(Style::default().fg(ctx.accent_sky())),
                ),
                detail_separator_span(ctx),
                detail_label_span("DL ", ctx),
                Span::raw(format_bytes(endpoint.total_downloaded)),
                detail_separator_span(ctx),
                detail_label_span("UL ", ctx),
                Span::raw(format_bytes(endpoint.total_uploaded)),
            ]));
        }
    }
    lines
}

fn detail_label_span(label: &'static str, ctx: &ThemeContext) -> Span<'static> {
    Span::styled(
        label,
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
    )
}

fn detail_separator_span(ctx: &ThemeContext) -> Span<'static> {
    Span::styled(
        "  •  ",
        ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
    )
}

fn tracked_peer_client_label(peer: &PeerManagerTrackedPeer) -> String {
    preferred_client_label(peer.clients.iter().map(String::as_str))
}

fn section_line(label: &str, ctx: &ThemeContext) -> Line<'static> {
    Line::from(Span::styled(
        label.to_string(),
        ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
    ))
}

fn key_value_line(label: &str, value: String, ctx: &ThemeContext) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label}: "),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ),
        Span::raw(sanitize_text(&value)),
    ])
}

fn restriction_reason_detail(reason: &PeerRestrictionReason) -> String {
    match reason {
        PeerRestrictionReason::ExcessiveUpload {
            uploaded_bytes,
            threshold_bytes,
        } => format!(
            "Excessive upload: {} / {}",
            format_bytes(*uploaded_bytes),
            format_bytes(*threshold_bytes)
        ),
        PeerRestrictionReason::ExcessiveDownload {
            downloaded_bytes,
            threshold_bytes,
        } => format!(
            "Excessive download: {} / {}",
            format_bytes(*downloaded_bytes),
            format_bytes(*threshold_bytes)
        ),
        PeerRestrictionReason::ReconnectChurn {
            reconnects,
            threshold,
            window_secs,
        } => format!(
            "Reconnect churn: {reconnects}/{threshold} in {}",
            compact_duration(Duration::from_secs(*window_secs))
        ),
        PeerRestrictionReason::Manual => "Manual restriction".to_string(),
    }
}

fn draw_peer_footer(f: &mut Frame, app_state: &AppState, area: Rect, ctx: &ThemeContext) {
    if area.height == 0 {
        return;
    }
    let mut spans = Vec::new();
    let mut push = |key: &str, action: &str, tone: ActionTone| {
        spans.push(Span::styled(
            format!("[{key}]"),
            footer_key_style(ctx, tone),
        ));
        spans.push(Span::styled(
            action.to_string(),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
        ));
        spans.push(Span::styled(
            " | ",
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
        ));
    };

    if app_state.ui.peer_management.is_searching
        || app_state.ui.peer_management.details_is_searching
    {
        push("Enter", "apply", ActionTone::Confirm);
        push("Tab", "mode", ActionTone::Mode);
        push("Esc", "clear", ActionTone::Cancel);
    } else if peer_details_overlay_active(app_state) {
        push("↑/↓", "scroll", ActionTone::Navigate);
        push("/", "search", ActionTone::Search);
        if peer_details_search_panel_active(app_state) {
            push("Tab", "mode", ActionTone::Mode);
            push("Esc", "clear", ActionTone::Clear);
            push("Enter", "table", ActionTone::Navigate);
        } else {
            push("Enter/Esc", "table", ActionTone::Navigate);
        }
        push("q", "back", ActionTone::Cancel);
    } else {
        push("arrows", "nav", ActionTone::Navigate);
        push("h/l", "column", ActionTone::Navigate);
        push("s", "ort", ActionTone::Sort);
        if peer_table_search_panel_active(app_state) {
            push("Tab", "mode", ActionTone::Mode);
            push("Shift+Tab", "filter", ActionTone::Mode);
        } else {
            push("Tab", "filter", ActionTone::Mode);
        }
        push("/", "search", ActionTone::Search);
        if peer_uses_details_overlay(app_state) {
            push("Enter", "details", ActionTone::Info);
        }
        push("x", "privacy", ActionTone::Toggle);
        if peer_table_search_panel_active(app_state) {
            push("Esc", "clear", ActionTone::Clear);
            push("q", "back", ActionTone::Cancel);
        } else {
            push("Esc", "back", ActionTone::Cancel);
        }
    }
    if !spans.is_empty() {
        spans.pop();
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
        area,
    );
}

fn display_torrent_name(name: &str, privacy: bool) -> String {
    if !privacy {
        return sanitize_text(name);
    }
    let masked = anonymize_preserving_shape(name);
    if masked.is_empty() {
        "Torrent".to_string()
    } else {
        sanitize_text(&masked)
    }
}

fn display_ip(ip: IpAddr, privacy: bool) -> String {
    if !privacy {
        return ip.to_string();
    }
    format!("peer-{:016x}", stable_mask_id(&ip.to_string()))
}

fn display_endpoint(address: &str, privacy: bool) -> String {
    if !privacy {
        return sanitize_text(address);
    }
    format!("endpoint-{:016x}", stable_mask_id(address))
}

fn stable_mask_id(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn short_info_hash(hash: &[u8], privacy: bool) -> String {
    if privacy {
        return "hidden-hash".to_string();
    }
    let encoded = hex::encode(hash);
    encoded.chars().take(12).collect()
}

fn evidence_percent(observed: u64, threshold: u64) -> f64 {
    if threshold == 0 {
        0.0
    } else {
        observed as f64 * 100.0 / threshold as f64
    }
}

fn format_elapsed(now: SystemTime, time: SystemTime) -> String {
    let elapsed = now.duration_since(time).unwrap_or_default();
    if elapsed < Duration::from_secs(1) {
        "<1s ago".to_string()
    } else {
        let label = format!("{} ago", compact_duration(elapsed));
        if fits_column(&label, LAST_SEEN_COLUMN_WIDTH) {
            label
        } else {
            ">999d ago".to_string()
        }
    }
}

fn format_remaining(now: SystemTime, deadline: SystemTime) -> String {
    let remaining = deadline.duration_since(now).unwrap_or_default();
    if remaining.is_zero() {
        "expired".to_string()
    } else {
        let label = format!("{} left", compact_duration(remaining));
        if fits_column(&label, RESTRICTION_REMAINING_WIDTH) {
            label
        } else {
            ">99d left".to_string()
        }
    }
}

fn compact_duration(duration: Duration) -> String {
    let mut seconds = duration.as_secs();
    let days = seconds / 86_400;
    seconds %= 86_400;
    let hours = seconds / 3_600;
    seconds %= 3_600;
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

fn plural_suffix(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

fn inner_rect(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

fn centered_line_rect(area: Rect) -> Rect {
    Rect::new(
        area.x,
        area.y + area.height.saturating_sub(1) / 2,
        area.width,
        1,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer_manager::{PeerManagerEndpointView, PeerManagerView, PeerPolicy};
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn test_now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(10_000)
    }

    #[test]
    fn peer_table_viewport_builds_only_rows_that_can_be_rendered() {
        assert_eq!(peer_table_viewport(0, None, 14), 0..0);
        assert_eq!(peer_table_viewport(5, Some(4), 14), 0..5);
        assert_eq!(peer_table_viewport(100, Some(0), 14), 0..11);
        assert_eq!(peer_table_viewport(100, Some(10), 14), 0..11);
        assert_eq!(peer_table_viewport(100, Some(11), 14), 1..12);
        assert_eq!(peer_table_viewport(100, Some(99), 14), 89..100);
    }

    fn tracked_peer(ip: &str, name: &str, hash_seed: u8) -> PeerManagerTrackedPeer {
        PeerManagerTrackedPeer {
            torrent_info_hash: vec![hash_seed; 20],
            torrent_name: name.to_string(),
            ip: ip.parse().unwrap(),
            is_active: false,
            endpoints: Vec::new(),
            downloaded_evidence_bytes: 0,
            uploaded_evidence_bytes: 0,
            total_downloaded_bytes: 0,
            total_uploaded_bytes: 0,
            connection_count: 0,
            disconnect_count: 0,
            transfer_threshold_bytes: 100,
            reconnect_count: 0,
            reconnect_limit: 10,
            reconnect_window_secs: 10,
            last_seen: Some(test_now() - Duration::from_secs(30)),
            clients: Vec::new(),
        }
    }

    fn state_with_peers(peers: Vec<PeerManagerTrackedPeer>) -> AppState {
        let mut state = AppState {
            mode: AppMode::PeerManagement,
            ..Default::default()
        };
        state.peer_manager_view = Arc::new(PeerManagerView {
            registered_torrents: peers
                .iter()
                .map(|peer| peer.torrent_info_hash.clone())
                .collect::<BTreeSet<_>>()
                .len(),
            metrics_updates: 1,
            tracked_peers: peers,
        });
        recompute_peer_management_derived(&mut state, test_now());
        state
    }

    fn restriction(blocked_until: SystemTime, reason: PeerRestrictionReason) -> PeerRestriction {
        PeerRestriction {
            detected_at: test_now() - Duration::from_secs(60),
            blocked_until,
            torrent_info_hash: None,
            reason,
        }
    }

    #[test]
    fn repeat_events_are_limited_to_navigation_and_text_editing() {
        let mut state = state_with_peers(vec![tracked_peer("192.0.2.31", "Amber Field Notes", 31)]);

        for code in [
            KeyCode::Char('s'),
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Char('/'),
            KeyCode::Char('x'),
            KeyCode::Enter,
            KeyCode::Esc,
        ] {
            let repeat = KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Repeat);
            assert_eq!(
                map_key_event_to_peer_management_action(repeat, &state),
                None,
                "repeat should be ignored for {code:?}"
            );
        }

        let navigation_repeat =
            KeyEvent::new_with_kind(KeyCode::Down, KeyModifiers::NONE, KeyEventKind::Repeat);
        assert_eq!(
            map_key_event_to_peer_management_action(navigation_repeat, &state),
            Some(PeerManagementAction::MoveDown)
        );

        reduce_peer_management_action(&mut state, PeerManagementAction::StartSearch);
        let text_repeat =
            KeyEvent::new_with_kind(KeyCode::Char('a'), KeyModifiers::NONE, KeyEventKind::Repeat);
        assert_eq!(
            map_key_event_to_peer_management_action(text_repeat, &state),
            Some(PeerManagementAction::SearchInsert('a'))
        );

        let slash_repeat =
            KeyEvent::new_with_kind(KeyCode::Char('/'), KeyModifiers::NONE, KeyEventKind::Repeat);
        assert_eq!(
            map_key_event_to_peer_management_action(slash_repeat, &state),
            None
        );
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .concat()
    }

    fn rendered_peer_details(state: &AppState) -> String {
        let rows = build_peer_rows_at(state, test_now());
        let row = pinned_peer_detail_row(state, &rows);
        let ctx = ThemeContext::new(state.theme, 0.0);
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_peer_details(
                    frame,
                    state,
                    row,
                    Rect::new(0, 0, 100, 24),
                    test_now(),
                    &ctx,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn take_peer_derived_recompute_count() -> usize {
        PEER_DERIVED_RECOMPUTE_COUNT.with(|count| count.replace(0))
    }

    #[test]
    fn normalized_ip_rows_keep_per_torrent_evidence_separate() {
        let mut first = tracked_peer("192.0.2.10", "Quartz Archive", 1);
        first.uploaded_evidence_bytes = 60;
        first.connection_count = 2;
        first.disconnect_count = 1;
        first.total_downloaded_bytes = 1_024;
        first.total_uploaded_bytes = 2_048;
        let mut second = tracked_peer("::ffff:192.0.2.10", "Cinder Atlas", 2);
        second.downloaded_evidence_bytes = 55;
        second.connection_count = 3;
        second.disconnect_count = 2;
        second.total_downloaded_bytes = 4_096;
        second.total_uploaded_bytes = 8_192;
        let state = state_with_peers(vec![first, second]);

        let rows = build_peer_rows_at(&state, test_now());

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tracked_indices.len(), 2);
        assert_eq!(rows[0].strongest_evidence().observed, 60);
        assert_eq!(rows[0].strongest_evidence().threshold, 100);
        assert_eq!(rows[0].strongest_evidence().compact_label(), "UL 60%");
        assert_eq!(rows[0].connection_count, 5);
        assert_eq!(rows[0].disconnect_count, 3);
        assert_eq!(rows[0].total_downloaded_bytes, 5_120);
        assert_eq!(rows[0].total_uploaded_bytes, 10_240);
    }

    #[test]
    fn policy_only_rows_include_only_live_restrictions() {
        let live_ip: IpAddr = "198.51.100.20".parse().unwrap();
        let expired_ip: IpAddr = "203.0.113.30".parse().unwrap();
        let mut state = state_with_peers(Vec::new());
        state.peer_policy = Arc::new(PeerPolicy {
            restrictions: HashMap::from([
                (
                    live_ip,
                    restriction(
                        test_now() + Duration::from_secs(300),
                        PeerRestrictionReason::Manual,
                    ),
                ),
                (
                    expired_ip,
                    restriction(test_now(), PeerRestrictionReason::Manual),
                ),
            ]),
        });

        let rows = build_peer_rows_at(&state, test_now());

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ip, live_ip);
        assert!(rows[0].tracked_indices.is_empty());
        assert!(rows[0].is_restricted());
    }

    #[test]
    fn restriction_reason_preserves_trigger_after_counters_reset() {
        let ip: IpAddr = "192.0.2.44".parse().unwrap();
        let peer = tracked_peer("192.0.2.44", "Mica Field Notes", 3);
        let mut state = state_with_peers(vec![peer]);
        state.peer_policy = Arc::new(PeerPolicy {
            restrictions: HashMap::from([(
                ip,
                restriction(
                    test_now() + Duration::from_secs(600),
                    PeerRestrictionReason::ExcessiveDownload {
                        downloaded_bytes: 125,
                        threshold_bytes: 100,
                    },
                ),
            )]),
        });

        let rows = build_peer_rows_at(&state, test_now());
        let evidence = rows[0].strongest_evidence();

        assert_eq!(evidence.kind, EvidenceKind::Download);
        assert_eq!(evidence.observed, 125);
        assert!(evidence.from_policy);
    }

    #[test]
    fn restricted_reconnect_evidence_leaves_a_column_gutter() {
        let ip: IpAddr = "192.0.2.46".parse().unwrap();
        let mut state = state_with_peers(Vec::new());
        state.ui.peer_management.filter = PeerManagementFilter::Restricted;
        state.peer_policy = Arc::new(PeerPolicy {
            restrictions: HashMap::from([(
                ip,
                restriction(
                    test_now() + Duration::from_secs(600),
                    PeerRestrictionReason::ReconnectChurn {
                        reconnects: 10,
                        threshold: 10,
                        window_secs: 10,
                    },
                ),
            )]),
        });

        let rows = build_peer_rows_at(&state, test_now());
        let evidence = rows[0].strongest_evidence().compact_label();

        assert_eq!(evidence, "Reconnect 10/10");
        assert!(fits_column(&evidence, EVIDENCE_CONTENT_WIDTH));
    }

    #[test]
    fn restricted_peer_countdown_is_combined_into_state() {
        let ip: IpAddr = "192.0.2.47".parse().unwrap();
        let mut state = state_with_peers(vec![tracked_peer("192.0.2.47", "Ember Field Notes", 47)]);
        state.peer_policy = Arc::new(PeerPolicy {
            restrictions: HashMap::from([(
                ip,
                restriction(
                    test_now() + Duration::from_secs(12 * 60),
                    PeerRestrictionReason::Manual,
                ),
            )]),
        });

        let rows = build_peer_rows_at(&state, test_now());

        assert_eq!(rows[0].state_column_label(test_now()), "BLOCKED 12m");
        assert!(!peer_columns()
            .iter()
            .any(|column| column.header == "Restricted"));
    }

    #[test]
    fn state_filters_keep_restricted_peers_out_of_active() {
        let ip: IpAddr = "192.0.2.45".parse().unwrap();
        let mut peer = tracked_peer("192.0.2.45", "Ember Field Notes", 8);
        peer.is_active = true;
        let mut state = state_with_peers(vec![peer]);
        state.peer_policy = Arc::new(PeerPolicy {
            restrictions: HashMap::from([(
                ip,
                restriction(
                    test_now() + Duration::from_secs(600),
                    PeerRestrictionReason::Manual,
                ),
            )]),
        });

        state.ui.peer_management.filter = PeerManagementFilter::Active;
        assert!(build_peer_rows_at(&state, test_now()).is_empty());
        state.ui.peer_management.filter = PeerManagementFilter::Restricted;
        assert_eq!(build_peer_rows_at(&state, test_now()).len(), 1);
    }

    #[test]
    fn filters_and_both_search_modes_use_peer_manager_fields() {
        let mut active = tracked_peer("192.0.2.1", "Quartz Archive", 4);
        active.is_active = true;
        active.clients = vec!["Unknown (ZZ1234)".to_string()];
        active.endpoints.push(PeerManagerEndpointView {
            address: "192.0.2.1:6881".to_string(),
            total_downloaded: 10,
            total_uploaded: 20,
        });
        let recent = tracked_peer("198.51.100.2", "Cinder Atlas", 5);
        let mut state = state_with_peers(vec![active, recent]);

        state.ui.peer_management.filter = PeerManagementFilter::Active;
        assert_eq!(build_peer_rows_at(&state, test_now()).len(), 1);

        state.ui.peer_management.filter = PeerManagementFilter::All;
        state.ui.peer_management.search_mode = SearchMode::Regex;
        state.ui.peer_management.search_query = "quartz|6881".to_string();
        assert_eq!(build_peer_rows_at(&state, test_now()).len(), 1);

        state.ui.peer_management.search_mode = SearchMode::Fuzzy;
        state.ui.peer_management.search_query = "CNDRATLS".to_string();
        assert_eq!(build_peer_rows_at(&state, test_now()).len(), 1);

        state.ui.peer_management.search_mode = SearchMode::Regex;
        state.ui.peer_management.search_query = "ZZ1234".to_string();
        assert_eq!(build_peer_rows_at(&state, test_now()).len(), 1);

        state.ui.peer_management.search_query = "[".to_string();
        assert!(build_peer_rows_at(&state, test_now()).is_empty());
        assert_eq!(
            peer_search_error(&state).as_deref(),
            Some("Invalid regular expression")
        );
    }

    #[test]
    fn cursor_index_stays_fixed_when_sorting_reorders_rows() {
        let mut lower = tracked_peer("192.0.2.11", "Opal Ledger", 6);
        lower.uploaded_evidence_bytes = 10;
        let mut higher = tracked_peer("192.0.2.12", "Sable Ledger", 7);
        higher.uploaded_evidence_bytes = 90;
        let mut state = state_with_peers(vec![lower, higher]);
        let evidence_index = peer_columns()
            .iter()
            .position(|column| column.id == PeerColumnId::Evidence)
            .unwrap();
        state.ui.peer_management.selected_index = 1;
        state.ui.peer_management.selected_column_index = evidence_index;
        state.ui.peer_management.sort_column_index = None;

        reduce_peer_management_action(&mut state, PeerManagementAction::SortBySelectedColumn);

        assert_eq!(state.ui.peer_management.selected_index, 1);
        assert_eq!(
            state.ui.peer_management.sort_column_index,
            Some(evidence_index)
        );
        let rows = build_peer_rows_at(&state, test_now());
        assert_eq!(rows[1].ip, "192.0.2.11".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn telemetry_reorder_preserves_cursor_position() {
        let mut first = tracked_peer("192.0.2.11", "Opal Ledger", 6);
        first.uploaded_evidence_bytes = 10;
        let mut second = tracked_peer("192.0.2.12", "Sable Ledger", 7);
        second.uploaded_evidence_bytes = 90;
        let mut state = state_with_peers(vec![first.clone(), second.clone()]);
        state.ui.peer_management.sort_column_index = peer_columns()
            .iter()
            .position(|column| column.id == PeerColumnId::Evidence);
        state.ui.peer_management.sort_direction = SortDirection::Descending;
        recompute_peer_management_derived(&mut state, test_now());
        reduce_peer_management_action(&mut state, PeerManagementAction::MoveDown);
        assert_eq!(state.ui.peer_management.selected_index, 1);
        assert_eq!(
            selected_peer_row(&state, &state.peer_management_derived.rows).map(|row| row.ip),
            Some(first.ip)
        );

        first.uploaded_evidence_bytes = 95;
        state.peer_manager_view = Arc::new(PeerManagerView {
            registered_torrents: 2,
            metrics_updates: 2,
            tracked_peers: vec![first, second.clone()],
        });
        recompute_peer_management_derived(&mut state, test_now());

        assert_eq!(state.ui.peer_management.selected_index, 1);
        assert_eq!(
            selected_peer_row(&state, &state.peer_management_derived.rows).map(|row| row.ip),
            Some(second.ip)
        );
    }

    #[test]
    fn telemetry_reorder_keeps_top_cursor_at_top() {
        let mut first = tracked_peer("192.0.2.11", "Opal Ledger", 6);
        first.uploaded_evidence_bytes = 10;
        let mut second = tracked_peer("192.0.2.12", "Sable Ledger", 7);
        second.uploaded_evidence_bytes = 90;
        let mut state = state_with_peers(vec![first.clone(), second.clone()]);
        state.ui.peer_management.sort_column_index = peer_columns()
            .iter()
            .position(|column| column.id == PeerColumnId::Evidence);
        state.ui.peer_management.sort_direction = SortDirection::Descending;
        recompute_peer_management_derived(&mut state, test_now());
        assert_eq!(state.peer_management_derived.rows[0].ip, second.ip);

        first.uploaded_evidence_bytes = 95;
        state.peer_manager_view = Arc::new(PeerManagerView {
            registered_torrents: 2,
            metrics_updates: 2,
            tracked_peers: vec![first.clone(), second],
        });
        recompute_peer_management_derived(&mut state, test_now());

        assert_eq!(state.ui.peer_management.selected_index, 0);
        assert_eq!(
            selected_peer_row(&state, &state.peer_management_derived.rows).map(|row| row.ip),
            Some(first.ip)
        );
    }

    #[test]
    fn client_metadata_is_aggregated_for_the_peer_row() {
        let mut first = tracked_peer("192.0.2.24", "Copper Notebook", 12);
        first.clients = vec!["Unknown (BB2000)".to_string()];
        let mut second = tracked_peer("192.0.2.24", "Quartz Notebook", 13);
        second.clients = vec!["Unknown (AA1000)".to_string()];
        let state = state_with_peers(vec![first, second]);

        let rows = build_peer_rows_at(&state, test_now());

        assert_eq!(rows[0].client_label, "Unknown (AA1000), Unknown (BB2000)");
    }

    #[test]
    fn resolved_clients_suppress_unknown_variants() {
        let mut unresolved = tracked_peer("192.0.2.26", "Copper Notebook", 14);
        unresolved.clients = vec!["Unknown".to_string(), "Unknown (AA1000)".to_string()];
        let mut resolved = tracked_peer("192.0.2.26", "Quartz Notebook", 15);
        resolved.clients = vec![
            "Unknown (BB2000)".to_string(),
            "Resolved Client 3000".to_string(),
        ];
        let state = state_with_peers(vec![unresolved, resolved.clone()]);

        let rows = build_peer_rows_at(&state, test_now());

        assert_eq!(rows[0].client_label, "Resolved Client 3000");
        assert_eq!(tracked_peer_client_label(&resolved), "Resolved Client 3000");
        assert_eq!(
            preferred_client_label(std::iter::empty::<&str>()),
            "Unknown"
        );
    }

    #[test]
    fn cursor_index_stays_fixed_when_live_peers_sort_before_it() {
        let first = tracked_peer("192.0.2.11", "Opal Ledger", 6);
        let second = tracked_peer("192.0.2.12", "Sable Ledger", 7);
        let mut state = state_with_peers(vec![first, second]);
        state.ui.peer_management.selected_index = 1;
        reduce_peer_management_action(&mut state, PeerManagementAction::ToggleDetails);
        assert_eq!(
            state.ui.peer_management.details_peer_ip,
            Some("192.0.2.12".parse::<IpAddr>().unwrap())
        );

        Arc::make_mut(&mut state.peer_manager_view)
            .tracked_peers
            .push(tracked_peer("192.0.2.1", "Cinder Ledger", 8));
        let rows = build_peer_rows_at(&state, test_now());

        assert_eq!(state.ui.peer_management.selected_index, 1);
        assert_eq!(selected_peer_index(&state, &rows), Some(1));
        assert_eq!(
            selected_peer_row(&state, &rows).unwrap().ip,
            "192.0.2.11".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            pinned_peer_detail_row(&state, &rows).unwrap().ip,
            "192.0.2.12".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn cursor_index_clamps_when_live_rows_shrink() {
        let first = tracked_peer("192.0.2.11", "Opal Ledger", 6);
        let second = tracked_peer("192.0.2.12", "Sable Ledger", 7);
        let mut state = state_with_peers(vec![first, second]);
        state.ui.peer_management.selected_index = 1;
        Arc::make_mut(&mut state.peer_manager_view)
            .tracked_peers
            .truncate(1);
        let rows = build_peer_rows_at(&state, test_now());

        assert_eq!(selected_peer_index(&state, &rows), Some(0));
    }

    #[test]
    fn navigation_uses_the_existing_projection_without_recomputing() {
        let first = tracked_peer("192.0.2.11", "Cinder Atlas", 6);
        let second = tracked_peer("192.0.2.12", "Quartz Archive", 7);
        let mut state = state_with_peers(vec![first, second]);
        take_peer_derived_recompute_count();

        reduce_peer_management_action(&mut state, PeerManagementAction::MoveDown);

        assert_eq!(take_peer_derived_recompute_count(), 0);
        assert_eq!(state.ui.peer_management.selected_index, 1);
    }

    #[test]
    fn filter_changes_recompute_the_projection_once() {
        let mut active = tracked_peer("192.0.2.11", "Cinder Atlas", 6);
        active.is_active = true;
        let recent = tracked_peer("192.0.2.12", "Quartz Archive", 7);
        let mut state = state_with_peers(vec![active, recent]);
        take_peer_derived_recompute_count();

        reduce_peer_management_action(&mut state, PeerManagementAction::FilterNext);

        assert_eq!(take_peer_derived_recompute_count(), 1);
        assert_eq!(state.peer_management_derived.rows.len(), 1);
        assert!(state.peer_management_derived.rows[0].is_active());
    }

    #[test]
    fn restriction_expiry_recomputes_only_after_its_deadline() {
        let mut state = state_with_peers(Vec::new());
        state.peer_policy = Arc::new(PeerPolicy {
            restrictions: HashMap::from([(
                "192.0.2.20".parse().unwrap(),
                restriction(
                    test_now() + Duration::from_secs(5),
                    PeerRestrictionReason::Manual,
                ),
            )]),
        });
        recompute_peer_management_derived(&mut state, test_now());
        take_peer_derived_recompute_count();

        refresh_peer_management_expiries(&mut state, test_now() + Duration::from_secs(4));
        assert_eq!(take_peer_derived_recompute_count(), 0);
        assert_eq!(state.peer_management_derived.rows.len(), 1);

        refresh_peer_management_expiries(&mut state, test_now() + Duration::from_secs(5));
        assert_eq!(take_peer_derived_recompute_count(), 1);
        assert!(state.peer_management_derived.rows.is_empty());
    }

    #[test]
    fn last_seen_is_default_sort_and_torrents_are_numeric() {
        let mut two_first = tracked_peer("192.0.2.21", "Zephyr Notebook", 9);
        two_first.uploaded_evidence_bytes = 90;
        two_first.last_seen = Some(test_now() - Duration::from_secs(90));
        let mut one = tracked_peer("192.0.2.22", "Amber Notebook", 10);
        one.is_active = true;
        one.last_seen = Some(test_now() - Duration::from_secs(10));
        let mut two_second = tracked_peer("192.0.2.21", "Cinder Notebook", 11);
        two_second.uploaded_evidence_bytes = 80;
        two_second.last_seen = Some(test_now() - Duration::from_secs(60));
        let state = state_with_peers(vec![two_first, one, two_second]);

        assert_eq!(state.ui.peer_management.selected_column_index, 9);
        assert_eq!(state.ui.peer_management.sort_column_index, Some(9));
        assert_eq!(
            state.ui.peer_management.sort_direction,
            SortDirection::Descending
        );
        let rows = build_peer_rows_at(&state, test_now());
        assert_eq!(rows[0].torrent_count, 1);
        assert_eq!(rows[1].torrent_count, 2);
        assert!(rows[0].is_active());
        assert_eq!(peer_torrents_label(&rows[0]), "1");
        assert_eq!(peer_torrents_label(&rows[1]), "2");
    }

    #[test]
    fn state_sort_keeps_blocked_peers_first_in_both_directions() {
        let blocked_ip: IpAddr = "192.0.2.41".parse().unwrap();
        let blocked = tracked_peer("192.0.2.41", "Ember Field Notes", 41);
        let mut active = tracked_peer("192.0.2.42", "Quartz Field Notes", 42);
        active.is_active = true;
        let recent = tracked_peer("192.0.2.43", "Copper Field Notes", 43);
        let mut state = state_with_peers(vec![recent, blocked, active]);
        state.peer_policy = Arc::new(PeerPolicy {
            restrictions: HashMap::from([(
                blocked_ip,
                restriction(
                    test_now() + Duration::from_secs(600),
                    PeerRestrictionReason::Manual,
                ),
            )]),
        });
        state.ui.peer_management.sort_column_index = peer_columns()
            .iter()
            .position(|column| column.id == PeerColumnId::State);

        state.ui.peer_management.sort_direction = SortDirection::Descending;
        let descending = build_peer_rows_at(&state, test_now());
        assert_eq!(descending[0].ip, blocked_ip);
        assert!(descending[1].is_active());

        state.ui.peer_management.sort_direction = SortDirection::Ascending;
        let ascending = build_peer_rows_at(&state, test_now());
        assert_eq!(ascending[0].ip, blocked_ip);
        assert!(!ascending[1].is_active());
    }

    #[test]
    fn primary_sort_ties_use_last_seen_newest_first() {
        let mut older = tracked_peer("192.0.2.31", "Copper Notebook", 31);
        older.is_active = true;
        older.last_seen = Some(test_now() - Duration::from_secs(90));
        let mut newer = tracked_peer("192.0.2.32", "Quartz Notebook", 32);
        newer.is_active = true;
        newer.last_seen = Some(test_now() - Duration::from_secs(5));
        let state = state_with_peers(vec![older, newer]);

        let rows = build_peer_rows_at(&state, test_now());

        assert_eq!(rows[0].ip, "192.0.2.32".parse::<IpAddr>().unwrap());
        assert_eq!(rows[1].ip, "192.0.2.31".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn peer_details_separate_titles_and_explain_reconnect_evidence() {
        let mut peer = tracked_peer("192.0.2.23", "Copper Notebook", 11);
        peer.reconnect_count = 3;
        let state = state_with_peers(vec![peer.clone()]);
        let ctx = ThemeContext::new(state.theme, 0.0);

        let lines = tracked_peer_detail_lines(&peer, 1, false, 80, &ctx);
        assert_eq!(line_text(&lines[0]), "01  Copper Notebook");
        assert!(line_text(&lines[1]).contains("Hash "));
        assert_eq!(line_text(&lines[2]), "  Client Unknown");
        assert_eq!(line_text(&lines[5]), "  Reconnects  3 / 10  •  10s window");

        let evidence = PeerEvidence {
            kind: EvidenceKind::Reconnect,
            observed: 3,
            threshold: 10,
            from_policy: false,
        };
        assert_eq!(evidence.compact_label(), "Reconnect 3/10");
    }

    #[test]
    fn fixed_peer_column_labels_stay_within_declared_widths() {
        for state in ["ACTIVE", "RECENT", "BLOCKED"] {
            assert!(fits_column(state, STATE_COLUMN_WIDTH));
        }

        let state = state_with_peers(vec![tracked_peer("192.0.2.90", "Cinder Atlas", 90)]);
        let mut rows = build_peer_rows_at(&state, test_now());
        rows[0].torrent_count = usize::MAX;
        let torrents = peer_torrents_label(&rows[0]);
        assert!(fits_column(&torrents, TORRENTS_COLUMN_WIDTH));
        if usize::BITS > 32 {
            assert_eq!(torrents, "999999999+");
        }

        assert!(fits_column(&compact_count(u64::MAX), CONNECTS_COLUMN_WIDTH));
        assert!(fits_column(
            &compact_count(u64::MAX),
            DISCONNECTS_COLUMN_WIDTH
        ));
        assert!(fits_column(
            &compact_transfer_bytes(u64::MAX),
            TRANSFER_COLUMN_WIDTH
        ));

        let reconnect = PeerEvidence {
            kind: EvidenceKind::Reconnect,
            observed: u64::MAX,
            threshold: u64::MAX,
            from_policy: false,
        }
        .compact_label();
        assert_eq!(reconnect, "R 18E/18E");
        assert!(fits_column(&reconnect, EVIDENCE_CONTENT_WIDTH));

        let upload = PeerEvidence {
            kind: EvidenceKind::Upload,
            observed: u64::MAX,
            threshold: 1,
            from_policy: false,
        }
        .compact_label();
        assert_eq!(upload, "UL 1.8Z%");
        assert!(fits_column(&upload, EVIDENCE_CONTENT_WIDTH));

        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 * 86_400);
        let last_seen = format_elapsed(now, SystemTime::UNIX_EPOCH);
        assert_eq!(last_seen, ">999d ago");
        assert!(fits_column(&last_seen, LAST_SEEN_COLUMN_WIDTH));

        let restricted = format_remaining(now, now + Duration::from_secs(1_000_000 * 86_400));
        assert_eq!(restricted, ">99d left");
        assert!(fits_column(&restricted, RESTRICTION_REMAINING_WIDTH));

        for column in peer_columns()
            .iter()
            .filter(|column| !matches!(column.id, PeerColumnId::Address | PeerColumnId::Client))
        {
            let header = format!("{} ▼", column.header);
            assert!(fits_column(&header, column.min_width), "{header}");
        }
    }

    #[test]
    fn selected_sort_header_underlines_only_the_column_label() {
        let state = state_with_peers(Vec::new());
        let ctx = ThemeContext::new(state.theme, 0.0);
        let column = peer_columns()
            .iter()
            .find(|column| matches!(column.id, PeerColumnId::Torrents))
            .expect("torrents column");

        let spans = peer_column_header_spans(column, true, true, SortDirection::Descending, &ctx);

        assert_eq!(spans.len(), 2);
        assert!(spans[0].style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(!spans[1].style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn details_search_filters_only_torrent_names_for_the_pinned_peer() {
        let first = tracked_peer("192.0.2.27", "Quartz Archive", 27);
        let second = tracked_peer("192.0.2.27", "Cinder Atlas", 28);
        let mut state = state_with_peers(vec![first, second]);
        state.screen_area = Rect::new(0, 0, 100, 30);
        reduce_peer_management_action(&mut state, PeerManagementAction::ToggleDetails);

        assert_eq!(
            map_key_to_peer_management_action(KeyCode::Char('/'), &state),
            Some(PeerManagementAction::StartDetailsSearch)
        );
        reduce_peer_management_action(&mut state, PeerManagementAction::StartDetailsSearch);
        for character in "quartz".chars() {
            reduce_peer_management_action(
                &mut state,
                PeerManagementAction::DetailsSearchInsert(character),
            );
        }
        reduce_peer_management_action(&mut state, PeerManagementAction::DetailsSearchCommit);

        let rows = build_peer_rows_at(&state, test_now());
        let row = pinned_peer_detail_row(&state, &rows).unwrap();
        let matching = matching_detail_torrents(&state, row);
        let ctx = ThemeContext::new(state.theme, 0.0);
        let rendered = peer_detail_lines(&state, row, test_now(), 80, &ctx)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].torrent_name, "Quartz Archive");
        assert!(rendered.contains("1 of 2"));
        assert!(rendered.contains("Quartz Archive"));
        assert!(!rendered.contains("Cinder Atlas"));
        assert_eq!(
            state.ui.peer_management.details_peer_ip,
            Some("192.0.2.27".parse().unwrap())
        );
    }

    #[test]
    fn peer_details_scroll_through_large_torrent_histories() {
        let peers = (0..40)
            .map(|index| {
                tracked_peer(
                    "192.0.2.24",
                    &format!("Archive Volume {}", index + 1),
                    index as u8,
                )
            })
            .collect::<Vec<_>>();
        let mut state = state_with_peers(peers);
        state.screen_area = Rect::new(0, 0, 140, 36);
        reduce_peer_management_action(&mut state, PeerManagementAction::ToggleDetails);
        let rows = build_peer_rows_at(&state, test_now());
        let row = &rows[0];
        let ctx = ThemeContext::new(state.theme, 0.0);

        assert_eq!(row.torrent_count, 40);
        assert_eq!(
            peer_detail_lines(&state, row, test_now(), 48, &ctx).len(),
            peer_detail_line_count(&state, row)
        );
        assert!(peer_details_max_scroll_at(&state, test_now()) > 0);

        let before_scroll = rendered_peer_details(&state);
        state.ui.needs_redraw = false;
        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            &mut state,
        );
        assert_eq!(state.ui.peer_management.details_scroll_offset, 1);
        assert!(state.ui.needs_redraw);
        assert_ne!(rendered_peer_details(&state), before_scroll);
        reduce_peer_management_action(&mut state, PeerManagementAction::ScrollDetailsPageDown);
        assert!(state.ui.peer_management.details_scroll_offset > 1);
        reduce_peer_management_action(&mut state, PeerManagementAction::ScrollDetailsUp);
        assert!(state.ui.peer_management.details_scroll_offset > 0);
    }

    #[test]
    fn details_overlay_owns_plain_navigation_keys_at_wide_widths() {
        let mut state = state_with_peers(vec![tracked_peer("192.0.2.25", "Copper Notebook", 25)]);
        state.screen_area = Rect::new(0, 0, 150, 40);
        state.ui.peer_management.show_details = true;

        assert_eq!(
            map_key_to_peer_management_action(KeyCode::Down, &state),
            Some(PeerManagementAction::ScrollDetailsDown)
        );
        assert_eq!(
            map_key_to_peer_management_action(KeyCode::Enter, &state),
            Some(PeerManagementAction::CloseDetails)
        );
    }

    #[test]
    fn search_panel_keys_take_precedence_while_visible() {
        let mut state = state_with_peers(Vec::new());
        assert_eq!(
            map_key_to_peer_management_action(KeyCode::Tab, &state),
            Some(PeerManagementAction::FilterNext)
        );

        state.ui.peer_management.is_searching = true;
        assert_eq!(
            map_key_to_peer_management_action(KeyCode::Tab, &state),
            Some(PeerManagementAction::ToggleSearchMode)
        );
        assert_eq!(
            map_key_to_peer_management_action(KeyCode::Esc, &state),
            Some(PeerManagementAction::SearchCancel)
        );

        state.ui.peer_management.is_searching = false;
        state.ui.peer_management.search_query = "quartz".to_string();
        assert_eq!(
            map_key_to_peer_management_action(KeyCode::Tab, &state),
            Some(PeerManagementAction::ToggleSearchMode)
        );
    }

    #[test]
    fn privacy_masks_numeric_peer_identity_and_endpoints() {
        let ip: IpAddr = "203.0.113.55".parse().unwrap();
        let address = "203.0.113.55:51413";

        let masked_ip = display_ip(ip, true);
        let masked_endpoint = display_endpoint(address, true);

        assert!(!masked_ip.contains("203.0.113.55"));
        assert!(!masked_endpoint.contains("203.0.113.55"));
        assert_ne!(masked_ip, display_ip(ip, false));
        assert_ne!(masked_endpoint, display_endpoint(address, false));

        let mut state = state_with_peers(Vec::new());
        state.ui.peer_management.search_query = address.to_string();
        reduce_peer_management_action(&mut state, PeerManagementAction::TogglePrivacy);
        assert!(state.anonymize_torrent_names);
        assert!(state.ui.peer_management.search_query.is_empty());
        assert!(!state.ui.peer_management.is_searching);
    }

    #[test]
    fn privacy_pseudonyms_preserve_the_full_stable_hash() {
        let pseudonyms = (0..4_096_u128)
            .map(|index| {
                let ip = IpAddr::V6(std::net::Ipv6Addr::from(
                    0x2001_0db8_0000_0000_0000_0000_0000_0000_u128 + index,
                ));
                display_ip(ip, true)
            })
            .collect::<BTreeSet<_>>();

        assert_eq!(pseudonyms.len(), 4_096);
        assert!(pseudonyms
            .iter()
            .all(|pseudonym| pseudonym.len() == "peer-".len() + 16));
    }

    #[test]
    fn privacy_search_matches_the_values_displayed_to_the_user() {
        let mut peer = tracked_peer("192.0.2.61", "Quartz Archive", 61);
        peer.endpoints.push(PeerManagerEndpointView {
            address: "192.0.2.61:6881".to_string(),
            total_downloaded: 10,
            total_uploaded: 20,
        });
        let displayed_ip = display_ip(peer.ip, true);
        let displayed_endpoint = display_endpoint(&peer.endpoints[0].address, true);
        let displayed_torrent = display_torrent_name(&peer.torrent_name, true);
        let mut state = state_with_peers(vec![peer]);
        state.anonymize_torrent_names = true;

        for query in [&displayed_ip, &displayed_endpoint, &displayed_torrent] {
            state.ui.peer_management.search_query.clone_from(query);
            assert_eq!(build_peer_rows_at(&state, test_now()).len(), 1);
        }

        state.ui.peer_management.search_query.clear();
        state
            .ui
            .peer_management
            .details_search_query
            .clone_from(&displayed_torrent);
        let rows = build_peer_rows_at(&state, test_now());
        assert_eq!(matching_detail_torrents(&state, &rows[0]).len(), 1);

        state.ui.peer_management.details_search_query.clear();
        state.ui.peer_management.search_query = "192.0.2.61".to_string();
        assert!(build_peer_rows_at(&state, test_now()).is_empty());
    }

    #[test]
    fn subsecond_last_seen_is_not_rounded_to_now() {
        let now = test_now();

        assert_eq!(format_elapsed(now, now), "<1s ago");
        assert_eq!(
            format_elapsed(now, now - Duration::from_millis(999)),
            "<1s ago"
        );
        assert_eq!(format_elapsed(now, now - Duration::from_secs(1)), "1s ago");
    }

    #[test]
    fn evidence_ratios_are_compared_without_truncation() {
        let left = PeerEvidence {
            kind: EvidenceKind::Upload,
            observed: 1_000_001,
            threshold: 1_000_000,
            from_policy: false,
        };
        let right = PeerEvidence {
            kind: EvidenceKind::Download,
            observed: 1_000_000,
            threshold: 999_999,
            from_policy: false,
        };

        assert_eq!(compare_evidence_ratio(&left, &right), Ordering::Less);
    }

    #[test]
    fn narrow_table_keeps_state_address_and_evidence() {
        let (_, visible) = compute_visible_peer_management_columns(64);
        let columns = peer_columns();
        let ids = visible
            .into_iter()
            .map(|index| columns[index].id)
            .collect::<Vec<_>>();

        assert!(ids.contains(&PeerColumnId::State));
        assert!(ids.contains(&PeerColumnId::Address));
        assert!(ids.contains(&PeerColumnId::Evidence));
        assert!(!ids.contains(&PeerColumnId::Torrents));
        assert!(!ids.contains(&PeerColumnId::Client));
    }

    #[test]
    fn wide_table_splits_remaining_space_evenly_between_address_and_client() {
        let (constraints, visible) = compute_visible_peer_management_columns(160);
        let columns = peer_columns();
        let constraint_for = |column_id| {
            visible
                .iter()
                .position(|index| columns[*index].id == column_id)
                .map(|position| constraints[position])
        };

        assert_eq!(
            constraint_for(PeerColumnId::Address),
            Some(Constraint::Fill(1))
        );
        assert_eq!(
            constraint_for(PeerColumnId::Client),
            Some(Constraint::Fill(1))
        );
    }

    #[test]
    fn standard_table_exposes_all_peer_activity_columns() {
        let (_, visible) = compute_visible_peer_management_columns(140);
        let columns = peer_columns();
        let ids = visible
            .into_iter()
            .map(|index| columns[index].id)
            .collect::<Vec<_>>();

        assert!(ids.contains(&PeerColumnId::Connects));
        assert!(ids.contains(&PeerColumnId::Disconnects));
        assert!(ids.contains(&PeerColumnId::Downloaded));
        assert!(ids.contains(&PeerColumnId::Uploaded));
    }

    #[test]
    fn selected_peer_row_uses_torrent_manager_warning_highlight() {
        let state = state_with_peers(Vec::new());
        let ctx = ThemeContext::new(state.theme, 0.0);

        let style = peer_row_highlight_style(&ctx);

        assert_eq!(style.fg, Some(ctx.state_warning()));
        assert_eq!(style.bg, None);
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }
}
