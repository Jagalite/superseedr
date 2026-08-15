// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{
    torrent_completion_percent, App, AppCommand, AppMode, AppState, TagInputMode,
    TagManagementPane, TagManagementUiState, TorrentControlState, TorrentDisplayState,
    TorrentPreviewPayload,
};
use crate::config::{Settings, TorrentSettings};
use crate::integrations::control::ControlRequest;
use crate::tags::{TagDefinition, TagId};
use crate::theme::ThemeContext;
use crate::torrent_identity::info_hash_from_torrent_source;
use crate::tui::action_style::{footer_key_style, ActionTone};
use crate::tui::app_command::spawn_app_command_sender;
use crate::tui::formatters::{centered_rect, format_bytes, sanitize_text, truncate_with_ellipsis};
use crate::tui::screen_context::ScreenContext;
use crate::tui::screens::{input_panel::draw_prompt_panel, tag_picker};
use crate::tui::tree::RawNode;
use ratatui::crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use ratatui::Frame;

const MAX_FILES_PER_TORRENT: usize = 3;
const MAX_TAG_NAME_WIDTH: usize = 22;

#[derive(Clone, Debug, PartialEq, Eq)]
struct FilePreviewRow {
    path: String,
    size: u64,
}

struct ResultItemContext<'a> {
    settings: &'a Settings,
    active_tag_id: Option<TagId>,
    assignment_mode: bool,
    width: u16,
    theme: &'a ThemeContext,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeleteConfirmationInput {
    Confirm,
    Cancel,
    Consume,
}

fn delete_confirmation_input(key_code: KeyCode) -> DeleteConfirmationInput {
    match key_code {
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
            DeleteConfirmationInput::Confirm
        }
        KeyCode::Esc | KeyCode::Char('q') => DeleteConfirmationInput::Cancel,
        _ => DeleteConfirmationInput::Consume,
    }
}

pub fn initialize(app: &mut App) {
    let state = &mut app.app_state.ui.tag_management;
    state.selected_tag_index = 0;
    state.selected_torrent_index = 0;
    state.active_pane = TagManagementPane::Tags;
    state.assignment_mode = false;
    state.input_mode = None;
    state.input_buffer.clear();
    state.search_query.clear();
    state.delete_confirm_tag_id = None;
    state.status_message = None;
    clamp_selection(app);
}

pub fn handle_event(event: CrosstermEvent, app: &mut App) -> bool {
    if !matches!(app.app_state.mode, AppMode::TagManagement) {
        return false;
    }
    if tag_picker::is_open(&app.app_state) {
        return tag_picker::handle_event(event, app);
    }
    let CrosstermEvent::Key(key) = event else {
        return false;
    };
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        || !matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
    {
        return false;
    }

    if app
        .app_state
        .ui
        .tag_management
        .delete_confirm_tag_id
        .is_some()
    {
        match delete_confirmation_input(key.code) {
            DeleteConfirmationInput::Confirm => {
                if let Some(tag_id) = app.app_state.ui.tag_management.delete_confirm_tag_id.take() {
                    submit_request(app, ControlRequest::DeleteTag { tag_id });
                }
            }
            DeleteConfirmationInput::Cancel => {
                app.app_state.ui.tag_management.delete_confirm_tag_id.take();
            }
            DeleteConfirmationInput::Consume => {}
        }
        app.app_state.ui.needs_redraw = true;
        return true;
    }

    if app.app_state.ui.tag_management.input_mode.is_some() {
        return handle_input_key(key.code, app);
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('L') => {
            app.app_state.mode = AppMode::Normal;
        }
        KeyCode::Tab | KeyCode::BackTab => toggle_focus(app),
        KeyCode::Left | KeyCode::Char('h') => move_selection(app, -1),
        KeyCode::Right | KeyCode::Char('l') => move_selection(app, 1),
        KeyCode::Up | KeyCode::Char('k') => move_selection(app, -1),
        KeyCode::Down | KeyCode::Char('j') => move_selection(app, 1),
        KeyCode::Home => move_selection_to_edge(app, false),
        KeyCode::End => move_selection_to_edge(app, true),
        KeyCode::Char('0') => {
            let state = &mut app.app_state.ui.tag_management;
            state.selected_tag_index = 0;
            state.selected_torrent_index = 0;
            state.assignment_mode = false;
            state.status_message = Some("Showing all torrents".to_string());
        }
        KeyCode::Char('m') => toggle_assignment_mode(app),
        KeyCode::Char('t') => open_picker_for_selected_torrent(app),
        KeyCode::Char('/') => {
            let state = &mut app.app_state.ui.tag_management;
            state.input_buffer = state.search_query.clone();
            state.input_mode = Some(TagInputMode::Search);
        }
        KeyCode::Char('n') => {
            let state = &mut app.app_state.ui.tag_management;
            state.input_buffer.clear();
            state.input_mode = Some(TagInputMode::Create);
        }
        KeyCode::Char('e') => begin_rename(app),
        KeyCode::Char('d') => begin_delete(app),
        KeyCode::Enter => {
            if matches!(
                app.app_state.ui.tag_management.active_pane,
                TagManagementPane::Tags
            ) {
                app.app_state.ui.tag_management.active_pane = TagManagementPane::Torrents;
            }
        }
        KeyCode::Char(' ') => toggle_selected_assignment(app),
        _ => return false,
    }

    clamp_selection(app);
    app.app_state.ui.needs_redraw = true;
    true
}

