// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use ratatui::{prelude::*, widgets::*};

use crate::tui::screen_context::ScreenContext;
use crate::tui::screens::{
    browser, config, delete_confirm, help, journal, normal, power, rss, welcome,
};

use crate::app::{AppMode, AppState};
use crate::theme::ThemeContext;

use crate::tui::effects::apply_theme_effects_to_frame;
use crate::tui::layout::normal::{calculate_layout, LayoutContext, DEFAULT_SIDEBAR_PERCENT};
use crate::tui::particles::{
    apply_theme_particles_background_to_frame, apply_theme_particles_foreground_to_frame,
};

use crate::config::Settings;

pub fn draw(f: &mut Frame, app_state: &AppState, settings: &Settings) {
    let area = f.area();

    let ctx = ThemeContext::new(app_state.theme, app_state.ui.effects_phase_time);
    let screen = ScreenContext::new(app_state, settings, &ctx);

    match &app_state.mode {
        AppMode::Help => {
            apply_theme_particles_background_to_frame(f, &ctx);
            help::draw(f, &screen);
            apply_theme_effects_to_frame(f, &ctx);
            apply_theme_particles_foreground_to_frame(f, &ctx);
            return;
        }
        AppMode::Journal => {
            apply_theme_particles_background_to_frame(f, &ctx);
            journal::draw(f, &screen);
            apply_theme_effects_to_frame(f, &ctx);
            apply_theme_particles_foreground_to_frame(f, &ctx);
            return;
        }
        AppMode::Welcome => {
            welcome::draw(f, &screen);
            apply_theme_effects_to_frame(f, &ctx);
            return;
        }
        AppMode::PowerSaving => {
            apply_theme_particles_background_to_frame(f, &ctx);
            power::draw(f, &screen);
            apply_theme_effects_to_frame(f, &ctx);
            apply_theme_particles_foreground_to_frame(f, &ctx);
            return;
        }
        AppMode::Config => {
            apply_theme_particles_background_to_frame(f, &ctx);
            config::draw(
                f,
                &screen,
                &app_state.ui.config.settings_edit,
                app_state.ui.config.selected_index,
                &app_state.ui.config.items,
                &app_state.ui.config.editing,
            );
            apply_theme_effects_to_frame(f, &ctx);
            apply_theme_particles_foreground_to_frame(f, &ctx);
            return;
        }
        AppMode::DeleteConfirm => {
            apply_theme_particles_background_to_frame(f, &ctx);
            delete_confirm::draw(f, &screen);
            apply_theme_effects_to_frame(f, &ctx);
            apply_theme_particles_foreground_to_frame(f, &ctx);
            return;
        }
        AppMode::FileBrowser => {
            apply_theme_particles_background_to_frame(f, &ctx);
            browser::draw(
                f,
                &screen,
                &app_state.ui.file_browser.state,
                &app_state.ui.file_browser.data,
                &app_state.ui.file_browser.browser_mode,
            );
            apply_theme_effects_to_frame(f, &ctx);
            apply_theme_particles_foreground_to_frame(f, &ctx);
            return;
        }
        AppMode::Rss => {
            apply_theme_particles_background_to_frame(f, &ctx);
            rss::draw(f, &screen);
            apply_theme_effects_to_frame(f, &ctx);
            apply_theme_particles_foreground_to_frame(f, &ctx);
            return;
        }
        _ => {}
    }

    apply_theme_particles_background_to_frame(f, &ctx);
    let layout_ctx = LayoutContext::new(area, app_state, DEFAULT_SIDEBAR_PERCENT);
    let plan = calculate_layout(area, &layout_ctx);

    normal::draw(f, &screen, &plan);

    if let Some(msg) = &plan.warning_message {
        f.render_widget(
            Paragraph::new(msg.as_str()).style(
                Style::default()
                    .fg(ctx.state_error())
                    .bg(ctx.theme.semantic.surface0),
            ),
            plan.list,
        );
    }
    if let Some(error_text) = &app_state.system_error {
        normal::draw_status_error_popup(f, error_text, screen.theme);
    }
    if app_state.should_quit {
        normal::draw_shutdown_screen(f, app_state, screen.theme);
    }

    apply_theme_effects_to_frame(f, &ctx);
    apply_theme_particles_foreground_to_frame(f, &ctx);
}

