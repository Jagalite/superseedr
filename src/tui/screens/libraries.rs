// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{
    App, AppCommand, ConfigPane, FileBrowserMode, LibraryInputKind, LibraryInputState,
};
use crate::tui::action_style::{footer_key_style, ActionTone};
use crate::tui::app_command::spawn_app_command_sender;
use crate::tui::formatters::{sanitize_text, truncate_with_ellipsis};
use crate::tui::screen_context::ScreenContext;
use ratatui::crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEventKind};
use ratatui::{prelude::*, widgets::*};
use std::path::PathBuf;

pub fn refresh(app: &mut App) {
    match crate::library::list_libraries() {
        Ok(entries) => {
            app.app_state.ui.libraries.entries = entries;
            let len = app.app_state.ui.libraries.entries.len();
            app.app_state.ui.libraries.selected_index = app
                .app_state
                .ui
                .libraries
                .selected_index
                .min(len.saturating_sub(1));
            request_previews(app);
        }
        Err(error) => {
            app.app_state.ui.libraries.status_message = Some(error.to_string());
            app.app_state.ui.libraries.previews.clear();
            app.app_state.ui.libraries.previews_loading = false;
        }
    }
}

fn request_previews(app: &mut App) {
    let entries = app.app_state.ui.libraries.entries.clone();
    let libraries = &mut app.app_state.ui.libraries;
    libraries.preview_request_id = libraries.preview_request_id.wrapping_add(1);
    libraries.preview_scroll = 0;
    libraries.previews.clear();
    libraries.previews_loading = !entries.is_empty();
    if entries.is_empty() {
        return;
    }

    let request_id = libraries.preview_request_id;
    let app_command_tx = app.app_command_tx.clone();
    tokio::spawn(async move {
        let fallback_entries = entries.clone();
        let previews =
            match tokio::task::spawn_blocking(move || crate::library::load_previews(entries)).await
            {
                Ok(previews) => previews,
                Err(error) => fallback_entries
                    .into_iter()
                    .map(|library| crate::library::LibraryPreview {
                        name: library.name,
                        torrent_names: Vec::new(),
                        error: Some(format!("Metadata task failed: {error}")),
                    })
                    .collect(),
            };
        let _ = app_command_tx
            .send(AppCommand::LibraryPreviewsLoaded {
                request_id,
                previews,
            })
            .await;
    });
}

pub fn handle_event(event: CrosstermEvent, app: &mut App) {
    let CrosstermEvent::Key(key) = event else {
        return;
    };
    if key.kind != KeyEventKind::Press {
        return;
    }

    if app.app_state.ui.libraries.input.is_some() {
        handle_input_key(key.code, app);
        return;
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.app_state.ui.config.active_pane = ConfigPane::Settings;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.app_state.ui.libraries.selected_index =
                app.app_state.ui.libraries.selected_index.saturating_sub(1);
            app.app_state.ui.libraries.preview_scroll = 0;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let last = app.app_state.ui.libraries.entries.len().saturating_sub(1);
            app.app_state.ui.libraries.selected_index =
                (app.app_state.ui.libraries.selected_index + 1).min(last);
            app.app_state.ui.libraries.preview_scroll = 0;
        }
        KeyCode::PageUp => {
            app.app_state.ui.libraries.preview_scroll =
                app.app_state.ui.libraries.preview_scroll.saturating_sub(10);
        }
        KeyCode::PageDown => {
            let last = selected_preview_len(app).saturating_sub(1);
            app.app_state.ui.libraries.preview_scroll = app
                .app_state
                .ui
                .libraries
                .preview_scroll
                .saturating_add(10)
                .min(last);
        }
        KeyCode::Home => app.app_state.ui.libraries.preview_scroll = 0,
        KeyCode::End => {
            app.app_state.ui.libraries.preview_scroll = selected_preview_len(app).saturating_sub(1);
        }
        KeyCode::Enter | KeyCode::Char('u') => switch_selected(app),
        KeyCode::Char('a') => {
            app.app_state.ui.libraries.input = Some(LibraryInputState {
                kind: LibraryInputKind::AddName,
                buffer: String::new(),
            });
        }
        KeyCode::Char('r') => {
            if let Some(entry) = selected(app) {
                app.app_state.ui.libraries.input = Some(LibraryInputState {
                    kind: LibraryInputKind::Rename {
                        current_name: entry.name.clone(),
                    },
                    buffer: entry.name.clone(),
                });
            }
        }
        KeyCode::Char('d') => {
            if let Some(entry) = selected(app) {
                app.app_state.ui.libraries.input = Some(LibraryInputState {
                    kind: LibraryInputKind::ConfirmRemove {
                        name: entry.name.clone(),
                    },
                    buffer: String::new(),
                });
            }
        }
        KeyCode::Char('o') => {
            if let Some(entry) = selected(app) {
                app.app_state.ui.libraries.status_message =
                    match crate::library::reveal_directory(&entry.shared_config_path) {
                        Ok(()) => Some(format!("Opened '{}'.", entry.name)),
                        Err(error) => Some(error.to_string()),
                    };
            }
        }
        _ => {}
    }
}