fn handle_input_key(key_code: KeyCode, app: &mut App) -> bool {
    match key_code {
        KeyCode::Esc => {
            let state = &mut app.app_state.ui.tag_management;
            state.input_mode = None;
            state.input_buffer.clear();
        }
        KeyCode::Backspace => {
            app.app_state.ui.tag_management.input_buffer.pop();
        }
        KeyCode::Enter => {
            let state = &mut app.app_state.ui.tag_management;
            let Some(mode) = state.input_mode.take() else {
                return true;
            };
            let value = std::mem::take(&mut state.input_buffer);
            match mode {
                TagInputMode::Search => {
                    state.search_query = value;
                    state.selected_torrent_index = 0;
                }
                TagInputMode::Create => {
                    submit_request(app, ControlRequest::CreateTag { name: value });
                }
                TagInputMode::Rename(tag_id) => {
                    submit_request(
                        app,
                        ControlRequest::RenameTag {
                            tag_id,
                            new_name: value,
                        },
                    );
                }
            }
        }
        KeyCode::Char(character) => {
            app.app_state.ui.tag_management.input_buffer.push(character);
        }
        _ => return false,
    }
    clamp_selection(app);
    app.app_state.ui.needs_redraw = true;
    true
}

fn toggle_focus(app: &mut App) {
    let state = &mut app.app_state.ui.tag_management;
    state.active_pane = match state.active_pane {
        TagManagementPane::Tags => TagManagementPane::Torrents,
        TagManagementPane::Torrents => TagManagementPane::Tags,
    };
}

fn toggle_assignment_mode(app: &mut App) {
    let selected = selected_tag(&app.client_configs, &app.app_state.ui.tag_management);
    let Some(tag) = selected else {
        app.app_state.ui.tag_management.status_message =
            Some("Choose a tag before managing assignments".to_string());
        return;
    };
    if !tag.origin.is_manual() {
        app.app_state.ui.tag_management.status_message =
            Some("Generated tags cannot be changed manually".to_string());
        return;
    }
    let state = &mut app.app_state.ui.tag_management;
    state.assignment_mode = !state.assignment_mode;
    state.selected_torrent_index = 0;
    state.active_pane = TagManagementPane::Torrents;
    state.status_message = Some(if state.assignment_mode {
        "Assignment mode: Space toggles the selected tag".to_string()
    } else {
        "Showing torrents that use this tag".to_string()
    });
}

fn open_picker_for_selected_torrent(app: &mut App) {
    let state = &app.app_state.ui.tag_management;
    let info_hash = visible_torrents(&app.client_configs, &app.app_state, state)
        .get(state.selected_torrent_index)
        .and_then(|torrent| info_hash_from_torrent_source(&torrent.torrent_or_magnet))
        .map(|hash| hash.to_vec());
    if let Some(info_hash) = info_hash {
        tag_picker::open(app, vec![info_hash]);
    } else {
        app.app_state.ui.tag_management.status_message = Some("No torrent is selected".to_string());
    }
}

fn begin_rename(app: &mut App) {
    let selected = selected_tag(&app.client_configs, &app.app_state.ui.tag_management)
        .map(|tag| (tag.id, tag.name.clone()));
    let Some((tag_id, name)) = selected else {
        app.app_state.ui.tag_management.status_message =
            Some("Choose a tag before renaming".to_string());
        return;
    };
    if app
        .client_configs
        .tag_catalog
        .get(tag_id)
        .is_some_and(|tag| !tag.origin.is_manual())
    {
        app.app_state.ui.tag_management.status_message =
            Some("Generated tags cannot be renamed manually".to_string());
        return;
    }
    let state = &mut app.app_state.ui.tag_management;
    state.input_buffer = name;
    state.input_mode = Some(TagInputMode::Rename(tag_id));
}

fn begin_delete(app: &mut App) {
    let selected = selected_tag(&app.client_configs, &app.app_state.ui.tag_management)
        .map(|tag| (tag.id, tag.origin.is_manual()));
    let Some((tag_id, is_manual)) = selected else {
        app.app_state.ui.tag_management.status_message =
            Some("Choose a tag before deleting".to_string());
        return;
    };
    if !is_manual {
        app.app_state.ui.tag_management.status_message =
            Some("Generated tags cannot be deleted manually".to_string());
        return;
    }
    app.app_state.ui.tag_management.delete_confirm_tag_id = Some(tag_id);
}

fn submit_request(app: &mut App, request: ControlRequest) {
    app.app_state.ui.tag_management.status_message =
        Some(format!("Submitted {} request", request.action_name()));
    spawn_app_command_sender(
        app.app_command_tx.clone(),
        app.shutdown_tx.subscribe(),
        AppCommand::SubmitControlRequest(request),
    );
}

fn toggle_selected_assignment(app: &mut App) {
    let state = &app.app_state.ui.tag_management;
    let Some(tag) = selected_tag(&app.client_configs, state) else {
        app.app_state.ui.tag_management.status_message =
            Some("Choose a tag filter before changing assignments".to_string());
        return;
    };
    if !tag.origin.is_manual() {
        app.app_state.ui.tag_management.status_message =
            Some("Generated tags cannot be changed manually".to_string());
        return;
    }
    let tag_id = tag.id;
    let selected_torrent = visible_torrents(&app.client_configs, &app.app_state, state)
        .get(state.selected_torrent_index)
        .copied();
    let Some(torrent) = selected_torrent else {
        app.app_state.ui.tag_management.status_message = Some("No torrent is selected".to_string());
        return;
    };
    let Some(info_hash) = info_hash_from_torrent_source(&torrent.torrent_or_magnet) else {
        app.app_state.ui.tag_management.status_message =
            Some("Selected torrent has no resolvable info hash".to_string());
        return;
    };
    let info_hash_hex = hex::encode(info_hash);
    let request = if torrent.tag_ids.contains(&tag_id) {
        ControlRequest::RemoveTag {
            tag_id,
            info_hash_hex,
        }
    } else {
        ControlRequest::AssignTag {
            tag_id,
            info_hash_hex,
        }
    };
    submit_request(app, request);
}

