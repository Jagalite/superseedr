// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::AppState;
use crate::config::Settings;
use crate::theme::{color_to_rgb, ThemeContext};
use ratatui::buffer::Buffer;
use ratatui::prelude::{Color, Frame, Rect};

pub(crate) fn compute_effects_activity_speed_multiplier(
    app_state: &AppState,
    settings: &Settings,
) -> f64 {
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