fn selected_preview_len(app: &App) -> usize {
    selected(app)
        .and_then(|entry| app.app_state.ui.libraries.previews.get(&entry.name))
        .map(|preview| preview.torrent_names.len())
        .unwrap_or(0)
}

fn selected(app: &App) -> Option<&crate::library::LibraryStatus> {
    app.app_state
        .ui
        .libraries
        .entries
        .get(app.app_state.ui.libraries.selected_index)
}

fn switch_selected(app: &mut App) {
    let Some(entry) = selected(app).cloned() else {
        app.app_state.ui.libraries.status_message = Some("No library is selected.".to_string());
        return;
    };
    if entry.active {
        app.app_state.ui.libraries.status_message =
            Some(format!("'{}' is already active.", entry.name));
    } else if !entry.available {
        app.app_state.ui.libraries.status_message = Some(format!(
            "'{}' is unavailable at {}.",
            entry.name,
            entry.shared_config_path.display()
        ));
    } else if let Err(error) = app
        .app_command_tx
        .try_send(AppCommand::SwitchLibrary(entry.name))
    {
        app.app_state.ui.libraries.status_message =
            Some(format!("Could not queue library switch: {error}"));
    }
}

fn handle_input_key(code: KeyCode, app: &mut App) {
    let Some(mut input) = app.app_state.ui.libraries.input.take() else {
        return;
    };
    match (&input.kind, code) {
        (_, KeyCode::Esc) => {}
        (LibraryInputKind::ConfirmRemove { name }, KeyCode::Char('y')) => {
            app.app_state.ui.libraries.status_message = match crate::library::remove_library(name) {
                Ok(_) => Some(format!(
                    "Removed '{name}' from the registry; no files were deleted."
                )),
                Err(error) => Some(error.to_string()),
            };
            refresh(app);
        }
        (LibraryInputKind::ConfirmRemove { .. }, KeyCode::Char('n')) => {}
        (LibraryInputKind::ConfirmRemove { .. }, _) => {
            app.app_state.ui.libraries.input = Some(input);
        }
        (LibraryInputKind::AddName, KeyCode::Enter) => {
            let name = input.buffer.trim().to_string();
            if name.is_empty() {
                app.app_state.ui.libraries.status_message =
                    Some("Library name must not be empty.".to_string());
                app.app_state.ui.libraries.input = Some(input);
            } else {
                open_library_path_picker(name, app);
            }
        }
        (LibraryInputKind::Rename { current_name }, KeyCode::Enter) => {
            let new_name = input.buffer.trim();
            app.app_state.ui.libraries.status_message =
                match crate::library::rename_library(current_name, new_name) {
                    Ok(()) => Some(format!("Renamed '{current_name}' to '{new_name}'.")),
                    Err(error) => Some(error.to_string()),
                };
            refresh(app);
        }
        (_, KeyCode::Backspace) => {
            input.buffer.pop();
            app.app_state.ui.libraries.input = Some(input);
        }
        (_, KeyCode::Char(character)) => {
            input.buffer.push(character);
            app.app_state.ui.libraries.input = Some(input);
        }
        _ => {
            app.app_state.ui.libraries.input = Some(input);
        }
    }
}