fn move_selection(app: &mut App, delta: isize) {
    let state = &mut app.app_state.ui.tag_management;
    match state.active_pane {
        TagManagementPane::Tags => {
            state.selected_tag_index = state.selected_tag_index.saturating_add_signed(delta);
            state.selected_torrent_index = 0;
            state.assignment_mode = false;
        }
        TagManagementPane::Torrents => {
            state.selected_torrent_index =
                state.selected_torrent_index.saturating_add_signed(delta);
        }
    }
}

fn move_selection_to_edge(app: &mut App, last: bool) {
    let state = &app.app_state.ui.tag_management;
    let tag_end = app.client_configs.tag_catalog.tags.len();
    let torrent_end = visible_torrents(&app.client_configs, &app.app_state, state)
        .len()
        .saturating_sub(1);
    let state = &mut app.app_state.ui.tag_management;
    match state.active_pane {
        TagManagementPane::Tags => {
            state.selected_tag_index = if last { tag_end } else { 0 };
            state.selected_torrent_index = 0;
            state.assignment_mode = false;
        }
        TagManagementPane::Torrents => {
            state.selected_torrent_index = if last { torrent_end } else { 0 };
        }
    }
}

fn clamp_selection(app: &mut App) {
    let tag_end = app.client_configs.tag_catalog.tags.len();
    app.app_state.ui.tag_management.selected_tag_index = app
        .app_state
        .ui
        .tag_management
        .selected_tag_index
        .min(tag_end);
    let torrent_end = visible_torrents(
        &app.client_configs,
        &app.app_state,
        &app.app_state.ui.tag_management,
    )
    .len()
    .saturating_sub(1);
    app.app_state.ui.tag_management.selected_torrent_index = app
        .app_state
        .ui
        .tag_management
        .selected_torrent_index
        .min(torrent_end);
}

fn selected_tag<'a>(
    settings: &'a Settings,
    state: &TagManagementUiState,
) -> Option<&'a TagDefinition> {
    let index = state.selected_tag_index.checked_sub(1)?;
    settings.tag_catalog.tags.get(index)
}

fn selected_tag_id(settings: &Settings, state: &TagManagementUiState) -> Option<TagId> {
    selected_tag(settings, state).map(|tag| tag.id)
}

fn visible_torrents<'a>(
    settings: &'a Settings,
    app_state: &AppState,
    state: &TagManagementUiState,
) -> Vec<&'a TorrentSettings> {
    let selected_tag = selected_tag_id(settings, state);
    let query = state.search_query.trim().to_lowercase();
    settings
        .torrents
        .iter()
        .filter(|torrent| {
            (state.assignment_mode
                || selected_tag.is_none_or(|tag_id| torrent.tag_ids.contains(&tag_id)))
                && torrent_matches_query(torrent, app_state, &query)
        })
        .collect()
}

fn torrent_matches_query(torrent: &TorrentSettings, app_state: &AppState, query: &str) -> bool {
    if query.is_empty() || torrent.name.to_lowercase().contains(query) {
        return true;
    }
    runtime_torrent(torrent, app_state)
        .is_some_and(|runtime| preview_tree_contains_query(&runtime.file_preview_tree, query))
}

fn runtime_torrent<'a>(
    torrent: &TorrentSettings,
    app_state: &'a AppState,
) -> Option<&'a TorrentDisplayState> {
    let info_hash = info_hash_from_torrent_source(&torrent.torrent_or_magnet)?;
    app_state.torrents.get(info_hash.as_slice())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FilePreviewSummary {
    total_files: usize,
    rows: Vec<FilePreviewRow>,
}

fn preview_tree_contains_query(nodes: &[RawNode<TorrentPreviewPayload>], query: &str) -> bool {
    for node in nodes {
        if node.is_dir {
            if preview_tree_contains_query(&node.children, query) {
                return true;
            }
        } else if node
            .full_path
            .to_string_lossy()
            .to_lowercase()
            .contains(query)
        {
            return true;
        }
    }
    false
}

fn count_preview_files(nodes: &[RawNode<TorrentPreviewPayload>]) -> usize {
    nodes
        .iter()
        .map(|node| {
            if node.is_dir {
                count_preview_files(&node.children)
            } else {
                1
            }
        })
        .sum()
}

fn collect_first_preview_files(
    nodes: &[RawNode<TorrentPreviewPayload>],
    rows: &mut Vec<FilePreviewRow>,
) {
    for node in nodes {
        if rows.len() >= MAX_FILES_PER_TORRENT {
            return;
        }
        if node.is_dir {
            collect_first_preview_files(&node.children, rows);
        } else {
            rows.push(FilePreviewRow {
                path: node.full_path.to_string_lossy().to_string(),
                size: node.payload.size,
            });
        }
    }
}

