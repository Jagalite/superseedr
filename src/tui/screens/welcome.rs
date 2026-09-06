// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::terminal_event::{Event as CrosstermEvent, KeyCode, KeyEventKind};
use ratatui::{prelude::*, widgets::*};
use web_time::{SystemTime, UNIX_EPOCH};

use crate::app::{AppMode, AppState};
use crate::theme::{blend_colors, color_to_rgb, ThemeContext};
use crate::tui::action_style::{footer_key_style, ActionTone};
use crate::tui::screen_context::ScreenContext;

const WELCOME_LICENSE_LABEL: &str = "GNU General Public License v3.0";

const LOGO_LARGE: &str = r#"
                                                             __          
                                                            /\ \         
  ____  __  __  _____      __   _ __   ____     __     __   \_\ \  _ __  
 /',__\/\ \/\ \/\ '__`\  /'__`\/\`'__\/',__\  /'__`\ /'__`\ /'_` \/\`'__\
/\__, `\ \ \_\ \ \ \L\ \/\  __/\ \ \//\__, `\/\  __//\  __//\ \L\ \ \ \/ 
\/\____/\ \____/\ \ ,__/\ \____\\ \_\\/\____/\ \____\ \____\ \___,_\ \_\ 
 \/___/  \/___/  \ \ \/  \/____/ \/_/ \/___/  \/____/\/____/\/__,_ /\/_/ 
                  \ \_\                                                  
                   \/_/                                                  
"#;

const LOGO_MEDIUM: &str = r#"
                        __          
                       /\ \         
  ____     __     __   \_\ \  _ __  
 /',__\  /'__`\ /'__`\ /'_` \/\`'__\
