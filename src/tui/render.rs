// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use ratatui::{prelude::*, widgets::*};

use crate::tui::screen_context::ScreenContext;
use crate::tui::screens::{
    browser, config, delete_confirm, help, journal, normal, peers, power, rss, torrents, welcome,
};

use crate::app::{AppMode, AppState};
use crate::dht_service::{DhtStatus, DhtWaveTelemetry};
use crate::theme::{color_to_rgb, ThemeContext};

use crate::tui::layout::normal::{calculate_layout, LayoutContext, DEFAULT_SIDEBAR_PERCENT};
use crate::tui::particles::{
    apply_theme_particles_background_to_frame, apply_theme_particles_foreground_to_frame,
};

use crate::config::Settings;

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

fn apply_theme_effects_to_frame(f: &mut Frame, ctx: &ThemeContext) {
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

fn apply_visualization_focus_dimming_to_frame(f: &mut Frame, selected: Rect) {
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

pub fn draw(
    f: &mut Frame,
    app_state: &AppState,
    dht_status: &DhtStatus,
    dht_wave_telemetry: &DhtWaveTelemetry,
    settings: &Settings,
) {
    let area = f.area();

    let ctx = ThemeContext::new(app_state.theme, app_state.ui.effects_phase_time);
    let screen = ScreenContext::new(app_state, dht_status, dht_wave_telemetry, settings, &ctx);

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
        AppMode::TorrentManagement => {
            apply_theme_particles_background_to_frame(f, &ctx);
            torrents::draw(f, &screen);
            apply_theme_effects_to_frame(f, &ctx);
            apply_theme_particles_foreground_to_frame(f, &ctx);
            return;
        }
        AppMode::PeerManagement => {
            apply_theme_particles_background_to_frame(f, &ctx);
            peers::draw(f, &screen);
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
                config::ConfigDrawState {
                    settings: &app_state.ui.config.settings_edit,
                    selected_index: app_state.ui.config.selected_index,
                    items: &app_state.ui.config.items,
                    active_pane: app_state.ui.config.active_pane,
                    editing: &app_state.ui.config.editing,
                    reset_confirmation: &app_state.ui.config.reset_confirmation,
                },
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
    let layout_ctx = LayoutContext::new(
        area,
        app_state,
        settings.ui_layout_mode,
        DEFAULT_SIDEBAR_PERCENT,
    );
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

    if app_state.system_error.is_none() && !app_state.should_quit {
        if let Some((_panel, panel_area)) =
            normal::selected_visualization_focus_panel(app_state, &plan)
        {
            apply_visualization_focus_dimming_to_frame(f, panel_area);
            normal::draw_visualization_focus_overlay(f, panel_area, plan.footer, &ctx);
        }
    }
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
mod visual_effect_tests {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::layout::common::{compute_smart_table_layout, SmartCol};
    use crate::tui::layout::normal::{DEFAULT_SIDEBAR_PERCENT, MIN_SIDEBAR_WIDTH};
    use ratatui::layout::Rect;

    /// Helper to create a LayoutContext manually since we don't want to mock AppState.
    /// Accessing the struct fields directly allows us to bypass `LayoutContext::new`.
    fn create_ctx(width: u16, height: u16) -> LayoutContext {
        create_ctx_with_mode(width, height, crate::config::UiLayoutMode::Auto)
    }

    fn create_ctx_with_mode(
        width: u16,
        height: u16,
        layout_mode: crate::config::UiLayoutMode,
    ) -> LayoutContext {
        LayoutContext {
            width,
            height,
            layout_mode,
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
    fn test_layout_mode_forces_vertical_on_wide_area() {
        let width = 120;
        let height = 40;
        let area = Rect::new(0, 0, width, height);
        let ctx = create_ctx_with_mode(width, height, crate::config::UiLayoutMode::Vertical);

        let plan = calculate_layout(area, &ctx);

        assert!(
            plan.chart.is_some(),
            "forced vertical layout should show chart"
        );
        assert!(
            plan.block_stream.is_some(),
            "forced vertical layout should keep the vertical information band"
        );
    }

    #[test]
    fn test_layout_mode_forces_horizontal_on_tall_area() {
        let width = 100;
        let height = 80;
        let area = Rect::new(0, 0, width, height);
        let ctx = create_ctx_with_mode(width, height, crate::config::UiLayoutMode::Horizontal);

        let plan = calculate_layout(area, &ctx);

        assert!(
            plan.peer_stream.is_some(),
            "forced horizontal layout should use the wide header stream"
        );
        assert!(
            plan.block_stream.is_none(),
            "width <= 135 in horizontal layout hides block stream"
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

    #[test]
    fn test_footer_fps_label_shows_target_until_measurement_exists() {
        let app_state = crate::app::AppState {
            data_rate: crate::app::DataRate::Rate60s,
            ..Default::default()
        };

        assert_eq!(
            crate::tui::screens::normal::footer_fps_label(&app_state),
            "60 fps"
        );
    }

    #[test]
    fn test_footer_fps_label_shows_measured_and_target_when_below_target() {
        let mut app_state = crate::app::AppState {
            data_rate: crate::app::DataRate::Rate60s,
            ..Default::default()
        };
        app_state.ui.measured_fps = Some(44.2);

        assert_eq!(
            crate::tui::screens::normal::footer_fps_label(&app_state),
            "44/60 fps"
        );
    }

    #[test]
    fn test_footer_fps_label_hides_measured_when_at_target() {
        let mut app_state = crate::app::AppState {
            data_rate: crate::app::DataRate::Rate60s,
            ..Default::default()
        };
        app_state.ui.measured_fps = Some(60.0);

        assert_eq!(
            crate::tui::screens::normal::footer_fps_label(&app_state),
            "60 fps"
        );
    }

    #[test]
    fn test_footer_fps_label_hides_measured_when_above_target() {
        let mut app_state = crate::app::AppState {
            data_rate: crate::app::DataRate::Rate60s,
            ..Default::default()
        };
        app_state.ui.measured_fps = Some(61.8);

        assert_eq!(
            crate::tui::screens::normal::footer_fps_label(&app_state),
            "60 fps"
        );
    }

    #[test]
    fn test_footer_fps_label_hides_measured_when_rounded_to_target() {
        let mut app_state = crate::app::AppState {
            data_rate: crate::app::DataRate::Rate60s,
            ..Default::default()
        };
        app_state.ui.measured_fps = Some(59.8);

        assert_eq!(
            crate::tui::screens::normal::footer_fps_label(&app_state),
            "60 fps"
        );
    }

    #[test]
    fn test_footer_fps_label_preserves_fractional_targets_when_below_target() {
        let mut app_state = crate::app::AppState {
            data_rate: crate::app::DataRate::RateQuarter,
            ..Default::default()
        };
        app_state.ui.measured_fps = Some(0.24);

        assert_eq!(
            crate::tui::screens::normal::footer_fps_label(&app_state),
            "0.24/0.25 fps"
        );
    }

    #[test]
    fn test_footer_fps_label_hides_fractional_measured_when_at_target() {
        let mut app_state = crate::app::AppState {
            data_rate: crate::app::DataRate::RateQuarter,
            ..Default::default()
        };
        app_state.ui.measured_fps = Some(0.25);

        assert_eq!(
            crate::tui::screens::normal::footer_fps_label(&app_state),
            "0.25 fps"
        );
    }
}