fn collect_matching_preview_files(
    nodes: &[RawNode<TorrentPreviewPayload>],
    query: &str,
    summary: &mut FilePreviewSummary,
) {
    for node in nodes {
        if node.is_dir {
            collect_matching_preview_files(&node.children, query, summary);
        } else if node
            .full_path
            .to_string_lossy()
            .to_lowercase()
            .contains(query)
        {
            summary.total_files += 1;
            if summary.rows.len() < MAX_FILES_PER_TORRENT {
                summary.rows.push(FilePreviewRow {
                    path: node.full_path.to_string_lossy().to_string(),
                    size: node.payload.size,
                });
            }
        }
    }
}

fn summarize_preview_files(
    torrent: &TorrentSettings,
    runtime: Option<&TorrentDisplayState>,
    query: &str,
) -> FilePreviewSummary {
    let Some(runtime) = runtime else {
        return FilePreviewSummary::default();
    };
    let query = query.trim().to_lowercase();
    if query.is_empty() || torrent.name.to_lowercase().contains(&query) {
        let mut rows = Vec::with_capacity(MAX_FILES_PER_TORRENT);
        collect_first_preview_files(&runtime.file_preview_tree, &mut rows);
        return FilePreviewSummary {
            total_files: runtime
                .latest_state
                .file_count
                .unwrap_or_else(|| count_preview_files(&runtime.file_preview_tree)),
            rows,
        };
    }
    let mut summary = FilePreviewSummary::default();
    collect_matching_preview_files(&runtime.file_preview_tree, &query, &mut summary);
    summary
}

pub fn draw(frame: &mut Frame, screen: &ScreenContext<'_>) {
    let area = centered_rect(92, 92, frame.area());
    let state = &screen.ui.ui.tag_management;
    let ctx = screen.theme;
    frame.render_widget(Clear, frame.area());

    let show_input = state.input_mode.is_some();
    let layout = if show_input {
        Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .split(area)
    } else {
        Layout::vertical([Constraint::Min(8), Constraint::Length(1)]).split(area)
    };
    let body_index = usize::from(show_input);
    let footer_index = body_index + 1;

    if show_input {
        draw_tag_input(frame, layout[0], state, ctx);
    }
    draw_tag_workspace(frame, layout[body_index], screen);
    draw_tag_footer(frame, layout[footer_index], state, screen.settings, ctx);

    if let Some(tag_id) = state.delete_confirm_tag_id {
        draw_delete_confirmation(frame, area, tag_id, screen.settings, ctx);
    }
    tag_picker::draw(frame, screen);
}

fn draw_tag_input(frame: &mut Frame, area: Rect, state: &TagManagementUiState, ctx: &ThemeContext) {
    let title = match state.input_mode.as_ref() {
        Some(TagInputMode::Search) => " Search torrents and files ",
        Some(TagInputMode::Create) => " Create tag ",
        Some(TagInputMode::Rename(_)) => " Rename tag ",
        None => return,
    };
    draw_prompt_panel(
        frame,
        area,
        title.to_string(),
        sanitize_text(&state.input_buffer),
        Vec::new(),
        ctx,
    );
}

fn draw_tag_workspace(frame: &mut Frame, area: Rect, screen: &ScreenContext<'_>) {
    let state = &screen.ui.ui.tag_management;
    let inner_width = area.width.saturating_sub(6).max(1);
    let pill_lines = build_tag_pill_lines(screen.settings, state, inner_width, screen.theme);
    let filter_height = (pill_lines.len() as u16)
        .saturating_add(2)
        .min(area.height.saturating_sub(5).max(3));
    let regions = Layout::vertical([
        Constraint::Length(filter_height),
        Constraint::Length(1),
        Constraint::Min(4),
    ])
    .split(area);

    draw_tag_filters(frame, regions[0], screen, pill_lines);
    draw_results(frame, regions[2], screen);
}

fn build_tag_pill_lines(
    settings: &Settings,
    state: &TagManagementUiState,
    width: u16,
    ctx: &ThemeContext,
) -> Vec<Line<'static>> {
    let total = settings.torrents.len();
    let mut pills = Vec::with_capacity(settings.tag_catalog.tags.len() + 1);
    pills.push(("All".to_string(), total, None));
    pills.extend(settings.tag_catalog.tags.iter().map(|tag| {
        let count = settings
            .torrents
            .iter()
            .filter(|torrent| torrent.tag_ids.contains(&tag.id))
            .count();
        (tag.name.clone(), count, Some(tag.id))
    }));

    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut used = 0usize;
    let max_width = usize::from(width.max(1));
    for (index, (name, count, tag_id)) in pills.into_iter().enumerate() {
        let name = truncate_with_ellipsis(&sanitize_text(&name), MAX_TAG_NAME_WIDTH);
        let label = format!(" {name}  {count} ");
        let pill_width = label.chars().count() + 2;
        let separator_width = usize::from(!spans.is_empty());
        if !spans.is_empty() && used + separator_width + pill_width > max_width {
            lines.push(Line::from(std::mem::take(&mut spans)));
            used = 0;
        }
        if !spans.is_empty() {
            spans.push(Span::raw(" "));
            used += 1;
        }
        let color = tag_id.map_or_else(|| ctx.state_selected(), |id| tag_color(id, ctx));
        let selected = index == state.selected_tag_index;
        let focused = selected && matches!(state.active_pane, TagManagementPane::Tags);
        let border_style = if selected {
            ctx.apply(Style::default().fg(color).add_modifier(Modifier::BOLD))
        } else {
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0))
        };
        let label_style = if selected {
            let mut modifiers = Modifier::UNDERLINED;
            if focused {
                modifiers |= Modifier::BOLD;
            }
            ctx.apply(
                Style::default()
                    .fg(ctx.theme.semantic.text)
                    .add_modifier(modifiers),
            )
        } else {
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0))
        };
        spans.push(Span::styled("[", border_style));
        spans.push(Span::styled(label, label_style));
        spans.push(Span::styled("]", border_style));
        used += pill_width;
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

