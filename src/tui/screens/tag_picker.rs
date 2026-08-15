// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{App, AppCommand, AppState};
use crate::config::{Settings, TorrentSettings};
use crate::integrations::control::ControlRequest;
use crate::tags::{TagDefinition, TagId};
use crate::theme::ThemeContext;
use crate::torrent_identity::info_hash_from_torrent_source;
use crate::tui::action_style::{footer_key_style, ActionTone};
use crate::tui::app_command::{spawn_app_command_batch_sender, spawn_app_command_sender};
use crate::tui::formatters::{sanitize_text, truncate_with_ellipsis};
use crate::tui::screen_context::ScreenContext;
use crate::tui::screens::input_panel::draw_prompt_panel;
use ratatui::crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph};
use ratatui::Frame;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AssignmentState {
    None,
    Partial,
    All,
}

pub fn is_open(app_state: &AppState) -> bool {
    app_state.ui.tag_picker.open
}

pub fn open(app: &mut App, mut target_hashes: Vec<Vec<u8>>) {
    target_hashes.sort();
    target_hashes.dedup();
    target_hashes.retain(|hash| app.app_state.torrents.contains_key(hash));
    let picker = &mut app.app_state.ui.tag_picker;
    picker.open = !target_hashes.is_empty();
    picker.selected_index = 0;
    picker.creating = false;
    picker.input_buffer.clear();
    picker.target_hashes = target_hashes;
    picker.status_message = if picker.open {
        None
    } else {
        Some("No torrents are selected".to_string())
    };
    app.app_state.ui.needs_redraw = true;
}

pub fn handle_event(event: CrosstermEvent, app: &mut App) -> bool {
    if !is_open(&app.app_state) {
        return false;
    }
    let CrosstermEvent::Key(key) = event else {
        return false;
    };
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        || !matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
    {
        return false;
    }

    if app.app_state.ui.tag_picker.creating {
        return handle_create_input(key.code, app);
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('t') => close(app),
        KeyCode::Up | KeyCode::Char('k') => move_selection(app, -1),
        KeyCode::Down | KeyCode::Char('j') => move_selection(app, 1),
        KeyCode::Home => app.app_state.ui.tag_picker.selected_index = 0,
        KeyCode::End => {
            app.app_state.ui.tag_picker.selected_index =
                app.client_configs.tag_catalog.tags.len().saturating_sub(1);
        }
        KeyCode::Char('n') => {
            let picker = &mut app.app_state.ui.tag_picker;
            picker.creating = true;
            picker.input_buffer.clear();
            picker.status_message = None;
        }
        KeyCode::Enter | KeyCode::Char(' ') => toggle_selected_tag(app),
        _ => return false,
    }
    clamp_selection(app);
    app.app_state.ui.needs_redraw = true;
    true
}

fn handle_create_input(key_code: KeyCode, app: &mut App) -> bool {
    match key_code {
        KeyCode::Esc => {
            let picker = &mut app.app_state.ui.tag_picker;
            picker.creating = false;
            picker.input_buffer.clear();
        }
        KeyCode::Backspace => {
            app.app_state.ui.tag_picker.input_buffer.pop();
        }
        KeyCode::Enter => create_and_assign(app),
        KeyCode::Char(character) => {
            app.app_state.ui.tag_picker.input_buffer.push(character);
        }
        _ => return false,
    }
    app.app_state.ui.needs_redraw = true;
    true
}

fn close(app: &mut App) {
    let picker = &mut app.app_state.ui.tag_picker;
    picker.open = false;
    picker.creating = false;
    picker.input_buffer.clear();
    picker.target_hashes.clear();
    picker.status_message = None;
}

fn move_selection(app: &mut App, delta: isize) {
    app.app_state.ui.tag_picker.selected_index = app
        .app_state
        .ui
        .tag_picker
        .selected_index
        .saturating_add_signed(delta);
}

fn clamp_selection(app: &mut App) {
    app.app_state.ui.tag_picker.selected_index = app
        .app_state
        .ui
        .tag_picker
        .selected_index
        .min(app.client_configs.tag_catalog.tags.len().saturating_sub(1));
}