fn open_library_path_picker(name: String, app: &mut App) {
    let initial_path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    app.app_state.ui.file_browser.browser_generation = app
        .app_state
        .ui
        .file_browser
        .browser_generation
        .wrapping_add(1);
    spawn_app_command_sender(
        app.app_command_tx.clone(),
        app.shutdown_tx.subscribe(),
        AppCommand::FetchFileTree {
            browser_generation: app.app_state.ui.file_browser.browser_generation,
            path: initial_path,
            browser_mode: FileBrowserMode::LibraryPathSelection { name },
            preserve_browser_mode: false,
            highlight_path: None,
        },
    );
}

pub fn draw_panel(f: &mut Frame, screen: &ScreenContext<'_>, area: Rect, focused: bool) {
    let state = &screen.ui.ui.libraries;
    let ctx = screen.theme;
    let block = Block::default()
        .title(" Libraries ")
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .border_style(ctx.apply(Style::default().fg(if focused {
            ctx.state_selected()
        } else {
            ctx.theme.semantic.border
        })));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let list_height =
        (state.entries.len() as u16 + 2).clamp(3, inner.height.saturating_div(2).max(3));
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(list_height),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .split(inner);
    f.render_widget(
        Paragraph::new("One active configuration · previews are read-only"),
        chunks[0],
    );

    let libraries_block = Block::default()
        .title(" Saved ")
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)));
    let libraries_inner = libraries_block.inner(chunks[1]);
    f.render_widget(libraries_block, chunks[1]);

    let name_width = usize::from(libraries_inner.width.saturating_sub(10)).clamp(8, 24);
    let items = state.entries.iter().map(|entry| {
        let marker = if entry.active { "*" } else { " " };
        let count = if !entry.available {
            "!".to_string()
        } else {
            state
                .previews
                .get(&entry.name)
                .map(|preview| {
                    if preview.error.is_some() {
                        "!".to_string()
                    } else {
                        preview.torrent_names.len().to_string()
                    }
                })
                .unwrap_or_else(|| {
                    if state.previews_loading {
                        "…".to_string()
                    } else {
                        "—".to_string()
                    }
                })
        };
        ListItem::new(format!("{marker} {:<name_width$} {:>6}", entry.name, count,))
    });
    let mut list_state = ListState::default().with_selected(Some(state.selected_index));
    f.render_stateful_widget(
        List::new(items).highlight_symbol("> ").highlight_style(
            ctx.apply(
                Style::default()
                    .fg(ctx.state_selected())
                    .add_modifier(Modifier::BOLD),
            ),
        ),
        libraries_inner,
        &mut list_state,
    );

    render_torrent_preview(f, screen, chunks[2]);

    let detail = if let Some(input) = &state.input {
        match &input.kind {
            LibraryInputKind::AddName => format!("New library name: {}", input.buffer),
            LibraryInputKind::Rename { current_name } => {
                format!("Rename '{current_name}' to: {}", input.buffer)
            }
            LibraryInputKind::ConfirmRemove { name } => {
                format!("Remove '{name}' from the registry? No files will be deleted. [y/N]")
            }
        }
    } else if let Some(message) = &state.status_message {
        message.clone()
    } else if let Some(entry) = state.entries.get(state.selected_index) {
        let path = entry.shared_config_path.display().to_string();
        let max_width = chunks[3].width.saturating_sub(2) as usize;
        let path = truncate_with_ellipsis(&path, max_width);
        entry
            .description
            .as_ref()
            .map(|description| format!("{description}\n{path}"))
            .unwrap_or(path)
    } else {
        "No libraries yet. Press [a] to add one.".to_string()
    };
    f.render_widget(Paragraph::new(detail).wrap(Wrap { trim: true }), chunks[3]);
}