fn draw_tag_filters(
    frame: &mut Frame,
    area: Rect,
    screen: &ScreenContext<'_>,
    lines: Vec<Line<'static>>,
) {
    let state = &screen.ui.ui.tag_management;
    let ctx = screen.theme;
    let border_color = if matches!(state.active_pane, TagManagementPane::Tags) {
        ctx.state_selected()
    } else {
        ctx.theme.semantic.border
    };
    let selected_label = selected_tag(screen.settings, state)
        .map(|tag| sanitize_text(&tag.name))
        .unwrap_or_else(|| "All torrents".to_string());
    let mut block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(2))
        .border_style(ctx.apply(Style::default().fg(border_color)))
        .title(Span::styled(
            " Filter by tag ",
            ctx.apply(
                Style::default()
                    .fg(ctx.state_selected())
                    .add_modifier(Modifier::BOLD),
            ),
        ))
        .title_bottom(Span::styled(
            format!(" {selected_label} "),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ));
    if !state.search_query.is_empty() {
        block = block.title_bottom(Span::styled(
            format!(" search: {} ", sanitize_text(&state.search_query)),
            ctx.apply(Style::default().fg(ctx.accent_sapphire())),
        ));
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_results(frame: &mut Frame, area: Rect, screen: &ScreenContext<'_>) {
    let state = &screen.ui.ui.tag_management;
    let ctx = screen.theme;
    let torrents = visible_torrents(screen.settings, screen.ui, state);
    let query = state.search_query.as_str();
    let rendered_torrents = torrents
        .iter()
        .map(|torrent| {
            let runtime = runtime_torrent(torrent, screen.ui);
            let summary = summarize_preview_files(torrent, runtime, query);
            (*torrent, runtime, summary)
        })
        .collect::<Vec<_>>();
    let file_count = rendered_torrents
        .iter()
        .map(|(_, _, summary)| summary.total_files)
        .sum::<usize>();
    let border_color = if matches!(state.active_pane, TagManagementPane::Torrents) {
        ctx.state_selected()
    } else {
        ctx.theme.semantic.border
    };
    let surface_label = if state.assignment_mode {
        "Assignments"
    } else {
        "Results"
    };
    let title = format!(
        " {surface_label} · {} torrent{} · {} file{} ",
        torrents.len(),
        if torrents.len() == 1 { "" } else { "s" },
        file_count,
        if file_count == 1 { "" } else { "s" },
    );
    let mut block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::new(1, 1, 0, 0))
        .border_style(ctx.apply(Style::default().fg(border_color)))
        .title(Span::styled(
            title,
            ctx.apply(
                Style::default()
                    .fg(ctx.accent_sky())
                    .add_modifier(Modifier::BOLD),
            ),
        ));
    if let Some(message) = &state.status_message {
        block = block.title_bottom(Span::styled(
            format!(" {} ", truncate_with_ellipsis(&sanitize_text(message), 72)),
            ctx.apply(
                Style::default()
                    .fg(ctx.state_info())
                    .add_modifier(Modifier::BOLD),
            ),
        ));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if torrents.is_empty() {
        let message = if state.search_query.is_empty() {
            "No torrents use this tag yet"
        } else {
            "No torrents or files match this search"
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "No matches",
                    ctx.apply(
                        Style::default()
                            .fg(ctx.state_warning())
                            .add_modifier(Modifier::BOLD),
                    ),
                )),
                Line::from(Span::styled(
                    message,
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
                )),
            ])
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
            centered_fixed(inner, inner.width.min(54), 2),
        );
        return;
    }

    let items = rendered_torrents
        .iter()
        .enumerate()
        .map(|(index, (torrent, runtime, summary))| {
            result_item(
                torrent,
                *runtime,
                summary,
                index == state.selected_torrent_index,
                ResultItemContext {
                    settings: screen.settings,
                    active_tag_id: selected_tag_id(screen.settings, state),
                    assignment_mode: state.assignment_mode,
                    width: inner.width,
                    theme: ctx,
                },
            )
        })
        .collect::<Vec<_>>();
    let mut list_state = ListState::default();
    list_state.select(Some(state.selected_torrent_index));
    frame.render_stateful_widget(List::new(items), inner, &mut list_state);
}

