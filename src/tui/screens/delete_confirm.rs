// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

#[cfg(not(target_arch = "wasm32"))]
use crate::app::App;
use crate::app::{AppCommand, AppMode, TorrentControlState};
use crate::integrations::control::ControlRequest;
use crate::terminal_event::{Event as CrosstermEvent, KeyCode};
use crate::tui::action_style::{footer_key_style, ActionTone};
use crate::tui::app_command::spawn_app_command_sender;
use crate::tui::formatters::{centered_rect, sanitize_text};
use crate::tui::screen_context::ScreenContext;
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::prelude::{Frame, Line, Span, Style};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DeleteConfirmAction {
    Confirm,
    Cancel,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DeleteConfirmEffect {
    SendManagerCommand {
        info_hash: Vec<u8>,
        with_files: bool,
    },
    MarkDeleting {
        info_hash: Vec<u8>,
    },
    ToNormal,
}

#[derive(Default)]
pub struct DeleteConfirmReduceResult {
    pub consumed: bool,
    pub effects: Vec<DeleteConfirmEffect>,
}

fn map_key_to_delete_confirm_action(key_code: KeyCode) -> Option<DeleteConfirmAction> {
    match key_code {
        KeyCode::Char('Y') => Some(DeleteConfirmAction::Confirm),
        KeyCode::Esc => Some(DeleteConfirmAction::Cancel),
        _ => None,
    }
}

pub fn reduce_delete_confirm_action(
    app_state: &crate::app::AppState,
    action: DeleteConfirmAction,
) -> DeleteConfirmReduceResult {
    match action {
        DeleteConfirmAction::Cancel => DeleteConfirmReduceResult {
            consumed: true,
            effects: vec![DeleteConfirmEffect::ToNormal],
        },
        DeleteConfirmAction::Confirm => {
            let info_hash = app_state.ui.delete_confirm.info_hash.clone();
            let with_files = app_state.ui.delete_confirm.with_files;
            DeleteConfirmReduceResult {
                consumed: true,
                effects: vec![
                    DeleteConfirmEffect::SendManagerCommand {
                        info_hash: info_hash.clone(),
                        with_files,
                    },
                    DeleteConfirmEffect::MarkDeleting { info_hash },
                    DeleteConfirmEffect::ToNormal,
                ],
            }
        }
    }
}

pub fn draw(f: &mut Frame, screen: &ScreenContext<'_>) {
    let app_state = screen.ui;
    let ctx = screen.theme;

    if !matches!(app_state.mode, AppMode::DeleteConfirm) {
        return;
    }

    let info_hash = &app_state.ui.delete_confirm.info_hash;
    let with_files = app_state.ui.delete_confirm.with_files;

    if let Some(torrent_to_delete) = app_state.torrents.get(info_hash) {
        let terminal_area = f.area();
        let rect_width = if terminal_area.width < 60 { 90 } else { 50 };
        let rect_height = if terminal_area.height < 20 { 95 } else { 18 };

        let area = centered_rect(rect_width, rect_height, terminal_area);
        f.render_widget(Clear, area);

        let vert_padding = if area.height < 10 { 0 } else { 1 };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(ctx.apply(Style::default().fg(ctx.state_error())))
            .padding(Padding::new(2, 2, vert_padding, vert_padding));

        let inner_area = block.inner(area);
        f.render_widget(block, area);

        let chunks = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner_area);

        let name = sanitize_text(&torrent_to_delete.latest_state.torrent_name);
        let path = torrent_to_delete
            .latest_state
            .download_path
            .as_ref()
            .map(|p| sanitize_text(&p.to_string_lossy()))
            .unwrap_or_else(|| "Unknown Path".to_string());

        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    name,
                    ctx.apply(Style::default().fg(ctx.state_warning()).bold().underlined()),
                )),
                Line::from(Span::styled(
                    path,
                    ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                )),
            ])
            .alignment(Alignment::Center),
            chunks[0],
        );

        if chunks[1].height > 0 {
            let body = if with_files {
                vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        "⚠️ PERMANENT TORRENT FILES DELETION ON ⚠️",
                        ctx.apply(Style::default().fg(ctx.state_error()).bold()),
                    )),
                    Line::from(vec![
                        Span::raw("All local data for this torrent will be "),
                        Span::styled(
                            "ERASED",
                            ctx.apply(Style::default().fg(ctx.state_error()).bold().underlined()),
                        ),
                    ]),
                ]
            } else {
                vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        "Safe Removal (Files Kept)",
                        ctx.apply(Style::default().fg(ctx.state_success())),
                    )),
                    Line::from(vec![
                        Span::raw("Use "),
                        Span::styled("[D]", footer_key_style(ctx, ActionTone::Destructive)),
                        Span::raw(" to remove files..."),
                    ]),
                ]
            };
            f.render_widget(
                Paragraph::new(body)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: true }),
                chunks[1],
            );
        }

        let actions = Line::from(vec![
            Span::styled("[Y]", footer_key_style(ctx, ActionTone::Destructive)),
            Span::raw(" Confirm  "),
            Span::styled("[Esc]", footer_key_style(ctx, ActionTone::Cancel)),
            Span::raw(" Cancel"),
        ]);

        f.render_widget(
            Paragraph::new(actions).alignment(Alignment::Center),
            chunks[3],
        );
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn handle_event(event: CrosstermEvent, app: &mut App) -> bool {
    if let CrosstermEvent::Key(key) = event {
        if let Some(action) = map_key_to_delete_confirm_action(key.code) {
            let reduced = reduce_delete_confirm_action(&app.app_state, action);
            for effect in reduced.effects {
                match effect {
                    DeleteConfirmEffect::SendManagerCommand {
                        info_hash,
                        with_files,
                    } => {
                        spawn_app_command_sender(
                            app.app_command_tx.clone(),
                            app.shutdown_tx.subscribe(),
                            AppCommand::SubmitControlRequest(ControlRequest::Delete {
                                info_hash_hex: hex::encode(info_hash),
                                delete_files: with_files,
                            }),
                        );
                    }
                    DeleteConfirmEffect::MarkDeleting { info_hash } => {
                        if !app.is_current_shared_follower() {
                            if let Some(torrent) = app.app_state.torrents.get_mut(&info_hash) {
                                torrent.latest_state.torrent_control_state =
                                    TorrentControlState::Deleting;
                                torrent.latest_state.delete_files =
                                    app.app_state.ui.delete_confirm.with_files;
                            }
                        }
                    }
                    DeleteConfirmEffect::ToNormal => {
                        app.app_state.mode = AppMode::Normal;
                    }
                }
            }
            return reduced.consumed;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppMode, AppState};

    #[test]
    fn keymap_uses_shift_y_for_confirm() {
        assert_eq!(
            map_key_to_delete_confirm_action(KeyCode::Char('Y')),
            Some(DeleteConfirmAction::Confirm)
        );
        assert_eq!(map_key_to_delete_confirm_action(KeyCode::Enter), None);
    }

    #[test]
    fn reducer_cancel_closes_without_effects() {
        let app_state = AppState::default();
        let out = reduce_delete_confirm_action(&app_state, DeleteConfirmAction::Cancel);
        assert!(out.consumed);
        assert_eq!(out.effects, vec![DeleteConfirmEffect::ToNormal]);
    }

    #[test]
    fn reducer_confirm_emits_command_and_mark_deleting() {
        let app_state = AppState {
            mode: AppMode::DeleteConfirm,
            ui: crate::app::UiState {
                delete_confirm: crate::app::DeleteConfirmUiState {
                    info_hash: b"abc".to_vec(),
                    with_files: true,
                },
                ..Default::default()
            },
            ..Default::default()
        };

        let out = reduce_delete_confirm_action(&app_state, DeleteConfirmAction::Confirm);

        assert!(out.consumed);
        assert_eq!(out.effects.len(), 3);
        assert!(matches!(
            out.effects[0],
            DeleteConfirmEffect::SendManagerCommand {
                ref info_hash,
                with_files: true
            } if info_hash == b"abc"
        ));
        assert!(matches!(
            out.effects[1],
            DeleteConfirmEffect::MarkDeleting { ref info_hash } if info_hash == b"abc"
        ));
        assert!(matches!(out.effects[2], DeleteConfirmEffect::ToNormal));
    }
}