pub(crate) fn calculate_player_stats(app_state: &AppState) -> (u32, f64) {
    const XP_FOR_LEVEL_1: f64 = 5_000_000.0;

    const LEVEL_EXPONENT: f64 = 2.6;

    let total_seeding_size_bytes: u64 = app_state
        .torrents
        .values()
        .map(|t| t.latest_state.total_size)
        .sum();

    let total_gb = (total_seeding_size_bytes as f64) / 1_073_741_824.0;

    // - 100 GB Library -> ~500 XP/sec (~1.8 MB/hr)
    // - 1 TB Library   -> ~1500 XP/sec (~5.4 MB/hr)
    let passive_rate_per_sec = (total_gb + 1.0).powf(0.5) * 50.0;

    // Calculate total passive XP generated over the session runtime.
    let passive_xp = passive_rate_per_sec * (app_state.run_time as f64);

    // 1 Byte = 1 XP.
    let active_xp = app_state.session_total_uploaded as f64;

    let total_xp = active_xp + passive_xp;

    // Curve: Level = (XP / Base) ^ (1 / Exponent)
    // Inverse of: XP = Base * Level ^ Exponent
    //
    // L1   = 5 MB
    // L10  = 5 MB * 10^2.6 ~= 2 GB
    // L50  = 5 MB * 50^2.6 ~= 130 GB
    // L100 = 5 MB * 100^2.6 ~= 800 GB
    let raw_level = (total_xp / XP_FOR_LEVEL_1).powf(1.0 / LEVEL_EXPONENT);
    let current_level = raw_level.floor() as u32;

    // --- 5. PROGRESS BAR ---
    let xp_current_level_start = XP_FOR_LEVEL_1 * (current_level as f64).powf(LEVEL_EXPONENT);
    let xp_next_level_start = XP_FOR_LEVEL_1 * ((current_level + 1) as f64).powf(LEVEL_EXPONENT);

    let range = xp_next_level_start - xp_current_level_start;
    let progress_into_level = total_xp - xp_current_level_start;

    let ratio = if range <= 0.001 {
        0.0
    } else {
        progress_into_level / range
    };

    (current_level, ratio.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::layout::common::{compute_smart_table_layout, SmartCol};
    use crate::tui::layout::normal::{DEFAULT_SIDEBAR_PERCENT, MIN_SIDEBAR_WIDTH};
    use ratatui::layout::Rect;

    /// Helper to create a LayoutContext manually since we don't want to mock AppState.
    /// Accessing the struct fields directly allows us to bypass `LayoutContext::new`.
    fn create_ctx(width: u16, height: u16) -> LayoutContext {
        LayoutContext {
            width,
            height,
            settings_sidebar_percent: DEFAULT_SIDEBAR_PERCENT,
        }
    }

    #[test]
    fn test_too_small_window_width() {
        let width = 39;
        let height = 50;
        let area = Rect::new(0, 0, width, height);
        let ctx = create_ctx(width, height);

        let plan = calculate_layout(area, &ctx);

        assert!(plan.warning_message.is_some(), "Should warn if width < 40");
        assert_eq!(plan.warning_message.unwrap(), "Window too small");
    }

    #[test]
    fn test_too_small_window_height() {
        let width = 100;
        let height = 9;
        let area = Rect::new(0, 0, width, height);
        let ctx = create_ctx(width, height);

        let plan = calculate_layout(area, &ctx);

        assert!(plan.warning_message.is_some(), "Should warn if height < 10");
        assert_eq!(plan.warning_message.unwrap(), "Window too small");
    }

    #[test]
    fn test_short_window_layout() {
        // Condition: height < 30 (but not too small)
        let width = 100;
        let height = 25;
        let area = Rect::new(0, 0, width, height);
        let ctx = create_ctx(width, height);

        let plan = calculate_layout(area, &ctx);

        // Short layout specific checks
        assert!(plan.stats.is_some(), "Short layout should show stats");
        assert!(plan.chart.is_none(), "Short layout hides the large chart");

        // Ensure footer is at the bottom
        assert_eq!(plan.footer.height, 1);
        assert_eq!(plan.footer.y, height - 1);
    }

    #[test]
    fn test_narrow_vertical_layout() {
        // Condition: width < 100 (triggers "is_narrow")
        let width = 90;
        let height = 60;
        let area = Rect::new(0, 0, width, height);
        let ctx = create_ctx(width, height);

        let plan = calculate_layout(area, &ctx);

        // Vertical/Narrow layout expectations
        assert!(plan.chart.is_some(), "Narrow layout should show chart");
        // In narrow mode (< 90 width), block stream is generally hidden or rearranged
        // The code for width < 90 splits info_cols into just details and block_stream(as vertical stack?)
        // Let's check logic: if ctx.width < 90: left_v split details/block_stream
        assert!(
            plan.block_stream.is_some(),
            "Narrow layout (w<90) preserves block stream in vertical stack"
        );
        assert!(
            plan.peer_stream.is_none(),
            "Height < 70 in vertical mode hides peer_stream"
        );
    }

    #[test]
    fn test_tall_vertical_layout() {
        // Condition: height > width * 0.6 AND height >= 70
        let width = 100;
        let height = 80; // 80 > 60
        let area = Rect::new(0, 0, width, height);
        let ctx = create_ctx(width, height);

        let plan = calculate_layout(area, &ctx);

        assert!(
            plan.peer_stream.is_some(),
            "Tall vertical layout (>70h) should show peer stream"
        );
        assert!(plan.chart.is_some());
    }

    #[test]
    fn test_standard_wide_layout_no_block_stream() {
        // Condition: Not short, not narrow (<100), not vertical aspect.
        // Width 120, Height 40.
        // Aspect: 40 vs 120*0.6=72. 40 < 72.
        // Wait, logic is: is_vertical_aspect = height > width * 0.6
        // If H=40, W=120, is_vertical=False.
        // is_narrow=False (120 > 100).
        // is_short=False (40 > 30).
        // -> Hits the "Standard/Wide" else block.

        let width = 120;
        let height = 40;
        let area = Rect::new(0, 0, width, height);
        let ctx = create_ctx(width, height);

        let plan = calculate_layout(area, &ctx);

        assert!(
            plan.list.width >= MIN_SIDEBAR_WIDTH,
            "Sidebar should respect min width"
        );
        assert!(plan.peer_stream.is_some());

        // Width 120 is < 135, so block_stream should be hidden in standard view
        assert!(
            plan.block_stream.is_none(),
            "Standard width < 135 should hide block stream"
        );
    }

    #[test]
    fn test_ultra_wide_layout_with_block_stream() {
        // Condition: Width > 135
        let width = 150;
        let height = 60;
        let area = Rect::new(0, 0, width, height);
        let ctx = create_ctx(width, height);

        let plan = calculate_layout(area, &ctx);

        assert!(
            plan.block_stream.is_some(),
            "Wide width > 135 should show block stream"
        );

        // Ensure stats and block stream are splitting the bottom area
        if let Some(bs) = plan.block_stream {
            assert_eq!(
                bs.width, 17,
                "Block stream has fixed width of 17 in wide mode"
            );
        }
    }

    #[test]
    fn test_smart_table_layout_priorities() {
        // Test the smart column dropper logic
        let cols = vec![
            SmartCol {
                min_width: 10,
                priority: 0,
                constraint: Constraint::Length(10),
            }, // Must show
            SmartCol {
                min_width: 20,
                priority: 1,
                constraint: Constraint::Length(20),
            }, // High priority
            SmartCol {
                min_width: 50,
                priority: 2,
                constraint: Constraint::Length(50),
            }, // Low priority
        ];

        // 1. Very narrow: only priority 0 fits
        let (constraints, indices) = compute_smart_table_layout(&cols, 15, 0);
        assert_eq!(indices, vec![0], "Only priority 0 should fit in 15 width");
        assert_eq!(constraints.len(), 1);

        // 2. Medium: priority 0 + 1 fit (10 + 20 = 30 width needed)
        // With expansion_reserve logic: if width < 80, reserve is 15.
        // Available effective = 45 - 15 = 30.
        // Cost = 10 (p0) + 20 (p1) = 30. Fits exactly.
        let (_constraints, indices) = compute_smart_table_layout(&cols, 45, 0);
        assert!(indices.contains(&0));
        assert!(indices.contains(&1));
        assert!(!indices.contains(&2));

        // 3. Wide: all fit
        let (_constraints, indices) = compute_smart_table_layout(&cols, 200, 0);
        assert_eq!(indices.len(), 3, "All columns should fit in 200 width");
    }

    #[test]
    fn test_truncate_theme_label_preserves_fx_suffix_when_truncated() {
        let out = crate::tui::screens::normal::truncate_theme_label_preserving_fx(
            "Bioluminescent Reef",
            true,
            13,
        );
        assert_eq!(out, "Biolum...[FX]");
    }

    #[test]
    fn test_truncate_theme_label_shows_full_fx_label_when_space_allows() {
        let out = crate::tui::screens::normal::truncate_theme_label_preserving_fx(
            "Bioluminescent Reef",
            true,
            25,
        );
        assert_eq!(out, "Bioluminescent Reef [FX]");
    }

    #[test]
    fn test_footer_left_width_expands_with_terminal_width() {
        let small = crate::tui::screens::normal::compute_footer_left_width(90, false);
        let large = crate::tui::screens::normal::compute_footer_left_width(180, false);
        assert!(
            large > small,
            "left footer width should expand on wider terminals"
        );
    }

    #[test]
    fn test_footer_left_width_respects_bounds() {
        assert_eq!(
            crate::tui::screens::normal::compute_footer_left_width(90, false),
            51
        );
        assert_eq!(
            crate::tui::screens::normal::compute_footer_left_width(200, false),
            90
        );
        assert_eq!(
            crate::tui::screens::normal::compute_footer_left_width(200, true),
            110
        );
    }

    #[test]
    fn test_footer_side_widths_use_actual_status_width_on_right() {
        let (left, right) =
            crate::tui::screens::normal::compute_footer_side_widths(180, true, 110, 30);
        assert_eq!(left, 110);
        assert_eq!(right, 30);
    }

    #[test]
    fn test_footer_side_widths_preserve_command_space() {
        let footer_width = 90;
        let status_width = 30;
        let (left, right) = crate::tui::screens::normal::compute_footer_side_widths(
            footer_width,
            false,
            90,
            status_width,
        );
        let commands_width = footer_width.saturating_sub(left + right);

        assert_eq!(commands_width, 18);
    }

    #[test]
    fn test_footer_status_width_reserves_visual_gutter() {
        let raw = "Port 6681 | IPv4/IPv6 | OPEN".len() as u16;
        let computed = crate::tui::screens::normal::compute_footer_status_width(6681, "OPEN");

        assert!(computed > raw);
        assert_eq!(computed, raw + 2);
    }
}