fn result_item(
    torrent: &TorrentSettings,
    runtime: Option<&TorrentDisplayState>,
    preview: &FilePreviewSummary,
    selected: bool,
    render: ResultItemContext<'_>,
) -> ListItem<'static> {
    let ResultItemContext {
        settings,
        active_tag_id,
        assignment_mode,
        width,
        theme: ctx,
    } = render;
    let (status, status_color, progress) = torrent_status(torrent, runtime, ctx);
    let marker = if selected { "▶ " } else { "  " };
    let assignment_marker = if assignment_mode {
        if active_tag_id.is_some_and(|tag_id| torrent.tag_ids.contains(&tag_id)) {
            "[x] "
        } else {
            "[ ] "
        }
    } else {
        ""
    };
    let name_budget = usize::from(width)
        .saturating_sub(28 + assignment_marker.chars().count())
        .max(12);
    let name = truncate_with_ellipsis(&sanitize_text(&torrent.name), name_budget);
    let mut header = vec![
        Span::styled(
            marker,
            ctx.apply(
                Style::default()
                    .fg(ctx.state_warning())
                    .add_modifier(Modifier::BOLD),
            ),
        ),
        Span::styled(
            assignment_marker.to_string(),
            ctx.apply(Style::default().fg(if assignment_mode {
                ctx.state_selected()
            } else {
                ctx.theme.semantic.text
            })),
        ),
        Span::styled("● ", ctx.apply(Style::default().fg(status_color))),
        Span::styled(
            name,
            ctx.apply(
                Style::default()
                    .fg(if selected {
                        ctx.theme.semantic.white
                    } else {
                        ctx.theme.semantic.text
                    })
                    .add_modifier(Modifier::BOLD),
            ),
        ),
        Span::styled(
            format!("  {progress:>3.0}% · {status}"),
            ctx.apply(Style::default().fg(status_color)),
        ),
    ];
    append_inline_tag_chips(&mut header, torrent, settings, ctx, width);

    let total_files = preview.total_files;
    let mut lines = vec![Line::from(header)];
    for (index, file) in preview.rows.iter().enumerate() {
        let is_last_visible = index + 1 == preview.rows.len() && total_files == preview.rows.len();
        let branch = if is_last_visible { "└─" } else { "├─" };
        let size = format_bytes(file.size);
        let path_budget = usize::from(width).saturating_sub(size.chars().count() + 11);
        lines.push(Line::from(vec![
            Span::styled(
                format!("     {branch} "),
                ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
            ),
            Span::styled(
                truncate_with_ellipsis(&sanitize_text(&file.path), path_budget.max(8)),
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
            ),
            Span::styled(
                format!("  {size}"),
                ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
            ),
        ]));
    }
    if total_files > preview.rows.len() {
        lines.push(Line::from(Span::styled(
            format!("     └─ … {} more files", total_files - preview.rows.len()),
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
        )));
    } else if total_files == 0 {
        lines.push(Line::from(Span::styled(
            "     └─ metadata pending",
            ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
        )));
    }
    lines.push(Line::default());
    ListItem::new(lines)
}

fn append_inline_tag_chips(
    spans: &mut Vec<Span<'static>>,
    torrent: &TorrentSettings,
    settings: &Settings,
    ctx: &ThemeContext,
    width: u16,
) {
    let mut used = spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum::<usize>();
    for tag_id in &torrent.tag_ids {
        let Some(tag) = settings.tag_catalog.get(*tag_id) else {
            continue;
        };
        let name = truncate_with_ellipsis(&sanitize_text(&tag.name), 14);
        let label = format!(" {name} ");
        if used + label.chars().count() + 1 >= usize::from(width) {
            break;
        }
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            label.clone(),
            ctx.apply(
                Style::default()
                    .fg(ctx.theme.semantic.white)
                    .bg(tag_color(*tag_id, ctx)),
            ),
        ));
        used += label.chars().count() + 1;
    }
}

fn torrent_status(
    torrent: &TorrentSettings,
    runtime: Option<&TorrentDisplayState>,
    ctx: &ThemeContext,
) -> (&'static str, Color, f64) {
    if matches!(torrent.torrent_control_state, TorrentControlState::Deleting) {
        return ("Deleting", ctx.state_error(), 0.0);
    }
    if matches!(torrent.torrent_control_state, TorrentControlState::Paused) {
        let progress = runtime.map_or(0.0, |state| torrent_completion_percent(&state.latest_state));
        return ("Paused", ctx.state_warning(), progress);
    }
    let Some(runtime) = runtime else {
        return ("Queued", ctx.theme.semantic.overlay0, 0.0);
    };
    let metrics = &runtime.latest_state;
    let progress = torrent_completion_percent(metrics);
    if !metrics.data_available {
        ("Unavailable", ctx.state_error(), progress)
    } else if metrics.is_complete || progress >= 100.0 {
        ("Complete", ctx.state_complete(), 100.0)
    } else if metrics.number_of_pieces_total == 0 {
        ("Metadata", ctx.state_info(), 0.0)
    } else {
        ("Downloading", ctx.metric_download(), progress)
    }
}

fn tag_color(tag_id: TagId, ctx: &ThemeContext) -> Color {
    let colors = [
        ctx.theme.scale.categorical.lavender,
        ctx.accent_sapphire(),
        ctx.accent_teal(),
        ctx.accent_peach(),
        ctx.theme.scale.categorical.mauve,
        ctx.theme.scale.categorical.green,
        ctx.accent_maroon(),
    ];
    colors[(tag_id as usize).wrapping_sub(1) % colors.len()]
}

fn draw_tag_footer(
    frame: &mut Frame,
    area: Rect,
    state: &TagManagementUiState,
    settings: &Settings,
    ctx: &ThemeContext,
) {
    let mut spans = Vec::new();
    let mut used = 0usize;
    let max_width = usize::from(area.width);
    let mut push = |key: &str, label: &str, tone: ActionTone| {
        let _ = try_push_footer_action(&mut spans, &mut used, max_width, key, label, tone, ctx);
    };

    if state.input_mode.is_some() {
        push("Enter", "apply", ActionTone::Confirm);
        push("Esc", "cancel", ActionTone::Cancel);
    } else if state.delete_confirm_tag_id.is_some() {
        push("Y/Enter", "delete", ActionTone::Destructive);
        push("Esc", "cancel", ActionTone::Cancel);
    } else if matches!(state.active_pane, TagManagementPane::Tags) {
        push("←/→", "filter", ActionTone::Navigate);
        push("Tab", "results", ActionTone::Mode);
        push("0", "all", ActionTone::Clear);
        push("n", "new", ActionTone::Add);
        if selected_tag(settings, state).is_some() {
            push("m", "assign", ActionTone::Mode);
            push("e", "rename", ActionTone::Edit);
            push("d", "delete", ActionTone::Destructive);
        }
        push("/", "search", ActionTone::Search);
        push("Esc", "back", ActionTone::Cancel);
    } else {
        push("↑/↓", "torrent", ActionTone::Navigate);
        push("t", "assign", ActionTone::Mode);
        if selected_tag(settings, state).is_some() {
            push("Space", "toggle", ActionTone::Toggle);
            push(
                "m",
                if state.assignment_mode {
                    "filter"
                } else {
                    "assign"
                },
                ActionTone::Mode,
            );
        }
        push("Tab", "filters", ActionTone::Mode);
        push("/", "search", ActionTone::Search);
        push("Esc", "back", ActionTone::Cancel);
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .alignment(Alignment::Center)
            .style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1))),
        area,
    );
}

