// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::AppState;
use crate::config::UiLayoutMode;
use ratatui::prelude::{Constraint, Layout, Rect};

pub const MIN_SIDEBAR_WIDTH: u16 = 25;
pub const MIN_DETAILS_HEIGHT: u16 = 10;
pub const DEFAULT_SIDEBAR_PERCENT: u16 = 45;

#[derive(Default, Debug)]
pub struct LayoutPlan {
    pub list: Rect,
    pub footer: Rect,
    pub details: Rect,
    pub peers: Rect,
    pub chart: Option<Rect>,
    pub stats: Option<Rect>,
    pub peer_stream: Option<Rect>,
    pub block_stream: Option<Rect>,
    pub warning_message: Option<String>,
}

pub struct LayoutContext {
    pub width: u16,
    pub height: u16,
    pub layout_mode: UiLayoutMode,
    pub settings_sidebar_percent: u16,
}

impl LayoutContext {
    pub fn new(
        area: Rect,
        _app_state: &AppState,
        layout_mode: UiLayoutMode,
        sidebar_pct: u16,
    ) -> Self {
        Self {
            width: area.width,
            height: area.height,
            layout_mode,
            settings_sidebar_percent: sidebar_pct,
        }
    }
}

pub(crate) fn uses_vertical_layout(area: Rect, layout_mode: UiLayoutMode) -> bool {
    match layout_mode {
        UiLayoutMode::Auto => {
            let is_narrow = area.width < 100;
            let is_vertical_aspect = area.height as f32 > (area.width as f32 * 0.6);
            is_narrow || is_vertical_aspect
        }
        UiLayoutMode::Vertical | UiLayoutMode::Square => true,
        UiLayoutMode::Horizontal => false,
    }
}

pub fn calculate_layout(area: Rect, ctx: &LayoutContext) -> LayoutPlan {
    let mut plan = LayoutPlan::default();

    if ctx.width < 40 || ctx.height < 10 {
        let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
        plan.list = chunks[0];
        plan.footer = chunks[1];
        plan.warning_message = Some("Window too small".to_string());
        return plan;
    }

    let is_short = ctx.height < 30;
    let use_vertical_layout = uses_vertical_layout(area, ctx.layout_mode);

    if is_short {
        let main = Layout::vertical([
            Constraint::Min(5),
            Constraint::Length(12),
            Constraint::Length(1),
        ])
        .split(area);

        plan.list = main[0];
        let bottom_cols =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(main[1]);
        plan.stats = Some(bottom_cols[0]);

        let detail_chunks =
            Layout::vertical([Constraint::Length(9), Constraint::Length(0)]).split(bottom_cols[1]);
        plan.details = detail_chunks[0];
        plan.peers = detail_chunks[1];

        plan.footer = main[2];
    } else if use_vertical_layout {
        let (chart_height, info_height) = if ctx.height < 50 {
            (10, MIN_DETAILS_HEIGHT)
        } else {
            (14, 20)
        };

        let v_chunks = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(chart_height),
            Constraint::Length(info_height),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .split(area);

        if ctx.height < 70 {
            plan.list = v_chunks[0];
            plan.peer_stream = None;
        } else {
            let top_split =
                Layout::vertical([Constraint::Min(0), Constraint::Length(9)]).split(v_chunks[0]);

            plan.list = top_split[0];
            plan.peer_stream = Some(top_split[1]);
        }

        plan.chart = Some(v_chunks[1]);

        if ctx.width < 90 {
            let info_cols =
                Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1)]).split(v_chunks[2]);

            let left_v =
                Layout::vertical([Constraint::Length(MIN_DETAILS_HEIGHT), Constraint::Min(0)])
                    .split(info_cols[0]);

            plan.details = left_v[0];
            plan.block_stream = Some(left_v[1]);
            plan.stats = Some(info_cols[1]);
        } else {
            let info_cols = Layout::horizontal([
                Constraint::Fill(1),
                Constraint::Length(17),
                Constraint::Fill(1),
            ])
            .split(v_chunks[2]);

            plan.details = info_cols[0];
            plan.block_stream = Some(info_cols[1]);
            plan.stats = Some(info_cols[2]);
        }

        plan.peers = v_chunks[3];
        plan.footer = v_chunks[4];
    } else {
        let main = Layout::vertical([
            Constraint::Min(10),
            Constraint::Length(27),
            Constraint::Length(1),
        ])
        .split(area);

        let top_area = main[0];
        let bottom_area = main[1];
        plan.footer = main[2];

        let target_sidebar =
            (ctx.width as f32 * (ctx.settings_sidebar_percent as f32 / 100.0)) as u16;
        let sidebar_width = target_sidebar.max(MIN_SIDEBAR_WIDTH);

        let top_h = Layout::horizontal([Constraint::Length(sidebar_width), Constraint::Min(0)])
            .split(top_area);

        plan.list = top_h[0];
        let right_v = Layout::vertical([Constraint::Length(9), Constraint::Min(0)]).split(top_h[1]);

        let header_h =
            Layout::horizontal([Constraint::Length(35), Constraint::Min(0)]).split(right_v[0]);

        plan.details = header_h[0];
        plan.peer_stream = Some(header_h[1]);
        plan.peers = right_v[1];

        let show_block_stream = ctx.width > 135;
        let right_pane_width = if show_block_stream { 54 } else { 40 };

        let bottom_h =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(right_pane_width)])
                .split(bottom_area);

        plan.chart = Some(bottom_h[0]);
        let stats_area = bottom_h[1];

        if show_block_stream {
            let stats_h =
                Layout::horizontal([Constraint::Length(17), Constraint::Min(0)]).split(stats_area);

            plan.block_stream = Some(stats_h[0]);
            plan.stats = Some(stats_h[1]);
        } else {
            plan.stats = Some(stats_area);
            plan.block_stream = None;
        }
    }

    plan
}
