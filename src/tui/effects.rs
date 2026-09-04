// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{AppMode, AppState};
use crate::config::Settings;
use crate::theme::{color_to_rgb, ThemeContext, ThemeName};

use ratatui::buffer::Buffer;
use ratatui::prelude::{Color, Frame, Rect};

mod show;

pub(crate) fn show_theme_animation_active(app_state: &AppState) -> bool {
    app_state.theme.name == ThemeName::Show
        && app_state.theme.effects.enabled()
        && !matches!(app_state.mode, AppMode::PowerSaving)
}

pub(crate) fn compute_effects_phase_delta(theme: ThemeName, elapsed: f64) -> f64 {
    if !elapsed.is_finite() {
        return 0.0;
    }
    // Show samples a score directly, so low frame rates should skip ahead in
    // the score instead of slowing its tempo with the simulation delta clamp.
    if theme == ThemeName::Show {
        elapsed.max(0.0)
    } else {
        elapsed.clamp(0.0, 0.25)
    }
}

pub(crate) fn compute_effects_activity_speed_multiplier(
    app_state: &AppState,
    settings: &Settings,
) -> f64 {
    // Show has a musical score: traffic must not speed up its pulses or scene changes.
    if app_state.theme.name == ThemeName::Show {
        return 1.0;
    }
    let dl_bps = app_state.avg_download_history.last().copied().unwrap_or(0) as f64;
    let ul_bps = app_state.avg_upload_history.last().copied().unwrap_or(0) as f64;

    let dl_limit = app_state.effective_download_limit_bps;
    let dl_ref = if !crate::config::is_unlimited_rate_limit_bps(dl_limit) {
        dl_limit as f64
    } else {
        4_000_000.0
    };
    let ul_ref = if !crate::config::is_unlimited_rate_limit_bps(settings.global_upload_limit_bps) {
        settings.global_upload_limit_bps as f64
    } else {
        1_000_000.0
    };

    let dl_activity = (dl_bps / dl_ref).clamp(0.0, 1.0);
    let ul_activity = (ul_bps / ul_ref).clamp(0.0, 1.0);

    let activity_score = (dl_activity * 0.60) + (ul_activity * 0.40);
    1.0 + (activity_score * 2.0)
}

pub(crate) fn apply_theme_effects_to_frame(f: &mut Frame, ctx: &ThemeContext) {
    if !ctx.theme.effects.enabled() {
        return;
    }

    let area = f.area();
    let buf = f.buffer_mut();
    if ctx.theme.name == ThemeName::Show {
        show::apply(buf, area, ctx);
        return;
    }

    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                if cell.fg != Color::Reset {
                    cell.fg = ctx.apply_effects_to_color_at(cell.fg, x, y, area.width, area.height);
                }
            }
        }
    }
}

pub(crate) fn apply_visualization_focus_dimming_to_frame(f: &mut Frame, selected: Rect) {
    let area = f.area();
    let buf = f.buffer_mut();
    apply_visualization_focus_dimming(buf, area, selected);
}

fn apply_visualization_focus_dimming(buf: &mut Buffer, area: Rect, selected: Rect) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if rect_contains(selected, x, y) {
                continue;
            }
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.fg = grayscale_color(cell.fg);
                cell.bg = grayscale_color(cell.bg);
            }
        }
    }
}

fn rect_contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.left() && x < area.right() && y >= area.top() && y < area.bottom()
}

fn grayscale_color(color: Color) -> Color {
    if color == Color::Reset {
        return color;
    }

    let (red, green, blue) = color_to_rgb(color);
    let luminance =
        ((u32::from(red) * 54 + u32::from(green) * 183 + u32::from(blue) * 19) / 256) as u8;
    Color::Rgb(luminance, luminance, luminance)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_clock_keeps_tempo_across_frame_rates() {
        for frames in [1, 4, 10, 30, 60] {
            let elapsed: f64 = (0..frames)
                .map(|_| compute_effects_phase_delta(ThemeName::Show, 1.0 / frames as f64))
                .sum();
            assert!((elapsed - 1.0).abs() < 1e-10);
        }
        assert_eq!(compute_effects_phase_delta(ThemeName::Neon, 1.0), 0.25);
        for elapsed in [-1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(compute_effects_phase_delta(ThemeName::Show, elapsed), 0.0);
        }
    }

    #[test]
    fn show_keeps_its_score_tempo_and_respects_power_saving() {
        let settings = Settings::default();
        let mut state = AppState {
            theme: crate::theme::Theme::show(),
            mode: AppMode::PeerManagement,
            ..AppState::default()
        };
        assert!(show_theme_animation_active(&state));
        assert_eq!(
            compute_effects_activity_speed_multiplier(&state, &settings),
            1.0
        );
        state.avg_download_history.push(100_000_000);
        state.avg_upload_history.push(100_000_000);
        assert_eq!(
            compute_effects_activity_speed_multiplier(&state, &settings),
            1.0
        );
        state.mode = AppMode::PowerSaving;
        assert!(!show_theme_animation_active(&state));
        state.theme = crate::theme::Theme::neon();
        assert!(!show_theme_animation_active(&state));
        assert!(compute_effects_activity_speed_multiplier(&state, &settings) > 1.0);
    }

    #[test]
    fn grayscale_color_removes_saturation_and_preserves_reset() {
        assert_eq!(
            grayscale_color(Color::Rgb(200, 100, 50)),
            Color::Rgb(117, 117, 117)
        );
        assert_eq!(grayscale_color(Color::Reset), Color::Reset);
    }

    #[test]
    fn selected_rectangle_uses_exclusive_right_and_bottom_edges() {
        let area = Rect::new(10, 20, 5, 3);
        assert!(rect_contains(area, 10, 20));
        assert!(rect_contains(area, 14, 22));
        assert!(!rect_contains(area, 15, 22));
        assert!(!rect_contains(area, 14, 23));
    }

    #[test]
    fn focus_dimming_preserves_selected_cells_and_grays_the_surroundings() {
        let area = Rect::new(0, 0, 4, 1);
        let selected = Rect::new(1, 0, 2, 1);
        let mut buffer = Buffer::empty(area);
        for x in area.left()..area.right() {
            buffer.cell_mut((x, 0)).expect("cell").fg = Color::Rgb(200, 100, 50);
        }

        apply_visualization_focus_dimming(&mut buffer, area, selected);

        assert_eq!(
            buffer.cell((0, 0)).expect("outside cell").fg,
            Color::Rgb(117, 117, 117)
        );
        assert_eq!(
            buffer.cell((1, 0)).expect("selected cell").fg,
            Color::Rgb(200, 100, 50)
        );
        assert_eq!(
            buffer.cell((2, 0)).expect("selected cell").fg,
            Color::Rgb(200, 100, 50)
        );
        assert_eq!(
            buffer.cell((3, 0)).expect("outside cell").fg,
            Color::Rgb(117, 117, 117)
        );
    }
}