fn create_and_assign(app: &mut App) {
    let name = app.app_state.ui.tag_picker.input_buffer.trim().to_string();
    if name.is_empty() {
        app.app_state.ui.tag_picker.status_message = Some("Tag name cannot be empty".to_string());
        return;
    }
    let info_hashes = app
        .app_state
        .ui
        .tag_picker
        .target_hashes
        .iter()
        .map(hex::encode)
        .collect::<Vec<_>>();
    let target_count = info_hashes.len();
    let request = ControlRequest::CreateAndAssignTag {
        name: name.clone(),
        info_hashes,
    };
    let picker = &mut app.app_state.ui.tag_picker;
    picker.creating = false;
    picker.input_buffer.clear();
    picker.status_message = Some(format!(
        "Creating '{name}' for {target_count} torrent{}",
        if target_count == 1 { "" } else { "s" }
    ));
    spawn_app_command_sender(
        app.app_command_tx.clone(),
        app.shutdown_tx.subscribe(),
        AppCommand::SubmitControlRequest(request),
    );
}

fn toggle_selected_tag(app: &mut App) {
    let Some(tag) = selected_tag(&app.client_configs, &app.app_state) else {
        app.app_state.ui.tag_picker.status_message =
            Some("Create a tag to begin assigning".to_string());
        return;
    };
    if !tag.origin.is_manual() {
        app.app_state.ui.tag_picker.status_message =
            Some("Generated tags cannot be changed manually".to_string());
        return;
    }
    let tag_id = tag.id;
    let tag_name = tag.name.clone();
    let targets = app.app_state.ui.tag_picker.target_hashes.clone();
    let remove = matches!(
        assignment_state(&app.client_configs, tag_id, &targets),
        AssignmentState::All
    );
    let requests = targets
        .iter()
        .map(|hash| {
            let info_hash_hex = hex::encode(hash);
            let request = if remove {
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
            AppCommand::SubmitControlRequest(request)
        })
        .collect::<Vec<_>>();
    app.app_state.ui.tag_picker.status_message = Some(format!(
        "{} '{}' for {} torrent{}",
        if remove { "Removing" } else { "Assigning" },
        tag_name,
        targets.len(),
        if targets.len() == 1 { "" } else { "s" }
    ));
    spawn_app_command_batch_sender(
        app.app_command_tx.clone(),
        app.shutdown_tx.subscribe(),
        requests,
    );
}

fn selected_tag<'a>(settings: &'a Settings, app_state: &AppState) -> Option<&'a TagDefinition> {
    settings
        .tag_catalog
        .tags
        .get(app_state.ui.tag_picker.selected_index)
}

fn assignment_state(settings: &Settings, tag_id: TagId, targets: &[Vec<u8>]) -> AssignmentState {
    let assigned = targets
        .iter()
        .filter(|hash| {
            torrent_settings_for_hash(settings, hash)
                .is_some_and(|torrent| torrent.tag_ids.contains(&tag_id))
        })
        .count();
    match assigned {
        0 => AssignmentState::None,
        count if count == targets.len() => AssignmentState::All,
        _ => AssignmentState::Partial,
    }
}

fn torrent_settings_for_hash<'a>(
    settings: &'a Settings,
    target_hash: &[u8],
) -> Option<&'a TorrentSettings> {
    settings.torrents.iter().find(|torrent| {
        info_hash_from_torrent_source(&torrent.torrent_or_magnet)
            .is_some_and(|hash| hash.as_slice() == target_hash)
    })
}