/\__, `\/\  __//\  __//\ \L\ \ \ \/ 
\/\____/\ \____\ \____\ \___,_\ \_\ 
 \/___/  \/____/\/____/\/__,_ /\/_/ 
"#;

const LOGO_SMALL: &str = r#"
  ____    ____  
 /',__\  /',__\ 
/\__, `\/\__, `\
\/\____/\/\____/
 \/___/  \/___/ 
"#;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WelcomeAction {
    Dismiss,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WelcomeEffect {
    ToNormal,
}

#[derive(Default)]
pub struct WelcomeReduceResult {
    pub consumed: bool,
    pub effects: Vec<WelcomeEffect>,
}

fn map_key_to_welcome_action(key_code: KeyCode, key_kind: KeyEventKind) -> Option<WelcomeAction> {
    if key_kind == KeyEventKind::Press && key_code == KeyCode::Esc {
        return Some(WelcomeAction::Dismiss);
    }
    None
}

pub fn reduce_welcome_action(action: WelcomeAction) -> WelcomeReduceResult {
    match action {
        WelcomeAction::Dismiss => WelcomeReduceResult {
            consumed: true,
            effects: vec![WelcomeEffect::ToNormal],
        },
    }
}

pub fn execute_welcome_effects(app_state: &mut AppState, effects: Vec<WelcomeEffect>) {
    for effect in effects {
        match effect {
            WelcomeEffect::ToNormal => app_state.mode = AppMode::Normal,
        }
    }
}

pub fn draw(f: &mut Frame, screen: &ScreenContext<'_>) {
    let settings = screen.settings;
    let ctx = screen.theme;
    let area = f.area();

    draw_background_dust(f, area, ctx);

    let get_dims = |text: &str| -> (u16, u16) {
        let h = text.lines().count() as u16;
        let w = text.lines().map(|l| l.len()).max().unwrap_or(0) as u16;
        (w, h)
    };

    let (w_large, h_large) = get_dims(LOGO_LARGE);
    let (w_medium, h_medium) = get_dims(LOGO_MEDIUM);

    let download_path_str = settings
        .default_download_folder
        .as_ref()
        .map(|p| p.to_string_lossy())
        .unwrap_or_else(|| std::borrow::Cow::Borrowed("Manual Selection"));

    let text_lines = vec![
        Line::from(Span::styled(
            "How to Get Started:",
            ctx.apply(Style::default().fg(ctx.state_warning()).bold()),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(" ★ ", ctx.apply(Style::default().fg(ctx.state_success()))),
            Span::raw("Paste using "),
            Span::styled(
                "your terminal shortcut",
                ctx.apply(Style::default().fg(ctx.accent_sky()).bold()),
            ),
            Span::raw(" to add a "),
            Span::styled(
                "magnet link",
                ctx.apply(Style::default().fg(ctx.accent_peach())),
            ),
            Span::raw("."),
        ]),
        Line::from(vec![
            Span::raw("      - "),
            Span::styled(
                "e.g. \"magnet:?xt=urn:btih:...\"",
                Style::default()
                    .fg(ctx.theme.semantic.surface2)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]),
        Line::from(vec![
            Span::styled(" ★ ", ctx.apply(Style::default().fg(ctx.state_success()))),
            Span::raw("Press "),
            Span::styled("[a]", footer_key_style(ctx, ActionTone::Add)),
            Span::raw(" to open the file picker and select a "),
            Span::styled(
                "`.torrent`",
                ctx.apply(Style::default().fg(ctx.accent_peach())),
            ),
            Span::raw(" file."),
        ]),
        Line::from(vec![
            Span::styled(" ★ ", ctx.apply(Style::default().fg(ctx.state_success()))),
            Span::raw("Use the "),
            Span::styled(
                "CLI",
                ctx.apply(Style::default().fg(ctx.accent_sky()).bold()),
            ),
            Span::raw(" from another terminal:"),
        ]),
        Line::from(vec![
            Span::raw("      - magnet: "),
            Span::styled(
                "superseedr add \"magnet:?xt=urn:btih:...\"",
                ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
            ),
        ]),
        Line::from(vec![
            Span::raw("      - file:   "),
            Span::styled(
                "superseedr add \"/path/to/my.torrent\"",
                ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
            ),
        ]),
        Line::from(vec![
            Span::styled(" ★ ", ctx.apply(Style::default().fg(ctx.state_success()))),
            Span::raw("Drop files into your "),
            Span::styled(
                "Watch Folder",
                ctx.apply(Style::default().fg(ctx.accent_sky()).bold()),
            ),
            Span::raw(" to add them automatically."),
        ]),
        Line::from(vec![
            Span::styled(" ★ ", ctx.apply(Style::default().fg(ctx.state_success()))),
            Span::raw("Download Location: "),
            Span::styled(
                download_path_str,
                ctx.apply(Style::default().fg(ctx.accent_sky()).bold()),
            ),
        ]),
        Line::from(vec![
            Span::raw("      - "),
            Span::styled(
                "Change or remove in Config ",
                ctx.apply(Style::default().fg(ctx.theme.semantic.surface2))
                    .italic(),
            ),
            Span::styled("[C]", footer_key_style(ctx, ActionTone::Open).italic()),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "Browser Support: ",
                ctx.apply(Style::default().fg(ctx.state_warning()).bold()),
            ),
            Span::raw("To open magnet links directly from your browser,"),
        ]),
        Line::from(vec![
            Span::raw("   natively install superseedr: "),
            Span::styled(
                "https://github.com/Jagalite/superseedr/releases",
                Style::default().fg(ctx.state_info()).underlined(),
            ),
        ]),
    ];

    let footer_line = Line::from(vec![
        Span::styled(" [m] ", footer_key_style(ctx, ActionTone::Open)),
        Span::styled(
            "Manual/Help",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ),
        Span::styled(
            " | ",
            ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
        ),
        Span::styled(" [C] ", footer_key_style(ctx, ActionTone::Open)),
        Span::styled(
            "Config",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ),
        Span::styled(
            " | ",
            ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
        ),
        Span::styled(" [Q] ", footer_key_style(ctx, ActionTone::Destructive)),
        Span::styled(
            "Quit",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ),
        Span::styled(
            " | ",
            ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
        ),
        Span::styled(" [Esc] ", footer_key_style(ctx, ActionTone::Cancel)),
        Span::styled(
            "Dismiss",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ),
    ]);

    let text_content_height = text_lines.len() as u16;
    let text_content_width = text_lines.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
    let footer_width = footer_line.width() as u16;

    let box_vertical_gap = 1;
    let box_horizontal_padding = 4;
    let box_height_needed = text_content_height + box_vertical_gap + 1 + 2;

    let gap_height = 1;
    let available_height_for_logo = area
        .height
        .saturating_sub(box_height_needed + gap_height + 2);
    let margin_x = 6;

    let logo_text = if area.width >= (w_large + margin_x) && available_height_for_logo >= h_large {
        LOGO_LARGE
    } else if area.width >= (w_medium + margin_x) && available_height_for_logo >= h_medium {
        LOGO_MEDIUM
    } else {
        LOGO_SMALL
    };

    let (logo_w, logo_h) = get_dims(logo_text);

    let content_width_max = text_content_width
        .max(footer_width)
        .max(logo_w.min(text_content_width + 10));
    let box_width = (content_width_max + box_horizontal_padding + 2).min(area.width);
    let box_height = box_height_needed.min(area.height);

    let vertical_chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(logo_h),
        Constraint::Length(gap_height),
        Constraint::Length(box_height),
        Constraint::Min(0),
    ])
    .split(area);

    let logo_area = vertical_chunks[1];
    let box_area = vertical_chunks[3];

    let logo_layout = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(logo_w),
        Constraint::Min(0),
    ])
    .split(logo_area);

    let box_layout = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(box_width),
        Constraint::Min(0),
    ])
    .split(box_area);

    let final_logo_area = logo_layout[1];
    let final_box_area = box_layout[1];

    let buf = f.buffer_mut();
    for (y_local, line) in logo_text.lines().enumerate() {
        if y_local >= final_logo_area.height as usize {
            break;
        }

        let y_global = final_logo_area.y + y_local as u16;

        for (x_local, c) in line.chars().enumerate() {
            if x_local >= final_logo_area.width as usize {
                break;
            }

            if c == ' ' {
                continue;
            }

            let x_global = final_logo_area.x + x_local as u16;
            let style = get_animated_style(ctx, x_local, y_local);
            buf.set_string(x_global, y_global, c.to_string(), style);
        }
    }

    crate::tui::render::clear(f, final_box_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)));
    let inner_box = block.inner(final_box_area);
    f.render_widget(block, final_box_area);

    let box_internal_chunks = Layout::vertical([
        Constraint::Length(text_content_height),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(inner_box);

    let text_padding_layout = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(text_content_width),
        Constraint::Min(0),
    ])
    .split(box_internal_chunks[0]);

    let text_paragraph = Paragraph::new(text_lines)
        .style(ctx.apply(Style::default().fg(ctx.theme.semantic.text)))
        .alignment(Alignment::Left);

    f.render_widget(text_paragraph, text_padding_layout[1]);

    let footer_paragraph = Paragraph::new(footer_line).alignment(Alignment::Center);
    f.render_widget(footer_paragraph, box_internal_chunks[2]);

    let license_area = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
    let license_paragraph = Paragraph::new(WELCOME_LICENSE_LABEL)
        .style(ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)))
        .alignment(Alignment::Center);
    f.render_widget(license_paragraph, license_area);
}

pub fn handle_event(event: CrosstermEvent, app_state: &mut AppState) {
    if let CrosstermEvent::Key(key) = event {
        if let Some(action) = map_key_to_welcome_action(key.code, key.kind) {
            let reduced = reduce_welcome_action(action);
            if reduced.consumed {
                execute_welcome_effects(app_state, reduced.effects);
            }
        }
    }
}

fn get_animated_style(ctx: &ThemeContext, x: usize, y: usize) -> Style {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    let speed = 3.0;
    let freq_x = 0.1;
    let freq_y = 0.2;
    let phase = (x as f64 * freq_x) + (y as f64 * freq_y) - (time * speed);
    let ratio = (phase.sin() + 1.0) / 2.0;

    let color_blue = color_to_rgb(ctx.theme.scale.stream.inflow);
    let color_green = color_to_rgb(ctx.theme.scale.stream.outflow);
    let base_color = blend_colors(color_blue, color_green, ratio);

    let seed = (x as f64 * 13.0 + y as f64 * 29.0 + time * 15.0).sin();

    let style = if seed > 0.85 {
        Style::default()
            .fg(ctx.theme.semantic.white)
            .add_modifier(Modifier::BOLD)
    } else if seed > 0.5 {
        ctx.apply(Style::default().fg(base_color))
            .add_modifier(Modifier::BOLD)
    } else {
        ctx.apply(Style::default().fg(base_color))
            .add_modifier(Modifier::DIM)
    };

    ctx.apply(style)
}

fn draw_background_dust(f: &mut Frame, area: Rect, ctx: &ThemeContext) {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    let width = area.width as usize;
    let height = area.height as usize;

    let mut lines = Vec::with_capacity(height);

    let move_angle_x = 0.8;
    let move_angle_y = 0.4;

    for y in 0..height {
        let mut spans = Vec::with_capacity(width);
        for x in 0..width {
            let speed_3 = 4.0;
            let pos_x_3 = x as f64 - (time * speed_3 * move_angle_x);
            let pos_y_3 = y as f64 + (time * speed_3 * move_angle_y);
            let noise_3 = (pos_x_3 * 0.73 + pos_y_3 * 0.19).sin() * (pos_y_3 * 1.3).cos();
            if noise_3 > 0.985 {
                spans.push(Span::styled(
                    "+",
                    Style::default()
                        .fg(ctx.state_success())
                        .add_modifier(Modifier::BOLD),
                ));
                continue;
            }

            let speed_2 = 4.0;
            let pos_x_2 = x as f64 - (time * speed_2 * move_angle_x);
            let pos_y_2 = y as f64 + (time * speed_2 * move_angle_y);
            let noise_2 = (pos_x_2 * 0.3 + pos_y_2 * 0.8).sin() * (pos_x_2 * 0.4).cos();
            if noise_2 > 0.95 {
                spans.push(Span::styled(
                    "·",
                    ctx.apply(Style::default().fg(ctx.state_info())),
                ));
                continue;
            }

            let speed_1 = 1.5;
            let pos_x_1 = x as f64 - (time * speed_1 * move_angle_x);
            let pos_y_1 = y as f64 + (time * speed_1 * move_angle_y);
            let noise_1 = (pos_x_1 * 0.15 + pos_y_1 * 0.15).sin();
            if noise_1 > 0.96 {
                spans.push(Span::styled(
                    ".",
                    Style::default()
                        .fg(ctx.theme.semantic.surface2)
                        .add_modifier(Modifier::DIM),
                ));
                continue;
            }

            spans.push(Span::raw(" "));
        }
        lines.push(Line::from(spans));
    }

    let p = Paragraph::new(lines);
    f.render_widget(p, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_event::{KeyEvent, KeyModifiers};

    #[test]
    fn welcome_esc_transitions_to_normal() {
        let mut app_state = AppState {
            mode: AppMode::Welcome,
            ..Default::default()
        };

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &mut app_state,
        );

        assert!(matches!(app_state.mode, AppMode::Normal));
    }

    #[test]
    fn welcome_ignores_non_esc_keys() {
        let mut app_state = AppState {
            mode: AppMode::Welcome,
            ..Default::default()
        };

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::NONE)),
            &mut app_state,
        );

        assert!(matches!(app_state.mode, AppMode::Welcome));
    }
}
