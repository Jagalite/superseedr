// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use ratatui::{prelude::*, widgets::*};
use web_time::{SystemTime, UNIX_EPOCH};

use crate::app::{AppMode, AppState};
use crate::terminal_event::{Event as CrosstermEvent, KeyCode, KeyEventKind};
use crate::tui::action_style::{footer_key_style, ActionTone};
use crate::tui::formatters::{
    auto_download_limit_applied, centered_rect, format_limit_bps, format_speed,
};
use crate::tui::render::calculate_player_stats;
use crate::tui::screen_context::ScreenContext;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PowerAction {
    Resume,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PowerEffect {
    ToNormal,
}

#[derive(Default)]
pub struct PowerReduceResult {
    pub consumed: bool,
    pub effects: Vec<PowerEffect>,
}

fn map_key_to_power_action(key_code: KeyCode, key_kind: KeyEventKind) -> Option<PowerAction> {
    if key_kind == KeyEventKind::Press && matches!(key_code, KeyCode::Char('z')) {
        return Some(PowerAction::Resume);
    }
    None
}

pub fn reduce_power_action(action: PowerAction) -> PowerReduceResult {
    match action {
        PowerAction::Resume => PowerReduceResult {
            consumed: true,
            effects: vec![PowerEffect::ToNormal],
        },
    }
}

pub fn execute_power_effects(app_state: &mut AppState, effects: Vec<PowerEffect>) {
    for effect in effects {
        match effect {
            PowerEffect::ToNormal => app_state.mode = AppMode::Normal,
        }
    }
}

pub fn handle_event(event: CrosstermEvent, app_state: &mut AppState) {
    if let CrosstermEvent::Key(key) = event {
        if let Some(action) = map_key_to_power_action(key.code, key.kind) {
            let reduced = reduce_power_action(action);
            if reduced.consumed {
                execute_power_effects(app_state, reduced.effects);
            }
        }
    }
}

pub fn draw(f: &mut Frame, screen: &ScreenContext<'_>) {
    let app_state = screen.ui;
    let settings = screen.settings;
    let ctx = screen.theme;
    const LEVEL_GAUGE_WIDTH: usize = 16;
    const TRANQUIL_MESSAGES: &[&str] = &[
        "Quietly seeding...",
        "Awaiting peers...",
        "Sharing data...",
        "Connecting to the swarm...",
        "Sharing pieces...",
        "The network is vast...",
        "Listening for connections...",
        "Seeding the cloud...",
        "Uptime is a gift...",
        "Data flows...",
        "Maintaining the ratio...",
        "A torrent of tranquility...",
        "A piece at a time...",
        "The swarm is peaceful...",
        "Be the torrent...",
        "Nurturing the swarm...",
        "Awaiting the handshake...",
        "Distributing packets...",
        "The ratio is balanced...",
        "Each piece finds its home...",
        "Announcing to the tracker...",
        "The bitfield is complete...",
    ];

    let dl_speed = *app_state.avg_download_history.last().unwrap_or(&0);
    let ul_speed = *app_state.avg_upload_history.last().unwrap_or(&0);
    let dl_limit = app_state.effective_download_limit_bps;
    let ul_limit = settings.global_upload_limit_bps;
    let dl_auto_limited = auto_download_limit_applied(settings.global_download_limit_bps, dl_limit);
    let (level, level_progress) = calculate_player_stats(app_state);
    let level_filled_len = (level_progress * LEVEL_GAUGE_WIDTH as f64).round() as usize;
    let level_empty_len = LEVEL_GAUGE_WIDTH.saturating_sub(level_filled_len);
    let level_gauge = format!(
        "[{}{}]",
        "=".repeat(level_filled_len),
        "-".repeat(level_empty_len),
    );
    let level_percent = format!("{:.0}%", level_progress * 100.0);

    let area = centered_rect(40, 60, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)));
    let inner_area = block.inner(area);
    f.render_widget(block, area);

    let vertical_chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(8),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(inner_area);
    let content_area = vertical_chunks[1];
    let footer_area = vertical_chunks[3];

    let mut dl_spans = vec![
        Span::styled("DL: ", ctx.apply(Style::default().fg(ctx.accent_sky()))),
        Span::styled(
            format_speed(dl_speed),
            ctx.apply(Style::default().fg(ctx.accent_sky())),
        ),
        Span::raw(" / "),
    ];
    if dl_auto_limited || (dl_limit > 0 && dl_speed >= dl_limit) {
        dl_spans.push(Span::styled(
            format_limit_bps(dl_limit),
            ctx.apply(Style::default().fg(ctx.state_error())),
        ));
    } else {
        dl_spans.push(Span::styled(
            format_limit_bps(dl_limit),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
        ));
    }

    let mut ul_spans = vec![
        Span::styled("UL: ", ctx.apply(Style::default().fg(ctx.accent_teal()))),
        Span::styled(
            format_speed(ul_speed),
            ctx.apply(Style::default().fg(ctx.accent_teal())),
        ),
        Span::raw(" / "),
    ];
    if ul_limit > 0 && ul_speed >= ul_limit {
        ul_spans.push(Span::styled(
            format_limit_bps(ul_limit),
            ctx.apply(Style::default().fg(ctx.state_error())),
        ));
    } else {
        ul_spans.push(Span::styled(
            format_limit_bps(ul_limit),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
        ));
    }

    const MESSAGE_INTERVAL_SECONDS: u64 = 500;
    let seconds_since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let seed = seconds_since_epoch / MESSAGE_INTERVAL_SECONDS;
    let mut rng = StdRng::seed_from_u64(seed);
    let message_index = rng.random_range(0..TRANQUIL_MESSAGES.len());
    let current_message = TRANQUIL_MESSAGES[message_index];

    let main_content_lines = vec![
        Line::from(vec![
            Span::styled("super", ctx.apply(Style::default().fg(ctx.accent_sky()))),
            Span::styled("seedr", ctx.apply(Style::default().fg(ctx.accent_teal()))),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            current_message,
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        )),
        Line::from(""),
        Line::from(dl_spans),
        Line::from(ul_spans),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("Level {}", level),
                ctx.apply(Style::default().fg(ctx.state_selected())),
            ),
            Span::raw("  "),
            Span::styled(
                level_gauge,
                ctx.apply(Style::default().fg(ctx.state_success())),
            ),
            Span::raw("  "),
            Span::styled(
                level_percent,
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
            ),
        ]),
    ];
    let main_paragraph = Paragraph::new(main_content_lines).alignment(Alignment::Center);
    let footer_line = Line::from(vec![
        Span::styled(
            "Press ",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
        ),
        Span::styled("[z]", footer_key_style(ctx, ActionTone::Toggle)),
        Span::styled(
            " to resume",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
        ),
    ]);
    let footer_paragraph = Paragraph::new(footer_line).alignment(Alignment::Center);

    f.render_widget(main_paragraph, content_area);
    f.render_widget(footer_paragraph, footer_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_event::{KeyEvent, KeyModifiers};

    #[test]
    fn power_z_returns_to_normal() {
        let mut app_state = AppState {
            mode: AppMode::PowerSaving,
            ..Default::default()
        };

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE)),
            &mut app_state,
        );

        assert!(matches!(app_state.mode, AppMode::Normal));
    }

    #[test]
    fn power_ignores_other_keys() {
        let mut app_state = AppState {
            mode: AppMode::PowerSaving,
            ..Default::default()
        };

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            &mut app_state,
        );

        assert!(matches!(app_state.mode, AppMode::PowerSaving));
    }
}