#[allow(clippy::too_many_arguments)]
fn try_push_footer_action(
    spans: &mut Vec<Span<'static>>,
    used: &mut usize,
    max_width: usize,
    key: &str,
    label: &str,
    tone: ActionTone,
    ctx: &ThemeContext,
) -> bool {
    let separator = if spans.is_empty() { "" } else { " | " };
    let key_text = format!("[{key}]");
    let width = separator.chars().count() + key_text.chars().count() + label.chars().count();
    if used.saturating_add(width) > max_width {
        return false;
    }
    if !separator.is_empty() {
        spans.push(Span::styled(
            separator.to_string(),
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
        ));
    }
    spans.push(Span::styled(key_text, footer_key_style(ctx, tone)));
    spans.push(Span::styled(
        label.to_string(),
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
    ));
    *used += width;
    true
}

fn draw_delete_confirmation(
    frame: &mut Frame,
    area: Rect,
    tag_id: TagId,
    settings: &Settings,
    ctx: &ThemeContext,
) {
    let tag_name = settings
        .tag_catalog
        .get(tag_id)
        .map(|tag| sanitize_text(&tag.name))
        .unwrap_or_else(|| format!("Tag {tag_id}"));
    let assignments = settings
        .torrents
        .iter()
        .filter(|torrent| torrent.tag_ids.contains(&tag_id))
        .count();
    let dialog = centered_fixed(area, area.width.min(58), area.height.min(9));
    frame.render_widget(Clear, dialog);
    let block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::new(2, 2, 1, 1))
        .border_style(ctx.apply(Style::default().fg(ctx.state_error())));
    let inner = block.inner(dialog);
    frame.render_widget(block, dialog);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "Delete tag?",
                ctx.apply(
                    Style::default()
                        .fg(ctx.state_warning())
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                ),
            )),
            Line::from(Span::styled(
                tag_name,
                ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
            )),
            Line::default(),
            Line::from(Span::styled(
                format!(
                    "This removes {assignments} assignment{}; torrent data is untouched.",
                    if assignments == 1 { "" } else { "s" }
                ),
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
            )),
        ])
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true }),
        inner,
    );
}