pub fn draw(frame: &mut Frame, screen: &ScreenContext<'_>) {
    let picker = &screen.ui.ui.tag_picker;
    if !picker.open {
        return;
    }
    let ctx = screen.theme;
    let area = centered_fixed(
        frame.area(),
        frame.area().width.min(64),
        frame.area().height.min(18),
    );
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::new(2, 2, 1, 1))
        .border_style(ctx.apply(Style::default().fg(ctx.state_selected())))
        .title(Span::styled(
            format!(
                " Tags · {} torrent{} ",
                picker.target_hashes.len(),
                if picker.target_hashes.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ),
            ctx.apply(
                Style::default()
                    .fg(ctx.state_selected())
                    .add_modifier(Modifier::BOLD),
            ),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = if picker.creating {
        Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(inner)
    } else {
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(inner)
    };
    let list_index = usize::from(picker.creating);
    let footer_index = list_index + 1;
    if picker.creating {
        draw_prompt_panel(
            frame,
            rows[0],
            " New tag and assign ".to_string(),
            sanitize_text(&picker.input_buffer),
            Vec::new(),
            ctx,
        );
    }

    draw_tag_list(frame, rows[list_index], screen);
    draw_footer(frame, rows[footer_index], picker.creating, ctx);
}

fn draw_tag_list(frame: &mut Frame, area: Rect, screen: &ScreenContext<'_>) {
    let picker = &screen.ui.ui.tag_picker;
    let ctx = screen.theme;
    let items = screen
        .settings
        .tag_catalog
        .tags
        .iter()
        .map(|tag| {
            let (marker, marker_color) =
                match assignment_state(screen.settings, tag.id, &picker.target_hashes) {
                    AssignmentState::None => ("[ ]", ctx.theme.semantic.overlay0),
                    AssignmentState::Partial => ("[~]", ctx.state_warning()),
                    AssignmentState::All => ("[x]", ctx.state_success()),
                };
            let count = screen
                .settings
                .torrents
                .iter()
                .filter(|torrent| torrent.tag_ids.contains(&tag.id))
                .count();
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{marker} "),
                    ctx.apply(Style::default().fg(marker_color)),
                ),
                Span::styled(
                    truncate_with_ellipsis(&sanitize_text(&tag.name), 36),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                ),
                Span::styled(
                    format!("  {count}"),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
                ),
            ]))
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "No tags yet",
                    ctx.apply(
                        Style::default()
                            .fg(ctx.state_warning())
                            .add_modifier(Modifier::BOLD),
                    ),
                )),
                Line::from("Press n to create and assign one here."),
            ])
            .alignment(Alignment::Center),
            area,
        );
        return;
    }
    let mut state = ListState::default();
    state.select(Some(
        picker.selected_index.min(items.len().saturating_sub(1)),
    ));
    let mut list = List::new(items).highlight_symbol("▶ ").highlight_style(
        ctx.apply(
            Style::default()
                .fg(ctx.state_warning())
                .add_modifier(Modifier::BOLD),
        ),
    );
    if let Some(message) = &picker.status_message {
        list = list.block(Block::default().title_bottom(Span::styled(
            format!(" {} ", truncate_with_ellipsis(&sanitize_text(message), 52)),
            ctx.apply(Style::default().fg(ctx.state_info())),
        )));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_footer(frame: &mut Frame, area: Rect, creating: bool, ctx: &ThemeContext) {
    let actions = if creating {
        vec![
            ("Enter", "create + assign", ActionTone::Confirm),
            ("Esc", "cancel", ActionTone::Cancel),
        ]
    } else {
        vec![
            ("↑/↓", "tag", ActionTone::Navigate),
            ("Space", "toggle", ActionTone::Toggle),
            ("n", "new + assign", ActionTone::Add),
            ("Esc", "close", ActionTone::Cancel),
        ]
    };
    let mut spans = Vec::new();
    for (index, (key, label, tone)) in actions.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(
                " | ",
                ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
            ));
        }
        spans.push(Span::styled(
            format!("[{key}]"),
            footer_key_style(ctx, tone),
        ));
        spans.push(Span::styled(
            label,
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
        area,
    );
}

fn centered_fixed(area: Rect, width: u16, height: u16) -> Rect {
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

    #[test]
    fn assignment_state_reports_none_partial_and_all() {
        let mut settings = Settings::default();
        let tag_id = settings
            .tag_catalog
            .create_manual("Curated")
            .expect("create tag");
        let first = vec![0x11; 20];
        let second = vec![0x22; 20];
        settings.torrents = vec![
            TorrentSettings {
                torrent_or_magnet: format!("magnet:?xt=urn:btih:{}", hex::encode(&first)),
                name: "Sample Archive".to_string(),
                ..Default::default()
            },
            TorrentSettings {
                torrent_or_magnet: format!("magnet:?xt=urn:btih:{}", hex::encode(&second)),
                name: "Field Notes".to_string(),
                ..Default::default()
            },
        ];
        let targets = vec![first, second];
        assert_eq!(
            assignment_state(&settings, tag_id, &targets),
            AssignmentState::None
        );
        settings.torrents[0].tag_ids.push(tag_id);
        assert_eq!(
            assignment_state(&settings, tag_id, &targets),
            AssignmentState::Partial
        );
        settings.torrents[1].tag_ids.push(tag_id);
        assert_eq!(
            assignment_state(&settings, tag_id, &targets),
            AssignmentState::All
        );
    }
}