pub fn draw_footer(f: &mut Frame, screen: &ScreenContext<'_>, area: Rect) {
    let ctx = screen.theme;
    let input = screen.ui.ui.libraries.input.as_ref();
    let actions = match input.map(|input| &input.kind) {
        Some(LibraryInputKind::ConfirmRemove { .. }) => vec![
            ("y", "remove", ActionTone::Destructive),
            ("n/Esc", "cancel", ActionTone::Cancel),
        ],
        Some(LibraryInputKind::AddName) => vec![
            ("Enter", "choose folder", ActionTone::Confirm),
            ("Esc", "cancel", ActionTone::Cancel),
        ],
        Some(LibraryInputKind::Rename { .. }) => vec![
            ("Enter", "rename", ActionTone::Confirm),
            ("Esc", "cancel", ActionTone::Cancel),
        ],
        None => vec![
            ("Enter", "switch", ActionTone::Confirm),
            ("a", "add", ActionTone::Add),
            ("Esc", "settings", ActionTone::Cancel),
            ("r", "rename", ActionTone::Edit),
            ("d", "remove", ActionTone::Destructive),
            ("PgUp/PgDn", "names", ActionTone::Navigate),
        ],
    };
    let mut spans = Vec::new();
    let mut used = 0usize;
    for (key, label, tone) in &actions {
        let separator_width = usize::from(!spans.is_empty()) * 2;
        let action_width = key.chars().count() + label.chars().count() + 3;
        if used + separator_width + action_width > area.width as usize {
            continue;
        }
        if !spans.is_empty() {
            spans.push(Span::styled(
                "  ",
                ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
            ));
            used += 2;
        }
        spans.push(Span::styled(
            format!("[{key}]"),
            footer_key_style(ctx, *tone),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
        ));
        used += action_width;
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
        area,
    );
}

fn render_torrent_preview(f: &mut Frame, screen: &ScreenContext<'_>, area: Rect) {
    let state = &screen.ui.ui.libraries;
    let ctx = screen.theme;
    let selected_entry = state.entries.get(state.selected_index);
    let preview = selected_entry.and_then(|entry| state.previews.get(&entry.name));
    let title = match (selected_entry, preview) {
        (Some(entry), Some(preview)) if preview.error.is_none() => format!(
            " Torrents · {} · {} ",
            entry.name,
            preview.torrent_names.len()
        ),
        (Some(entry), _) => format!(" Torrents · {} ", entry.name),
        (None, _) => " Torrent names ".to_string(),
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(preview) = preview else {
        let message = if state.previews_loading && selected_entry.is_some() {
            "Loading torrent metadata…"
        } else if selected_entry.is_some() {
            "Torrent metadata is not loaded."
        } else {
            "Select or add a library."
        };
        f.render_widget(Paragraph::new(message).wrap(Wrap { trim: true }), inner);
        return;
    };
    if let Some(error) = &preview.error {
        f.render_widget(
            Paragraph::new(format!("Could not read catalog: {error}")).wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }
    if preview.torrent_names.is_empty() {
        f.render_widget(Paragraph::new("No torrents in this library."), inner);
        return;
    }

    let visible_height = inner.height as usize;
    let start = torrent_preview_start(
        preview.torrent_names.len(),
        state.preview_scroll,
        visible_height,
    );
    let names = preview
        .torrent_names
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_height)
        .map(|(index, name)| {
            let name = if screen.ui.anonymize_torrent_names {
                format!("torrent-{:05}", index + 1)
            } else {
                sanitize_text(name)
            };
            ListItem::new(format!("{:>5}. {name}", index + 1))
        });
    f.render_widget(List::new(names), inner);
}

fn torrent_preview_start(total: usize, requested_start: usize, visible_height: usize) -> usize {
    requested_start.min(total.saturating_sub(visible_height))
}

#[cfg(test)]
mod tests {
    use super::torrent_preview_start;

    #[test]
    fn torrent_preview_scroll_keeps_the_last_page_full() {
        assert_eq!(torrent_preview_start(25, 0, 10), 0);
        assert_eq!(torrent_preview_start(25, 10, 10), 10);
        assert_eq!(torrent_preview_start(25, 24, 10), 15);
        assert_eq!(torrent_preview_start(4, 20, 10), 0);
    }
}