fn centered_fixed(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{TorrentMetrics, TorrentPreviewPayload};
    use crate::config::TorrentSettings;
    use crate::dht_service::{DhtStatus, DhtWaveTelemetry};
    use crate::theme::ThemeContext;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::PathBuf;

    fn test_settings() -> Settings {
        let mut settings = Settings::default();
        let curated = settings
            .tag_catalog
            .create_manual("Curated")
            .expect("create tag");
        let later = settings
            .tag_catalog
            .create_manual("Read Later")
            .expect("create tag");
        settings.torrents = vec![
            TorrentSettings {
                torrent_or_magnet: "magnet:?xt=urn:btih:1111111111111111111111111111111111111111"
                    .to_string(),
                name: "Sample Archive".to_string(),
                tag_ids: vec![curated],
                ..Default::default()
            },
            TorrentSettings {
                torrent_or_magnet: "magnet:?xt=urn:btih:2222222222222222222222222222222222222222"
                    .to_string(),
                name: "Field Notes".to_string(),
                tag_ids: vec![later],
                ..Default::default()
            },
        ];
        settings
    }

    fn test_app_state() -> AppState {
        let mut state = AppState {
            mode: AppMode::TagManagement,
            ..Default::default()
        };
        let hash = vec![0x11; 20];
        let file = RawNode {
            name: "summary.txt".to_string(),
            full_path: PathBuf::from("notes/summary.txt"),
            children: Vec::new(),
            payload: TorrentPreviewPayload {
                file_index: Some(0),
                size: 2048,
                ..Default::default()
            },
            is_dir: false,
        };
        state.torrents.insert(
            hash.clone(),
            TorrentDisplayState {
                latest_state: TorrentMetrics {
                    info_hash: hash.clone(),
                    torrent_name: "Sample Archive".to_string(),
                    number_of_pieces_total: 4,
                    number_of_pieces_completed: 2,
                    ..Default::default()
                },
                file_preview_tree: vec![file],
                ..Default::default()
            },
        );
        state.torrent_list_order.push(hash);
        state
    }

    fn render_screen(settings: &Settings, state: &AppState, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let dht_status = DhtStatus::default();
        let telemetry = DhtWaveTelemetry::default();
        let ctx = ThemeContext::new(state.theme, 0.0);
        terminal
            .draw(|frame| {
                let screen = ScreenContext::new(state, &dht_status, &telemetry, settings, &ctx);
                draw(frame, &screen);
            })
            .expect("draw tag screen");
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
    fn all_filter_is_the_first_pill_and_specific_tags_filter_results() {
        let settings = test_settings();
        let app_state = test_app_state();
        let mut state = TagManagementUiState::default();

        assert!(selected_tag(&settings, &state).is_none());
        assert_eq!(visible_torrents(&settings, &app_state, &state).len(), 2);

        state.selected_tag_index = 1;
        assert_eq!(
            selected_tag(&settings, &state).map(|tag| tag.name.as_str()),
            Some("Curated")
        );
        assert_eq!(visible_torrents(&settings, &app_state, &state).len(), 1);
        assert_eq!(
            visible_torrents(&settings, &app_state, &state)[0].name,
            "Sample Archive"
        );
    }

    #[test]
    fn file_search_keeps_its_parent_torrent_visible() {
        let settings = test_settings();
        let app_state = test_app_state();
        let state = TagManagementUiState {
            search_query: "summary".to_string(),
            ..Default::default()
        };

        let visible = visible_torrents(&settings, &app_state, &state);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].name, "Sample Archive");
    }

    #[test]
    fn assignment_mode_keeps_unassigned_torrents_available() {
        let settings = test_settings();
        let app_state = test_app_state();
        let filtered = TagManagementUiState {
            selected_tag_index: 1,
            ..Default::default()
        };
        assert_eq!(visible_torrents(&settings, &app_state, &filtered).len(), 1);

        let assigning = TagManagementUiState {
            assignment_mode: true,
            ..filtered
        };
        assert_eq!(visible_torrents(&settings, &app_state, &assigning).len(), 2);
    }

    #[test]
    fn delete_confirmation_consumes_background_actions() {
        for key in [
            KeyCode::Char('n'),
            KeyCode::Char('m'),
            KeyCode::Char('t'),
            KeyCode::Char(' '),
            KeyCode::Tab,
        ] {
            assert_eq!(
                delete_confirmation_input(key),
                DeleteConfirmationInput::Consume
            );
        }
        assert_eq!(
            delete_confirmation_input(KeyCode::Enter),
            DeleteConfirmationInput::Confirm
        );
        assert_eq!(
            delete_confirmation_input(KeyCode::Esc),
            DeleteConfirmationInput::Cancel
        );
    }

    #[test]
    fn normal_preview_summary_uses_cached_count_and_caps_materialized_rows() {
        let torrent = TorrentSettings {
            name: "Large Fictional Dataset".to_string(),
            ..Default::default()
        };
        let files = (0..10)
            .map(|index| RawNode {
                name: format!("part-{index}.bin"),
                full_path: PathBuf::from(format!("dataset/part-{index}.bin")),
                children: Vec::new(),
                payload: TorrentPreviewPayload {
                    file_index: Some(index),
                    size: 1024,
                    ..Default::default()
                },
                is_dir: false,
            })
            .collect();
        let runtime = TorrentDisplayState {
            latest_state: TorrentMetrics {
                file_count: Some(100_000),
                ..Default::default()
            },
            file_preview_tree: files,
            ..Default::default()
        };

        let summary = summarize_preview_files(&torrent, Some(&runtime), "");

        assert_eq!(summary.total_files, 100_000);
        assert_eq!(summary.rows.len(), MAX_FILES_PER_TORRENT);
        assert_eq!(summary.rows[0].path, "dataset/part-0.bin");
        assert_eq!(summary.rows[2].path, "dataset/part-2.bin");
    }

    #[test]
    fn inactive_pills_are_grey_and_selected_pill_is_underlined() {
        let settings = test_settings();
        let state = TagManagementUiState::default();
        let app_state = test_app_state();
        let ctx = ThemeContext::new(app_state.theme, 0.0);
        let lines = build_tag_pill_lines(&settings, &state, 120, &ctx);
        let spans = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .collect::<Vec<_>>();

        let selected = spans
            .iter()
            .find(|span| span.content.contains("All"))
            .expect("selected All pill");
        assert!(selected.style.add_modifier.contains(Modifier::UNDERLINED));

        let inactive = spans
            .iter()
            .find(|span| span.content.contains("Curated"))
            .expect("inactive Curated pill");
        assert_eq!(
            inactive.style.fg,
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0))
                .fg
        );
        assert!(!inactive.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn redesigned_screen_renders_filter_pills_and_file_results() {
        let settings = test_settings();
        let state = test_app_state();
        let rendered = render_screen(&settings, &state, 100, 30);

        for expected in [
            "Filter by tag",
            "All",
            "Curated",
            "Read Later",
            "Results · 2 torrents · 1 file",
            "Sample Archive",
            "notes/summary.txt",
            "Field Notes",
            "[Tab]results",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}\n{rendered}"
            );
        }
    }

    #[test]
    fn inline_picker_renders_existing_assignment_for_highlighted_torrent() {
        let settings = test_settings();
        let mut state = test_app_state();
        state.ui.tag_picker.open = true;
        state.ui.tag_picker.target_hashes = vec![vec![0x11; 20]];
        let rendered = render_screen(&settings, &state, 100, 30);

        for expected in [
            "Tags · 1 torrent",
            "[x] Curated",
            "[ ] Read Later",
            "[a]add tag",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}\n{rendered}"
            );
        }
    }

    #[test]
    fn narrow_tag_pills_wrap_onto_multiple_lines() {
        let settings = test_settings();
        let state = TagManagementUiState::default();
        let app_state = test_app_state();
        let ctx = ThemeContext::new(app_state.theme, 0.0);

        let lines = build_tag_pill_lines(&settings, &state, 18, &ctx);
        assert!(lines.len() >= 2);
    }
}
