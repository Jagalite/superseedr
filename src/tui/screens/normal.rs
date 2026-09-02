// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::align_unpinned_sort_with_visible_activity;
use crate::app::file_activity_wave_steps_per_second;
use crate::app::sort_and_filter_torrent_list_state;
use crate::app::swarm_availability_counts;
use crate::app::torrent_completion_percent;
use crate::app::torrent_is_effectively_incomplete;
use crate::app::torrent_manager_protocol::TorrentFileProbeStatus;
use crate::app::ChartPanelView;
use crate::app::DhtVisualization;
use crate::app::DiskHealthVisualization;
use crate::app::GraphDisplayMode;
use crate::app::PeerInfo;
use crate::app::PeerStreamVisualization;
use crate::app::SwarmAvailabilityFlashState;
use crate::app::{
    AppMode, AppState, SelectedHeader, TorrentControlState, TorrentDisplayState,
    VisualizationFocusPanel,
};
use crate::config::{PeerSortColumn, Settings, SortDirection, TorrentSortColumn, UiLayoutMode};
use crate::dht_model::{DhtStatus, DhtWaveTelemetry};
use crate::networking::{NetworkActivationStatus, NetworkRuntimePhase};
use crate::persistence::activity_history::{ActivityHistoryPoint, ActivityHistorySeries};
use crate::persistence::network_history::NetworkHistoryPoint;
use crate::theme::ThemeContext;
use crate::tui::action_style::{footer_key_style, ActionTone};
use crate::tui::component_visualizations::{
    disk_health_status_color, draw_dht_visualization, draw_disk_health_visualization,
    normalize_dht_peer_yield, normalize_dht_query_signal, DiskHealthSignals,
};
pub use crate::tui::effects::UiEffect;
use crate::tui::formatters::{
    anonymize_preserving_shape, auto_download_limit_applied, calculate_nice_upper_bound,
    format_bytes, format_countdown, format_duration, format_iops, format_latency, format_limit_bps,
    format_memory, format_speed, format_time, generate_x_axis_labels, ip_to_color, parse_peer_id,
    sanitize_text, speed_to_style, truncate_with_ellipsis,
};
use crate::tui::layout::common::compute_visible_peer_columns;
use crate::tui::layout::common::compute_visible_torrent_columns;
use crate::tui::layout::common::get_peer_columns;
use crate::tui::layout::common::get_torrent_columns;
use crate::tui::layout::common::ColumnId;
use crate::tui::layout::common::PeerColumnId;
use crate::tui::layout::normal::LayoutContext;
use crate::tui::layout::normal::LayoutPlan;
use crate::tui::layout::normal::DEFAULT_SIDEBAR_PERCENT;
use crate::tui::layout::normal::{
    calculate_layout, uses_vertical_layout, PEER_STREAM_MIN_HEIGHT, PEER_STREAM_MIN_WIDTH,
};
use crate::tui::peer_stream::{
    draw_peer_stream_visualization, peer_stream_event_counts,
    should_use_compact_peer_stream_legend, PeerStreamEventCounts,
};
use crate::tui::screen_context::ScreenContext;
use crate::tui::tree::{TreeFilter, TreeMathHelper, TreeViewState};
use chrono::{DateTime, Utc};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;
use web_time::{Instant, SystemTime, UNIX_EPOCH};

use crate::terminal_event::{
    Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::layout::Layout;
#[cfg(not(all(feature = "dht", feature = "pex")))]
use ratatui::prelude::Stylize;
use ratatui::prelude::{
    symbols, Alignment, Color, Constraint, Direction, Frame, Line, Modifier, Rect, Span, Style,
};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Gauge, LineGauge, List, ListItem, Padding, Paragraph, Row, Table,
    TableState, Wrap,
};

static APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const SECONDS_HISTORY_MAX: usize = 3600;
const MINUTES_HISTORY_MAX: usize = 48 * 60;
const TUNING_LABEL_WIDTH: usize = 14;
const FOOTER_STATUS_GUTTER: u16 = 2;
const ASCII_TREE_DIR_ICON: &str = "> ";
const ASCII_TREE_FILE_ICON: &str = "  ";
const FILE_ACTIVITY_HIGHLIGHT_WINDOW: Duration = Duration::from_millis(1800);
const FILE_ACTIVITY_MAX_BAND_WIDTH: usize = 9;
const MIN_SWARM_AVAILABILITY_HEIGHT: u16 = 1;
const FILES_SWARM_SPACER_HEIGHT: u16 = 1;
const SATURATED_ACTIVE_PEER_FILE_ROWS: u16 = 5;
const MIN_SATURATED_ACTIVE_PEER_TABLE_HEIGHT: u16 = 7;
const MAX_INACTIVE_ONLY_PEERS_IN_TABLE: usize = 10;
const NO_VISIBLE_VISUALIZATION_FOCUS_PANEL_ERROR: &str =
    "No visible panel has multiple visualization renderers yet.";
const DISK_HEALTH_ORB_SIZE_SCALE: f64 = 1.35;
const DISK_HEALTH_ORB_CELL_Y_ASPECT: f64 = 2.0;
const DISK_HEALTH_ORB_BRAILLE_BITS: [[u8; 2]; 4] =
    [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];

fn build_time_aligned_window(
    points: &[NetworkHistoryPoint],
    step_secs: u64,
    window_points: usize,
    now_unix: u64,
) -> (Vec<u64>, Vec<u64>, Vec<u64>) {
    if window_points == 0 || step_secs == 0 {
        return (Vec::new(), Vec::new(), Vec::new());
    }

    let mut dl = vec![0_u64; window_points];
    let mut ul = vec![0_u64; window_points];
    let mut backoff = vec![0_u64; window_points];
    let end_ts = now_unix.saturating_sub(now_unix % step_secs);
    let start_ts = end_ts.saturating_sub((window_points.saturating_sub(1) as u64) * step_secs);

    for point in points {
        if point.ts_unix < start_ts || point.ts_unix > end_ts {
            continue;
        }
        let idx = ((point.ts_unix - start_ts) / step_secs) as usize;
        if idx < window_points {
            dl[idx] = point.download_bps;
            ul[idx] = point.upload_bps;
            backoff[idx] = backoff[idx].max(point.backoff_ms_max);
        }
    }

    (dl, ul, backoff)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HistoryTier {
    Second1s,
    Minute1m,
    Minute15m,
    Hour1h,
}

fn graph_window_spec(mode: GraphDisplayMode) -> (usize, u64, HistoryTier) {
    match mode {
        GraphDisplayMode::OneMinute
        | GraphDisplayMode::FiveMinutes
        | GraphDisplayMode::TenMinutes
        | GraphDisplayMode::ThirtyMinutes
        | GraphDisplayMode::OneHour => (
            mode.as_seconds().clamp(1, SECONDS_HISTORY_MAX),
            1_u64,
            HistoryTier::Second1s,
        ),
        GraphDisplayMode::ThreeHours
        | GraphDisplayMode::TwelveHours
        | GraphDisplayMode::TwentyFourHours => (
            (mode.as_seconds() / 60).clamp(1, MINUTES_HISTORY_MAX),
            60_u64,
            HistoryTier::Minute1m,
        ),
        GraphDisplayMode::SevenDays => (7 * 24 * 4, 15 * 60_u64, HistoryTier::Minute15m),
        GraphDisplayMode::ThirtyDays => (30 * 24 * 4, 15 * 60_u64, HistoryTier::Minute15m),
        GraphDisplayMode::OneYear => (365 * 24, 60 * 60_u64, HistoryTier::Hour1h),
    }
}

fn build_time_aligned_pair_window(
    points: &[ActivityHistoryPoint],
    step_secs: u64,
    window_points: usize,
    now_unix: u64,
) -> (Vec<u64>, Vec<u64>) {
    if window_points == 0 || step_secs == 0 {
        return (Vec::new(), Vec::new());
    }

    let mut primary = vec![0_u64; window_points];
    let mut secondary = vec![0_u64; window_points];
    let end_ts = now_unix.saturating_sub(now_unix % step_secs);
    let start_ts = end_ts.saturating_sub((window_points.saturating_sub(1) as u64) * step_secs);

    for point in points {
        if point.ts_unix < start_ts || point.ts_unix > end_ts {
            continue;
        }
        let idx = ((point.ts_unix - start_ts) / step_secs) as usize;
        if idx < window_points {
            primary[idx] = point.primary;
            secondary[idx] = point.secondary;
        }
    }

    (primary, secondary)
}

fn activity_points_for_tier(
    series: &ActivityHistorySeries,
    tier: HistoryTier,
) -> &[ActivityHistoryPoint] {
    match tier {
        HistoryTier::Second1s => &series.tiers.second_1s,
        HistoryTier::Minute1m => &series.tiers.minute_1m,
        HistoryTier::Minute15m => &series.tiers.minute_15m,
        HistoryTier::Hour1h => &series.tiers.hour_1h,
    }
}

fn network_points_for_tier(app_state: &AppState, tier: HistoryTier) -> &[NetworkHistoryPoint] {
    match tier {
        HistoryTier::Second1s => &app_state.network_history_state.tiers.second_1s,
        HistoryTier::Minute1m => &app_state.network_history_state.tiers.minute_1m,
        HistoryTier::Minute15m => &app_state.network_history_state.tiers.minute_15m,
        HistoryTier::Hour1h => &app_state.network_history_state.tiers.hour_1h,
    }
}

fn disk_series_draw_read_last(read: &[u64], write: &[u64]) -> bool {
    let read_key = (
        read.iter().rposition(|&value| value > 0),
        read.iter().copied().max().unwrap_or(0),
    );
    let write_key = (
        write.iter().rposition(|&value| value > 0),
        write.iter().copied().max().unwrap_or(0),
    );
    read_key > write_key
}

fn torrent_activity_label(app_state: &AppState, info_hash: &[u8]) -> String {
    let key = hex::encode(info_hash);
    if app_state.anonymize_torrent_names {
        format!("torrent-{}", &key[..key.len().min(6)])
    } else {
        app_state
            .torrents
            .get(info_hash)
            .map(|torrent| torrent.latest_state.torrent_name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| format!("torrent-{}", &key[..key.len().min(6)]))
    }
}

fn torrent_period_traffic(
    app_state: &AppState,
    info_hash: &[u8],
    tier: HistoryTier,
    step_secs: u64,
    points_to_show: usize,
    now_unix: u64,
) -> u64 {
    let key = hex::encode(info_hash);
    let points = app_state
        .activity_history_state
        .torrents
        .get(&key)
        .map(|series| activity_points_for_tier(series, tier))
        .unwrap_or(&[]);
    let (dl_hist, ul_hist) =
        build_time_aligned_pair_window(points, step_secs, points_to_show, now_unix);
    dl_hist
        .iter()
        .zip(ul_hist.iter())
        .map(|(dl, ul)| dl.saturating_add(*ul))
        .sum()
}

fn torrent_current_traffic(
    app_state: &AppState,
    info_hash: &[u8],
    tier: HistoryTier,
    step_secs: u64,
    points_to_show: usize,
    now_unix: u64,
    alpha: f64,
) -> u64 {
    let key = hex::encode(info_hash);
    let points = app_state
        .activity_history_state
        .torrents
        .get(&key)
        .map(|series| activity_points_for_tier(series, tier))
        .unwrap_or(&[]);
    let (dl_hist, ul_hist) =
        build_time_aligned_pair_window(points, step_secs, points_to_show, now_unix);
    let net_hist: Vec<u64> = dl_hist
        .iter()
        .zip(ul_hist.iter())
        .map(|(dl, ul)| dl.saturating_add(*ul))
        .collect();
    smoothed_last_value(&net_hist, alpha)
}

fn smoothed_last_value(data: &[u64], alpha: f64) -> u64 {
    if data.is_empty() {
        return 0;
    }

    let mut last_ema = data[0] as f64;
    for &value in data.iter().skip(1) {
        last_ema = (value as f64 * alpha) + (last_ema * (1.0 - alpha));
    }

    last_ema as u64
}

fn chart_hidden_legend_constraints(view: ChartPanelView) -> (Constraint, Constraint) {
    if matches!(
        view,
        ChartPanelView::TorrentOverlay | ChartPanelView::MultiTorrentOverlay
    ) {
        (Constraint::Percentage(100), Constraint::Percentage(100))
    } else {
        (Constraint::Ratio(1, 4), Constraint::Ratio(1, 4))
    }
}

fn chart_legend_position(view: ChartPanelView) -> Option<ratatui::widgets::LegendPosition> {
    if matches!(
        view,
        ChartPanelView::TorrentOverlay | ChartPanelView::MultiTorrentOverlay
    ) {
        Some(ratatui::widgets::LegendPosition::TopLeft)
    } else {
        Some(ratatui::widgets::LegendPosition::TopRight)
    }
}

fn selector_content_width(labels: &[&str]) -> usize {
    labels.iter().map(|label| label.len()).sum::<usize>() + labels.len().saturating_sub(1)
}

fn selector_window<'a>(labels: &'a [&'a str], active_idx: usize, compact: bool) -> Vec<&'a str> {
    if !compact || labels.len() <= 3 {
        return labels.to_vec();
    }

    if active_idx == 0 {
        return labels[..3].to_vec();
    }

    if active_idx >= labels.len().saturating_sub(1) {
        return labels[labels.len() - 3..].to_vec();
    }

    vec![
        labels[active_idx - 1],
        labels[active_idx],
        labels[active_idx + 1],
    ]
}

fn selector_active_position(labels_len: usize, active_idx: usize, compact: bool) -> usize {
    if !compact || labels_len <= 3 {
        return active_idx;
    }

    if active_idx == 0 {
        return 0;
    }

    if active_idx >= labels_len.saturating_sub(1) {
        return 2;
    }

    1
}

fn build_selector_spans(
    ctx: &ThemeContext,
    labels: &[&str],
    active_idx: usize,
    compact: bool,
) -> Vec<Span<'static>> {
    let visible = selector_window(labels, active_idx, compact);
    let active_pos = selector_active_position(labels.len(), active_idx, compact);

    let mut spans = Vec::with_capacity(visible.len().saturating_mul(2));
    for (i, label) in visible.iter().enumerate() {
        let style = if i == active_pos {
            ctx.apply(
                Style::default()
                    .fg(ctx.state_warning())
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            ctx.apply(Style::default().fg(ctx.theme.semantic.surface0))
        };
        spans.push(Span::styled((*label).to_string(), style));
        if i < visible.len().saturating_sub(1) {
            spans.push(Span::styled(
                " ",
                ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
            ));
        }
    }
    spans
}

fn speed_chart_upper_bound(max_displayed_speed: u64) -> u64 {
    if max_displayed_speed == 0 {
        return 10_000;
    }

    let padded = max_displayed_speed.saturating_mul(105).div_ceil(100);
    let half_step = calculate_nice_upper_bound((padded / 2).max(1));
    half_step.saturating_mul(2)
}

#[derive(Clone, Debug, PartialEq)]
pub enum UiAction {
    ClearSystemError,
    ToggleVisualizationFocus,
    ExitVisualizationFocus,
    VisualizationFocusNext,
    VisualizationFocusPrev,
    VisualizationRendererNext,
    VisualizationRendererPrev,
    ResetVisualizationRenderer,
    StartSearch,
    Navigate(KeyCode),
    ToggleAnonymizeNames,
    EnterPowerSaving,
    RequestQuit,
    ChartViewNext,
    ChartViewPrev,
    GraphNext,
    GraphPrev,
    OpenAddTorrentBrowser,
    OpenSelectedTorrentFiles,
    OpenDeleteConfirm { with_files: bool },
    OpenConfig,
    OpenRss,
    OpenJournal,
    OpenPeerManagement,
    OpenTorrentManagement,
    DataRateSlower,
    DataRateFaster,
    ThemePrev,
    ThemeNext,
    TogglePauseSelected,
    SortBySelectedColumn,
    ClearManualSorting,
    OpenHelp,
    PasteText(String),
}

#[derive(Default)]
pub struct ReduceResult {
    pub redraw: bool,
    pub effects: Vec<UiEffect>,
}

pub fn reduce_ui_action(app_state: &mut AppState, action: UiAction) -> ReduceResult {
    reduce_ui_action_with_layout_mode(app_state, action, UiLayoutMode::Auto)
}

fn calculate_header_interaction_layout(
    app_state: &AppState,
    layout_mode: UiLayoutMode,
) -> LayoutPlan {
    let layout_ctx = LayoutContext::new(
        app_state.screen_area,
        app_state,
        layout_mode,
        DEFAULT_SIDEBAR_PERCENT,
    );
    calculate_layout(app_state.screen_area, &layout_ctx)
}

fn visible_visualization_focus_panels(
    app_state: &AppState,
    layout_mode: UiLayoutMode,
) -> Vec<(VisualizationFocusPanel, Rect)> {
    let plan = calculate_header_interaction_layout(app_state, layout_mode);
    visualization_focus_panels(app_state, &plan)
}

fn deactivate_visualization_focus_if_hidden(
    app_state: &mut AppState,
    layout_mode: UiLayoutMode,
) -> bool {
    if !app_state.ui.visualization_focus.active {
        return false;
    }

    let selected = app_state.ui.visualization_focus.selected;
    if visible_visualization_focus_panels(app_state, layout_mode)
        .iter()
        .any(|(panel, _)| *panel == selected)
    {
        return false;
    }

    app_state.ui.visualization_focus.active = false;
    true
}

fn cycle_visualization_focus(app_state: &mut AppState, layout_mode: UiLayoutMode, forward: bool) {
    if !app_state.ui.visualization_focus.active {
        return;
    }

    let panels = visible_visualization_focus_panels(app_state, layout_mode);
    if panels.is_empty() {
        app_state.ui.visualization_focus.active = false;
        return;
    }

    let current = panels
        .iter()
        .position(|(panel, _)| *panel == app_state.ui.visualization_focus.selected)
        .unwrap_or(0);
    let next = if forward {
        (current + 1) % panels.len()
    } else if current == 0 {
        panels.len() - 1
    } else {
        current - 1
    };
    app_state.ui.visualization_focus.selected = panels[next].0;
}

fn cycle_selected_visualization_renderer(app_state: &mut AppState, forward: bool) {
    if !app_state.ui.visualization_focus.active {
        return;
    }

    match app_state.ui.visualization_focus.selected {
        VisualizationFocusPanel::PeerStream => {
            let current = app_state.ui.visualization_focus.peer_stream;
            app_state.ui.visualization_focus.peer_stream = if forward {
                current.next()
            } else {
                current.previous()
            };
        }
        VisualizationFocusPanel::DiskHealth => {
            let current = app_state.ui.visualization_focus.disk_health;
            app_state.ui.visualization_focus.disk_health = if forward {
                current.next()
            } else {
                current.previous()
            };
        }
        VisualizationFocusPanel::DhtWave => {
            let current = app_state.ui.visualization_focus.dht;
            app_state.ui.visualization_focus.dht = if forward {
                current.next()
            } else {
                current.previous()
            };
        }
        _ => {}
    }
}

fn reset_selected_visualization_renderer(app_state: &mut AppState) {
    if !app_state.ui.visualization_focus.active {
        return;
    }
    match app_state.ui.visualization_focus.selected {
        VisualizationFocusPanel::PeerStream => {
            app_state.ui.visualization_focus.peer_stream = PeerStreamVisualization::default();
        }
        VisualizationFocusPanel::DiskHealth => {
            app_state.ui.visualization_focus.disk_health = DiskHealthVisualization::default();
        }
        VisualizationFocusPanel::DhtWave => {
            app_state.ui.visualization_focus.dht = DhtVisualization::default();
        }
        _ => {}
    }
}

pub fn reduce_ui_action_with_layout_mode(
    app_state: &mut AppState,
    action: UiAction,
    layout_mode: UiLayoutMode,
) -> ReduceResult {
    match action {
        UiAction::ClearSystemError => {
            app_state.system_error = None;
            ReduceResult {
                redraw: true,
                effects: Vec::new(),
            }
        }
        UiAction::ToggleVisualizationFocus => {
            if app_state.ui.visualization_focus.active {
                app_state.ui.visualization_focus.active = false;
            } else if app_state.system_error.is_none()
                || app_state.system_error.as_deref()
                    == Some(NO_VISIBLE_VISUALIZATION_FOCUS_PANEL_ERROR)
            {
                let panels = visible_visualization_focus_panels(app_state, layout_mode);
                if let Some((panel, _)) = panels
                    .iter()
                    .find(|(panel, _)| *panel == app_state.ui.visualization_focus.selected)
                    .or_else(|| panels.first())
                {
                    app_state.ui.visualization_focus.active = true;
                    app_state.ui.visualization_focus.selected = *panel;
                    if app_state.system_error.as_deref()
                        == Some(NO_VISIBLE_VISUALIZATION_FOCUS_PANEL_ERROR)
                    {
                        app_state.system_error = None;
                    }
                } else {
                    app_state.system_error =
                        Some(NO_VISIBLE_VISUALIZATION_FOCUS_PANEL_ERROR.to_string());
                }
            }
            ReduceResult {
                redraw: true,
                effects: Vec::new(),
            }
        }
        UiAction::ExitVisualizationFocus => {
            app_state.ui.visualization_focus.active = false;
            ReduceResult {
                redraw: true,
                effects: Vec::new(),
            }
        }
        UiAction::VisualizationFocusNext => {
            cycle_visualization_focus(app_state, layout_mode, true);
            ReduceResult {
                redraw: true,
                effects: Vec::new(),
            }
        }
        UiAction::VisualizationFocusPrev => {
            cycle_visualization_focus(app_state, layout_mode, false);
            ReduceResult {
                redraw: true,
                effects: Vec::new(),
            }
        }
        UiAction::VisualizationRendererNext => {
            cycle_selected_visualization_renderer(app_state, true);
            ReduceResult {
                redraw: true,
                effects: vec![UiEffect::PersistVisualizationSelections],
            }
        }
        UiAction::VisualizationRendererPrev => {
            cycle_selected_visualization_renderer(app_state, false);
            ReduceResult {
                redraw: true,
                effects: vec![UiEffect::PersistVisualizationSelections],
            }
        }
        UiAction::ResetVisualizationRenderer => {
            reset_selected_visualization_renderer(app_state);
            ReduceResult {
                redraw: true,
                effects: vec![UiEffect::PersistVisualizationSelections],
            }
        }
        UiAction::StartSearch => {
            app_state.ui.is_searching = true;
            app_state.ui.selected_torrent_index = 0;
            ReduceResult {
                redraw: true,
                effects: Vec::new(),
            }
        }
        UiAction::Navigate(key_code) => {
            handle_navigation_with_layout_mode(app_state, key_code, layout_mode);
            ReduceResult {
                redraw: true,
                effects: Vec::new(),
            }
        }
        UiAction::ToggleAnonymizeNames => {
            app_state.anonymize_torrent_names = !app_state.anonymize_torrent_names;
            ReduceResult {
                redraw: true,
                effects: Vec::new(),
            }
        }
        UiAction::EnterPowerSaving => ReduceResult {
            redraw: true,
            effects: vec![UiEffect::ToPowerSaving],
        },
        UiAction::RequestQuit => {
            app_state.should_quit = true;
            ReduceResult {
                redraw: true,
                effects: Vec::new(),
            }
        }
        UiAction::ChartViewNext => {
            app_state.chart_panel_view = app_state.chart_panel_view.next();
            ReduceResult {
                redraw: true,
                effects: Vec::new(),
            }
        }
        UiAction::ChartViewPrev => {
            app_state.chart_panel_view = app_state.chart_panel_view.prev();
            ReduceResult {
                redraw: true,
                effects: Vec::new(),
            }
        }
        UiAction::GraphNext => {
            app_state.graph_mode = app_state.graph_mode.next();
            ReduceResult {
                redraw: true,
                effects: Vec::new(),
            }
        }
        UiAction::GraphPrev => {
            app_state.graph_mode = app_state.graph_mode.prev();
            ReduceResult {
                redraw: true,
                effects: Vec::new(),
            }
        }
        UiAction::OpenAddTorrentBrowser => ReduceResult {
            redraw: true,
            effects: vec![UiEffect::OpenAddTorrentFileBrowser],
        },
        UiAction::OpenSelectedTorrentFiles => {
            let selected_hash = app_state
                .torrent_list_order
                .get(app_state.ui.selected_torrent_index)
                .cloned();
            if let Some(info_hash) = selected_hash {
                ReduceResult {
                    redraw: true,
                    effects: vec![UiEffect::OpenExistingTorrentFileBrowser(info_hash)],
                }
            } else {
                ReduceResult {
                    redraw: true,
                    effects: Vec::new(),
                }
            }
        }
        UiAction::OpenDeleteConfirm { with_files } => {
            if let Some(info_hash) = app_state
                .torrent_list_order
                .get(app_state.ui.selected_torrent_index)
                .cloned()
            {
                app_state.ui.delete_confirm.info_hash = info_hash;
                app_state.ui.delete_confirm.with_files = with_files;
                return ReduceResult {
                    redraw: true,
                    effects: vec![UiEffect::ToDeleteConfirm],
                };
            }
            ReduceResult {
                redraw: true,
                effects: Vec::new(),
            }
        }
        UiAction::OpenConfig => ReduceResult {
            redraw: true,
            effects: vec![UiEffect::OpenConfigScreen],
        },
        UiAction::OpenJournal => ReduceResult {
            redraw: true,
            effects: vec![UiEffect::OpenJournalScreen],
        },
        UiAction::OpenPeerManagement => ReduceResult {
            redraw: true,
            effects: vec![UiEffect::OpenPeerManagementScreen],
        },
        UiAction::OpenTorrentManagement => ReduceResult {
            redraw: true,
            effects: vec![UiEffect::OpenTorrentManagementScreen],
        },
        UiAction::DataRateSlower => {
            app_state.data_rate = app_state.data_rate.next_slower();
            ReduceResult {
                redraw: true,
                effects: vec![UiEffect::BroadcastManagerDataRate(
                    app_state.data_rate.as_ms(),
                )],
            }
        }
        UiAction::DataRateFaster => {
            app_state.data_rate = app_state.data_rate.next_faster();
            ReduceResult {
                redraw: true,
                effects: vec![UiEffect::BroadcastManagerDataRate(
                    app_state.data_rate.as_ms(),
                )],
            }
        }
        UiAction::ThemePrev => ReduceResult {
            redraw: true,
            effects: vec![UiEffect::ApplyThemePrev],
        },
        UiAction::ThemeNext => ReduceResult {
            redraw: true,
            effects: vec![UiEffect::ApplyThemeNext],
        },
        UiAction::TogglePauseSelected => {
            let selected_hash = app_state
                .torrent_list_order
                .get(app_state.ui.selected_torrent_index)
                .cloned();
            if let Some(info_hash) = selected_hash {
                if let Some(torrent_display) = app_state.torrents.get_mut(&info_hash) {
                    match torrent_display.latest_state.torrent_control_state {
                        TorrentControlState::Running => {
                            torrent_display.latest_state.torrent_control_state =
                                TorrentControlState::Paused;
                            return ReduceResult {
                                redraw: true,
                                effects: vec![UiEffect::SendPause(info_hash)],
                            };
                        }
                        TorrentControlState::Paused => {
                            torrent_display.latest_state.torrent_control_state =
                                TorrentControlState::Running;
                            return ReduceResult {
                                redraw: true,
                                effects: vec![UiEffect::SendResume(info_hash)],
                            };
                        }
                        TorrentControlState::Deleting => {}
                    }
                }
            }
            ReduceResult {
                redraw: true,
                effects: Vec::new(),
            }
        }
        UiAction::SortBySelectedColumn => {
            let layout_plan = calculate_header_interaction_layout(app_state, layout_mode);
            let (_, visible_torrent_columns) =
                compute_visible_torrent_columns(app_state, layout_plan.list.width);
            let (_, visible_peer_columns) =
                compute_visible_peer_columns(app_state, layout_plan.peers.width);
            let raw_selected_header = app_state.ui.selected_header;
            let selected_torrent_has_peers = selected_torrent_has_peers(app_state);
            let selected_header = normalize_selected_header(
                raw_selected_header,
                selected_torrent_has_peers,
                &visible_torrent_columns,
                &visible_peer_columns,
            );
            app_state.ui.selected_header = selected_header;

            match selected_header {
                SelectedHeader::Torrent(column_id) => {
                    let cols = get_torrent_columns();
                    if let Some(i) = torrent_column_index(column_id) {
                        if !visible_torrent_columns.contains(&i) {
                            return ReduceResult {
                                redraw: true,
                                effects: Vec::new(),
                            };
                        }
                        let Some(def) = cols.get(i) else {
                            return ReduceResult {
                                redraw: true,
                                effects: Vec::new(),
                            };
                        };
                        if let Some(column) = def.sort_enum {
                            if app_state.torrent_sort.0 == column {
                                app_state.torrent_sort.1 =
                                    if app_state.torrent_sort.1 == SortDirection::Ascending {
                                        SortDirection::Descending
                                    } else {
                                        SortDirection::Ascending
                                    };
                            } else {
                                app_state.torrent_sort.0 = column;
                                app_state.torrent_sort.1 = column.default_direction();
                            }
                            app_state.torrent_sort_pinned =
                                !torrent_sort_column_uses_autosort(column);
                            sort_and_filter_torrent_list_state(app_state);
                        }
                    }
                }
                SelectedHeader::Peer(column_id) => {
                    let cols = get_peer_columns();
                    if let Some(i) = peer_column_index(column_id) {
                        if !visible_peer_columns.contains(&i) {
                            return ReduceResult {
                                redraw: true,
                                effects: Vec::new(),
                            };
                        }
                        let Some(def) = cols.get(i) else {
                            return ReduceResult {
                                redraw: true,
                                effects: Vec::new(),
                            };
                        };
                        if let Some(column) = def.sort_enum {
                            if app_state.peer_sort.0 == column {
                                app_state.peer_sort.1 =
                                    if app_state.peer_sort.1 == SortDirection::Ascending {
                                        SortDirection::Descending
                                    } else {
                                        SortDirection::Ascending
                                    };
                            } else {
                                app_state.peer_sort.0 = column;
                                app_state.peer_sort.1 = column.default_direction();
                            }
                            app_state.peer_sort_pinned = !peer_sort_column_uses_autosort(column);
                        }
                    }
                }
            };

            ReduceResult {
                redraw: true,
                effects: Vec::new(),
            }
        }
        UiAction::ClearManualSorting => {
            app_state.torrent_sort_pinned = false;
            app_state.peer_sort_pinned = false;
            align_unpinned_sort_with_visible_activity(app_state);
            sort_and_filter_torrent_list_state(app_state);

            ReduceResult {
                redraw: true,
                effects: Vec::new(),
            }
        }
        UiAction::OpenHelp => ReduceResult {
            redraw: true,
            effects: vec![UiEffect::OpenHelpScreen],
        },
        UiAction::OpenRss => ReduceResult {
            redraw: true,
            effects: vec![UiEffect::OpenRssScreen],
        },
        UiAction::PasteText(text) => ReduceResult {
            redraw: true,
            effects: vec![UiEffect::HandlePastedText(text)],
        },
    }
}

fn map_key_to_ui_action(key: KeyEvent) -> Option<UiAction> {
    if key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }

    match key.code {
        KeyCode::Esc => Some(UiAction::ClearSystemError),
        KeyCode::Char('v') => Some(UiAction::ToggleVisualizationFocus),
        KeyCode::Char('/') => Some(UiAction::StartSearch),
        KeyCode::Char('x') => Some(UiAction::ToggleAnonymizeNames),
        KeyCode::Char('z') => Some(UiAction::EnterPowerSaving),
        KeyCode::Char('Q') => Some(UiAction::RequestQuit),
        KeyCode::Char('g') => Some(UiAction::ChartViewNext),
        KeyCode::Char('G') => Some(UiAction::ChartViewPrev),
        KeyCode::Char('t') => Some(UiAction::GraphNext),
        KeyCode::Char('T') => Some(UiAction::GraphPrev),
        KeyCode::Char('a') => Some(UiAction::OpenAddTorrentBrowser),
        KeyCode::Char('f') => Some(UiAction::OpenSelectedTorrentFiles),
        KeyCode::Char('d') => Some(UiAction::OpenDeleteConfirm { with_files: false }),
        KeyCode::Char('D') => Some(UiAction::OpenDeleteConfirm { with_files: true }),
        KeyCode::Char('c') => Some(UiAction::OpenConfig),
        KeyCode::Char('r') => Some(UiAction::OpenRss),
        KeyCode::Char('J') => Some(UiAction::OpenJournal),
        KeyCode::Char('P') => Some(UiAction::OpenPeerManagement),
        KeyCode::Char('M') => Some(UiAction::OpenTorrentManagement),
        KeyCode::Char('m') => Some(UiAction::OpenHelp),
        KeyCode::Char('[') | KeyCode::Char('{') => Some(UiAction::DataRateSlower),
        KeyCode::Char(']') | KeyCode::Char('}') => Some(UiAction::DataRateFaster),
        KeyCode::Char('<') => Some(UiAction::ThemePrev),
        KeyCode::Char('>') => Some(UiAction::ThemeNext),
        KeyCode::Char('p') => Some(UiAction::TogglePauseSelected),
        KeyCode::Char('s') => Some(UiAction::SortBySelectedColumn),
        KeyCode::Char('S') => Some(UiAction::ClearManualSorting),
        KeyCode::Up
        | KeyCode::Char('k')
        | KeyCode::Down
        | KeyCode::Char('j')
        | KeyCode::PageUp
        | KeyCode::PageDown
        | KeyCode::Home
        | KeyCode::End
        | KeyCode::Left
        | KeyCode::Char('h')
        | KeyCode::Char('l')
        | KeyCode::Right => Some(UiAction::Navigate(key.code)),
        _ => None,
    }
}

fn map_visualization_focus_key(key: KeyEvent) -> Option<UiAction> {
    if key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }

    match key.code {
        KeyCode::Char('v') => Some(UiAction::ToggleVisualizationFocus),
        KeyCode::Esc => Some(UiAction::ExitVisualizationFocus),
        KeyCode::Tab => Some(UiAction::VisualizationFocusNext),
        KeyCode::BackTab => Some(UiAction::VisualizationFocusPrev),
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('<') => {
            Some(UiAction::VisualizationRendererPrev)
        }
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('>') => {
            Some(UiAction::VisualizationRendererNext)
        }
        KeyCode::Char('u') => Some(UiAction::ResetVisualizationRenderer),
        _ => None,
    }
}

fn torrent_sort_column_uses_autosort(column: TorrentSortColumn) -> bool {
    matches!(column, TorrentSortColumn::Down | TorrentSortColumn::Up)
}

fn peer_sort_column_uses_autosort(column: PeerSortColumn) -> bool {
    matches!(column, PeerSortColumn::DL | PeerSortColumn::UL)
}

fn sort_direction_arrow_for_torrent_column(
    column: TorrentSortColumn,
    direction: SortDirection,
) -> &'static str {
    match (column, direction) {
        (TorrentSortColumn::Down | TorrentSortColumn::Up, SortDirection::Descending) => " ▼",
        (_, SortDirection::Ascending) => " ▼",
        _ => " ▲",
    }
}

fn sort_direction_arrow_for_peer_column(
    column: PeerSortColumn,
    direction: SortDirection,
) -> &'static str {
    match (column, direction) {
        (PeerSortColumn::DL | PeerSortColumn::UL, SortDirection::Descending) => " ▼",
        (_, SortDirection::Ascending) => " ▼",
        _ => " ▲",
    }
}

pub fn draw(f: &mut Frame, screen: &ScreenContext<'_>, plan: &LayoutPlan) {
    let app_state = screen.app.state;
    let settings = screen.settings;
    let ctx = screen.theme;

    draw_torrent_list(f, app_state, plan.list, ctx);
    draw_footer(f, app_state, settings, plan.footer, ctx);
    draw_details_panel(f, app_state, plan.details, ctx);
    draw_peer_files_area(f, app_state, plan.peers, ctx);

    if let Some(r) = plan.chart {
        draw_network_chart(f, app_state, r, ctx);
    }
    if let Some(r) = plan.peer_stream {
        draw_peer_stream(f, app_state, r, ctx);
    }
    if let Some(r) = plan.block_stream {
        draw_block_stream_and_disk_orb(
            f,
            app_state,
            screen.dht_status,
            screen.dht_wave_telemetry,
            r,
            ctx,
        );
    }
    if let Some(r) = plan.stats {
        draw_stats_panel(f, app_state, settings, r, ctx);
    }
}

pub(crate) fn draw_visualization_focus_overlay(
    f: &mut Frame,
    panel_area: Rect,
    footer_area: Rect,
    ctx: &ThemeContext,
) {
    highlight_visualization_focus_border(f, panel_area, ctx.state_warning());

    if footer_area.width == 0 || footer_area.height == 0 {
        return;
    }
    f.render_widget(Clear, footer_area);
    let footer = Line::from(vec![
        Span::styled("[←/→ h/l]", Style::default().fg(Color::Gray).bold()),
        Span::styled(" view  |  ", Style::default().fg(Color::DarkGray)),
        Span::styled("[u]", Style::default().fg(Color::Gray).bold()),
        Span::styled(" default  |  ", Style::default().fg(Color::DarkGray)),
        Span::styled("[Tab]", Style::default().fg(Color::Gray).bold()),
        Span::styled(" panel  |  ", Style::default().fg(Color::DarkGray)),
        Span::styled("[v]", Style::default().fg(Color::Gray).bold()),
        Span::styled(" exit", Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(
        Paragraph::new(footer).alignment(Alignment::Center),
        footer_area,
    );
}

fn highlight_visualization_focus_border(f: &mut Frame, area: Rect, color: Color) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let left = area.left();
    let right = area.right().saturating_sub(1);
    let top = area.top();
    let bottom = area.bottom().saturating_sub(1);
    let buffer = f.buffer_mut();

    for x in left..=right {
        if let Some(cell) = buffer.cell_mut((x, top)) {
            cell.fg = color;
        }
        if let Some(cell) = buffer.cell_mut((x, bottom)) {
            cell.fg = color;
        }
    }
    for y in top..=bottom {
        if let Some(cell) = buffer.cell_mut((left, y)) {
            cell.fg = color;
        }
        if let Some(cell) = buffer.cell_mut((right, y)) {
            cell.fg = color;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PeerFilesAreaLayout {
    peer_table: Option<Rect>,
    files: Rect,
    swarm: Option<Rect>,
    files_mode: TorrentFilesRenderMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TorrentFilesRenderMode {
    Tree,
    ActivitySorted,
}

#[derive(Clone, Copy)]
struct SwarmHeatmapFlash<'a> {
    info_hash: &'a [u8],
    state: &'a SwarmAvailabilityFlashState,
    now: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwarmHeatmapLevel {
    Empty,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwarmHeatmapFlashTone {
    Regular,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PeerTableRow {
    Peer(PeerInfo),
    InactiveSummary { count: usize },
}

fn draw_peer_files_area(f: &mut Frame, app_state: &AppState, area: Rect, ctx: &ThemeContext) {
    let Some(layout) = torrent_peer_files_layout(app_state, area) else {
        draw_peers_table(f, app_state, area, ctx);
        return;
    };

    if let Some(peer_table) = layout.peer_table {
        draw_peers_table_without_swarm(f, app_state, peer_table, ctx);
    }
    draw_torrent_files_panel_without_swarm(f, app_state, layout.files, ctx, layout.files_mode);

    if let Some(swarm) = layout.swarm {
        if let Some((info_hash, torrent)) = selected_torrent_entry(app_state) {
            draw_swarm_heatmap(
                f,
                ctx,
                &torrent.latest_state.peers,
                torrent.latest_state.number_of_pieces_total,
                swarm,
                Some(swarm_heatmap_flash(app_state, info_hash)),
            );
        } else {
            draw_swarm_heatmap(f, ctx, &[], 0, swarm, None);
        }
    }
}

fn torrent_peer_files_layout(app_state: &AppState, area: Rect) -> Option<PeerFilesAreaLayout> {
    if area.height < 2 || area.width < 2 {
        return None;
    }

    let torrent = selected_torrent(app_state)?;
    let (sort_by, sort_direction) = app_state.peer_sort;
    let peer_rows = displayed_peers_for_table(&torrent.latest_state, sort_by, sort_direction);
    let peer_table_height = peer_table_height_for_row_count(peer_rows.len());

    if area.height >= MIN_SWARM_AVAILABILITY_HEIGHT {
        let max_files_height = area
            .height
            .saturating_sub(peer_table_height)
            .saturating_sub(FILES_SWARM_SPACER_HEIGHT)
            .saturating_sub(MIN_SWARM_AVAILABILITY_HEIGHT);
        if let Some(files_height) = torrent_files_panel_height_needed(
            torrent,
            area.width,
            app_state.anonymize_torrent_names,
            max_files_height,
        ) {
            return Some(peer_files_layout_with_swarm(
                area,
                peer_table_height,
                files_height,
            ));
        }
    }

    saturated_active_peer_files_layout(torrent, &peer_rows, peer_table_height, area)
}

fn peer_files_layout_with_swarm(
    area: Rect,
    peer_table_height: u16,
    files_height: u16,
) -> PeerFilesAreaLayout {
    let mut y = area.y;
    let peer_table = if peer_table_height > 0 {
        let rect = Rect::new(area.x, y, area.width, peer_table_height);
        y = y.saturating_add(peer_table_height);
        Some(rect)
    } else {
        None
    };

    let files = Rect::new(area.x, y, area.width, files_height);
    y = y.saturating_add(files_height);
    y = y.saturating_add(FILES_SWARM_SPACER_HEIGHT);

    let used_height = y.saturating_sub(area.y);
    let swarm_height = area.height.saturating_sub(used_height);
    let swarm = Rect::new(area.x, y, area.width, swarm_height);

    PeerFilesAreaLayout {
        peer_table,
        files,
        swarm: Some(swarm),
        files_mode: TorrentFilesRenderMode::Tree,
    }
}

fn saturated_active_peer_files_layout(
    torrent: &TorrentDisplayState,
    peer_rows: &[PeerTableRow],
    peer_table_height: u16,
    area: Rect,
) -> Option<PeerFilesAreaLayout> {
    if !peer_rows_are_all_active(peer_rows) {
        return None;
    }

    let files_height = saturated_active_peer_files_height(torrent)?;
    if area.height < MIN_SATURATED_ACTIVE_PEER_TABLE_HEIGHT.saturating_add(files_height) {
        return None;
    }

    let peer_table_height_available = area.height.saturating_sub(files_height);
    if peer_table_height <= peer_table_height_available {
        return None;
    }

    let peer_table = Rect::new(area.x, area.y, area.width, peer_table_height_available);
    let files = Rect::new(
        area.x,
        area.y.saturating_add(peer_table_height_available),
        area.width,
        files_height,
    );

    Some(PeerFilesAreaLayout {
        peer_table: Some(peer_table),
        files,
        swarm: None,
        files_mode: TorrentFilesRenderMode::ActivitySorted,
    })
}

fn peer_rows_are_all_active(rows: &[PeerTableRow]) -> bool {
    !rows.is_empty()
        && rows.iter().all(|row| match row {
            PeerTableRow::Peer(peer) => !peer_is_inactive_for_table(peer),
            PeerTableRow::InactiveSummary { .. } => false,
        })
}

fn saturated_active_peer_files_height(torrent: &TorrentDisplayState) -> Option<u16> {
    let file_count = activity_sorted_file_count(torrent);
    if file_count == 0 {
        return None;
    }

    Some(usize_to_u16_saturating(file_count).min(SATURATED_ACTIVE_PEER_FILE_ROWS))
}

fn selected_torrent(app_state: &AppState) -> Option<&TorrentDisplayState> {
    selected_torrent_entry(app_state).map(|(_, torrent)| torrent)
}

fn selected_torrent_entry(app_state: &AppState) -> Option<(&[u8], &TorrentDisplayState)> {
    app_state
        .torrent_list_order
        .get(app_state.ui.selected_torrent_index)
        .and_then(|info_hash| {
            app_state
                .torrents
                .get(info_hash)
                .map(|torrent| (info_hash.as_slice(), torrent))
        })
}

fn swarm_heatmap_flash<'a>(app_state: &'a AppState, info_hash: &'a [u8]) -> SwarmHeatmapFlash<'a> {
    SwarmHeatmapFlash {
        info_hash,
        state: &app_state.ui.swarm_availability_flash,
        now: Instant::now(),
    }
}

fn swarm_heatmap_flashing_peer_addresses(
    flash: Option<SwarmHeatmapFlash<'_>>,
    peers: &[PeerInfo],
    total_pieces: usize,
) -> HashSet<String> {
    let Some(flash) = flash else {
        return HashSet::new();
    };

    let mut addresses = HashSet::new();
    for piece_index in flash
        .state
        .active_flash_piece_indices(flash.info_hash, flash.now)
    {
        if piece_index >= total_pieces {
            continue;
        }
        if let Some(peer) = swarm_heatmap_flash_peer(peers, total_pieces, piece_index) {
            addresses.insert(peer.address.clone());
        }
    }
    addresses
}

#[derive(Debug, Clone, Copy)]
struct DhtWaveProfile {
    amplitude: f64,
    harmonic_amplitude: f64,
    frequency: f64,
    phase_speed: f64,
    crest_bias: f64,
}

impl DhtWaveProfile {
    fn from_signal(signal: f64) -> Self {
        let signal = signal.clamp(0.0, 1.0);
        let amplitude = (0.01 + signal * 0.24).clamp(0.0, 0.52);
        let harmonic_amplitude = (0.004 + signal * 0.13).clamp(0.0, 0.20);
        let frequency = (0.08 + signal * 0.18).clamp(0.06, 0.38);
        let phase_speed = (0.03 + signal * (0.85 + signal * 0.75)).clamp(0.0, 2.0);
        let crest_bias = ((signal - 0.5) * 0.06).clamp(-0.22, 0.22);

        Self {
            amplitude,
            harmonic_amplitude,
            frequency,
            phase_speed,
            crest_bias,
        }
    }

    fn from_inputs(_status: &DhtStatus, telemetry: &DhtWaveTelemetry) -> Self {
        Self::from_signal(dht_wave_query_signal(telemetry))
    }
}

fn dht_wave_query_signal(telemetry: &DhtWaveTelemetry) -> f64 {
    normalize_dht_query_signal(telemetry.inflight_ipv4_queries + telemetry.inflight_ipv6_queries)
}

fn dht_wave_y_axis_bounds(points: &[(f64, f64)]) -> [f64; 2] {
    const MIN_HALF_SPAN: f64 = 0.18;
    const MAX_HALF_SPAN: f64 = 1.08;

    let max_abs = points.iter().map(|(_, y)| y.abs()).fold(0.0_f64, f64::max);
    let half_span = (max_abs * 1.12).clamp(MIN_HALF_SPAN, MAX_HALF_SPAN);

    [-half_span, half_span]
}

fn dht_wave_title_spans(
    total_queries: usize,
    unique_peers_found_last_10s: usize,
    demand_power_scale_halves: u8,
    ctx: &ThemeContext,
) -> Vec<Span<'static>> {
    let query_style = ctx.apply(
        Style::default()
            .fg(ctx.peer_discovered())
            .add_modifier(Modifier::BOLD),
    );
    let peer_yield_style = ctx.apply(
        Style::default()
            .fg(ctx.peer_connected())
            .add_modifier(Modifier::BOLD),
    );
    let multiplier_style = ctx.apply(
        Style::default()
            .fg(ctx.accent_peach())
            .add_modifier(Modifier::BOLD),
    );
    let mut spans = Vec::new();
    let scale_halves = if demand_power_scale_halves == 0 {
        2
    } else {
        demand_power_scale_halves
    };
    if scale_halves != 2 {
        spans.extend([
            Span::styled(dht_power_scale_label(scale_halves), multiplier_style),
            Span::styled("(", multiplier_style),
        ]);
    }
    spans.extend([
        Span::styled(total_queries.to_string(), query_style),
        Span::styled(
            " ",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
        ),
        Span::styled(unique_peers_found_last_10s.to_string(), peer_yield_style),
    ]);
    if scale_halves != 2 {
        spans.push(Span::styled(")", multiplier_style));
    }
    spans
}

fn dht_wave_title_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.chars().count()).sum()
}

fn dht_wave_should_show_left_title(area_width: u16, right_title_width: usize) -> bool {
    const LEFT_TITLE_WIDTH: usize = 3;
    const MIN_TITLE_GAP: usize = 1;

    let top_border_width = usize::from(area_width).saturating_sub(2);
    top_border_width >= LEFT_TITLE_WIDTH + MIN_TITLE_GAP + right_title_width
}

fn dht_power_scale_label(scale_halves: u8) -> String {
    if scale_halves.is_multiple_of(2) {
        format!("{}x", scale_halves / 2)
    } else {
        format!("{}.5x", scale_halves / 2)
    }
}

fn dht_peer_yield_signal(unique_peers_found_last_10s: usize) -> f64 {
    normalize_dht_peer_yield(unique_peers_found_last_10s)
}

fn dht_peer_yield_wave_points(
    phase: f64,
    unique_peers_found_last_10s: usize,
    sample_count: usize,
    x_step: f64,
) -> Vec<(f64, f64)> {
    let yield_signal = dht_peer_yield_signal(unique_peers_found_last_10s);
    if yield_signal <= 0.0 {
        return Vec::new();
    }

    let peer_profile = DhtWaveProfile::from_signal(yield_signal);
    let peer_phase = phase + std::f64::consts::TAU * 0.31;
    let mut points = Vec::with_capacity(sample_count + 1);

    for i in 0..=sample_count {
        let x = i as f64 * x_step;
        let theta = x * peer_profile.frequency;
        let envelope = 0.84 + 0.16 * (theta * 0.33 + peer_phase * 0.28).sin();
        let carrier = peer_profile.crest_bias * 0.35
            + envelope * peer_profile.amplitude.clamp(0.05, 0.82) * (theta + peer_phase).sin()
            + peer_profile.harmonic_amplitude * ((theta * 2.35) - peer_phase * 0.72).sin();
        points.push((x, carrier.clamp(-1.04, 1.04)));
    }

    points
}

fn dht_peer_yield_draws_on_top(query_signal: f64, peer_yield_signal: f64) -> bool {
    peer_yield_signal >= query_signal
}

fn draw_dht_wave_panel(
    f: &mut Frame,
    app_state: &AppState,
    dht_status: &DhtStatus,
    dht_wave_telemetry: &DhtWaveTelemetry,
    area: Rect,
    ctx: &ThemeContext,
) {
    if area.height < 3 || area.width < 10 {
        return;
    }

    let visualization = app_state.ui.visualization_focus.dht;
    if visualization != DhtVisualization::Classic {
        draw_dht_visualization(
            f,
            app_state,
            dht_status,
            dht_wave_telemetry,
            area,
            visualization,
            ctx,
        );
        return;
    }

    let profile = if app_state.ui.dht_wave.initialized {
        DhtWaveProfile {
            amplitude: app_state.ui.dht_wave.amplitude,
            harmonic_amplitude: app_state.ui.dht_wave.harmonic_amplitude,
            frequency: app_state.ui.dht_wave.frequency,
            phase_speed: app_state.ui.dht_wave.phase_speed,
            crest_bias: app_state.ui.dht_wave.crest_bias,
        }
    } else {
        DhtWaveProfile::from_inputs(dht_status, dht_wave_telemetry)
    };
    let total_queries =
        dht_wave_telemetry.inflight_ipv4_queries + dht_wave_telemetry.inflight_ipv6_queries;
    let x_bound = area.width.saturating_sub(3).max(1) as usize;
    let phase = if app_state.ui.dht_wave.initialized {
        app_state.ui.dht_wave.phase
    } else {
        app_state.ui.effects_phase_time * profile.phase_speed
    };

    let sample_count = (x_bound.max(1) * 3).max(16);
    let x_step = x_bound as f64 / sample_count as f64;
    let mut dht_points = Vec::with_capacity(sample_count + 1);

    for i in 0..=sample_count {
        let x = i as f64 * x_step;
        let theta = x * profile.frequency;
        let envelope = 0.84 + 0.16 * (theta * 0.33 + phase * 0.28).sin();
        let transient_boost = if app_state.ui.dht_wave.initialized {
            app_state.ui.dht_wave.discovery_boost + app_state.ui.dht_wave.query_surge
        } else {
            0.0
        };
        let dht_amplitude = (profile.amplitude + transient_boost).clamp(0.05, 0.82);
        let carrier = profile.crest_bias * 0.35
            + envelope * dht_amplitude * (theta + phase).sin()
            + profile.harmonic_amplitude * ((theta * 2.35) - phase * 0.72).sin();
        dht_points.push((x, carrier.clamp(-1.04, 1.04)));
    }
    let peer_yield_points = dht_peer_yield_wave_points(
        phase,
        dht_wave_telemetry.unique_peers_found_last_10s,
        sample_count,
        x_step,
    );
    let query_signal = if app_state.ui.dht_wave.initialized {
        app_state.ui.dht_wave.query_load
    } else {
        dht_wave_query_signal(dht_wave_telemetry)
    };
    let peer_yield_signal = dht_peer_yield_signal(dht_wave_telemetry.unique_peers_found_last_10s);
    let mut y_axis_points = dht_points.clone();
    y_axis_points.extend(peer_yield_points.iter().copied());
    let y_axis_bounds = dht_wave_y_axis_bounds(&y_axis_points);

    let dht_dataset = ratatui::widgets::Dataset::default()
        .marker(ratatui::symbols::Marker::Braille)
        .graph_type(ratatui::widgets::GraphType::Line)
        .style(
            ctx.apply(
                Style::default()
                    .fg(ctx.peer_discovered())
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .data(&dht_points);
    let peer_yield_dataset = ratatui::widgets::Dataset::default()
        .marker(ratatui::symbols::Marker::Braille)
        .graph_type(ratatui::widgets::GraphType::Line)
        .style(
            ctx.apply(
                Style::default()
                    .fg(ctx.peer_connected())
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .data(&peer_yield_points);
    let datasets = if peer_yield_points.is_empty() {
        vec![dht_dataset]
    } else if dht_peer_yield_draws_on_top(query_signal, peer_yield_signal) {
        vec![dht_dataset, peer_yield_dataset]
    } else {
        vec![peer_yield_dataset, dht_dataset]
    };

    let title_spans = dht_wave_title_spans(
        total_queries,
        dht_wave_telemetry.unique_peers_found_last_10s,
        dht_wave_telemetry.demand_power_scale_halves,
        ctx,
    );
    let mut block = Block::default();
    if dht_wave_should_show_left_title(area.width, dht_wave_title_width(&title_spans)) {
        block = block.title_top(
            Line::from(Span::styled(
                "DHT",
                ctx.apply(Style::default().fg(ctx.peer_discovered())),
            ))
            .alignment(Alignment::Left),
        );
    }
    block = block
        .title_top(Line::from(title_spans).alignment(Alignment::Right))
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)));

    let chart = ratatui::widgets::Chart::new(datasets)
        .block(block)
        .x_axis(ratatui::widgets::Axis::default().bounds([0.0, x_bound as f64]))
        .y_axis(ratatui::widgets::Axis::default().bounds(y_axis_bounds));

    f.render_widget(chart, area);
}

pub fn draw_status_error_popup(f: &mut Frame, error_text: &str, ctx: &ThemeContext) {
    let popup_width_percent: u16 = 50;
    let popup_height: u16 = 8;
    let vertical_chunks = ratatui::layout::Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(popup_height),
        Constraint::Min(0),
    ])
    .split(f.area());
    let area = ratatui::layout::Layout::horizontal([
        Constraint::Percentage((100 - popup_width_percent) / 2),
        Constraint::Percentage(popup_width_percent),
        Constraint::Percentage((100 - popup_width_percent) / 2),
    ])
    .split(vertical_chunks[1])[1];

    f.render_widget(Clear, area);
    let text = vec![
        Line::from(Span::styled(
            "Error",
            ctx.apply(Style::default().fg(ctx.state_error()).bold()),
        )),
        Line::from(""),
        Line::from(Span::styled(
            error_text,
            ctx.apply(Style::default().fg(ctx.state_warning())),
        )),
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "[Press Esc to dismiss]",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        )),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(ctx.state_error())));
    let paragraph = Paragraph::new(text)
        .block(block)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true });
    f.render_widget(paragraph, area);
}

pub fn draw_shutdown_screen(f: &mut Frame, app_state: &AppState, ctx: &ThemeContext) {
    const POPUP_WIDTH: u16 = 40;
    const POPUP_HEIGHT: u16 = 3;
    let area = f.area();
    let width = POPUP_WIDTH.min(area.width);
    let height = POPUP_HEIGHT.min(area.height);
    let vertical_chunks = ratatui::layout::Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(height),
        Constraint::Min(0),
    ])
    .split(area);
    let area = ratatui::layout::Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(width),
        Constraint::Min(0),
    ])
    .split(vertical_chunks[1])[1];

    f.render_widget(Clear, area);
    let container_block = Block::default()
        .title(Span::styled(
            " Exiting ",
            ctx.apply(Style::default().fg(ctx.accent_peach())),
        ))
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)));
    let inner_area = container_block.inner(area);
    f.render_widget(container_block, area);

    let chunks = ratatui::layout::Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1)])
        .split(inner_area);
    let progress_label = format!("{:.0}%", (app_state.shutdown_progress * 100.0).min(100.0));
    let progress_bar = Gauge::default()
        .ratio(app_state.shutdown_progress)
        .label(progress_label)
        .gauge_style(
            ctx.apply(
                Style::default()
                    .fg(ctx.state_selected())
                    .bg(ctx.theme.semantic.surface0),
            ),
        );
    f.render_widget(progress_bar, chunks[0]);
}

pub(crate) fn truncate_theme_label_preserving_fx(
    theme_name: &str,
    fx_enabled: bool,
    max_len: usize,
) -> String {
    if max_len == 0 {
        return String::new();
    }

    if !fx_enabled {
        return truncate_with_ellipsis(theme_name, max_len);
    }

    let suffix = "[FX]";
    let suffix_len = suffix.chars().count();
    let full = format!("{theme_name} {suffix}");
    if full.chars().count() <= max_len {
        return full;
    }

    if max_len <= 3 {
        return ".".repeat(max_len);
    }

    if max_len <= suffix_len + 3 {
        return truncate_with_ellipsis(&full, max_len);
    }

    let name_len = max_len.saturating_sub(3 + suffix_len);
    let name_prefix: String = theme_name.chars().take(name_len).collect();
    format!("{name_prefix}...{suffix}")
}

pub(crate) fn compute_footer_left_width(footer_width: u16, is_update: bool) -> u16 {
    let min_left = if is_update { 68u16 } else { 48u16 };
    let max_left = if is_update { 110u16 } else { 90u16 };
    let right_status = 21u16;
    let min_commands = 18u16;
    let reserved = right_status + min_commands;

    let available_for_left = footer_width.saturating_sub(reserved);
    available_for_left.clamp(min_left, max_left)
}

pub(crate) fn compute_footer_side_widths(
    footer_width: u16,
    is_update: bool,
    content_left: u16,
    status_width: u16,
) -> (u16, u16) {
    let min_left = if is_update { 52u16 } else { 40u16 };
    let min_commands = 18u16;
    let desired_left = compute_footer_left_width(footer_width, is_update);
    let left_target = desired_left.min(content_left.max(min_left));
    let max_left = footer_width.saturating_sub(status_width.saturating_add(min_commands));
    (left_target.min(max_left), status_width)
}

pub(crate) fn compute_footer_status_width(client_port: u16, overall_port_status: &str) -> u16 {
    format!("Port {} | IPv4/IPv6 | {}", client_port, overall_port_status).len() as u16
        + FOOTER_STATUS_GUTTER
}

fn format_measured_fps(fps: f64) -> String {
    let fps = fps.max(0.0);
    let precision = if fps >= 10.0 {
        0
    } else if fps >= 1.0 {
        1
    } else {
        2
    };

    let mut label = match precision {
        0 => format!("{fps:.0}"),
        1 => format!("{fps:.1}"),
        _ => format!("{fps:.2}"),
    };
    if label.contains('.') {
        while label.ends_with('0') {
            label.pop();
        }
        if label.ends_with('.') {
            label.pop();
        }
    }
    label
}

pub(crate) fn footer_fps_label(app_state: &AppState) -> String {
    let target_fps = app_state.data_rate.target_fps();
    let target_label = app_state.data_rate.fps_label();
    match app_state.ui.measured_fps {
        Some(measured_fps) if measured_fps.is_finite() => {
            let measured_label = format_measured_fps(measured_fps);
            if measured_fps >= target_fps || measured_label == target_label {
                format!("{target_label} fps")
            } else {
                format!("{measured_label}/{target_label} fps")
            }
        }
        _ => format!("{target_label} fps"),
    }
}

fn estimate_footer_left_content_width(app_state: &AppState, ctx: &ThemeContext) -> u16 {
    let fx_enabled = ctx.theme.effects.enabled();
    let theme_label = if fx_enabled {
        format!("{} [FX]", ctx.theme.name)
    } else {
        ctx.theme.name.to_string()
    };
    let fps_label = footer_fps_label(app_state);

    let content = if let Some(new_version) = &app_state.update_available {
        format!(
            "UPDATE AVAILABLE: v{} -> v{} | {} | {}",
            APP_VERSION, new_version, fps_label, theme_label
        )
    } else {
        #[cfg(all(feature = "dht", feature = "pex"))]
        {
            format!(
                "superseedr v{} | {} | {}",
                APP_VERSION, fps_label, theme_label
            )
        }
        #[cfg(not(all(feature = "dht", feature = "pex")))]
        {
            format!(
                "superseedr [PRIVATE] v{} | {} | {}",
                APP_VERSION, fps_label, theme_label
            )
        }
    };

    (content.chars().count() as u16).saturating_add(2)
}

fn footer_command_len(key: &str, suffix: &str) -> usize {
    key.chars().count() + suffix.chars().count()
}

fn try_push_footer_command(
    spans: &mut Vec<Span<'static>>,
    used_width: &mut usize,
    max_width: usize,
    key: &'static str,
    suffix: &'static str,
    key_style: Style,
) -> bool {
    let item_width = footer_command_len(key, suffix);
    let separator_width = if *used_width == 0 { 0 } else { 3 };
    if *used_width + separator_width + item_width > max_width {
        return false;
    }

    if separator_width > 0 {
        spans.push(Span::raw(" | "));
    }
    spans.push(Span::styled(key, key_style));
    spans.push(Span::raw(suffix));
    *used_width += separator_width + item_width;
    true
}

pub fn draw_footer(
    f: &mut Frame,
    app_state: &AppState,
    settings: &Settings,
    footer_chunk: ratatui::layout::Rect,
    ctx: &ThemeContext,
) {
    if let Some(interruption) = network_interruption(app_state) {
        let compact_layout = uses_vertical_layout(app_state.screen_area, settings.ui_layout_mode);
        let text = network_interruption_footer_text(
            &interruption,
            footer_chunk.width as usize,
            compact_layout,
        );
        let color = if interruption.blocked {
            ctx.state_error()
        } else {
            ctx.state_warning()
        };
        let warning = Paragraph::new(Line::from(Span::styled(
            text,
            ctx.apply(Style::default().fg(color).bold()),
        )))
        .alignment(Alignment::Center);
        f.render_widget(warning, footer_chunk);
        return;
    }

    let show_branding = footer_chunk.width >= 80;
    let any_port_open =
        app_state.externally_accessable_port_v4 || app_state.externally_accessable_port_v6;
    let overall_port_status = if any_port_open { "OPEN" } else { "CLOSED" };
    let now = Instant::now();
    let v4_highlight_active = app_state
        .externally_accessable_port_v4_highlight_until
        .is_some_and(|deadline| deadline > now);
    let v6_highlight_active = app_state
        .externally_accessable_port_v6_highlight_until
        .is_some_and(|deadline| deadline > now);
    let status_width = compute_footer_status_width(settings.client_port, overall_port_status);

    let is_update = app_state.update_available.is_some();
    let (left_constraint, right_constraint) = if show_branding {
        let content_left = estimate_footer_left_content_width(app_state, ctx);
        let (left_width, right_width) =
            compute_footer_side_widths(footer_chunk.width, is_update, content_left, status_width);
        (
            Constraint::Length(left_width),
            Constraint::Length(right_width),
        )
    } else {
        (Constraint::Length(0), Constraint::Length(status_width))
    };

    let footer_layout = ratatui::layout::Layout::default()
        .direction(Direction::Horizontal)
        .constraints([left_constraint, Constraint::Min(0), right_constraint])
        .split(footer_chunk);

    let client_id_chunk = footer_layout[0];
    let commands_chunk = footer_layout[1];
    let status_chunk = footer_layout[2];

    if show_branding {
        #[cfg(all(feature = "dht", feature = "pex"))]
        let current_dl_speed = *app_state.avg_download_history.last().unwrap_or(&0);
        #[cfg(all(feature = "dht", feature = "pex"))]
        let current_ul_speed = *app_state.avg_upload_history.last().unwrap_or(&0);
        let fx_enabled = ctx.theme.effects.enabled();
        let theme_name = ctx.theme.name.to_string();
        let fps_label = footer_fps_label(app_state);
        let fit_theme_label = |prefix: &str| -> String {
            let max_theme_width =
                (client_id_chunk.width as usize).saturating_sub(prefix.chars().count());
            if max_theme_width == 0 {
                String::new()
            } else if max_theme_width <= 3 {
                ".".repeat(max_theme_width)
            } else {
                truncate_theme_label_preserving_fx(&theme_name, fx_enabled, max_theme_width)
            }
        };

        let client_display_line = if let Some(new_version) = &app_state.update_available {
            let theme_display = fit_theme_label(&format!(
                "UPDATE AVAILABLE: v{} -> v{} | {} | ",
                APP_VERSION, new_version, fps_label
            ));
            Line::from(vec![
                Span::styled(
                    "UPDATE AVAILABLE: ",
                    ctx.apply(Style::default().fg(ctx.state_warning()).bold()),
                ),
                Span::styled(
                    format!("v{}", APP_VERSION),
                    Style::default()
                        .fg(ctx.theme.semantic.surface2)
                        .add_modifier(ratatui::prelude::Modifier::CROSSED_OUT),
                ),
                Span::styled(
                    " \u{2192} ",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
                ),
                Span::styled(
                    format!("v{}", new_version),
                    ctx.apply(Style::default().fg(ctx.state_success()).bold()),
                ),
                Span::styled(
                    " | ",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
                ),
                Span::styled(
                    fps_label.clone(),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
                ),
                Span::styled(
                    " | ",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
                ),
                Span::styled(
                    theme_display,
                    ctx.apply(Style::default().fg(ctx.state_selected())),
                ),
            ])
        } else {
            #[cfg(all(feature = "dht", feature = "pex"))]
            {
                let theme_display =
                    fit_theme_label(&format!("superseedr v{} | {} | ", APP_VERSION, fps_label));
                Line::from(vec![
                    Span::styled(
                        "super",
                        ctx.apply(
                            speed_to_style(ctx, current_dl_speed)
                                .add_modifier(ratatui::prelude::Modifier::BOLD),
                        ),
                    ),
                    Span::styled(
                        "seedr",
                        ctx.apply(
                            speed_to_style(ctx, current_ul_speed)
                                .add_modifier(ratatui::prelude::Modifier::BOLD),
                        ),
                    ),
                    Span::styled(
                        format!(" v{}", APP_VERSION),
                        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
                    ),
                    Span::styled(
                        " | ",
                        ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
                    ),
                    Span::styled(
                        fps_label.clone(),
                        ctx.apply(Style::default().fg(ctx.state_warning()).bold()),
                    ),
                    Span::styled(
                        " | ",
                        ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
                    ),
                    Span::styled(
                        theme_display,
                        ctx.apply(Style::default().fg(ctx.state_selected())),
                    ),
                ])
            }
            #[cfg(not(all(feature = "dht", feature = "pex")))]
            {
                let theme_display = fit_theme_label(&format!(
                    "superseedr [PRIVATE] v{} | {} | ",
                    APP_VERSION, fps_label
                ));
                Line::from(vec![
                    Span::styled(
                        "superseedr",
                        ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
                    )
                    .add_modifier(ratatui::prelude::Modifier::CROSSED_OUT),
                    Span::styled(
                        " [PRIVATE]",
                        Style::default()
                            .fg(ctx.state_error())
                            .add_modifier(ratatui::prelude::Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" v{}", APP_VERSION),
                        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
                    ),
                    Span::styled(
                        " | ",
                        ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
                    ),
                    Span::styled(
                        fps_label.clone(),
                        ctx.apply(Style::default().fg(ctx.state_warning()).bold()),
                    ),
                    Span::styled(
                        " | ",
                        ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
                    ),
                    Span::styled(
                        theme_display,
                        ctx.apply(Style::default().fg(ctx.state_selected())),
                    ),
                ])
            }
        };

        let client_id_paragraph = Paragraph::new(client_display_line).alignment(Alignment::Left);
        f.render_widget(client_id_paragraph, client_id_chunk);
    }

    let max_width = commands_chunk.width as usize;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used_width = 0usize;

    let manual_key = "[m]";
    let manual_fallback_suffix = "anual";
    let manual_suffix = if app_state.system_warning.is_some() {
        "anual (warning)"
    } else {
        manual_fallback_suffix
    };
    let manual_min_width = footer_command_len(manual_key, manual_fallback_suffix);

    let mut push_if_fits = |key: &'static str, suffix: &'static str, key_style: Style| {
        let separator_width = if used_width == 0 { 0 } else { 3 };
        let candidate_width = footer_command_len(key, suffix);
        let required_for_manual = if used_width + separator_width + candidate_width == 0 {
            manual_min_width
        } else {
            3 + manual_min_width
        };
        if used_width + separator_width + candidate_width + required_for_manual <= max_width {
            let _ = try_push_footer_command(
                &mut spans,
                &mut used_width,
                max_width,
                key,
                suffix,
                key_style,
            );
        }
    };

    push_if_fits(
        "[arrows]",
        " nav",
        footer_key_style(ctx, ActionTone::Navigate),
    );
    push_if_fits("[Q]", "uit", footer_key_style(ctx, ActionTone::Destructive));
    push_if_fits("[Paste]", "paste", footer_key_style(ctx, ActionTone::Paste));
    push_if_fits("[p]", "ause", footer_key_style(ctx, ActionTone::Queue));
    push_if_fits("[a]", "dd", footer_key_style(ctx, ActionTone::Add));
    push_if_fits(
        "[d]",
        "elete",
        footer_key_style(ctx, ActionTone::Destructive),
    );
    push_if_fits("[s]", "ort", footer_key_style(ctx, ActionTone::Sort));
    push_if_fits("[t]", "ime", footer_key_style(ctx, ActionTone::Rate));
    push_if_fits("[g]", "raph", footer_key_style(ctx, ActionTone::Mode));
    push_if_fits("[v]", "isual", footer_key_style(ctx, ActionTone::Mode));
    push_if_fits("[<]theme[>]", "", footer_key_style(ctx, ActionTone::Theme));
    push_if_fits("[/]", "search", footer_key_style(ctx, ActionTone::Search));
    push_if_fits("[c]", "onfig", footer_key_style(ctx, ActionTone::Open));
    push_if_fits("[r]", "ss", footer_key_style(ctx, ActionTone::Open));
    push_if_fits(
        "[d]",
        "elete",
        footer_key_style(ctx, ActionTone::Destructive),
    );
    push_if_fits("[x]", "anon", footer_key_style(ctx, ActionTone::Toggle));
    push_if_fits("[z]", "power", footer_key_style(ctx, ActionTone::Toggle));
    push_if_fits("[T]", "time++", footer_key_style(ctx, ActionTone::Rate));
    push_if_fits("[[]", "slower", footer_key_style(ctx, ActionTone::Rate));
    push_if_fits("[]]", "faster", footer_key_style(ctx, ActionTone::Rate));

    if !try_push_footer_command(
        &mut spans,
        &mut used_width,
        max_width,
        manual_key,
        manual_suffix,
        footer_key_style(ctx, ActionTone::Open),
    ) {
        let _ = try_push_footer_command(
            &mut spans,
            &mut used_width,
            max_width,
            manual_key,
            manual_fallback_suffix,
            ctx.apply(Style::default().fg(ctx.accent_teal())),
        );
    }
    if !spans.iter().any(|s| matches!(s.content.as_ref(), "[m]")) {
        let _ = try_push_footer_command(
            &mut spans,
            &mut used_width,
            max_width,
            manual_key,
            "",
            ctx.apply(Style::default().fg(ctx.accent_teal())),
        );
    }

    let footer_paragraph = Paragraph::new(Line::from(spans))
        .alignment(Alignment::Center)
        .style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)));
    f.render_widget(footer_paragraph, commands_chunk);

    let port_style = if any_port_open {
        ctx.apply(Style::default().fg(ctx.state_success()))
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))
    };
    let v4_port_style = if v4_highlight_active {
        ctx.apply(Style::default().fg(ctx.state_success()).bold())
    } else if app_state.externally_accessable_port_v4 {
        ctx.apply(Style::default().fg(ctx.state_success()))
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))
    };
    let v6_port_style = if v6_highlight_active {
        ctx.apply(Style::default().fg(ctx.state_success()).bold())
    } else if app_state.externally_accessable_port_v6 {
        ctx.apply(Style::default().fg(ctx.state_success()))
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))
    };

    let footer_status_spans = vec![
        Span::raw("Port "),
        Span::styled(settings.client_port.to_string(), port_style),
        Span::raw(" | "),
        Span::styled("IPv4", v4_port_style),
        Span::raw("/"),
        Span::styled("IPv6", v6_port_style),
        Span::raw(" | "),
        Span::styled(overall_port_status, port_style),
    ];
    let footer_status = Line::from(footer_status_spans).alignment(Alignment::Right);

    let status_paragraph = Paragraph::new(footer_status)
        .style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)));
    f.render_widget(status_paragraph, status_chunk);
}

fn format_peer_address_for_table(address: &str) -> String {
    match address.parse::<SocketAddr>() {
        Ok(SocketAddr::V4(addr)) => addr.to_string(),
        Ok(SocketAddr::V6(addr)) => format!("{}:{}", addr.ip(), addr.port()),
        Err(_) => address.to_string(),
    }
}

fn selected_torrent_has_peers(app_state: &AppState) -> bool {
    app_state
        .torrent_list_order
        .get(app_state.ui.selected_torrent_index)
        .and_then(|info_hash| app_state.torrents.get(info_hash))
        .is_some_and(|torrent| !torrent.latest_state.peers.is_empty())
}

fn nearest_visible_column(visible_columns: &[usize], selected_column: usize) -> Option<usize> {
    visible_columns
        .iter()
        .copied()
        .find(|&idx| idx >= selected_column)
        .or_else(|| visible_columns.last().copied())
}

fn torrent_column_id_for_index(index: usize) -> Option<ColumnId> {
    get_torrent_columns().get(index).map(|column| column.id)
}

fn peer_column_id_for_index(index: usize) -> Option<PeerColumnId> {
    get_peer_columns().get(index).map(|column| column.id)
}

fn torrent_column_index(column_id: ColumnId) -> Option<usize> {
    get_torrent_columns()
        .iter()
        .position(|column| column.id == column_id)
}

fn peer_column_index(column_id: PeerColumnId) -> Option<usize> {
    get_peer_columns()
        .iter()
        .position(|column| column.id == column_id)
}

fn nearest_visible_torrent_column(
    visible_columns: &[usize],
    selected_column: ColumnId,
) -> Option<ColumnId> {
    let selected_index = torrent_column_index(selected_column).unwrap_or(usize::MAX);
    nearest_visible_column(visible_columns, selected_index).and_then(torrent_column_id_for_index)
}

fn nearest_visible_peer_column(
    visible_columns: &[usize],
    selected_column: PeerColumnId,
) -> Option<PeerColumnId> {
    let selected_index = peer_column_index(selected_column).unwrap_or(usize::MAX);
    nearest_visible_column(visible_columns, selected_index).and_then(peer_column_id_for_index)
}

fn last_visible_torrent_column(visible_columns: &[usize]) -> Option<ColumnId> {
    nearest_visible_column(visible_columns, usize::MAX).and_then(torrent_column_id_for_index)
}

fn normalize_selected_header(
    selected_header: SelectedHeader,
    selected_torrent_has_peers: bool,
    visible_torrent_columns: &[usize],
    visible_peer_columns: &[usize],
) -> SelectedHeader {
    match selected_header {
        SelectedHeader::Torrent(column_id) => {
            nearest_visible_torrent_column(visible_torrent_columns, column_id)
                .map(SelectedHeader::Torrent)
                .unwrap_or(SelectedHeader::Torrent(ColumnId::Name))
        }
        SelectedHeader::Peer(column_id) => {
            if selected_torrent_has_peers {
                nearest_visible_peer_column(visible_peer_columns, column_id)
                    .map(SelectedHeader::Peer)
                    .unwrap_or_else(|| {
                        last_visible_torrent_column(visible_torrent_columns)
                            .map(SelectedHeader::Torrent)
                            .unwrap_or(SelectedHeader::Torrent(ColumnId::Name))
                    })
            } else {
                last_visible_torrent_column(visible_torrent_columns)
                    .map(SelectedHeader::Torrent)
                    .unwrap_or(SelectedHeader::Torrent(ColumnId::Name))
            }
        }
    }
}

pub fn draw_torrent_list(f: &mut Frame, app_state: &AppState, area: Rect, ctx: &ThemeContext) {
    let mut table_state = TableState::default();
    if matches!(app_state.ui.selected_header, SelectedHeader::Torrent(_)) {
        table_state.select(Some(app_state.ui.selected_torrent_index));
    }

    let all_cols = get_torrent_columns();
    let (constraints, visible_indices) = compute_visible_torrent_columns(app_state, area.width);

    let (sort_col, sort_dir) = app_state.torrent_sort;
    let header_cells: Vec<Cell> = visible_indices
        .iter()
        .map(|&real_idx| {
            let def = &all_cols[real_idx];
            let is_selected = app_state.ui.selected_header == SelectedHeader::Torrent(def.id);
            let is_sorting = def.sort_enum == Some(sort_col);

            let mut style = ctx.apply(Style::default().fg(ctx.state_warning()));
            if is_sorting {
                style = style.fg(ctx.state_selected());
            }
            style = ctx.apply(style);

            let mut spans = vec![];
            let mut text_span = Span::styled(def.header, style);
            if is_selected {
                text_span = text_span.style(
                    style
                        .fg(ctx.state_selected())
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                );
            }
            spans.push(text_span);

            if is_sorting {
                let arrow = sort_direction_arrow_for_torrent_column(sort_col, sort_dir);
                spans.push(Span::styled(arrow, style));
            }
            Cell::from(Line::from(spans))
        })
        .collect();
    let header = Row::new(header_cells).height(1);

    let rows =
        app_state
            .torrent_list_order
            .iter()
            .enumerate()
            .map(|(i, info_hash)| match app_state.torrents.get(info_hash) {
                Some(torrent) => {
                    let state = &torrent.latest_state;
                    let is_selected = i == app_state.ui.selected_torrent_index;
                    let row_color = torrent_list_row_color(torrent, ctx);
                    let mut row_style = ctx.apply(Style::default().fg(row_color));
                    row_style = ctx.apply(row_style);

                    if is_selected {
                        let is_safe_ascii = state.torrent_name.is_ascii();
                        if is_safe_ascii {
                            row_style = row_style.add_modifier(Modifier::BOLD);
                        }
                    }

                    let cells: Vec<Cell> = visible_indices
                        .iter()
                        .map(|&real_idx| {
                            let def = &all_cols[real_idx];
                            match def.id {
                                ColumnId::Status => {
                                    let status = torrent_status_cell(torrent, ctx);
                                    Cell::from(status.text).style(status.style)
                                }
                                ColumnId::Name => {
                                    let name = anonymize_tree_name(
                                        &state.torrent_name,
                                        false,
                                        app_state.anonymize_torrent_names,
                                    );
                                    let mut c = Cell::from(name);
                                    if is_selected {
                                        let s = ctx.apply(Style::default().fg(ctx.state_warning()));
                                        c = c.style(s);
                                    }
                                    c
                                }
                                ColumnId::DownSpeed => {
                                    let style = if state.data_available {
                                        speed_to_style(ctx, torrent.smoothed_download_speed_bps)
                                    } else {
                                        Style::default().fg(row_color)
                                    };
                                    Cell::from(format_speed(torrent.smoothed_download_speed_bps))
                                        .style(ctx.apply(style))
                                }
                                ColumnId::UpSpeed => {
                                    let style = if state.data_available {
                                        speed_to_style(ctx, torrent.smoothed_upload_speed_bps)
                                    } else {
                                        Style::default().fg(row_color)
                                    };
                                    Cell::from(format_speed(torrent.smoothed_upload_speed_bps))
                                        .style(ctx.apply(style))
                                }
                            }
                        })
                        .collect();

                    Row::new(cells).style(row_style)
                }
                None => Row::new(vec![Cell::from("Error retrieving data")]),
            });

    let border_style = if matches!(app_state.ui.selected_header, SelectedHeader::Torrent(_)) {
        ctx.apply(Style::default().fg(ctx.state_selected()))
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.surface2))
    };

    let mut title_spans = Vec::new();
    if app_state.ui.is_searching {
        title_spans.push(Span::raw("Search: /"));
        title_spans.push(Span::styled(
            &app_state.ui.search_query,
            ctx.apply(Style::default().fg(ctx.state_warning())),
        ));
    } else if !app_state.ui.search_query.is_empty() {
        title_spans.push(Span::styled(
            format!("[{}] ", app_state.ui.search_query),
            ctx.apply(
                Style::default()
                    .fg(ctx.theme.semantic.subtext1)
                    .add_modifier(Modifier::ITALIC),
            ),
        ));
    }

    if !app_state.ui.is_searching {
        if let Some(info_hash) = app_state
            .torrent_list_order
            .get(app_state.ui.selected_torrent_index)
        {
            if let Some(torrent) = app_state.torrents.get(info_hash) {
                let path_cow;
                let text_to_show = if app_state.anonymize_torrent_names {
                    "/path/to/torrent/file"
                } else {
                    path_cow = torrent
                        .latest_state
                        .download_path
                        .as_ref()
                        .map(|p| p.to_string_lossy())
                        .unwrap_or_else(|| std::borrow::Cow::Borrowed("Unknown path"));
                    &sanitize_text(&path_cow)
                };

                let avail_width = area.width.saturating_sub(10) as usize;
                let display_name = truncate_with_ellipsis(text_to_show, avail_width);

                title_spans.push(Span::styled(
                    display_name,
                    ctx.apply(Style::default().fg(ctx.state_warning())),
                ));
            }
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Line::from(title_spans));

    let inner_area = block.inner(area);
    let table = Table::new(rows, constraints).header(header).block(block);
    f.render_stateful_widget(table, area, &mut table_state);

    if app_state.torrent_list_order.is_empty() {
        let empty_msg = vec![
            Line::from(Span::styled(
                "No Torrents",
                ctx.apply(
                    Style::default()
                        .fg(ctx.theme.semantic.surface2)
                        .add_modifier(Modifier::BOLD),
                ),
            )),
            Line::from(Span::styled(
                "Press [a] to add a file or use your terminal paste shortcut",
                ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
            )),
        ];

        let center_y = inner_area.y + (inner_area.height / 2).saturating_sub(1);
        let text_area = Rect::new(inner_area.x, center_y, inner_area.width, 2);

        f.render_widget(
            Paragraph::new(empty_msg).alignment(Alignment::Center),
            text_area,
        );
    }
}

pub fn draw_details_panel(
    f: &mut Frame,
    app_state: &AppState,
    details_text_chunk: Rect,
    ctx: &ThemeContext,
) {
    let selected_torrent = app_state
        .torrent_list_order
        .get(app_state.ui.selected_torrent_index)
        .and_then(|h| app_state.torrents.get(h));

    let critical_panel = selected_torrent.and_then(|torrent| {
        selected_torrent_critical_details(torrent, app_state.anonymize_torrent_names)
    });

    let details_block = Block::default()
        .title(Span::styled(
            critical_panel
                .as_ref()
                .map_or("Details", |panel| panel.title),
            ctx.apply(Style::default().fg(if critical_panel.is_some() {
                ctx.state_error()
            } else {
                ctx.state_selected()
            })),
        ))
        .borders(Borders::ALL)
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(if critical_panel.is_some() {
            ctx.state_error()
        } else {
            ctx.theme.semantic.border
        })));
    let details_inner_chunk = details_block.inner(details_text_chunk);
    f.render_widget(details_block, details_text_chunk);

    if let Some(panel) = critical_panel {
        let mut text_parts = panel.text.splitn(2, '\n');
        let headline = text_parts.next().unwrap_or_default();
        let body = text_parts
            .next()
            .unwrap_or_default()
            .trim_start_matches('\n');
        let critical_chunks = ratatui::layout::Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(details_inner_chunk);

        f.render_widget(
            Paragraph::new(headline).style(
                ctx.apply(
                    Style::default()
                        .fg(ctx.state_error())
                        .add_modifier(Modifier::BOLD),
                ),
            ),
            critical_chunks[0],
        );
        f.render_widget(
            Paragraph::new(body)
                .wrap(Wrap { trim: true })
                .style(ctx.apply(Style::default().fg(ctx.state_error()))),
            critical_chunks[2],
        );
        return;
    }

    let detail_rows = ratatui::layout::Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(details_inner_chunk);

    if let Some(torrent) = selected_torrent {
        let state = &torrent.latest_state;

        let progress_chunks =
            ratatui::layout::Layout::horizontal([Constraint::Length(11), Constraint::Min(0)])
                .split(detail_rows[0]);

        f.render_widget(Paragraph::new("Progress: "), progress_chunks[0]);

        let progress_pct = if state.torrent_control_state != TorrentControlState::Running {
            100.0
        } else {
            torrent_completion_percent(state)
        };
        let progress_ratio = (progress_pct / 100.0).clamp(0.0, 1.0);
        let progress_label_text = format!("{:.1}%", progress_pct);
        let line_gauge = LineGauge::default()
            .ratio(progress_ratio)
            .label(progress_label_text)
            .filled_symbol("⣿")
            .unfilled_symbol(symbols::line::THICK.horizontal)
            .filled_style(ctx.apply(Style::default().fg(ctx.state_success())));
        f.render_widget(line_gauge, progress_chunks[1]);

        let status_text = if state.activity_message.is_empty() {
            "Waiting..."
        } else {
            state.activity_message.as_str()
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "Status:   ",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                ),
                Span::raw(status_text),
            ])),
            detail_rows[1],
        );

        let total_pieces = state.number_of_pieces_total as usize;
        let (seeds, leeches) = state
            .peers
            .iter()
            .filter(|p| p.last_action != "Connecting...")
            .fold((0, 0), |(s, l), peer| {
                if total_pieces > 0 {
                    let pieces_have = peer
                        .bitfield
                        .iter()
                        .take(total_pieces)
                        .filter(|&&b| b)
                        .count();
                    if pieces_have == total_pieces {
                        (s + 1, l)
                    } else {
                        (s, l + 1)
                    }
                } else {
                    (s, l + 1)
                }
            });
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "Peers:    ",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                ),
                Span::raw(format!(
                    "{} (",
                    state.number_of_successfully_connected_peers
                )),
                Span::styled(
                    format!("{}", seeds),
                    ctx.apply(Style::default().fg(ctx.state_success())),
                ),
                Span::raw(" / "),
                Span::styled(
                    format!("{}", leeches),
                    ctx.apply(Style::default().fg(ctx.state_error())),
                ),
                Span::raw(")"),
            ])),
            detail_rows[2],
        );

        let written_size_spans = if state.number_of_pieces_completed < state.number_of_pieces_total
        {
            vec![
                Span::styled(
                    "Written:  ",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                ),
                Span::raw(format_bytes(state.bytes_written)),
                Span::raw(format!(" / {}", format_bytes(state.total_size))),
            ]
        } else {
            vec![
                Span::styled(
                    "Size:     ",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                ),
                Span::raw(format_bytes(state.total_size)),
            ]
        };
        f.render_widget(
            Paragraph::new(Line::from(written_size_spans)),
            detail_rows[3],
        );

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "Pieces:   ",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                ),
                Span::raw(format!(
                    "{}/{}",
                    state.number_of_pieces_completed, state.number_of_pieces_total
                )),
            ])),
            detail_rows[4],
        );

        let (eta_or_probe_label, eta_or_probe_value) = details_eta_or_probe_text(torrent);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    eta_or_probe_label,
                    ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                ),
                Span::raw(eta_or_probe_value),
            ])),
            detail_rows[5],
        );

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "Announce: ",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                ),
                Span::raw(format_countdown(state.next_announce_in)),
            ])),
            detail_rows[6],
        );
    } else {
        let placeholder_style = ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0));
        let label_style = ctx.apply(Style::default().fg(ctx.theme.semantic.surface2));

        let progress_chunks =
            ratatui::layout::Layout::horizontal([Constraint::Length(11), Constraint::Min(0)])
                .split(detail_rows[0]);
        f.render_widget(
            Paragraph::new("Progress: ").style(label_style),
            progress_chunks[0],
        );
        let line_gauge = LineGauge::default()
            .ratio(0.0)
            .label(" --.--%")
            .style(placeholder_style);
        f.render_widget(line_gauge, progress_chunks[1]);

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Status:   ", label_style),
                Span::styled("No Selection", placeholder_style),
            ])),
            detail_rows[1],
        );

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Peers:    ", label_style),
                Span::styled("- (- / -)", placeholder_style),
            ])),
            detail_rows[2],
        );

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Size:     ", label_style),
                Span::styled("- / -", placeholder_style),
            ])),
            detail_rows[3],
        );

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Pieces:   ", label_style),
                Span::styled("- / -", placeholder_style),
            ])),
            detail_rows[4],
        );

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("ETA:      ", label_style),
                Span::styled("--:--:--", placeholder_style),
            ])),
            detail_rows[5],
        );

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Announce: ", label_style),
                Span::styled("--s", placeholder_style),
            ])),
            detail_rows[6],
        );
    }
}

fn torrent_list_row_color(torrent: &TorrentDisplayState, ctx: &ThemeContext) -> Color {
    if !torrent.latest_state.data_available {
        ctx.state_error()
    } else {
        match torrent.latest_state.torrent_control_state {
            TorrentControlState::Running => ctx.theme.semantic.text,
            TorrentControlState::Paused => ctx.theme.semantic.surface1,
            TorrentControlState::Deleting => ctx.state_error(),
        }
    }
}

struct TorrentStatusCell {
    text: String,
    style: Style,
}

fn torrent_status_cell(torrent: &TorrentDisplayState, ctx: &ThemeContext) -> TorrentStatusCell {
    let state = &torrent.latest_state;
    let metadata_pending = matches!(
        torrent.latest_file_probe_status,
        Some(TorrentFileProbeStatus::PendingMetadata)
    ) || (state.number_of_pieces_total == 0
        && torrent_is_effectively_incomplete(state));

    if metadata_pending {
        return TorrentStatusCell {
            text: "Meta".to_string(),
            style: ctx.apply(Style::default().fg(ctx.state_warning())),
        };
    }

    if !state.data_available {
        return TorrentStatusCell {
            text: "Files".to_string(),
            style: ctx.apply(Style::default().fg(ctx.state_error())),
        };
    }

    TorrentStatusCell {
        text: format!("{:.1}%", torrent_completion_percent(state)),
        style: ctx.apply(Style::default().fg(torrent_list_row_color(torrent, ctx))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CriticalDetailsPanel {
    title: &'static str,
    text: String,
}

fn details_eta_or_probe_text(torrent: &TorrentDisplayState) -> (&'static str, String) {
    let state = &torrent.latest_state;
    if state.number_of_pieces_total > 0
        && state.number_of_pieces_completed >= state.number_of_pieces_total
    {
        (
            "Probe:    ",
            torrent
                .integrity_next_probe_in
                .map(format_countdown)
                .unwrap_or_else(|| "-".to_string()),
        )
    } else {
        ("ETA:      ", format_duration(state.eta))
    }
}

fn selected_torrent_critical_details(
    torrent: &TorrentDisplayState,
    anonymize_torrent_names: bool,
) -> Option<CriticalDetailsPanel> {
    if torrent.latest_state.data_available {
        return None;
    }

    let (issue_count, first_issue_path) = match &torrent.latest_file_probe_status {
        Some(TorrentFileProbeStatus::Files(files)) => (
            files.len(),
            files.first().map(|file| file.relative_path.clone()),
        ),
        _ => (0, None),
    };

    let saved_location = if let Some(download_path) = &torrent.latest_state.download_path {
        if let Some(container_name) = torrent.latest_state.container_name.as_deref() {
            if !container_name.is_empty() {
                Some(download_path.join(container_name))
            } else {
                Some(download_path.clone())
            }
        } else {
            Some(download_path.clone())
        }
    } else {
        None
    };

    let display_path = if anonymize_torrent_names {
        "/path/to/torrent/file".to_string()
    } else {
        match (saved_location, first_issue_path) {
            (Some(saved_location), Some(first_issue_path)) => {
                saved_location.join(first_issue_path).display().to_string()
            }
            (Some(saved_location), None) => saved_location.display().to_string(),
            (None, Some(first_issue_path)) => first_issue_path.display().to_string(),
            (None, None) => "-".to_string(),
        }
    };

    Some(CriticalDetailsPanel {
        title: "Critical",
        text: format!(
            "DATA UNAVAILABLE ({})\nFiles Check: {}\n\n{}",
            issue_count,
            torrent
                .integrity_next_probe_in
                .map(format_countdown)
                .unwrap_or_else(|| "-".to_string()),
            display_path
        ),
    })
}

pub fn draw_network_chart(
    f: &mut Frame,
    app_state: &AppState,
    chart_chunk: Rect,
    ctx: &ThemeContext,
) {
    if chart_chunk.width < 5 || chart_chunk.height < 5 {
        return;
    }

    let smooth_data = |data: &[u64], alpha: f64| -> Vec<u64> {
        if data.is_empty() {
            return Vec::new();
        }
        let mut smoothed_data = Vec::with_capacity(data.len());
        let mut last_ema = data[0] as f64;
        smoothed_data.push(last_ema as u64);
        for &value in data.iter().skip(1) {
            let current_ema = (value as f64 * alpha) + (last_ema * (1.0 - alpha));
            smoothed_data.push(current_ema as u64);
            last_ema = current_ema;
        }
        smoothed_data
    };
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (points_to_show, step_secs, tier) = graph_window_spec(app_state.graph_mode);
    let smoothing_period = 5.0;
    let alpha = 2.0 / (smoothing_period + 1.0);

    let mut dataset_specs: Vec<(String, Color, bool, Option<ratatui::widgets::GraphType>)> =
        Vec::new();
    let mut dataset_data: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut y_axis_upper: f64;
    let y_axis_labels: Vec<Span>;

    match app_state.chart_panel_view {
        ChartPanelView::Network => {
            let source_points = network_points_for_tier(app_state, tier);
            let (dl_history_slice, ul_history_slice, backoff_history_relevant_ms) =
                build_time_aligned_window(source_points, step_secs, points_to_show, now_unix);
            let smoothed_dl_data = smooth_data(&dl_history_slice, alpha);
            let smoothed_ul_data = smooth_data(&ul_history_slice, alpha);
            let displayed_max_speed = smoothed_dl_data
                .iter()
                .chain(smoothed_ul_data.iter())
                .max()
                .copied()
                .unwrap_or(0);
            let nice_max_speed = speed_chart_upper_bound(displayed_max_speed);
            y_axis_upper = nice_max_speed as f64;
            y_axis_labels = vec![
                Span::raw("0"),
                Span::styled(
                    format_speed(nice_max_speed / 2),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
                Span::styled(
                    format_speed(nice_max_speed),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
            ];

            let dl_data: Vec<(f64, f64)> = smoothed_dl_data
                .iter()
                .enumerate()
                .map(|(i, &s)| (i as f64, s as f64))
                .collect();
            let ul_data: Vec<(f64, f64)> = smoothed_ul_data
                .iter()
                .enumerate()
                .map(|(i, &s)| (i as f64, s as f64))
                .collect();
            dataset_data.push(dl_data);
            dataset_specs.push((
                "Download".to_string(),
                ctx.state_info(),
                true,
                Some(ratatui::widgets::GraphType::Line),
            ));
            dataset_data.push(ul_data);
            dataset_specs.push((
                "Upload".to_string(),
                ctx.state_success(),
                true,
                Some(ratatui::widgets::GraphType::Line),
            ));

            let backoff_marker_data: Vec<(f64, f64)> = backoff_history_relevant_ms
                .iter()
                .enumerate()
                .filter_map(|(i, &ms)| {
                    if ms > 0 {
                        Some((
                            i as f64,
                            smoothed_dl_data.get(i).copied().unwrap_or(0) as f64,
                        ))
                    } else {
                        None
                    }
                })
                .collect();
            dataset_data.push(backoff_marker_data);
            dataset_specs.push((
                "File Limits".to_string(),
                ctx.state_error(),
                true,
                Some(ratatui::widgets::GraphType::Scatter),
            ));
        }
        ChartPanelView::Cpu => {
            let points = activity_points_for_tier(&app_state.activity_history_state.cpu, tier);
            let (cpu_x10, _) =
                build_time_aligned_pair_window(points, step_secs, points_to_show, now_unix);
            let smoothed = smooth_data(&cpu_x10, alpha);
            let cpu_data: Vec<(f64, f64)> = smoothed
                .iter()
                .enumerate()
                .map(|(i, &v)| (i as f64, v as f64 / 10.0))
                .collect();
            dataset_data.push(cpu_data);
            dataset_specs.push((
                "CPU".to_string(),
                ctx.state_error(),
                true,
                Some(ratatui::widgets::GraphType::Line),
            ));
            y_axis_upper = 100.0;
            y_axis_labels = vec![
                Span::raw("0%"),
                Span::styled(
                    "50%",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
                Span::styled(
                    "100%",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
            ];
        }
        ChartPanelView::Ram => {
            let points = activity_points_for_tier(&app_state.activity_history_state.ram, tier);
            let (ram_x10, _) =
                build_time_aligned_pair_window(points, step_secs, points_to_show, now_unix);
            let smoothed = smooth_data(&ram_x10, alpha);
            let ram_data: Vec<(f64, f64)> = smoothed
                .iter()
                .enumerate()
                .map(|(i, &v)| (i as f64, v as f64 / 10.0))
                .collect();
            dataset_data.push(ram_data);
            dataset_specs.push((
                "RAM".to_string(),
                ctx.state_warning(),
                true,
                Some(ratatui::widgets::GraphType::Line),
            ));
            y_axis_upper = 100.0;
            y_axis_labels = vec![
                Span::raw("0%"),
                Span::styled(
                    "50%",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
                Span::styled(
                    "100%",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
            ];
        }
        ChartPanelView::Disk => {
            let points = activity_points_for_tier(&app_state.activity_history_state.disk, tier);
            let (read_bps, write_bps) =
                build_time_aligned_pair_window(points, step_secs, points_to_show, now_unix);
            let smoothed_read = smooth_data(&read_bps, alpha);
            let smoothed_write = smooth_data(&write_bps, alpha);
            let displayed_max_speed = smoothed_read
                .iter()
                .chain(smoothed_write.iter())
                .max()
                .copied()
                .unwrap_or(0);
            let nice_max_speed = speed_chart_upper_bound(displayed_max_speed);
            y_axis_upper = nice_max_speed as f64;
            y_axis_labels = vec![
                Span::raw("0"),
                Span::styled(
                    format_speed(nice_max_speed / 2),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
                Span::styled(
                    format_speed(nice_max_speed),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
            ];

            let read_data: Vec<(f64, f64)> = smoothed_read
                .iter()
                .enumerate()
                .map(|(i, &v)| (i as f64, v as f64))
                .collect();
            let write_data: Vec<(f64, f64)> = smoothed_write
                .iter()
                .enumerate()
                .map(|(i, &v)| (i as f64, v as f64))
                .collect();
            if disk_series_draw_read_last(&smoothed_read, &smoothed_write) {
                dataset_data.push(write_data);
                dataset_specs.push((
                    "Write".to_string(),
                    ctx.accent_sky(),
                    true,
                    Some(ratatui::widgets::GraphType::Line),
                ));
                dataset_data.push(read_data);
                dataset_specs.push((
                    "Read".to_string(),
                    ctx.state_success(),
                    true,
                    Some(ratatui::widgets::GraphType::Line),
                ));
            } else {
                dataset_data.push(read_data);
                dataset_specs.push((
                    "Read".to_string(),
                    ctx.state_success(),
                    true,
                    Some(ratatui::widgets::GraphType::Line),
                ));
                dataset_data.push(write_data);
                dataset_specs.push((
                    "Write".to_string(),
                    ctx.accent_sky(),
                    true,
                    Some(ratatui::widgets::GraphType::Line),
                ));
            }
        }
        ChartPanelView::Tuning => {
            let points = activity_points_for_tier(&app_state.activity_history_state.tuning, tier);
            let (current_series, best_series) =
                build_time_aligned_pair_window(points, step_secs, points_to_show, now_unix);
            let stable_max = current_series
                .iter()
                .chain(best_series.iter())
                .max()
                .copied()
                .unwrap_or(1)
                .max(1);
            y_axis_upper = calculate_nice_upper_bound(stable_max) as f64;
            y_axis_labels = vec![
                Span::raw("0"),
                Span::styled(
                    (y_axis_upper as u64 / 2).to_string(),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
                Span::styled(
                    (y_axis_upper as u64).to_string(),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
            ];

            let current_data: Vec<(f64, f64)> = current_series
                .iter()
                .enumerate()
                .map(|(i, &v)| (i as f64, v as f64))
                .collect();
            let best_data: Vec<(f64, f64)> = best_series
                .iter()
                .enumerate()
                .map(|(i, &v)| (i as f64, v as f64))
                .collect();
            dataset_data.push(current_data);
            dataset_specs.push((
                "Current".to_string(),
                ctx.theme.semantic.text,
                true,
                Some(ratatui::widgets::GraphType::Line),
            ));
            dataset_data.push(best_data);
            dataset_specs.push((
                "Best".to_string(),
                ctx.state_success(),
                false,
                Some(ratatui::widgets::GraphType::Line),
            ));
        }
        ChartPanelView::TorrentOverlay => {
            let selected_hash = app_state
                .torrent_list_order
                .get(app_state.ui.selected_torrent_index)
                .cloned();
            let mut max_overlay_speed = 1_u64;

            if let Some(info_hash) = selected_hash {
                let key = hex::encode(&info_hash);
                let points = app_state
                    .activity_history_state
                    .torrents
                    .get(&key)
                    .map(|series| activity_points_for_tier(series, tier))
                    .unwrap_or(&[]);
                let (dl_hist, ul_hist) =
                    build_time_aligned_pair_window(points, step_secs, points_to_show, now_unix);
                let net_hist: Vec<u64> = dl_hist
                    .iter()
                    .zip(ul_hist.iter())
                    .map(|(dl, ul)| dl.saturating_add(*ul))
                    .collect();
                let smoothed = smooth_data(&net_hist, alpha);
                max_overlay_speed =
                    max_overlay_speed.max(smoothed.iter().copied().max().unwrap_or(0));
                dataset_data.push(
                    smoothed
                        .iter()
                        .enumerate()
                        .map(|(i, &v)| (i as f64, v as f64))
                        .collect(),
                );
                dataset_specs.push((
                    torrent_activity_label(app_state, &info_hash),
                    ctx.state_info(),
                    true,
                    Some(ratatui::widgets::GraphType::Line),
                ));
            }

            let nice_max_speed = speed_chart_upper_bound(max_overlay_speed);
            y_axis_upper = nice_max_speed as f64;
            y_axis_labels = vec![
                Span::raw("0"),
                Span::styled(
                    format_speed(nice_max_speed / 2),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
                Span::styled(
                    format_speed(nice_max_speed),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
            ];
        }
        ChartPanelView::MultiTorrentOverlay => {
            let mut ranked: Vec<(Vec<u8>, u64, u64)> = app_state
                .torrent_list_order
                .iter()
                .map(|info_hash| {
                    (
                        info_hash.clone(),
                        torrent_current_traffic(
                            app_state,
                            info_hash,
                            tier,
                            step_secs,
                            points_to_show,
                            now_unix,
                            alpha,
                        ),
                        torrent_period_traffic(
                            app_state,
                            info_hash,
                            tier,
                            step_secs,
                            points_to_show,
                            now_unix,
                        ),
                    )
                })
                .filter(|(_, _, period_total)| *period_total > 0)
                .collect();
            ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.2.cmp(&a.2)));

            let mut chosen_hashes: Vec<Vec<u8>> = ranked
                .into_iter()
                .take(5)
                .map(|(hash, _, _)| hash)
                .collect();

            let mut seen = HashSet::new();
            chosen_hashes.retain(|hash| seen.insert(hash.clone()));
            chosen_hashes.sort_by(|a, b| {
                torrent_current_traffic(
                    app_state,
                    b,
                    tier,
                    step_secs,
                    points_to_show,
                    now_unix,
                    alpha,
                )
                .cmp(&torrent_current_traffic(
                    app_state,
                    a,
                    tier,
                    step_secs,
                    points_to_show,
                    now_unix,
                    alpha,
                ))
                .then_with(|| {
                    torrent_period_traffic(app_state, b, tier, step_secs, points_to_show, now_unix)
                        .cmp(&torrent_period_traffic(
                            app_state,
                            a,
                            tier,
                            step_secs,
                            points_to_show,
                            now_unix,
                        ))
                })
            });

            let palette = [
                ctx.state_info(),
                ctx.state_success(),
                ctx.state_warning(),
                ctx.accent_teal(),
                ctx.accent_sapphire(),
                ctx.accent_sky(),
                ctx.accent_peach(),
                ctx.accent_maroon(),
                ctx.state_selected(),
                ctx.theme.semantic.text,
            ];

            let mut max_overlay_speed = 1_u64;
            for info_hash in chosen_hashes {
                let key = hex::encode(&info_hash);
                let points = app_state
                    .activity_history_state
                    .torrents
                    .get(&key)
                    .map(|series| activity_points_for_tier(series, tier))
                    .unwrap_or(&[]);
                let (dl_hist, ul_hist) =
                    build_time_aligned_pair_window(points, step_secs, points_to_show, now_unix);
                let base_idx = info_hash.iter().fold(0_u64, |acc, b| {
                    acc.wrapping_mul(131).wrapping_add(*b as u64)
                }) as usize;
                let color = palette[base_idx % palette.len()];
                let label = torrent_activity_label(app_state, &info_hash);

                let net_hist: Vec<u64> = dl_hist
                    .iter()
                    .zip(ul_hist.iter())
                    .map(|(dl, ul)| dl.saturating_add(*ul))
                    .collect();
                let smoothed = smooth_data(&net_hist, alpha);
                max_overlay_speed =
                    max_overlay_speed.max(smoothed.iter().copied().max().unwrap_or(0));
                let data: Vec<(f64, f64)> = smoothed
                    .iter()
                    .enumerate()
                    .map(|(i, &v)| (i as f64, v as f64))
                    .collect();
                dataset_data.push(data);
                dataset_specs.push((label, color, true, Some(ratatui::widgets::GraphType::Line)));
            }

            let nice_max_speed = speed_chart_upper_bound(max_overlay_speed);
            y_axis_upper = nice_max_speed as f64;
            y_axis_labels = vec![
                Span::raw("0"),
                Span::styled(
                    format_speed(nice_max_speed / 2),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
                Span::styled(
                    format_speed(nice_max_speed),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
            ];
        }
    }

    if y_axis_upper < 1.0 {
        y_axis_upper = 1.0;
    }

    let mut datasets: Vec<ratatui::widgets::Dataset> = Vec::with_capacity(dataset_specs.len());
    for (idx, (name, color, emphasize, graph_type)) in dataset_specs.iter().enumerate() {
        let mut style = Style::default().fg(*color);
        if *emphasize {
            style = style.add_modifier(Modifier::BOLD);
        }
        let mut dataset = ratatui::widgets::Dataset::default()
            .name(name.clone())
            .marker(ratatui::symbols::Marker::Braille)
            .style(ctx.apply(style))
            .data(&dataset_data[idx]);
        if let Some(graph_type) = graph_type {
            dataset = dataset.graph_type(*graph_type);
        }
        datasets.push(dataset);
    }

    let x_labels = generate_x_axis_labels(ctx, app_state.graph_mode);

    let all_views = [
        ChartPanelView::Network,
        ChartPanelView::Cpu,
        ChartPanelView::Ram,
        ChartPanelView::Disk,
        ChartPanelView::Tuning,
        ChartPanelView::TorrentOverlay,
        ChartPanelView::MultiTorrentOverlay,
    ];
    let all_modes = [
        GraphDisplayMode::OneMinute,
        GraphDisplayMode::FiveMinutes,
        GraphDisplayMode::TenMinutes,
        GraphDisplayMode::ThirtyMinutes,
        GraphDisplayMode::OneHour,
        GraphDisplayMode::ThreeHours,
        GraphDisplayMode::TwelveHours,
        GraphDisplayMode::TwentyFourHours,
        GraphDisplayMode::SevenDays,
        GraphDisplayMode::ThirtyDays,
        GraphDisplayMode::OneYear,
    ];
    let view_labels: Vec<&str> = all_views.iter().map(|view| view.to_string()).collect();
    let mode_labels: Vec<&str> = all_modes.iter().map(|mode| mode.to_string()).collect();
    let full_title_width = "Activity ".len()
        + selector_content_width(&view_labels)
        + " | ".len()
        + selector_content_width(&mode_labels);
    let available_title_width = chart_chunk.width.saturating_sub(2) as usize;
    let use_compact_title = full_title_width > available_title_width;
    let active_view_idx = all_views
        .iter()
        .position(|view| *view == app_state.chart_panel_view)
        .unwrap_or(0);
    let active_mode_idx = all_modes
        .iter()
        .position(|mode| *mode == app_state.graph_mode)
        .unwrap_or(0);

    let mut title_spans: Vec<Span> = vec![Span::styled(
        "Activity ",
        ctx.apply(Style::default().fg(ctx.accent_peach())),
    )];
    title_spans.extend(build_selector_spans(
        ctx,
        &view_labels,
        active_view_idx,
        use_compact_title,
    ));
    title_spans.push(Span::styled(
        " | ",
        ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
    ));
    title_spans.extend(build_selector_spans(
        ctx,
        &mode_labels,
        active_mode_idx,
        use_compact_title,
    ));
    let chart_title = Line::from(title_spans);

    let chart = ratatui::widgets::Chart::new(datasets)
        .block(
            Block::default()
                .title(chart_title)
                .borders(Borders::ALL)
                .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border))),
        )
        .x_axis(
            ratatui::widgets::Axis::default()
                .style(ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)))
                .bounds([0.0, points_to_show.saturating_sub(1) as f64])
                .labels(x_labels),
        )
        .y_axis(
            ratatui::widgets::Axis::default()
                .style(ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)))
                .bounds([0.0, y_axis_upper])
                .labels(y_axis_labels),
        )
        .hidden_legend_constraints(chart_hidden_legend_constraints(app_state.chart_panel_view))
        .legend_position(chart_legend_position(app_state.chart_panel_view));

    f.render_widget(chart, chart_chunk);
}

pub fn draw_stats_panel(
    f: &mut Frame,
    app_state: &AppState,
    settings: &Settings,
    stats_chunk: Rect,
    ctx: &ThemeContext,
) {
    let total_peers = app_state
        .torrents
        .values()
        .map(|t| t.latest_state.number_of_successfully_connected_peers)
        .sum::<usize>();

    let total_library_size: u64 = app_state
        .torrents
        .values()
        .map(|t| t.latest_state.total_size)
        .sum();

    let dl_speed = *app_state.avg_download_history.last().unwrap_or(&0);
    let dl_limit = app_state.effective_download_limit_bps;
    let dl_auto_limited = auto_download_limit_applied(settings.global_download_limit_bps, dl_limit);

    let dl_spans = build_limit_value_spans(
        ctx,
        "DL Speed: ".to_string(),
        format_speed(dl_speed),
        format_limit_bps(dl_limit),
        ctx.metric_download(),
        dl_auto_limited || (dl_limit > 0 && dl_speed >= dl_limit),
    );

    let ul_speed = *app_state.avg_upload_history.last().unwrap_or(&0);
    let ul_limit = settings.global_upload_limit_bps;
    let peer_slot_limit = app_state
        .active_peer_limit
        .unwrap_or(app_state.limits.max_connected_peers);
    let tuning_paused = app_state.active_peer_limit.is_some();
    let limiter_held_peer_permits = app_state
        .limits
        .max_connected_peers
        .saturating_sub(peer_slot_limit);
    let displayed_reserve_slots = app_state
        .limits
        .reserve_permits
        .saturating_add(limiter_held_peer_permits);

    let ul_spans = build_limit_value_spans(
        ctx,
        "UL Speed: ".to_string(),
        format_speed(ul_speed),
        format_limit_bps(ul_limit),
        ctx.metric_upload(),
        ul_limit > 0 && ul_speed >= ul_limit,
    );

    let thrash_value_text: String;
    let thrash_delta_text: String;
    let thrash_delta_style: Style;
    let baseline_val = app_state.adaptive_max_scpb;
    let thrash_score_val = app_state.global_disk_thrash_score;
    let thrash_score_str = format!("{:.0}", thrash_score_val);

    if thrash_score_val < 0.01 {
        thrash_value_text = "0".to_string();
        thrash_delta_text = "(0%)".to_string();
        thrash_delta_style = ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0));
    } else if baseline_val == 0.0 {
        thrash_value_text = thrash_score_str;
        thrash_delta_text = "(∞%)".to_string();
        thrash_delta_style = ctx.apply(Style::default().fg(ctx.state_error())).bold();
    } else {
        let diff = thrash_score_val - baseline_val;
        let thrash_percentage = (diff / baseline_val) * 100.0;
        let thrash_pct_display = if thrash_percentage.abs() < 0.5 {
            "0%".to_string()
        } else {
            format!("{:.0}%", thrash_percentage)
        };
        thrash_value_text = thrash_score_str;

        if thrash_percentage > -0.01 && thrash_percentage < 0.01 {
            thrash_delta_text = "(0%)".to_string();
            thrash_delta_style = ctx.apply(Style::default().fg(ctx.theme.semantic.text));
        } else {
            thrash_delta_text = format!("({})", thrash_pct_display);
            if thrash_percentage > 15.0 {
                thrash_delta_style = ctx.apply(Style::default().fg(ctx.state_error())).bold();
            } else if thrash_percentage > 0.0 {
                thrash_delta_style = ctx.apply(Style::default().fg(ctx.state_warning()));
            } else {
                thrash_delta_style = ctx.apply(Style::default().fg(ctx.state_success()));
            }
        }
    }

    let tune_delta_pct = if tuning_paused {
        None
    } else if app_state.last_tuning_score > 0 {
        let best = app_state.last_tuning_score as f64;
        let current = app_state.current_tuning_score as f64;
        Some(((current - best) / best) * 100.0)
    } else {
        Some(0.0)
    };
    let tune_header = if tuning_paused {
        "Self-Tune(0s): ".to_string()
    } else {
        format!("Self-Tune({}s): ", app_state.tuning_countdown)
    };
    let tune_value_text = if tuning_paused {
        "paused".to_string()
    } else {
        app_state.current_tuning_score.to_string()
    };
    let tune_value_style = if tuning_paused {
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.text))
    };
    let stats_text = vec![
        Line::from(vec![
            Span::styled(
                "Run Time: ",
                ctx.apply(Style::default().fg(ctx.accent_teal())),
            ),
            Span::styled(
                format_time(app_state.run_time),
                ctx.apply(Style::default().fg(ctx.accent_teal())),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                "RSS Sync: ",
                ctx.apply(Style::default().fg(ctx.accent_sapphire())),
            ),
            Span::styled(
                app_state
                    .rss_runtime
                    .next_sync_at
                    .as_deref()
                    .and_then(rss_sync_countdown_label)
                    .unwrap_or_else(|| "-".to_string()),
                ctx.apply(Style::default().fg(ctx.accent_sapphire())),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                "Torrents: ",
                ctx.apply(Style::default().fg(ctx.accent_peach())),
            ),
            Span::styled(
                format!(
                    "{} ({})",
                    app_state.torrents.len(),
                    format_bytes(total_library_size)
                ),
                ctx.apply(Style::default().fg(ctx.accent_peach())),
            ),
        ]),
        Line::from(""),
        Line::from(dl_spans),
        Line::from(vec![
            Span::styled(
                "Session DL: ",
                ctx.apply(Style::default().fg(ctx.accent_sky())),
            ),
            Span::styled(
                format_bytes(app_state.session_total_downloaded),
                ctx.apply(Style::default().fg(ctx.accent_sky())),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                "Lifetime DL: ",
                ctx.apply(Style::default().fg(ctx.accent_sky())),
            ),
            Span::styled(
                format_bytes(
                    app_state.lifetime_downloaded_from_config + app_state.session_total_downloaded,
                ),
                ctx.apply(Style::default().fg(ctx.accent_sky())),
            ),
        ]),
        Line::from(""),
        Line::from(ul_spans),
        Line::from(vec![
            Span::styled(
                "Session UL: ",
                ctx.apply(Style::default().fg(ctx.state_success())),
            ),
            Span::styled(
                format_bytes(app_state.session_total_uploaded),
                ctx.apply(Style::default().fg(ctx.state_success())),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                "Lifetime UL: ",
                ctx.apply(Style::default().fg(ctx.state_success())),
            ),
            Span::styled(
                format_bytes(
                    app_state.lifetime_uploaded_from_config + app_state.session_total_uploaded,
                ),
                ctx.apply(Style::default().fg(ctx.state_success())),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("CPU: ", ctx.apply(Style::default().fg(ctx.state_error()))),
            Span::styled(
                format!("{:.1}%", app_state.cpu_usage),
                ctx.apply(Style::default().fg(ctx.state_error())),
            ),
        ]),
        Line::from(vec![
            Span::styled("RAM: ", ctx.apply(Style::default().fg(ctx.state_warning()))),
            Span::styled(
                format!(
                    "{:.1}% ({})",
                    app_state.ram_usage_percent,
                    format_memory(app_state.app_ram_usage)
                ),
                ctx.apply(Style::default().fg(ctx.state_warning())),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                "Disk    ",
                ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
            ),
            Span::styled("↑ ", ctx.apply(Style::default().fg(ctx.state_success()))),
            Span::styled(
                format!("{:<12}", format_speed(app_state.avg_disk_read_bps)),
                ctx.apply(Style::default().fg(ctx.state_success())),
            ),
            Span::styled("↓ ", ctx.apply(Style::default().fg(ctx.accent_sky()))),
            Span::styled(
                format_speed(app_state.avg_disk_write_bps),
                ctx.apply(Style::default().fg(ctx.accent_sky())),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                "Seek    ",
                ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
            ),
            Span::styled("↑ ", ctx.apply(Style::default().fg(ctx.state_success()))),
            Span::styled(
                format!(
                    "{:<12}",
                    format_bytes(app_state.global_disk_read_thrash_score)
                ),
                ctx.apply(Style::default().fg(ctx.state_success())),
            ),
            Span::styled("↓ ", ctx.apply(Style::default().fg(ctx.accent_sky()))),
            Span::styled(
                format_bytes(app_state.global_disk_write_thrash_score),
                ctx.apply(Style::default().fg(ctx.accent_sky())),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                "Latency ",
                ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
            ),
            Span::styled("↑ ", ctx.apply(Style::default().fg(ctx.state_success()))),
            Span::styled(
                format!("{:<12}", format_latency(app_state.avg_disk_read_latency)),
                ctx.apply(Style::default().fg(ctx.state_success())),
            ),
            Span::styled("↓ ", ctx.apply(Style::default().fg(ctx.accent_sky()))),
            Span::styled(
                format_latency(app_state.avg_disk_write_latency),
                ctx.apply(Style::default().fg(ctx.accent_sky())),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                "IOPS    ",
                ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
            ),
            Span::styled("↑ ", ctx.apply(Style::default().fg(ctx.state_success()))),
            Span::styled(
                format!("{:<12}", format_iops(app_state.read_iops)),
                ctx.apply(Style::default().fg(ctx.state_success())),
            ),
            Span::styled("↓ ", ctx.apply(Style::default().fg(ctx.accent_sky()))),
            Span::styled(
                format_iops(app_state.write_iops),
                ctx.apply(Style::default().fg(ctx.accent_sky())),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                tune_header,
                ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
            ),
            Span::styled(tune_value_text, tune_value_style),
            if let Some(delta_pct) = tune_delta_pct {
                let delta_style = if delta_pct > 0.0 {
                    ctx.apply(Style::default().fg(ctx.state_success()))
                } else if delta_pct < 0.0 {
                    ctx.apply(Style::default().fg(ctx.state_error()))
                } else {
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))
                };
                Span::styled(format!(" ({:+.0}%)", delta_pct), delta_style)
            } else {
                Span::raw("")
            },
        ]),
        Line::from(vec![
            Span::styled(
                "Disk Thrash: ",
                ctx.apply(Style::default().fg(ctx.accent_teal())),
            ),
            Span::raw(format!("{} ", thrash_value_text)),
            Span::styled(thrash_delta_text, thrash_delta_style),
        ]),
        build_tuning_numeric_line(
            ctx,
            "Reserve Slots:",
            displayed_reserve_slots,
            app_state.last_tuning_limits.reserve_permits,
            ctx.accent_teal(),
            tuning_paused,
        ),
        build_tuning_peer_line(
            ctx,
            total_peers,
            peer_slot_limit,
            app_state.limits.max_connected_peers,
            app_state.last_tuning_limits.max_connected_peers,
            tuning_paused,
        ),
        build_tuning_numeric_line(
            ctx,
            "Read Slots:",
            app_state.limits.disk_read_permits,
            app_state.last_tuning_limits.disk_read_permits,
            ctx.state_success(),
            tuning_paused,
        ),
        build_tuning_numeric_line(
            ctx,
            "Write Slots:",
            app_state.limits.disk_write_permits,
            app_state.last_tuning_limits.disk_write_permits,
            ctx.accent_sky(),
            tuning_paused,
        ),
    ];

    let (lvl, progress) = crate::tui::render::calculate_player_stats(app_state);
    let available_width = stats_chunk.width.saturating_sub(18) as usize;

    let (gauge_width, show_pct) = if available_width > 25 {
        (20, true)
    } else if available_width > 15 {
        (10, true)
    } else {
        (10, false)
    };

    let filled_len = (progress * gauge_width as f64).round() as usize;
    let empty_len = gauge_width - filled_len;
    let gauge_str = format!("[{}{}]", "=".repeat(filled_len), "-".repeat(empty_len));

    let mut title_spans = vec![
        Span::styled(
            "Stats",
            ctx.apply(Style::default().fg(ctx.theme.semantic.white)),
        ),
        Span::raw(" | "),
        Span::styled(
            format!("Lvl {}", lvl),
            ctx.apply(Style::default().fg(ctx.state_warning()).bold()),
        ),
        Span::raw(" "),
        Span::styled(
            gauge_str,
            ctx.apply(Style::default().fg(ctx.state_success())),
        ),
    ];

    if show_pct {
        title_spans.push(Span::styled(
            format!(" {:.0}%", progress * 100.0),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ));
    }

    let stats_paragraph = Paragraph::new(stats_text)
        .block(
            Block::default()
                .title(Line::from(title_spans))
                .borders(Borders::ALL)
                .borders(Borders::ALL)
                .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border))),
        )
        .style(ctx.apply(Style::default().fg(ctx.theme.semantic.text)));

    f.render_widget(stats_paragraph, stats_chunk);
}

fn build_tuning_numeric_line(
    ctx: &ThemeContext,
    label: &str,
    current: usize,
    last: usize,
    label_color: Color,
    tuning_paused: bool,
) -> Line<'static> {
    let delta = current as isize - last as isize;
    let delta_style = if tuning_paused {
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))
    } else if delta > 0 {
        ctx.apply(Style::default().fg(ctx.state_success()))
    } else if delta < 0 {
        ctx.apply(Style::default().fg(ctx.state_error()))
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))
    };
    let delta_text = if tuning_paused {
        " (held)".to_string()
    } else if delta > 0 {
        format!(" (+{})", delta)
    } else if delta < 0 {
        format!(" ({})", delta)
    } else {
        String::new()
    };
    Line::from(vec![
        Span::styled(
            format!("{:<TUNING_LABEL_WIDTH$}", label),
            ctx.apply(Style::default().fg(label_color)),
        ),
        Span::raw(" "),
        Span::raw(current.to_string()),
        Span::styled(delta_text, delta_style),
    ])
}

fn build_limit_value_spans(
    ctx: &ThemeContext,
    label: String,
    value: String,
    limit: String,
    value_color: Color,
    limit_is_hot: bool,
) -> Vec<Span<'static>> {
    let value_style = ctx.apply(Style::default().fg(value_color).bold());
    let limit_style = if limit_is_hot {
        ctx.apply(Style::default().fg(ctx.state_error()))
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))
    };
    vec![
        Span::styled(label, value_style),
        Span::styled(value, value_style),
        Span::raw(" / "),
        Span::styled(limit, limit_style),
    ]
}

fn build_tuning_peer_line(
    ctx: &ThemeContext,
    used: usize,
    displayed_limit: usize,
    current_limit: usize,
    last_limit: usize,
    tuning_paused: bool,
) -> Line<'static> {
    let delta = current_limit as isize - last_limit as isize;
    let delta_style = if tuning_paused {
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))
    } else if delta > 0 {
        ctx.apply(Style::default().fg(ctx.state_success()))
    } else if delta < 0 {
        ctx.apply(Style::default().fg(ctx.state_error()))
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))
    };
    let delta_text = if tuning_paused {
        " (held)".to_string()
    } else if delta > 0 {
        format!(" (+{})", delta)
    } else if delta < 0 {
        format!(" ({})", delta)
    } else {
        String::new()
    };
    let mut spans = build_limit_value_spans(
        ctx,
        format!("{:<TUNING_LABEL_WIDTH$} ", "Peer Slots:"),
        used.to_string(),
        displayed_limit.to_string(),
        ctx.state_selected(),
        displayed_limit < current_limit
            || (displayed_limit > 0 && used >= displayed_limit)
            || (displayed_limit == 0 && used > 0),
    );
    spans.push(Span::styled(delta_text, delta_style));
    Line::from(spans)
}

fn rss_sync_countdown_label(next_sync_at: &str) -> Option<String> {
    let next_sync = DateTime::parse_from_rfc3339(next_sync_at).ok()?;
    let remaining_secs = next_sync
        .with_timezone(&Utc)
        .signed_duration_since(Utc::now())
        .num_seconds();
    if remaining_secs <= 0 {
        return None;
    }

    let hours = remaining_secs / 3600;
    let minutes = (remaining_secs % 3600) / 60;
    let seconds = remaining_secs % 60;
    let label = if hours > 0 {
        format!("{}h {}m {}s", hours, minutes, seconds)
    } else if minutes > 0 {
        format!("{}m {}s", minutes, seconds)
    } else {
        format!("{}s", seconds)
    };
    Some(label)
}

fn peer_stream_smoothed_activity(data_slice: &[u64], i: usize) -> f64 {
    let current = data_slice.get(i).copied().unwrap_or(0) as f64;
    let prev = if i > 0 {
        data_slice.get(i - 1).copied().unwrap_or(0) as f64
    } else {
        current
    };
    let next = data_slice.get(i + 1).copied().unwrap_or(0) as f64;
    (prev * 0.25) + (current * 0.5) + (next * 0.25)
}

fn peer_stream_wave_amplitude(smoothed_activity: f64) -> f64 {
    let min_amp = 0.10;
    let max_amp = 0.28;
    let normalized = (smoothed_activity / 10.0).clamp(0.0, 1.0);
    min_amp + (max_amp - min_amp) * normalized
}

#[derive(Debug, PartialEq, Eq)]
struct NetworkInterruption {
    title: &'static str,
    blocked: bool,
    interface: String,
    reason: String,
}

fn network_interruption(app_state: &AppState) -> Option<NetworkInterruption> {
    let runtime_status = app_state.network_runtime_status.as_ref();
    let interface = if app_state.anonymize_torrent_names {
        "Network interface"
    } else {
        runtime_status
            .and_then(|status| {
                status
                    .interface_display_name
                    .as_deref()
                    .or(status.interface.as_deref())
            })
            .unwrap_or("OS selected")
    };

    match app_state.network_activation_status.as_ref() {
        Some(NetworkActivationStatus::Active { .. }) => None,
        Some(NetworkActivationStatus::Pending { .. }) => Some(NetworkInterruption {
            title: "Network Rebinding",
            blocked: false,
            interface: sanitize_text(interface),
            reason: "Switching network".to_string(),
        }),
        Some(NetworkActivationStatus::Blocked { reason }) => Some(NetworkInterruption {
            title: "Network Blocked",
            blocked: true,
            interface: sanitize_text(interface),
            reason: if app_state.anonymize_torrent_names {
                "Interface unavailable".to_string()
            } else {
                concise_network_failure(reason)
            },
        }),
        None => runtime_status
            .filter(|status| status.phase == NetworkRuntimePhase::Blocked)
            .map(|status| NetworkInterruption {
                title: "Network Blocked",
                blocked: true,
                interface: sanitize_text(interface),
                reason: if app_state.anonymize_torrent_names {
                    "Interface unavailable".to_string()
                } else {
                    concise_network_failure(
                        status
                            .blocked_reason
                            .as_deref()
                            .unwrap_or("No usable network generation is available."),
                    )
                },
            }),
    }
}

fn concise_network_failure(reason: &str) -> String {
    let reason = sanitize_text(reason);
    let lower = reason.to_ascii_lowercase();
    if lower.contains("interface ")
        && (lower.contains(" was not found") || lower.contains(" is not currently usable"))
    {
        return "Interface unavailable".to_string();
    }
    if lower.contains("has no ipv4 address") {
        return "No IPv4 address".to_string();
    }
    if lower.contains("has no ipv6 address") {
        return "No IPv6 address".to_string();
    }
    if lower.contains("permission denied") {
        return "Permission denied".to_string();
    }
    if lower.contains("listener") && lower.contains("failed") {
        return "Listener unavailable".to_string();
    }

    let concise = reason
        .strip_prefix("network binding configuration could not be activated: ")
        .or_else(|| reason.strip_prefix("Networking blocked: "))
        .unwrap_or(&reason);
    truncate_with_ellipsis(concise, 48)
}

fn network_interruption_footer_text(
    interruption: &NetworkInterruption,
    width: usize,
    compact_layout: bool,
) -> String {
    if width == 0 {
        return String::new();
    }
    if !interruption.blocked {
        let label = if compact_layout {
            "REBINDING"
        } else {
            "NETWORK REBINDING"
        };
        let message = if interruption.interface == "OS selected" {
            label.to_string()
        } else {
            format!("{label} | {}", interruption.interface)
        };
        return truncate_with_ellipsis(&message, width);
    }

    let (label, compact_label, tiny_label) = ("NETWORK BLOCKED", "NET BLOCKED", "NET DOWN");
    if width < compact_label.chars().count() + 4 {
        return truncate_with_ellipsis(tiny_label, width);
    }
    if width < 40 {
        let prefix = format!("{compact_label} | ");
        let remaining = width.saturating_sub(prefix.chars().count());
        return format!(
            "{prefix}{}",
            truncate_with_ellipsis(&interruption.reason, remaining)
        );
    }

    if compact_layout {
        let message = if interruption.reason == "Interface unavailable"
            && interruption.interface != "OS selected"
        {
            format!("{label} | {} unavailable", interruption.interface)
        } else {
            format!("{label} | {}", interruption.reason)
        };
        return truncate_with_ellipsis(&message, width);
    }

    let interface_prefix = format!("{label} | {} | ", interruption.interface);
    let label_prefix = format!("{label} | ");
    let prefix = if width.saturating_sub(interface_prefix.chars().count()) >= 12 {
        interface_prefix
    } else {
        label_prefix
    };
    let remaining = width.saturating_sub(prefix.chars().count());
    format!(
        "{prefix}{}",
        truncate_with_ellipsis(&interruption.reason, remaining)
    )
}

pub fn draw_peer_stream(f: &mut Frame, app_state: &AppState, area: Rect, ctx: &ThemeContext) {
    if area.height < PEER_STREAM_MIN_HEIGHT || area.width < PEER_STREAM_MIN_WIDTH {
        return;
    }

    let selected_torrent = selected_torrent(app_state);
    let visualization = app_state.ui.visualization_focus.peer_stream;
    if visualization != PeerStreamVisualization::Classic {
        if let Some(torrent) = selected_torrent {
            draw_peer_stream_visualization(f, torrent, area, visualization, ctx, ctx.frame_time);
            return;
        }
    }

    let color_discovered = ctx.peer_discovered();
    let color_connected = ctx.peer_connected();
    let color_disconnected = ctx.peer_disconnected();
    let color_border = ctx.theme.semantic.border;

    let default_slice: Vec<u64> = Vec::new();

    let (disc_slice, conn_slice, disconn_slice) = if let Some(torrent) = selected_torrent {
        let width = area.width.saturating_sub(2).max(1) as usize;
        let dh = &torrent.peer_discovery_history;
        let ch = &torrent.peer_connection_history;
        let dch = &torrent.peer_disconnect_history;

        (
            &dh[dh.len().saturating_sub(width)..],
            &ch[ch.len().saturating_sub(width)..],
            &dch[dch.len().saturating_sub(width)..],
        )
    } else {
        (&default_slice[..], &default_slice[..], &default_slice[..])
    };

    let visible_columns = area.width.saturating_sub(2).max(1) as usize;
    let event_counts = selected_torrent
        .map(|torrent| peer_stream_event_counts(torrent, visible_columns))
        .unwrap_or_default();
    let PeerStreamEventCounts {
        connected: connected_count,
        discovered: discovered_count,
        disconnected: disconnected_count,
    } = event_counts;

    let legend_style_fn = |count: u64, color: Color| {
        if selected_torrent.is_some() && count > 0 {
            ctx.apply(Style::default().fg(color))
        } else {
            ctx.apply(Style::default().fg(ctx.theme.semantic.surface1))
        }
    };
    let use_compact_legend =
        should_use_compact_peer_stream_legend(area.width.saturating_sub(2) as usize, event_counts);
    let connected_label = if use_compact_legend { "C" } else { "Connected" };
    let discovered_label = if use_compact_legend {
        "D"
    } else {
        "Discovered"
    };
    let disconnected_label = if use_compact_legend {
        "X"
    } else {
        "Disconnected"
    };

    let legend_line = Line::from(vec![
        Span::styled(
            format!("{}:", connected_label),
            legend_style_fn(connected_count, color_connected),
        ),
        Span::styled(
            format!(" {} ", connected_count),
            legend_style_fn(connected_count, color_connected).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{}:", discovered_label),
            legend_style_fn(discovered_count, color_discovered),
        ),
        Span::styled(
            format!(" {} ", discovered_count),
            legend_style_fn(discovered_count, color_discovered).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{}:", disconnected_label),
            legend_style_fn(disconnected_count, color_disconnected),
        ),
        Span::styled(
            format!(" {} ", disconnected_count),
            legend_style_fn(disconnected_count, color_disconnected).add_modifier(Modifier::BOLD),
        ),
    ]);

    let time_seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let mut conn_points_small = Vec::new();
    let mut disc_points_small = Vec::new();
    let mut disconn_points_small = Vec::new();

    let mut conn_points_large = Vec::new();
    let mut disc_points_large = Vec::new();
    let mut disconn_points_large = Vec::new();

    let mut rng = StdRng::seed_from_u64(time_seed);

    let mut generate_points = |data_slice: &[u64],
                               small_points: &mut Vec<(f64, f64)>,
                               large_points: &mut Vec<(f64, f64)>,
                               base_y: f64,
                               lane_phase: f64| {
        let wave_frequency = 0.45;
        for (i, &val) in data_slice.iter().enumerate() {
            if val == 0 {
                continue;
            }
            let val_f = val as f64;
            let is_heavy = val > 3;
            let smoothed_activity = peer_stream_smoothed_activity(data_slice, i);
            let wave_amp = peer_stream_wave_amplitude(smoothed_activity);
            let wave_center = base_y + wave_amp * ((i as f64 * wave_frequency) + lane_phase).sin();

            let small_dot_count = (val_f.sqrt().ceil() as usize).clamp(1, 6);
            let activity_spread = (val_f * 0.08).min(0.6);
            let base_jitter = 0.05;
            let intensity = base_jitter + activity_spread;
            let x_intensity = (intensity * 0.90).max(0.02);
            let y_intensity = (intensity * 0.65).max(0.015);

            for _ in 0..small_dot_count {
                let x_jitter = rng.random_range(-x_intensity..x_intensity);
                let y_jitter = rng.random_range(-y_intensity..y_intensity);
                small_points.push((
                    i as f64 + x_jitter,
                    (wave_center + y_jitter).clamp(0.6, 3.4),
                ));
            }

            if is_heavy {
                let heavy_x_jitter = rng.random_range(-0.08..0.08);
                let heavy_y_jitter = rng.random_range(-0.05..0.05);
                large_points.push((
                    i as f64 + heavy_x_jitter,
                    (wave_center + heavy_y_jitter).clamp(0.6, 3.4),
                ));
            }
        }
    };

    generate_points(
        conn_slice,
        &mut conn_points_small,
        &mut conn_points_large,
        3.0,
        0.0,
    );
    generate_points(
        disc_slice,
        &mut disc_points_small,
        &mut disc_points_large,
        2.0,
        1.7,
    );
    generate_points(
        disconn_slice,
        &mut disconn_points_small,
        &mut disconn_points_large,
        1.0,
        3.4,
    );

    let datasets = vec![
        ratatui::widgets::Dataset::default()
            .marker(ratatui::symbols::Marker::Braille)
            .style(
                Style::default()
                    .fg(color_connected)
                    .add_modifier(Modifier::DIM),
            )
            .data(&conn_points_small),
        ratatui::widgets::Dataset::default()
            .marker(ratatui::symbols::Marker::Braille)
            .style(
                Style::default()
                    .fg(color_discovered)
                    .add_modifier(Modifier::DIM),
            )
            .data(&disc_points_small),
        ratatui::widgets::Dataset::default()
            .marker(ratatui::symbols::Marker::Braille)
            .style(
                Style::default()
                    .fg(color_disconnected)
                    .add_modifier(Modifier::DIM),
            )
            .data(&disconn_points_small),
        ratatui::widgets::Dataset::default()
            .marker(ratatui::symbols::Marker::Dot)
            .style(
                Style::default()
                    .fg(color_connected)
                    .add_modifier(Modifier::BOLD),
            )
            .data(&conn_points_large),
        ratatui::widgets::Dataset::default()
            .marker(ratatui::symbols::Marker::Dot)
            .style(
                Style::default()
                    .fg(color_discovered)
                    .add_modifier(Modifier::BOLD),
            )
            .data(&disc_points_large),
        ratatui::widgets::Dataset::default()
            .marker(ratatui::symbols::Marker::Dot)
            .style(
                Style::default()
                    .fg(color_disconnected)
                    .add_modifier(Modifier::BOLD),
            )
            .data(&disconn_points_large),
    ];

    let x_bound = disc_slice.len().max(1).saturating_sub(1) as f64;

    let chart = ratatui::widgets::Chart::new(datasets)
        .block(
            Block::default()
                .title_top(
                    Line::from(Span::styled(
                        " Peer Stream ",
                        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                    ))
                    .alignment(Alignment::Left),
                )
                .title_top(legend_line.alignment(Alignment::Right))
                .borders(Borders::ALL)
                .border_style(ctx.apply(Style::default().fg(color_border))),
        )
        .x_axis(ratatui::widgets::Axis::default().bounds([0.0, x_bound]))
        .y_axis(ratatui::widgets::Axis::default().bounds([0.5, 3.5]));

    f.render_widget(chart, area);
}

pub fn draw_block_stream_and_disk_orb(
    f: &mut Frame,
    app_state: &AppState,
    dht_status: &DhtStatus,
    dht_wave_telemetry: &DhtWaveTelemetry,
    area: Rect,
    ctx: &ThemeContext,
) {
    if area.width < 2 || area.height < 2 {
        return;
    }

    let panel_layout = block_stream_and_disk_panel_layout(app_state.screen_area, area);
    if let Some(block_stream) = panel_layout.block_stream {
        draw_vertical_block_stream_panel(f, app_state, block_stream, ctx);
    }
    if let Some(dht_wave) = panel_layout.dht_wave {
        draw_dht_wave_panel(f, app_state, dht_status, dht_wave_telemetry, dht_wave, ctx);
    }
    draw_disk_health_panel(f, app_state, panel_layout.disk_health, ctx);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockStreamDiskPanelLayout {
    block_stream: Option<Rect>,
    dht_wave: Option<Rect>,
    disk_health: Rect,
}

fn block_stream_and_disk_panel_layout(screen_area: Rect, area: Rect) -> BlockStreamDiskPanelLayout {
    match block_stream_and_disk_layout_mode(screen_area, area) {
        BlockStreamDiskLayoutMode::SideBySide => {
            let split =
                Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
                    .split(area);
            BlockStreamDiskPanelLayout {
                block_stream: Some(split[0]),
                dht_wave: None,
                disk_health: split[1],
            }
        }
        BlockStreamDiskLayoutMode::Stacked => {
            if should_insert_dht_between_blocks_and_disk(screen_area, area) {
                let split = Layout::vertical([
                    Constraint::Min(4),
                    Constraint::Length(6),
                    Constraint::Length(7),
                ])
                .split(area);
                BlockStreamDiskPanelLayout {
                    block_stream: Some(split[0]),
                    dht_wave: Some(split[1]),
                    disk_health: split[2],
                }
            } else {
                let split =
                    Layout::vertical([Constraint::Min(4), Constraint::Length(7)]).split(area);
                BlockStreamDiskPanelLayout {
                    block_stream: Some(split[0]),
                    dht_wave: None,
                    disk_health: split[1],
                }
            }
        }
        BlockStreamDiskLayoutMode::DiskOnly => BlockStreamDiskPanelLayout {
            block_stream: None,
            dht_wave: None,
            disk_health: area,
        },
    }
}

pub(crate) fn visualization_focus_panels(
    app_state: &AppState,
    plan: &LayoutPlan,
) -> Vec<(VisualizationFocusPanel, Rect)> {
    visualization_focus_panel_candidates(app_state, plan)
        .into_iter()
        .filter(|(panel, _)| visualization_focus_selection_count(*panel) >= 2)
        .collect()
}

fn visualization_focus_selection_count(panel: VisualizationFocusPanel) -> usize {
    match panel {
        VisualizationFocusPanel::TorrentList
        | VisualizationFocusPanel::TorrentDetails
        | VisualizationFocusPanel::PeerFiles
        | VisualizationFocusPanel::Chart
        | VisualizationFocusPanel::BlockStream
        | VisualizationFocusPanel::Statistics => 1,
        VisualizationFocusPanel::PeerStream => PeerStreamVisualization::ALL.len(),
        VisualizationFocusPanel::DhtWave => DhtVisualization::ALL.len(),
        VisualizationFocusPanel::DiskHealth => DiskHealthVisualization::ALL.len(),
    }
}

fn visualization_focus_panel_candidates(
    app_state: &AppState,
    plan: &LayoutPlan,
) -> Vec<(VisualizationFocusPanel, Rect)> {
    let mut panels = Vec::with_capacity(9);
    let mut push_panel = |panel, area: Rect| {
        if area.width >= 2 && area.height >= 2 {
            panels.push((panel, area));
        }
    };

    push_panel(VisualizationFocusPanel::TorrentList, plan.list);
    push_panel(VisualizationFocusPanel::TorrentDetails, plan.details);
    push_panel(VisualizationFocusPanel::PeerFiles, plan.peers);
    if let Some(chart) = plan.chart {
        push_panel(VisualizationFocusPanel::Chart, chart);
    }
    if let Some(peer_stream) = plan.peer_stream {
        push_panel(VisualizationFocusPanel::PeerStream, peer_stream);
    }
    if let Some(block_stream) = plan
        .block_stream
        .filter(|area| area.width >= 2 && area.height >= 2)
    {
        let nested = block_stream_and_disk_panel_layout(app_state.screen_area, block_stream);
        if let Some(block_stream) = nested.block_stream {
            push_panel(VisualizationFocusPanel::BlockStream, block_stream);
        }
        if let Some(dht_wave) = nested.dht_wave {
            push_panel(VisualizationFocusPanel::DhtWave, dht_wave);
        }
        push_panel(VisualizationFocusPanel::DiskHealth, nested.disk_health);
    }
    if let Some(stats) = plan.stats {
        push_panel(VisualizationFocusPanel::Statistics, stats);
    }

    panels.sort_by_key(|(_, area)| (area.y, area.x));
    panels
}

pub(crate) fn selected_visualization_focus_panel(
    app_state: &AppState,
    plan: &LayoutPlan,
) -> Option<(VisualizationFocusPanel, Rect)> {
    if !app_state.ui.visualization_focus.active {
        return None;
    }

    visualization_focus_panels(app_state, plan)
        .into_iter()
        .find(|(panel, _)| *panel == app_state.ui.visualization_focus.selected)
}

fn should_insert_dht_between_blocks_and_disk(screen_area: Rect, area: Rect) -> bool {
    const MIN_STACKED_BLOCK_HEIGHT: u16 = 4;
    const MIN_DHT_HEIGHT: u16 = 6;
    const MIN_DISK_PANEL_HEIGHT: u16 = 7;

    let is_horizontal_mode =
        screen_area.width >= 100 && (screen_area.height as f32 <= screen_area.width as f32 * 0.6);
    is_horizontal_mode
        && area.height >= MIN_STACKED_BLOCK_HEIGHT + MIN_DHT_HEIGHT + MIN_DISK_PANEL_HEIGHT
        && area.width >= 10
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockStreamDiskLayoutMode {
    SideBySide,
    Stacked,
    DiskOnly,
}

fn block_stream_and_disk_layout_mode(screen_area: Rect, area: Rect) -> BlockStreamDiskLayoutMode {
    const FORCE_STACKED_WIDTH: u16 = 34;
    const HIDE_BLOCKS_SCREEN_WIDTH: u16 = 64;
    const MIN_STACKED_BLOCK_HEIGHT: u16 = 4;
    const MIN_DISK_PANEL_HEIGHT: u16 = 7;

    // Decide split shape using the local pane geometry first; global screen mode can be too coarse
    // and causes unreadable side-by-side micro-panels at transition widths.
    let force_stacked =
        area.width < FORCE_STACKED_WIDTH || area.height > area.width.saturating_mul(2);
    let is_vertical_mode =
        screen_area.width < 100 || (screen_area.height as f32 > screen_area.width as f32 * 0.6);

    if force_stacked && area.height < MIN_STACKED_BLOCK_HEIGHT + MIN_DISK_PANEL_HEIGHT {
        return BlockStreamDiskLayoutMode::DiskOnly;
    }

    if is_vertical_mode && force_stacked && screen_area.width < HIDE_BLOCKS_SCREEN_WIDTH {
        return BlockStreamDiskLayoutMode::DiskOnly;
    }

    if !force_stacked && is_vertical_mode {
        BlockStreamDiskLayoutMode::SideBySide
    } else {
        BlockStreamDiskLayoutMode::Stacked
    }
}

fn draw_vertical_block_stream_panel(
    f: &mut Frame,
    app_state: &AppState,
    area: Rect,
    ctx: &ThemeContext,
) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let title_color = block_stream_title_color(app_state, ctx);
    let block = Block::default()
        .title(Span::styled(
            "Blocks",
            ctx.apply(Style::default().fg(title_color)),
        ))
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)));
    let inner = block.inner(area);
    f.render_widget(block, area);
    draw_vertical_block_stream_content(f, app_state, inner, ctx);
}

fn block_stream_title_color(app_state: &AppState, ctx: &ThemeContext) -> Color {
    let torrent = app_state
        .torrent_list_order
        .get(app_state.ui.selected_torrent_index)
        .and_then(|info_hash| app_state.torrents.get(info_hash));

    let Some(torrent) = torrent else {
        return ctx.theme.semantic.border;
    };

    let dl_tick = torrent.latest_state.blocks_in_this_tick;
    let ul_tick = torrent.latest_state.blocks_out_this_tick;
    if dl_tick > 0 || ul_tick > 0 {
        return if dl_tick >= ul_tick {
            ctx.theme.scale.stream.inflow
        } else {
            ctx.theme.scale.stream.outflow
        };
    }

    // Prevent title flicker by falling back to recent stream direction.
    let in_history = &torrent.latest_state.blocks_in_history;
    let out_history = &torrent.latest_state.blocks_out_history;
    let history_len = in_history.len().min(out_history.len());
    for i in (0..history_len).rev() {
        let dl = in_history[i];
        let ul = out_history[i];
        if dl == 0 && ul == 0 {
            continue;
        }
        return if dl >= ul {
            ctx.theme.scale.stream.inflow
        } else {
            ctx.theme.scale.stream.outflow
        };
    }

    ctx.theme.semantic.border
}

fn draw_disk_health_panel(f: &mut Frame, app_state: &AppState, area: Rect, ctx: &ThemeContext) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let disk_state_word = disk_health_state_word(app_state.disk_health_state_level);
    let border_color = disk_health_border_color(ctx, app_state.disk_health_state_level);
    let title_color = disk_health_title_color(ctx, app_state.disk_health_state_level);
    let available_width = usize::from(area.width.saturating_sub(2));
    let detail_width = disk_state_word.len();
    let show_state = "Disk".len() + 1 + detail_width <= available_width;
    let title = vec![Span::styled(
        "Disk",
        ctx.apply(Style::default().fg(title_color).bold()),
    )];
    let view = app_state.ui.visualization_focus.disk_health;
    let mut block = Block::default().title_top(Line::from(title));
    if show_state {
        block = block.title_top(
            Line::from(Span::styled(
                disk_state_word,
                ctx.apply(Style::default().fg(title_color).bold()),
            ))
            .alignment(Alignment::Right),
        );
    }
    if app_state.ui.visualization_focus.active && view != DiskHealthVisualization::Classic {
        let full_caption = view.label();
        let compact_caption = view.compact_label();
        let caption = if full_caption.chars().count() <= available_width {
            full_caption
        } else {
            compact_caption
        };
        block = block.title_bottom(
            Line::from(Span::styled(
                caption,
                ctx.apply(
                    Style::default()
                        .fg(ctx.accent_sapphire())
                        .add_modifier(Modifier::BOLD),
                ),
            ))
            .alignment(Alignment::Center),
        );
    }
    let block = block
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(border_color)));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if view == DiskHealthVisualization::Classic {
        draw_disk_health_orb(f, app_state, inner, ctx);
    } else {
        draw_disk_health_visualization(f, app_state, inner, view, ctx);
    }
}

fn disk_health_state_word(state_level: u8) -> &'static str {
    match state_level {
        0 => "Stable",
        1 => "Busy",
        2 => "Strain",
        _ => "Chaos",
    }
}

fn disk_health_title_color(ctx: &ThemeContext, state_level: u8) -> Color {
    disk_health_status_color(ctx, state_level)
}

fn disk_health_border_color(ctx: &ThemeContext, state_level: u8) -> Color {
    match state_level {
        0 => ctx.theme.semantic.border,
        _ => disk_health_status_color(ctx, state_level),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct DiskHealthOrbLayout {
    area: Rect,
    visual_radius: f64,
    center_y_offset_rows: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct DiskHealthOrbGeometry {
    visual_width: f64,
    visual_height: f64,
    visual_radius: f64,
    visual_center_x: f64,
    visual_center_y: f64,
}

fn disk_health_orb_layout(area: Rect) -> Option<DiskHealthOrbLayout> {
    if area.width < 3 || area.height < 3 {
        return None;
    }

    let visual_diameter = (area.width.min(area.height) as f64 * DISK_HEALTH_ORB_SIZE_SCALE)
        .min(area.width as f64)
        .min(area.height as f64 * DISK_HEALTH_ORB_CELL_Y_ASPECT);
    let base_width = (visual_diameter.ceil() as u16).clamp(3, area.width);
    let base_height =
        ((visual_diameter / DISK_HEALTH_ORB_CELL_Y_ASPECT).ceil() as u16).clamp(3, area.height);
    let base_x_slack = area.width.saturating_sub(base_width);
    let base_y_slack = area.height.saturating_sub(base_height);
    let mut width = base_width
        .saturating_add(2.min(base_x_slack))
        .min(area.width);
    if width % 2 != area.width % 2 && width < area.width {
        width += 1;
    }
    let height = base_height
        .saturating_add(base_y_slack % 2)
        .min(area.height);

    let x = area.x + area.width.saturating_sub(width) / 2;
    let y_slack = area.height.saturating_sub(height);
    let ideal_y_padding = f64::from(y_slack) / 2.0;
    let y_padding = ideal_y_padding.floor() as u16;
    let center_y_offset_rows = ideal_y_padding - f64::from(y_padding);
    let y = area.y + y_padding;

    Some(DiskHealthOrbLayout {
        area: Rect::new(x, y, width, height),
        visual_radius: (visual_diameter * 0.5).max(1.0),
        center_y_offset_rows,
    })
}

fn disk_health_orb_geometry(layout: DiskHealthOrbLayout) -> DiskHealthOrbGeometry {
    let visual_width = layout.area.width as f64;
    let visual_height = layout.area.height as f64 * DISK_HEALTH_ORB_CELL_Y_ASPECT;
    let visual_radius = layout.visual_radius;
    let visual_center_x = visual_width * 0.5;
    let visual_center_y =
        visual_height * 0.5 + layout.center_y_offset_rows * DISK_HEALTH_ORB_CELL_Y_ASPECT;

    DiskHealthOrbGeometry {
        visual_width,
        visual_height,
        visual_radius,
        visual_center_x,
        visual_center_y,
    }
}

fn build_disk_health_orb_rows(
    layout: DiskHealthOrbLayout,
    health: f64,
    deform_profile: DiskDeformProfile,
    gap: f64,
    phase: f64,
) -> Vec<String> {
    let cells_w = layout.area.width as usize;
    let cells_h = layout.area.height as usize;
    let mut rows: Vec<String> = Vec::with_capacity(cells_h);
    let geometry = disk_health_orb_geometry(layout);

    for cy in 0..cells_h {
        let mut row = String::with_capacity(cells_w);
        for cx in 0..cells_w {
            let mut bits: u8 = 0;
            for (sy, braille_row) in DISK_HEALTH_ORB_BRAILLE_BITS.iter().enumerate() {
                for (sx, &bit) in braille_row.iter().enumerate() {
                    let px = cx as f64 + (sx as f64 + 0.5) / 2.0;
                    let py = cy as f64 + (sy as f64 + 0.5) / 4.0;

                    let nx = (px - geometry.visual_center_x) / geometry.visual_radius;
                    let ny = ((py * DISK_HEALTH_ORB_CELL_Y_ASPECT) - geometry.visual_center_y)
                        / geometry.visual_radius;

                    // Keep gap-driven deformation centered by applying horizontal squeeze symmetrically.
                    let squeeze = (1.0 - (0.22 * gap)).max(0.35);
                    let x = nx / squeeze;
                    let y = ny;
                    let theta = y.atan2(x);
                    let dist = (x * x + y * y).sqrt();

                    let deform = (deform_profile.low_freq_base
                        + deform_profile.low_freq_health_scale * health)
                        * f64::sin(deform_profile.low_freq_wave * theta + phase)
                        + (deform_profile.high_freq_base
                            + deform_profile.high_freq_health_scale * health)
                            * f64::sin(
                                deform_profile.high_freq_wave * theta
                                    - deform_profile.high_freq_phase_scale * phase,
                            );
                    let edge = 0.96 + deform;

                    // Render as a solid blob (no hollow shell look).
                    let fill_factor = (deform_profile.fill_base
                        - deform_profile.fill_health_scale * health)
                        .clamp(0.90, 1.03);
                    let in_blob = dist <= edge * fill_factor;

                    if in_blob {
                        bits |= bit;
                    }
                }
            }
            row.push(if bits == 0 {
                ' '
            } else {
                char::from_u32(0x2800 + bits as u32).unwrap_or(' ')
            });
        }
        rows.push(row);
    }

    rows
}

fn draw_disk_health_orb(f: &mut Frame, app_state: &AppState, area: Rect, ctx: &ThemeContext) {
    if area.width < 2 || area.height < 2 {
        return;
    }

    let signals = DiskHealthSignals::from_app(app_state);
    let health = signals.health;
    let deform_profile = disk_health_deform_profile(signals.state_level);
    let gap = signals.throughput_gap;
    let phase = signals.phase;

    let orb_color = disk_health_status_color(ctx, app_state.disk_health_state_level);
    let orb_style = if signals.active {
        ctx.apply(Style::default().fg(orb_color))
    } else {
        ctx.apply(Style::default().fg(orb_color).dim())
    };

    let Some(orb_layout) = disk_health_orb_layout(area) else {
        return;
    };
    let orb_area = orb_layout.area;

    let lines = build_disk_health_orb_rows(orb_layout, health, deform_profile, gap, phase)
        .into_iter()
        .map(|row| Line::from(Span::styled(row, orb_style)))
        .collect::<Vec<_>>();

    f.render_widget(Paragraph::new(lines), orb_area);
}

#[derive(Clone, Copy)]
struct DiskDeformProfile {
    low_freq_base: f64,
    low_freq_health_scale: f64,
    low_freq_wave: f64,
    high_freq_base: f64,
    high_freq_health_scale: f64,
    high_freq_wave: f64,
    high_freq_phase_scale: f64,
    fill_base: f64,
    fill_health_scale: f64,
}

fn disk_health_deform_profile(state_level: u8) -> DiskDeformProfile {
    match state_level {
        // Stable: calm and rounded.
        0 => DiskDeformProfile {
            low_freq_base: 0.03,
            low_freq_health_scale: 0.12,
            low_freq_wave: 2.0,
            high_freq_base: 0.015,
            high_freq_health_scale: 0.05,
            high_freq_wave: 3.0,
            high_freq_phase_scale: 0.6,
            fill_base: 1.02,
            fill_health_scale: 0.03,
        },
        // Busy: moderate wobble, still relatively smooth.
        1 => DiskDeformProfile {
            low_freq_base: 0.04,
            low_freq_health_scale: 0.16,
            low_freq_wave: 2.0,
            high_freq_base: 0.02,
            high_freq_health_scale: 0.09,
            high_freq_wave: 3.2,
            high_freq_phase_scale: 0.75,
            fill_base: 1.01,
            fill_health_scale: 0.04,
        },
        // Strain: sharper and more turbulent silhouette.
        2 => DiskDeformProfile {
            low_freq_base: 0.06,
            low_freq_health_scale: 0.23,
            low_freq_wave: 2.35,
            high_freq_base: 0.035,
            high_freq_health_scale: 0.125,
            high_freq_wave: 4.1,
            high_freq_phase_scale: 0.98,
            fill_base: 0.995,
            fill_health_scale: 0.05,
        },
        // Chaos: most unstable / jagged.
        _ => DiskDeformProfile {
            low_freq_base: 0.09,
            low_freq_health_scale: 0.34,
            low_freq_wave: 3.0,
            high_freq_base: 0.06,
            high_freq_health_scale: 0.21,
            high_freq_wave: 5.8,
            high_freq_phase_scale: 1.30,
            fill_base: 0.965,
            fill_health_scale: 0.06,
        },
    }
}

fn draw_vertical_block_stream_content(
    f: &mut Frame,
    app_state: &AppState,
    area: Rect,
    ctx: &ThemeContext,
) {
    if area.width < 1 || area.height < 1 {
        return;
    }
    let selected_torrent = app_state
        .torrent_list_order
        .get(app_state.ui.selected_torrent_index)
        .and_then(|info_hash| app_state.torrents.get(info_hash));

    let Some(torrent) = selected_torrent else {
        return;
    };

    const UP_TRIANGLE: &str = "▲";
    const DOWN_TRIANGLE: &str = "▼";
    const SEPARATOR: &str = "·";

    let color_inflow = ctx.theme.scale.stream.inflow;
    let color_outflow = ctx.theme.scale.stream.outflow;
    let color_empty = ctx.theme.semantic.surface0;

    let history_len = area.height as usize;
    let content_width = area.width as usize;

    if history_len == 0 || content_width == 0 {
        return;
    }

    let in_history = &torrent.latest_state.blocks_in_history;
    let out_history = &torrent.latest_state.blocks_out_history;
    let allow_download_inflow = should_render_download_inflow(&torrent.latest_state);

    let in_slice = &in_history[in_history.len().saturating_sub(history_len)..];
    let out_slice = &out_history[out_history.len().saturating_sub(history_len)..];
    let has_activity = in_slice.iter().any(|&v| v > 0) || out_slice.iter().any(|&v| v > 0);
    let idle_slow_probability = if has_activity { 0.0 } else { 0.20 };

    let slice_len = in_slice.len();
    let mut lines: Vec<Line> = Vec::with_capacity(history_len);
    let frame_seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    for i in 0..history_len {
        let mut spans = Vec::new();
        let dl_slice_index = slice_len.saturating_sub(1).saturating_sub(i);
        let raw_blocks_in = if allow_download_inflow && i < slice_len {
            *in_slice.get(dl_slice_index).unwrap_or(&0)
        } else {
            0
        };
        let upload_padding = history_len.saturating_sub(slice_len);
        let ul_slice_index = i.saturating_sub(upload_padding);
        let raw_blocks_out = if i >= upload_padding {
            *out_slice.get(ul_slice_index).unwrap_or(&0)
        } else {
            0
        };

        let total_raw = raw_blocks_in + raw_blocks_out;
        let mut blocks_in: u64;
        let mut blocks_out: u64;

        if total_raw > content_width as u64 {
            blocks_in =
                (raw_blocks_in as f64 / total_raw as f64 * content_width as f64).round() as u64;
            blocks_out =
                (raw_blocks_out as f64 / total_raw as f64 * content_width as f64).round() as u64;
            if raw_blocks_in > 0 && blocks_in == 0 {
                blocks_in = 1;
            }
            if raw_blocks_out > 0 && blocks_out == 0 {
                blocks_out = 1;
            }

            let total_drawn = blocks_in + blocks_out;
            if total_drawn > content_width as u64 {
                let overfill = total_drawn - content_width as u64;
                if raw_blocks_in > raw_blocks_out {
                    blocks_in = blocks_in.saturating_sub(overfill);
                } else {
                    blocks_out = blocks_out.saturating_sub(overfill);
                }
            } else if total_drawn < content_width as u64 {
                let remainder = (content_width as u64) - total_drawn;
                if raw_blocks_in > raw_blocks_out {
                    blocks_in += remainder;
                } else {
                    blocks_out += remainder;
                }
            }
        } else {
            blocks_in = raw_blocks_in;
            blocks_out = raw_blocks_out;
        }

        let total_blocks = (blocks_in + blocks_out) as usize;
        if total_blocks == 0 {
            let padding = " ".repeat(content_width.saturating_sub(1) / 2);
            let trailing_padding = content_width
                .saturating_sub(1)
                .saturating_sub(padding.len());
            spans.push(Span::raw(padding));
            spans.push(Span::styled(
                SEPARATOR,
                ctx.apply(Style::default().fg(color_empty)),
            ));
            spans.push(Span::raw(" ".repeat(trailing_padding)));
        } else {
            let padding = (content_width.saturating_sub(total_blocks)) / 2;
            let trailing_padding = content_width
                .saturating_sub(total_blocks)
                .saturating_sub(padding);

            let (
                larger_stream_count,
                smaller_stream_count,
                larger_symbol,
                smaller_symbol,
                larger_color,
                smaller_color,
                larger_seed_salt,
                smaller_seed_salt,
            ) = if blocks_in >= blocks_out {
                (
                    blocks_in,
                    blocks_out,
                    DOWN_TRIANGLE,
                    UP_TRIANGLE,
                    color_inflow,
                    color_outflow,
                    dl_slice_index as u64,
                    (ul_slice_index as u64) ^ 0xABCDEF,
                )
            } else {
                (
                    blocks_out,
                    blocks_in,
                    UP_TRIANGLE,
                    DOWN_TRIANGLE,
                    color_outflow,
                    color_inflow,
                    (ul_slice_index as u64) ^ 0xABCDEF,
                    dl_slice_index as u64,
                )
            };

            let mut order_rng = StdRng::seed_from_u64(
                (dl_slice_index as u64) ^ (ul_slice_index as u64) ^ 0xDEADBEEF,
            );
            let total_scaled_blocks_f64 = (larger_stream_count + smaller_stream_count) as f64;
            let ratio_smaller = smaller_stream_count as f64 / total_scaled_blocks_f64;
            let smaller_first: bool = order_rng.random_bool(1.0 - ratio_smaller);
            let smaller_stay_probability = (idle_slow_probability * 3.0_f64).clamp(0.0, 1.0);
            let larger_stay_probability = (idle_slow_probability * 0.35_f64).clamp(0.0, 1.0);
            let mut slow_rng = StdRng::seed_from_u64(
                frame_seed
                    ^ (dl_slice_index as u64).rotate_left(7)
                    ^ (ul_slice_index as u64).rotate_right(11)
                    ^ 0xAC71_4D2F,
            );
            let smaller_seed = if slow_rng.random_bool(smaller_stay_probability) {
                smaller_seed_salt
            } else {
                frame_seed ^ smaller_seed_salt
            };
            let larger_seed = if slow_rng.random_bool(larger_stay_probability) {
                larger_seed_salt
            } else {
                frame_seed ^ larger_seed_salt
            };

            spans.push(Span::raw(" ".repeat(padding)));
            if smaller_first {
                render_sparkles(
                    &mut spans,
                    smaller_symbol,
                    smaller_stream_count,
                    smaller_color,
                    smaller_seed,
                );
                render_sparkles(
                    &mut spans,
                    larger_symbol,
                    larger_stream_count,
                    larger_color,
                    larger_seed,
                );
            } else {
                render_sparkles(
                    &mut spans,
                    larger_symbol,
                    larger_stream_count,
                    larger_color,
                    larger_seed,
                );
                render_sparkles(
                    &mut spans,
                    smaller_symbol,
                    smaller_stream_count,
                    smaller_color,
                    smaller_seed,
                );
            }
            spans.push(Span::raw(" ".repeat(trailing_padding)));
        }
        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(lines), area);
}

fn should_render_download_inflow(metrics: &crate::app::TorrentMetrics) -> bool {
    let total = metrics.number_of_pieces_total;
    total == 0 || metrics.number_of_pieces_completed < total
}

fn render_sparkles<'a>(
    spans: &mut Vec<Span<'a>>,
    symbol: &'a str,
    count: u64,
    color: Color,
    seed: u64,
) {
    let mut rng = StdRng::seed_from_u64(seed);
    for _ in 0..count {
        let is_bold: bool = rng.random();
        let mut style = Style::default().fg(color);
        style = if is_bold {
            style.add_modifier(Modifier::BOLD)
        } else {
            style.add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled(symbol, style));
    }
}

pub fn draw_peers_table(
    f: &mut Frame,
    app_state: &AppState,
    peers_chunk: Rect,
    ctx: &ThemeContext,
) {
    draw_peers_table_impl(f, app_state, peers_chunk, ctx, true);
}

fn draw_peers_table_without_swarm(
    f: &mut Frame,
    app_state: &AppState,
    peers_chunk: Rect,
    ctx: &ThemeContext,
) {
    draw_peers_table_impl(f, app_state, peers_chunk, ctx, false);
}

fn draw_peers_table_impl(
    f: &mut Frame,
    app_state: &AppState,
    peers_chunk: Rect,
    ctx: &ThemeContext,
    include_swarm: bool,
) {
    if peers_chunk.height < 2 || peers_chunk.width < 2 {
        return;
    }

    if let Some(info_hash) = app_state
        .torrent_list_order
        .get(app_state.ui.selected_torrent_index)
    {
        if let Some(torrent) = app_state.torrents.get(info_hash) {
            let state = &torrent.latest_state;
            let heatmap_flash = swarm_heatmap_flash(app_state, info_hash);

            if peers_chunk.height > 0 {
                let (sort_by, sort_direction) = app_state.peer_sort;
                let peer_rows_to_display =
                    displayed_peers_for_table(state, sort_by, sort_direction);
                let flashing_peer_addresses = swarm_heatmap_flashing_peer_addresses(
                    Some(heatmap_flash),
                    &state.peers,
                    state.number_of_pieces_total as usize,
                );

                let all_peer_cols = get_peer_columns();
                let (constraints, visible_indices) =
                    compute_visible_peer_columns(app_state, peers_chunk.width);

                let peer_border_style =
                    if matches!(app_state.ui.selected_header, SelectedHeader::Peer(_)) {
                        ctx.apply(Style::default().fg(ctx.state_selected()))
                    } else {
                        ctx.apply(Style::default().fg(ctx.theme.semantic.surface2))
                    };

                if peer_rows_to_display.is_empty() {
                    if !include_swarm {
                        return;
                    }
                    draw_swarm_heatmap(
                        f,
                        ctx,
                        &state.peers,
                        state.number_of_pieces_total,
                        peers_chunk,
                        Some(heatmap_flash),
                    );
                } else {
                    let header_cells: Vec<Cell> = visible_indices
                        .iter()
                        .map(|&real_idx| {
                            let def = &all_peer_cols[real_idx];

                            let is_selected =
                                app_state.ui.selected_header == SelectedHeader::Peer(def.id);
                            let is_sorting = def.sort_enum == Some(sort_by);

                            let mut style = ctx.apply(Style::default().fg(ctx.state_warning()));
                            if is_sorting {
                                style = style.fg(ctx.state_selected());
                            }
                            style = ctx.apply(style);

                            let mut text = def.header.to_string();
                            if is_sorting {
                                text.push_str(sort_direction_arrow_for_peer_column(
                                    sort_by,
                                    sort_direction,
                                ));
                            }

                            let mut span = Span::styled(text, style);
                            if is_selected {
                                span = span.style(
                                    style
                                        .fg(ctx.state_selected())
                                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                                );
                            }
                            Cell::from(Line::from(vec![span]))
                        })
                        .collect();

                    let peer_header = Row::new(header_cells).height(1);

                    let peer_rows: Vec<Row<'_>> = peer_rows_to_display
                        .iter()
                        .map(|row| {
                            let (cells, row_style) = match row {
                                PeerTableRow::Peer(peer) => {
                                    let row_color = if peer_is_inactive_for_table(peer) {
                                        ctx.theme.semantic.surface1
                                    } else {
                                        ip_to_color(ctx, &peer.address)
                                    };
                                    let row_style = if flashing_peer_addresses
                                        .contains(&peer.address)
                                    {
                                        Style::default().fg(row_color).add_modifier(Modifier::BOLD)
                                    } else {
                                        Style::default().fg(row_color)
                                    };

                                    let cells: Vec<Cell> = visible_indices
                                        .iter()
                                        .map(|&real_idx| {
                                            let def = &all_peer_cols[real_idx];
                                            match def.id {
                                                PeerColumnId::Flags => Line::from(vec![
                                                    Span::styled(
                                                        "■",
                                                        ctx.apply(Style::default().fg(
                                                            if peer.am_interested {
                                                                ctx.accent_sapphire()
                                                            } else {
                                                                ctx.theme.semantic.surface1
                                                            },
                                                        )),
                                                    ),
                                                    Span::styled(
                                                        "■",
                                                        ctx.apply(Style::default().fg(
                                                            if peer.peer_choking {
                                                                ctx.accent_maroon()
                                                            } else {
                                                                ctx.theme.semantic.surface1
                                                            },
                                                        )),
                                                    ),
                                                    Span::styled(
                                                        "■",
                                                        ctx.apply(Style::default().fg(
                                                            if peer.peer_interested {
                                                                ctx.accent_teal()
                                                            } else {
                                                                ctx.theme.semantic.surface1
                                                            },
                                                        )),
                                                    ),
                                                    Span::styled(
                                                        "■",
                                                        ctx.apply(Style::default().fg(
                                                            if peer.am_choking {
                                                                ctx.accent_peach()
                                                            } else {
                                                                ctx.theme.semantic.surface1
                                                            },
                                                        )),
                                                    ),
                                                ])
                                                .into(),
                                                PeerColumnId::Address => {
                                                    let display = if app_state
                                                        .anonymize_torrent_names
                                                    {
                                                        "xxx.xxx.xxx".to_string()
                                                    } else {
                                                        format_peer_address_for_table(&peer.address)
                                                    };
                                                    Cell::from(display)
                                                }
                                                PeerColumnId::Client => {
                                                    let raw_client = parse_peer_id(&peer.peer_id);
                                                    Cell::from(sanitize_text(&raw_client))
                                                }
                                                PeerColumnId::Action => {
                                                    Cell::from(peer.last_action.clone())
                                                }
                                                PeerColumnId::Progress => {
                                                    let total =
                                                        state.number_of_pieces_total as usize;
                                                    let pct = if total > 0 {
                                                        let c = peer
                                                            .bitfield
                                                            .iter()
                                                            .take(total)
                                                            .filter(|&&b| b)
                                                            .count();
                                                        (c as f64 / total as f64) * 100.0
                                                    } else {
                                                        0.0
                                                    };
                                                    Cell::from(format!("{pct:.0}%"))
                                                }
                                                PeerColumnId::DownSpeed => {
                                                    if peers_chunk.width > 120 {
                                                        Cell::from(format!(
                                                            "{} ({})",
                                                            format_speed(peer.download_speed_bps),
                                                            format_bytes(peer.total_downloaded)
                                                        ))
                                                    } else {
                                                        Cell::from(format_speed(
                                                            peer.download_speed_bps,
                                                        ))
                                                    }
                                                }
                                                PeerColumnId::UpSpeed => {
                                                    if peers_chunk.width > 120 {
                                                        Cell::from(format!(
                                                            "{} ({})",
                                                            format_speed(peer.upload_speed_bps),
                                                            format_bytes(peer.total_uploaded)
                                                        ))
                                                    } else {
                                                        Cell::from(format_speed(
                                                            peer.upload_speed_bps,
                                                        ))
                                                    }
                                                }
                                            }
                                        })
                                        .collect();
                                    (cells, row_style)
                                }
                                PeerTableRow::InactiveSummary { count } => (
                                    inactive_peer_summary_cells(
                                        *count,
                                        &all_peer_cols,
                                        &visible_indices,
                                    ),
                                    Style::default()
                                        .fg(ctx.theme.semantic.surface1)
                                        .add_modifier(Modifier::ITALIC),
                                ),
                            };
                            Row::new(cells).style(ctx.apply(row_style))
                        })
                        .collect();

                    let peers_table = Table::new(peer_rows, constraints)
                        .header(peer_header)
                        .block(Block::default());

                    let table_rows_needed: u16 = 1 + peer_rows_to_display.len() as u16;
                    let peer_block_height_needed: u16 = table_rows_needed + 1;
                    let remaining_height =
                        peers_chunk.height.saturating_sub(peer_block_height_needed);

                    let peers_block = Block::default()
                        .padding(Padding::new(1, 1, 0, 0))
                        .border_style(peer_border_style);

                    if include_swarm && remaining_height >= MIN_SWARM_AVAILABILITY_HEIGHT {
                        let layout_chunks = Layout::vertical([
                            Constraint::Length(peer_block_height_needed),
                            Constraint::Min(0),
                        ])
                        .split(peers_chunk);
                        let inner_peers_area = peers_block.inner(layout_chunks[0]);
                        f.render_widget(peers_block, layout_chunks[0]);
                        f.render_widget(peers_table, inner_peers_area);
                        draw_swarm_heatmap(
                            f,
                            ctx,
                            &state.peers,
                            state.number_of_pieces_total,
                            layout_chunks[1],
                            Some(heatmap_flash),
                        );
                    } else {
                        let inner_peers_area = peers_block.inner(peers_chunk);
                        f.render_widget(peers_block, peers_chunk);
                        f.render_widget(peers_table, inner_peers_area);
                    }
                }
            }
        }
    } else if include_swarm {
        draw_swarm_heatmap(f, ctx, &[], 0, peers_chunk, None);
    }
}

fn peer_table_height_for_row_count(row_count: usize) -> u16 {
    if row_count == 0 {
        0
    } else {
        usize_to_u16_saturating(row_count).saturating_add(2)
    }
}

fn displayed_peers_for_table(
    state: &crate::app::TorrentMetrics,
    sort_by: PeerSortColumn,
    sort_direction: SortDirection,
) -> Vec<PeerTableRow> {
    let has_established_peers = state.peers.iter().any(|p| p.last_action != "Connecting...");
    let mut peers_to_display: Vec<PeerInfo> = if has_established_peers {
        state
            .peers
            .iter()
            .filter(|p| p.last_action != "Connecting...")
            .cloned()
            .collect()
    } else {
        state.peers.clone()
    };

    peers_to_display.sort_by(|a, b| compare_peer_table_rows(a, b, state, sort_by, sort_direction));

    let active_count = peers_to_display
        .iter()
        .filter(|peer| !peer_is_inactive_for_table(peer))
        .count();
    let inactive_count = peers_to_display.len().saturating_sub(active_count);

    if active_count > 0 {
        let mut rows: Vec<PeerTableRow> = peers_to_display
            .into_iter()
            .filter(|peer| !peer_is_inactive_for_table(peer))
            .map(PeerTableRow::Peer)
            .collect();
        if inactive_count > 0 {
            rows.push(PeerTableRow::InactiveSummary {
                count: inactive_count,
            });
        }
        return rows;
    }

    if inactive_count <= MAX_INACTIVE_ONLY_PEERS_IN_TABLE {
        return peers_to_display
            .into_iter()
            .map(PeerTableRow::Peer)
            .collect();
    }

    let mut retained_inactive = 0usize;
    peers_to_display
        .into_iter()
        .filter(|peer| {
            if !peer_is_inactive_for_table(peer) {
                return true;
            }

            if retained_inactive < MAX_INACTIVE_ONLY_PEERS_IN_TABLE {
                retained_inactive += 1;
                true
            } else {
                false
            }
        })
        .map(PeerTableRow::Peer)
        .collect()
}

fn inactive_peer_summary_cells(
    count: usize,
    all_peer_cols: &[crate::tui::layout::common::PeerColumnDefinition],
    visible_indices: &[usize],
) -> Vec<Cell<'static>> {
    let summary_column = visible_indices
        .iter()
        .copied()
        .find(|&real_idx| all_peer_cols[real_idx].id == PeerColumnId::Address)
        .or_else(|| visible_indices.first().copied());
    let summary_label = format!(
        "{} inactive peer{}",
        count,
        if count == 1 { "" } else { "s" }
    );

    visible_indices
        .iter()
        .map(|&real_idx| {
            if Some(real_idx) == summary_column {
                Cell::from(summary_label.clone())
            } else {
                Cell::from("")
            }
        })
        .collect()
}

fn compare_peer_table_rows(
    a: &PeerInfo,
    b: &PeerInfo,
    state: &crate::app::TorrentMetrics,
    sort_by: PeerSortColumn,
    sort_direction: SortDirection,
) -> std::cmp::Ordering {
    use crate::config::PeerSortColumn::*;
    let ordering = match sort_by {
        Flags => a.peer_choking.cmp(&b.peer_choking),
        Completed => {
            let total = state.number_of_pieces_total as usize;
            if total == 0 {
                std::cmp::Ordering::Equal
            } else {
                let a_c = a.bitfield.iter().take(total).filter(|&&h| h).count();
                let b_c = b.bitfield.iter().take(total).filter(|&&h| h).count();
                a_c.cmp(&b_c)
            }
        }
        Address => a.address.cmp(&b.address),
        Client => a.peer_id.cmp(&b.peer_id),
        Action => a.last_action.cmp(&b.last_action),
        DL => a.download_speed_bps.cmp(&b.download_speed_bps),
        UL => a.upload_speed_bps.cmp(&b.upload_speed_bps),
    };
    if sort_direction == SortDirection::Ascending {
        ordering
    } else {
        ordering.reverse()
    }
}

fn peer_is_inactive_for_table(peer: &PeerInfo) -> bool {
    peer.download_speed_bps == 0 && peer.upload_speed_bps == 0
}

fn draw_torrent_files_panel_without_swarm(
    f: &mut Frame,
    app_state: &AppState,
    area: Rect,
    ctx: &ThemeContext,
    files_mode: TorrentFilesRenderMode,
) {
    draw_torrent_files_panel_impl(f, app_state, area, ctx, files_mode);
}

fn draw_torrent_files_panel_impl(
    f: &mut Frame,
    app_state: &AppState,
    area: Rect,
    ctx: &ThemeContext,
    files_mode: TorrentFilesRenderMode,
) {
    if area.height < 2 || area.width < 2 {
        return;
    }

    let Some((_, torrent)) = selected_torrent_entry(app_state) else {
        let body_area = draw_torrent_files_frame(f, area, ctx);
        let empty = Paragraph::new("No torrent selected")
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true });
        f.render_widget(empty, body_area);
        return;
    };

    let body_area = draw_torrent_files_frame(f, area, ctx);
    let list_items = build_torrent_file_list_items(
        torrent,
        TorrentFilesListRenderOptions {
            width: body_area.width,
            height: body_area.height,
            anonymize: app_state.anonymize_torrent_names,
            download_phase: app_state.ui.file_activity_download_phase,
            upload_phase: app_state.ui.file_activity_upload_phase,
            mode: files_mode,
        },
        ctx,
    );
    f.render_widget(List::new(list_items), body_area);
}

fn draw_torrent_files_frame(_f: &mut Frame, area: Rect, _ctx: &ThemeContext) -> Rect {
    if area.width == 0 || area.height == 0 {
        return area;
    }

    torrent_files_body_area(area)
}

fn torrent_files_body_area(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y,
        area.width.saturating_sub(2),
        area.height,
    )
}

fn torrent_files_panel_height_needed(
    torrent: &TorrentDisplayState,
    width: u16,
    anonymize: bool,
    max_height: u16,
) -> Option<u16> {
    if max_height == 0 {
        return None;
    }

    let body_width = width.saturating_sub(2);
    let max_body_rows = max_height as usize;
    let body_rows =
        torrent_file_list_desired_row_count(torrent, body_width, anonymize, max_body_rows);
    Some(usize_to_u16_saturating(body_rows.max(1)).min(max_height))
}

fn usize_to_u16_saturating(value: usize) -> u16 {
    value.min(u16::MAX as usize) as u16
}

fn torrent_file_list_desired_row_count(
    torrent: &TorrentDisplayState,
    width: u16,
    anonymize: bool,
    max_rows: usize,
) -> usize {
    if max_rows == 0 {
        return 0;
    }

    let root_path = torrent_root_path_label(&torrent.latest_state, anonymize);
    let root_width = width.saturating_sub(4) as usize;
    let root_rows = shape_root_path_for_viewport(&root_path, root_width.max(1), max_rows).len();
    if root_rows >= max_rows {
        return root_rows;
    }

    let remaining_rows = max_rows.saturating_sub(root_rows);
    if torrent.file_preview_tree.is_empty() {
        return root_rows
            + usize::from(!torrent.latest_state.torrent_name.is_empty()).min(remaining_rows);
    }

    let mut expanded_state = TreeViewState::default();
    for node in &torrent.file_preview_tree {
        node.expand_all(&mut expanded_state);
    }
    let visible_rows = TreeMathHelper::get_visible_slice(
        &torrent.file_preview_tree,
        &expanded_state,
        TreeFilter::default(),
        usize::MAX,
    )
    .len();

    root_rows + visible_rows.min(remaining_rows)
}

#[derive(Debug, Clone, Copy)]
struct TorrentFilesListRenderOptions {
    width: u16,
    height: u16,
    anonymize: bool,
    download_phase: f64,
    upload_phase: f64,
    mode: TorrentFilesRenderMode,
}

fn build_torrent_file_list_items(
    torrent: &TorrentDisplayState,
    options: TorrentFilesListRenderOptions,
    ctx: &ThemeContext,
) -> Vec<ListItem<'static>> {
    match options.mode {
        TorrentFilesRenderMode::Tree => build_torrent_file_tree_list_items(
            torrent,
            options.width,
            options.height,
            options.anonymize,
            options.download_phase,
            options.upload_phase,
            ctx,
        ),
        TorrentFilesRenderMode::ActivitySorted => build_activity_sorted_torrent_file_list_items(
            torrent,
            options.height,
            options.anonymize,
            options.download_phase,
            options.upload_phase,
            ctx,
        ),
    }
}

fn build_torrent_file_tree_list_items(
    torrent: &TorrentDisplayState,
    width: u16,
    height: u16,
    anonymize: bool,
    download_phase: f64,
    upload_phase: f64,
    ctx: &ThemeContext,
) -> Vec<ListItem<'static>> {
    let mut list_items = Vec::new();
    let root_style = ctx.apply(
        Style::default()
            .fg(ctx.theme.semantic.text)
            .add_modifier(Modifier::BOLD),
    );
    let root_path = torrent_root_path_label(&torrent.latest_state, anonymize);
    let root_path_char_len = root_path.chars().count();
    let root_width = width.saturating_sub(6) as usize;
    let root_rows = shape_root_path_for_viewport(&root_path, root_width.max(1), height as usize);
    let root_row_offsets = shaped_row_start_offsets(&root_rows);
    list_items.extend(root_rows.into_iter().zip(root_row_offsets).enumerate().map(
        |(idx, (row, row_start_offset))| {
            let indent = "  ".repeat(idx);
            let mut spans = vec![
                Span::styled(
                    indent,
                    ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
                ),
                Span::styled(ASCII_TREE_DIR_ICON, root_style),
            ];
            spans.extend(render_file_tree_name_spans(
                torrent,
                "",
                &row,
                true,
                FileTreeNameRenderContext {
                    download_phase,
                    upload_phase,
                    row_start_offset,
                    base_style: root_style,
                    ctx,
                },
            ));
            ListItem::new(Line::from(spans))
        },
    ));
    let root_depth = list_items.len();

    if torrent.file_preview_tree.is_empty() {
        if !torrent.latest_state.torrent_name.is_empty() {
            let child_name =
                anonymize_tree_name(&torrent.latest_state.torrent_name, false, anonymize);
            let child_indent = "  ".repeat(root_depth);
            let mut spans = vec![
                Span::styled(
                    child_indent,
                    ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
                ),
                Span::styled(
                    ASCII_TREE_FILE_ICON,
                    ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
                ),
            ];
            spans.extend(render_file_tree_name_spans(
                torrent,
                &torrent.latest_state.torrent_name,
                &child_name,
                false,
                FileTreeNameRenderContext {
                    download_phase,
                    upload_phase,
                    row_start_offset: root_path_char_len
                        + 1
                        + path_parent_prefix_len(&torrent.latest_state.torrent_name),
                    base_style: ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                    ctx,
                },
            ));
            if torrent.latest_state.total_size > 0 {
                spans.push(Span::styled(
                    format!(" ({})", format_bytes(torrent.latest_state.total_size)),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
                ));
            }
            list_items.push(ListItem::new(Line::from(spans)));
        }
        return list_items;
    }

    let mut expanded_state = TreeViewState::default();
    for node in &torrent.file_preview_tree {
        node.expand_all(&mut expanded_state);
    }
    let visible_tree_height = (height as usize).saturating_sub(root_depth);
    if visible_tree_height == 0 {
        return list_items;
    }

    let mut visible_rows = TreeMathHelper::get_visible_slice(
        &torrent.file_preview_tree,
        &expanded_state,
        TreeFilter::default(),
        usize::MAX,
    );
    if visible_rows.len() > visible_tree_height {
        visible_rows.sort_by_cached_key(|item| {
            let relative_path = normalize_tree_relative_path(item.path.as_path());
            let display_name = anonymize_tree_name(&item.node.name, item.node.is_dir, anonymize);
            file_tree_activity_sort_rank(
                torrent,
                &relative_path,
                item.node.is_dir,
                display_name.chars().count(),
            )
        });
        visible_rows.truncate(visible_tree_height);
    }

    list_items.extend(visible_rows.iter().map(|item| {
        let indent = "  ".repeat(item.depth + root_depth);
        let icon = if item.node.is_dir {
            ASCII_TREE_DIR_ICON
        } else {
            ASCII_TREE_FILE_ICON
        };
        let relative_path = normalize_tree_relative_path(item.path.as_path());

        let name_style = dashboard_file_name_style(item.node.is_dir, ctx);
        let mut spans = vec![
            Span::styled(
                indent,
                ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
            ),
            Span::styled(
                icon,
                ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
            ),
        ];
        let display_name = anonymize_tree_name(&item.node.name, item.node.is_dir, anonymize);
        spans.extend(render_file_tree_name_spans(
            torrent,
            &relative_path,
            &display_name,
            item.node.is_dir,
            FileTreeNameRenderContext {
                download_phase,
                upload_phase,
                row_start_offset: root_path_char_len + 1 + path_parent_prefix_len(&relative_path),
                base_style: name_style,
                ctx,
            },
        ));

        if !item.node.is_dir {
            spans.push(Span::styled(
                format!(" ({})", format_bytes(item.node.payload.size)),
                ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
            ));
        }

        ListItem::new(Line::from(spans))
    }));

    list_items
}

#[derive(Debug, Clone)]
struct ActivitySortedFileRow {
    relative_path: String,
    size: u64,
}

fn build_activity_sorted_torrent_file_list_items(
    torrent: &TorrentDisplayState,
    height: u16,
    anonymize: bool,
    download_phase: f64,
    upload_phase: f64,
    ctx: &ThemeContext,
) -> Vec<ListItem<'static>> {
    let mut rows = activity_sorted_file_rows(torrent);
    let height = height as usize;
    if height == 0 || rows.is_empty() {
        return Vec::new();
    }

    rows.sort_by(|a, b| compare_activity_sorted_file_rows(torrent, a, b));

    let total_rows = rows.len();
    let visible_file_rows = if total_rows > height {
        height.saturating_sub(1)
    } else {
        height
    };

    let mut items = rows
        .into_iter()
        .take(visible_file_rows)
        .map(|row| {
            render_activity_sorted_file_row(
                torrent,
                row,
                anonymize,
                download_phase,
                upload_phase,
                ctx,
            )
        })
        .collect::<Vec<_>>();

    if total_rows > height {
        let hidden_count = total_rows.saturating_sub(visible_file_rows);
        items.push(render_activity_sorted_overflow_row(hidden_count, ctx));
    }

    items
}

fn activity_sorted_file_count(torrent: &TorrentDisplayState) -> usize {
    activity_sorted_file_rows(torrent).len()
}

fn activity_sorted_file_rows(torrent: &TorrentDisplayState) -> Vec<ActivitySortedFileRow> {
    let mut rows = Vec::new();
    for node in &torrent.file_preview_tree {
        collect_activity_sorted_file_rows(node, &mut rows);
    }

    if rows.is_empty() && !torrent.latest_state.torrent_name.is_empty() {
        rows.push(ActivitySortedFileRow {
            relative_path: torrent.latest_state.torrent_name.clone(),
            size: torrent.latest_state.total_size,
        });
    }

    rows
}

fn collect_activity_sorted_file_rows(
    node: &crate::tui::tree::RawNode<crate::app::TorrentPreviewPayload>,
    rows: &mut Vec<ActivitySortedFileRow>,
) {
    if node.is_dir {
        for child in &node.children {
            collect_activity_sorted_file_rows(child, rows);
        }
        return;
    }

    rows.push(ActivitySortedFileRow {
        relative_path: normalize_tree_relative_path(node.full_path.as_path()),
        size: node.payload.size,
    });
}

fn compare_activity_sorted_file_rows(
    torrent: &TorrentDisplayState,
    a: &ActivitySortedFileRow,
    b: &ActivitySortedFileRow,
) -> std::cmp::Ordering {
    let a_activity = file_activity_last_seen(torrent, &a.relative_path);
    let b_activity = file_activity_last_seen(torrent, &b.relative_path);

    b_activity
        .cmp(&a_activity)
        .then_with(|| a.relative_path.cmp(&b.relative_path))
}

fn file_activity_last_seen(torrent: &TorrentDisplayState, relative_path: &str) -> Option<Instant> {
    torrent
        .recent_file_activity
        .get(relative_path)
        .and_then(
            |activity| match (activity.download_at, activity.upload_at) {
                (Some(download_at), Some(upload_at)) => Some(download_at.max(upload_at)),
                (Some(download_at), None) => Some(download_at),
                (None, Some(upload_at)) => Some(upload_at),
                (None, None) => None,
            },
        )
}

fn render_activity_sorted_file_row(
    torrent: &TorrentDisplayState,
    row: ActivitySortedFileRow,
    anonymize: bool,
    download_phase: f64,
    upload_phase: f64,
    ctx: &ThemeContext,
) -> ListItem<'static> {
    let name_style = dashboard_file_name_style(false, ctx);
    let display_name = anonymize_tree_name(&row.relative_path, false, anonymize);
    let mut spans = vec![Span::styled(
        ASCII_TREE_FILE_ICON,
        ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
    )];
    spans.extend(render_file_tree_name_spans(
        torrent,
        &row.relative_path,
        &display_name,
        false,
        FileTreeNameRenderContext {
            download_phase,
            upload_phase,
            row_start_offset: torrent_root_logical_len(torrent).saturating_add(1),
            base_style: name_style,
            ctx,
        },
    ));

    if row.size > 0 {
        spans.push(Span::styled(
            format!(" ({})", format_bytes(row.size)),
            ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
        ));
    }

    ListItem::new(Line::from(spans))
}

fn render_activity_sorted_overflow_row(
    hidden_count: usize,
    ctx: &ThemeContext,
) -> ListItem<'static> {
    let label = format!(
        "+ {} more file{}",
        hidden_count,
        if hidden_count == 1 { "" } else { "s" }
    );
    ListItem::new(Line::from(vec![
        Span::styled(
            ASCII_TREE_FILE_ICON,
            ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
        ),
        Span::styled(
            label,
            ctx.apply(
                Style::default()
                    .fg(ctx.theme.semantic.surface1)
                    .add_modifier(Modifier::ITALIC),
            ),
        ),
    ]))
}

fn dashboard_file_name_style(is_dir: bool, ctx: &ThemeContext) -> Style {
    ctx.apply(Style::default().fg(if is_dir {
        ctx.state_info()
    } else {
        ctx.theme.semantic.text
    }))
}

fn file_tree_activity_sort_rank(
    torrent: &TorrentDisplayState,
    relative_path: &str,
    is_dir: bool,
    text_len: usize,
) -> u8 {
    if !file_tree_row_has_visible_activity(torrent, relative_path, is_dir, text_len) {
        return 2;
    }

    if is_dir {
        1
    } else {
        0
    }
}

fn file_tree_row_has_visible_activity(
    torrent: &TorrentDisplayState,
    relative_path: &str,
    is_dir: bool,
    text_len: usize,
) -> bool {
    let download_wave = file_activity_wave_profile(torrent.smoothed_download_speed_bps, text_len);
    let upload_wave = file_activity_wave_profile(torrent.smoothed_upload_speed_bps, text_len);
    let (download_paths, upload_paths) =
        file_tree_activity_paths(torrent, relative_path, is_dir, download_wave, upload_wave);
    !download_paths.is_empty() || !upload_paths.is_empty()
}

fn normalize_tree_relative_path(path: &Path) -> String {
    path.iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn path_parent_prefix_len(relative_path: &str) -> usize {
    relative_path
        .rsplit_once('/')
        .map(|(prefix, _)| prefix.chars().count() + 1)
        .unwrap_or(0)
}

fn shaped_row_start_offsets(rows: &[String]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(rows.len());
    let mut current = 0usize;
    for (idx, row) in rows.iter().enumerate() {
        offsets.push(current);
        current += row.chars().count();
        if idx + 1 < rows.len() {
            current += 1;
        }
    }
    offsets
}

fn file_tree_activity_paths<'a>(
    torrent: &'a TorrentDisplayState,
    relative_path: &str,
    is_dir: bool,
    download_wave: FileActivityWaveProfile,
    upload_wave: FileActivityWaveProfile,
) -> (Vec<&'a str>, Vec<&'a str>) {
    let mut download_paths = Vec::new();
    let mut upload_paths = Vec::new();
    let root_path_char_len = torrent_root_logical_len(torrent);

    for (activity_path, activity) in &torrent.recent_file_activity {
        let matches_row = if is_dir && relative_path.is_empty() {
            true
        } else if is_dir {
            activity_path == relative_path
                || activity_path.starts_with(&format!("{relative_path}/"))
        } else {
            activity_path == relative_path
        };

        if !matches_row {
            continue;
        }

        let total_len = root_path_char_len
            + if activity_path.is_empty() {
                0
            } else {
                1 + activity_path.chars().count()
            };

        if activity
            .download_at
            .is_some_and(|seen_at| file_activity_is_visible(seen_at, total_len, download_wave))
        {
            download_paths.push(activity_path.as_str());
        }
        if activity
            .upload_at
            .is_some_and(|seen_at| file_activity_is_visible(seen_at, total_len, upload_wave))
        {
            upload_paths.push(activity_path.as_str());
        }
    }

    (download_paths, upload_paths)
}

#[derive(Clone, Copy)]
struct FileTreeNameRenderContext<'a> {
    download_phase: f64,
    upload_phase: f64,
    row_start_offset: usize,
    base_style: Style,
    ctx: &'a ThemeContext,
}

fn render_file_tree_name_spans(
    torrent: &TorrentDisplayState,
    relative_path: &str,
    display_name: &str,
    is_dir: bool,
    render_ctx: FileTreeNameRenderContext<'_>,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = display_name.chars().collect();
    let len = chars.len().max(1);
    let download_wave = file_activity_wave_profile(torrent.smoothed_download_speed_bps, len);
    let upload_wave = file_activity_wave_profile(torrent.smoothed_upload_speed_bps, len);
    let (download_paths, upload_paths) =
        file_tree_activity_paths(torrent, relative_path, is_dir, download_wave, upload_wave);
    let row_active = !download_paths.is_empty() || !upload_paths.is_empty();
    let active_base_style = render_ctx.ctx.apply(render_ctx.base_style);
    let inactive_base_style = render_ctx.ctx.apply(
        render_ctx
            .base_style
            .fg(render_ctx.ctx.theme.semantic.surface1),
    );

    if !row_active {
        return vec![Span::styled(display_name.to_string(), inactive_base_style)];
    }

    let download_step = render_ctx.download_phase.floor() as usize;
    let upload_step = render_ctx.upload_phase.floor() as usize;
    let root_path_char_len = torrent_root_logical_len(torrent);

    chars
        .into_iter()
        .enumerate()
        .map(|(idx, ch)| {
            let download_hit = download_paths.iter().any(|path| {
                file_activity_wave_hits(
                    path,
                    render_ctx.row_start_offset + idx,
                    root_path_char_len,
                    download_wave,
                    download_step,
                    false,
                )
            });
            let upload_hit = upload_paths.iter().any(|path| {
                file_activity_wave_hits(
                    path,
                    render_ctx.row_start_offset + idx,
                    root_path_char_len,
                    upload_wave,
                    upload_step,
                    true,
                )
            });

            let style = match (download_hit, upload_hit) {
                (true, true) => render_ctx.ctx.apply(
                    render_ctx
                        .base_style
                        .fg(render_ctx.ctx.state_selected())
                        .add_modifier(Modifier::BOLD),
                ),
                (true, false) => render_ctx.ctx.apply(
                    render_ctx
                        .base_style
                        .fg(render_ctx.ctx.state_info())
                        .add_modifier(Modifier::BOLD),
                ),
                (false, true) => render_ctx.ctx.apply(
                    render_ctx
                        .base_style
                        .fg(render_ctx.ctx.state_success())
                        .add_modifier(Modifier::BOLD),
                ),
                (false, false) => active_base_style,
            };
            Span::styled(ch.to_string(), style)
        })
        .collect()
}

fn torrent_root_logical_len(torrent: &TorrentDisplayState) -> usize {
    torrent
        .latest_state
        .download_path
        .as_ref()
        .map(|path| path.to_string_lossy().chars().count())
        .unwrap_or_else(|| torrent.latest_state.torrent_name.chars().count())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct FileActivityWaveProfile {
    band_width: usize,
    steps_per_second: f64,
}

fn file_activity_wave_cycle_duration(total_len: usize, wave: FileActivityWaveProfile) -> Duration {
    Duration::from_secs_f64(
        file_activity_wave_cycle_len(total_len) as f64 / wave.steps_per_second.max(1.0),
    )
}

fn file_activity_is_visible(
    seen_at: Instant,
    total_len: usize,
    wave: FileActivityWaveProfile,
) -> bool {
    seen_at.elapsed()
        <= FILE_ACTIVITY_HIGHLIGHT_WINDOW + file_activity_wave_cycle_duration(total_len, wave)
}

fn file_activity_wave_profile(speed_bps: u64, text_len: usize) -> FileActivityWaveProfile {
    let target_band_width = if speed_bps < 500_000 {
        4 + usize::from(speed_bps >= 50_000)
    } else if speed_bps < 20_000_000 {
        5 + usize::from(speed_bps >= 2_000_000)
    } else if speed_bps < 100_000_000 {
        7 + usize::from(speed_bps >= 50_000_000)
    } else {
        9
    };

    FileActivityWaveProfile {
        band_width: target_band_width.min(text_len.max(1)),
        steps_per_second: file_activity_wave_steps_per_second(speed_bps),
    }
}

fn file_activity_wave_cycle_len(total_len: usize) -> usize {
    total_len + FILE_ACTIVITY_MAX_BAND_WIDTH
}

fn file_activity_wave_hits(
    relative_path: &str,
    global_char_idx: usize,
    root_path_char_len: usize,
    wave: FileActivityWaveProfile,
    step: usize,
    left_to_right: bool,
) -> bool {
    let total_len = root_path_char_len
        + if relative_path.is_empty() {
            0
        } else {
            1 + relative_path.chars().count()
        };
    let cycle_len = file_activity_wave_cycle_len(total_len);
    let head = step % cycle_len;
    let logical_idx = if left_to_right {
        global_char_idx
    } else {
        total_len.saturating_sub(1).saturating_sub(global_char_idx)
    };

    (head as isize - logical_idx as isize) >= 0
        && (head as isize - logical_idx as isize) < wave.band_width as isize
}

fn torrent_root_path_label(metrics: &crate::app::TorrentMetrics, anonymize: bool) -> String {
    let Some(download_path) = metrics.download_path.as_ref() else {
        return if anonymize {
            anonymize_preserving_shape(&metrics.torrent_name)
        } else {
            metrics.torrent_name.clone()
        };
    };

    let display = download_path.to_string_lossy().to_string();
    if anonymize {
        anonymize_preserving_shape(&display)
    } else {
        display
    }
}

fn split_path_components(path: &str) -> Vec<String> {
    let separator = path_separator(path);
    path.split(separator)
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.to_string())
        .collect()
}

fn path_separator(path: &str) -> char {
    if path.contains('\\') || path.chars().nth(1).is_some_and(|ch| ch == ':') {
        '\\'
    } else {
        '/'
    }
}

fn path_root_prefix(path: &str, separator: char) -> Option<&'static str> {
    (separator == '/' && path.starts_with('/')).then_some("/")
}

fn append_path_component(base: &str, component: &str, separator: char) -> String {
    if base.is_empty() {
        component.to_string()
    } else if base == "/" {
        format!("/{}", component)
    } else {
        format!("{}{}{}", base, separator, component)
    }
}

fn render_path_slices(
    prefix: Option<&str>,
    left: &[String],
    right: &[String],
    separator: char,
) -> String {
    let separator_str = separator.to_string();
    let left_joined = left.join(&separator_str);
    let right_joined = right.join(&separator_str);

    match prefix {
        Some(prefix) if left_joined.is_empty() => {
            format!("{}...{}{}", prefix, separator, right_joined)
        }
        Some(prefix) => format!(
            "{}{}{}...{}{}",
            prefix, left_joined, separator, separator, right_joined
        ),
        None => format!(
            "{}{}...{}{}",
            left_joined, separator, separator, right_joined
        ),
    }
}

fn truncate_path_component(component: &str, width: usize) -> String {
    truncate_with_ellipsis(component, width.max(1))
}

fn middle_ellipsize_path(path: &str, width: usize) -> String {
    if path.chars().count() <= width {
        return path.to_string();
    }
    if width <= 3 {
        return ".".repeat(width);
    }

    let components = split_path_components(path);
    if components.len() <= 1 {
        return truncate_with_ellipsis(path, width);
    }

    let separator = path_separator(path);
    let prefix = path_root_prefix(path, separator);
    let render =
        |left: &[String], right: &[String]| render_path_slices(prefix, left, right, separator);

    let mut left = vec![components[0].clone()];
    let mut right = vec![components[components.len() - 1].clone()];
    let mut left_idx = 1usize;
    let mut right_idx = components.len() - 1;

    let initial = render(&left, &right);
    if initial.chars().count() > width {
        return truncate_with_ellipsis(&initial, width);
    }

    let mut best = initial;
    while left_idx < right_idx {
        let try_left = {
            let mut next_left = left.clone();
            next_left.push(components[left_idx].clone());
            render(&next_left, &right)
        };
        let try_right = {
            let mut next_right = right.clone();
            next_right.insert(0, components[right_idx - 1].clone());
            render(&left, &next_right)
        };

        let left_fits = try_left.chars().count() <= width;
        let right_fits = try_right.chars().count() <= width;

        match (left_fits, right_fits) {
            (false, false) => break,
            (true, false) => {
                left.push(components[left_idx].clone());
                left_idx += 1;
                best = try_left;
            }
            (false, true) => {
                right_idx -= 1;
                right.insert(0, components[right_idx].clone());
                best = try_right;
            }
            (true, true) => {
                if try_left.chars().count() >= try_right.chars().count() {
                    left.push(components[left_idx].clone());
                    left_idx += 1;
                    best = try_left;
                } else {
                    right_idx -= 1;
                    right.insert(0, components[right_idx].clone());
                    best = try_right;
                }
            }
        }
    }

    best
}

fn shape_root_path_for_viewport(path: &str, width: usize, height: usize) -> Vec<String> {
    if path.is_empty() || width == 0 || height == 0 {
        return Vec::new();
    }

    if path.chars().count() <= width {
        return vec![path.to_string()];
    }

    let components = split_path_components(path);
    if components.is_empty() {
        return vec![truncate_with_ellipsis(path, width.max(1))];
    }

    if height == 1 {
        return vec![middle_ellipsize_path(path, width)];
    }

    let mut rows: Vec<String> = Vec::new();
    let separator = path_separator(path);
    let prefix = path_root_prefix(path, separator).unwrap_or_default();
    let mut current = prefix.to_string();

    for component in components {
        let candidate = append_path_component(&current, &component, separator);

        if candidate.chars().count() <= width {
            current = candidate;
            continue;
        }

        if !current.is_empty() {
            rows.push(std::mem::take(&mut current));
            if rows.len() == height {
                break;
            }
        }

        let component_with_prefix = append_path_component(prefix, &component, separator);
        if component_with_prefix.chars().count() <= width && !prefix.is_empty() && rows.is_empty() {
            current = component_with_prefix;
        } else if component.chars().count() <= width {
            current = component;
        } else {
            rows.push(truncate_path_component(&component, width));
            current = String::new();
            if rows.len() == height {
                break;
            }
        }
    }

    if rows.len() < height && !current.is_empty() {
        rows.push(current);
    }

    if rows.len() > height {
        rows.truncate(height);
    }

    if rows.is_empty() {
        vec![truncate_with_ellipsis(path, width.max(1))]
    } else {
        rows
    }
}

fn anonymize_tree_name(name: &str, is_dir: bool, anonymize: bool) -> String {
    if !anonymize {
        return sanitize_text(name);
    }

    let _ = is_dir;
    anonymize_preserving_shape(name)
}

fn peer_has_all_pieces(peer: &PeerInfo, total_pieces: usize) -> bool {
    total_pieces > 0
        && peer
            .bitfield
            .iter()
            .take(total_pieces)
            .filter(|&&has| has)
            .count()
            == total_pieces
}

fn peer_has_piece(peer: &PeerInfo, piece_index: usize) -> bool {
    peer.bitfield.get(piece_index).copied().unwrap_or(false)
}

fn swarm_heatmap_display_availability_counts(
    peers: &[PeerInfo],
    total_pieces: usize,
) -> (Vec<u32>, bool) {
    let mut availability = vec![0; total_pieces];
    let mut has_complete_peer = false;

    for peer in peers {
        if peer_has_all_pieces(peer, total_pieces) {
            has_complete_peer = true;
            continue;
        }

        for (idx, has_piece) in peer.bitfield.iter().enumerate().take(total_pieces) {
            if *has_piece {
                availability[idx] += 1;
            }
        }
    }

    (availability, has_complete_peer)
}

fn swarm_heatmap_level(count: u32, max_avail: u32) -> SwarmHeatmapLevel {
    if count == 0 {
        return SwarmHeatmapLevel::Empty;
    }

    let max_avail = max_avail.max(1);
    if count >= max_avail {
        return SwarmHeatmapLevel::High;
    }

    let low_cutoff = (max_avail as f64 / 3.0).ceil() as u32;
    let medium_cutoff = (max_avail as f64 * 2.0 / 3.0).ceil() as u32;

    if count <= low_cutoff {
        SwarmHeatmapLevel::Low
    } else if count <= medium_cutoff {
        SwarmHeatmapLevel::Medium
    } else {
        SwarmHeatmapLevel::High
    }
}

fn swarm_heatmap_flash_peer(
    peers: &[PeerInfo],
    total_pieces: usize,
    piece_index: usize,
) -> Option<&PeerInfo> {
    peers
        .iter()
        .filter(|peer| {
            !peer_has_all_pieces(peer, total_pieces) && peer_has_piece(peer, piece_index)
        })
        .min_by(|a, b| {
            let a_inactive = peer_is_inactive_for_table(a);
            let b_inactive = peer_is_inactive_for_table(b);
            a_inactive
                .cmp(&b_inactive)
                .then_with(|| a.address.cmp(&b.address))
        })
}

fn swarm_heatmap_flash_color(
    ctx: &ThemeContext,
    peers: &[PeerInfo],
    total_pieces: usize,
    piece_index: usize,
    heatmap_block_color: Color,
) -> Color {
    let Some(peer) = swarm_heatmap_flash_peer(peers, total_pieces, piece_index) else {
        return ctx.theme.semantic.white;
    };

    if peer_is_inactive_for_table(peer) {
        ctx.theme.semantic.white
    } else {
        let peer_color = ip_to_color(ctx, &peer.address);
        if peer_color == heatmap_block_color {
            ctx.theme.semantic.white
        } else {
            peer_color
        }
    }
}

fn swarm_heatmap_flash_tone(
    level: SwarmHeatmapLevel,
    flash_new: bool,
) -> Option<SwarmHeatmapFlashTone> {
    if !flash_new || matches!(level, SwarmHeatmapLevel::Empty) {
        return None;
    }

    Some(SwarmHeatmapFlashTone::Regular)
}

fn draw_swarm_heatmap(
    f: &mut Frame,
    ctx: &ThemeContext,
    peers: &[PeerInfo],
    total_pieces: u32,
    area: Rect,
    flash: Option<SwarmHeatmapFlash<'_>>,
) {
    let color_status_low = ctx.apply(
        Style::default()
            .fg(ctx.state_error())
            .add_modifier(Modifier::DIM),
    );
    let color_status_medium = ctx.apply(
        Style::default()
            .fg(ctx.state_warning())
            .add_modifier(Modifier::DIM),
    );
    let color_status_high = ctx.apply(
        Style::default()
            .fg(ctx.state_info())
            .add_modifier(Modifier::DIM),
    );
    let color_status_complete = ctx.apply(
        Style::default()
            .fg(ctx.state_complete())
            .add_modifier(Modifier::BOLD),
    );
    let color_status_empty = ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1));
    let color_status_waiting = ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1));

    let color_heatmap_low = ctx.theme.scale.heatmap.low;
    let color_heatmap_medium = ctx.theme.scale.heatmap.medium;
    let color_heatmap_high = ctx.theme.scale.heatmap.high;
    let color_heatmap_empty = ctx.theme.scale.heatmap.empty;

    let shade_light = symbols::shade::LIGHT;
    let shade_medium = symbols::shade::MEDIUM;
    let shade_dark = symbols::shade::DARK;

    let availability = swarm_availability_counts(peers, total_pieces);
    let total_pieces_usize = availability.len();
    let (display_availability, _has_complete_peer) =
        swarm_heatmap_display_availability_counts(peers, total_pieces_usize);

    let max_avail = availability.iter().max().copied().unwrap_or(0);
    let display_max_avail = display_availability.iter().max().copied().unwrap_or(0);
    let pieces_available_in_swarm = availability.iter().filter(|&&count| count > 0).count();
    let is_swarm_complete =
        total_pieces_usize > 0 && pieces_available_in_swarm == total_pieces_usize;
    let total_peers = peers.len();

    let (status_text, status_style) = if total_pieces_usize == 0 {
        ("Waiting...".to_string(), color_status_waiting)
    } else if is_swarm_complete {
        ("Complete".to_string(), color_status_complete)
    } else if max_avail == 0 {
        ("Empty".to_string(), color_status_empty)
    } else if total_peers == 0 {
        ("Low (0%)".to_string(), color_status_low)
    } else {
        let availability_percentage =
            (pieces_available_in_swarm as f64 / total_pieces_usize as f64) * 100.0;
        if availability_percentage < 33.3 {
            (
                format!("Low ({:.0}%)", availability_percentage),
                color_status_low,
            )
        } else if availability_percentage < 66.6 {
            (
                format!("Medium ({:.0}%)", availability_percentage),
                color_status_medium,
            )
        } else {
            (
                format!("High ({:.0}%)", availability_percentage),
                color_status_high,
            )
        }
    };

    let title = Line::from(vec![
        Span::styled(
            " Swarm Availability: ",
            ctx.apply(Style::default().fg(ctx.state_complete())),
        ),
        Span::styled(status_text, status_style),
    ]);
    let block = Block::default()
        .title(title)
        .borders(Borders::NONE)
        .padding(Padding::new(1, 1, 0, 1))
        .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)));
    let inner_area = block.inner(area);
    f.render_widget(block, area);

    if total_pieces_usize == 0 {
        let available_width = inner_area.width as usize;
        let available_height = inner_area.height as usize;
        let mut lines = Vec::with_capacity(available_height);

        for _ in 0..available_height {
            let row_str = shade_light.repeat(available_width);
            lines.push(Line::from(Span::styled(
                row_str,
                ctx.apply(Style::default().fg(ctx.theme.semantic.surface1)),
            )));
        }

        let heatmap = Paragraph::new(lines);
        f.render_widget(heatmap, inner_area);
        return;
    }

    let available_width = inner_area.width as usize;
    let available_height = inner_area.height as usize;
    let total_cells = (available_width * available_height) as u64;

    if total_cells == 0 {
        return;
    }

    let mut lines = Vec::with_capacity(available_height);
    let total_pieces_u64 = total_pieces_usize as u64;

    for y in 0..available_height {
        let mut spans = Vec::with_capacity(available_width);
        for x in 0..available_width {
            let cell_index = (y * available_width + x) as u64;
            let piece_index = ((cell_index * total_pieces_u64) / total_cells) as usize;
            if piece_index >= total_pieces_usize {
                spans.push(Span::raw(" "));
                continue;
            }
            let display_count = display_availability[piece_index];
            let (piece_char, style) = if display_count == 0 {
                (
                    shade_light,
                    ctx.apply(Style::default().fg(color_heatmap_empty)),
                )
            } else {
                let level = swarm_heatmap_level(display_count, display_max_avail);
                let (piece_char, color) = match level {
                    SwarmHeatmapLevel::Empty => (shade_light, color_heatmap_empty),
                    SwarmHeatmapLevel::Low => (shade_light, color_heatmap_low),
                    SwarmHeatmapLevel::Medium => (shade_medium, color_heatmap_medium),
                    SwarmHeatmapLevel::High => (shade_dark, color_heatmap_high),
                };
                let flash_new = flash.is_some_and(|flash| {
                    flash
                        .state
                        .is_piece_flashing(flash.info_hash, piece_index, flash.now)
                });
                if let Some(tone) = swarm_heatmap_flash_tone(level, flash_new) {
                    let flash_color = swarm_heatmap_flash_color(
                        ctx,
                        peers,
                        total_pieces_usize,
                        piece_index,
                        color,
                    );
                    let style = match tone {
                        SwarmHeatmapFlashTone::Regular => Style::default().fg(flash_color),
                    };
                    (shade_dark, ctx.apply(style))
                } else {
                    (piece_char, ctx.apply(Style::default().fg(color)))
                }
            };
            spans.push(Span::styled(piece_char.to_string(), style));
        }
        lines.push(Line::from(spans));
    }
    let heatmap = Paragraph::new(lines);
    f.render_widget(heatmap, inner_area);
}

#[cfg(test)]
pub(crate) fn handle_navigation(app_state: &mut AppState, key_code: KeyCode) {
    handle_navigation_with_layout_mode(app_state, key_code, UiLayoutMode::Auto);
}

pub(crate) fn handle_navigation_with_layout_mode(
    app_state: &mut AppState,
    key_code: KeyCode,
    layout_mode: UiLayoutMode,
) {
    let selected_torrent = app_state
        .torrent_list_order
        .get(app_state.ui.selected_torrent_index)
        .and_then(|info_hash| app_state.torrents.get(info_hash));

    let selected_torrent_has_peers = selected_torrent_has_peers(app_state);

    let selected_torrent_peer_count =
        selected_torrent.map_or(0, |torrent| torrent.latest_state.peers.len());

    let layout_plan = calculate_header_interaction_layout(app_state, layout_mode);
    let (_, visible_torrent_columns) =
        compute_visible_torrent_columns(app_state, layout_plan.list.width);
    let (_, visible_peer_columns) =
        compute_visible_peer_columns(app_state, layout_plan.peers.width);

    app_state.ui.selected_header = normalize_selected_header(
        app_state.ui.selected_header,
        selected_torrent_has_peers,
        &visible_torrent_columns,
        &visible_peer_columns,
    );

    match key_code {
        KeyCode::Up | KeyCode::Char('k') => match app_state.ui.selected_header {
            SelectedHeader::Torrent(_) => {
                app_state.ui.selected_torrent_index =
                    app_state.ui.selected_torrent_index.saturating_sub(1);
                app_state.ui.selected_peer_index = 0;
            }
            SelectedHeader::Peer(_) => {
                app_state.ui.selected_peer_index =
                    app_state.ui.selected_peer_index.saturating_sub(1);
            }
        },
        KeyCode::Down | KeyCode::Char('j') => match app_state.ui.selected_header {
            SelectedHeader::Torrent(_) => {
                if !app_state.torrent_list_order.is_empty() {
                    let new_index = app_state.ui.selected_torrent_index.saturating_add(1);
                    if new_index < app_state.torrent_list_order.len() {
                        app_state.ui.selected_torrent_index = new_index;
                    }
                }
                app_state.ui.selected_peer_index = 0;
            }
            SelectedHeader::Peer(_) => {
                if selected_torrent_peer_count > 0 {
                    let new_index = app_state.ui.selected_peer_index.saturating_add(1);
                    if new_index < selected_torrent_peer_count {
                        app_state.ui.selected_peer_index = new_index;
                    }
                }
            }
        },
        KeyCode::PageUp => match app_state.ui.selected_header {
            SelectedHeader::Torrent(_) => {
                let page = normal_torrent_page_rows(layout_plan.list.height);
                app_state.ui.selected_torrent_index =
                    app_state.ui.selected_torrent_index.saturating_sub(page);
                app_state.ui.selected_peer_index = 0;
            }
            SelectedHeader::Peer(_) => {
                let page = normal_torrent_page_rows(layout_plan.peers.height);
                app_state.ui.selected_peer_index =
                    app_state.ui.selected_peer_index.saturating_sub(page);
            }
        },
        KeyCode::PageDown => match app_state.ui.selected_header {
            SelectedHeader::Torrent(_) => {
                if !app_state.torrent_list_order.is_empty() {
                    let page = normal_torrent_page_rows(layout_plan.list.height);
                    app_state.ui.selected_torrent_index = app_state
                        .ui
                        .selected_torrent_index
                        .saturating_add(page)
                        .min(app_state.torrent_list_order.len() - 1);
                }
                app_state.ui.selected_peer_index = 0;
            }
            SelectedHeader::Peer(_) => {
                if selected_torrent_peer_count > 0 {
                    let page = normal_torrent_page_rows(layout_plan.peers.height);
                    app_state.ui.selected_peer_index = app_state
                        .ui
                        .selected_peer_index
                        .saturating_add(page)
                        .min(selected_torrent_peer_count - 1);
                }
            }
        },
        KeyCode::Home => match app_state.ui.selected_header {
            SelectedHeader::Torrent(_) => {
                app_state.ui.selected_torrent_index = 0;
                app_state.ui.selected_peer_index = 0;
            }
            SelectedHeader::Peer(_) => {
                app_state.ui.selected_peer_index = 0;
            }
        },
        KeyCode::End => match app_state.ui.selected_header {
            SelectedHeader::Torrent(_) => {
                if !app_state.torrent_list_order.is_empty() {
                    app_state.ui.selected_torrent_index = app_state.torrent_list_order.len() - 1;
                }
                app_state.ui.selected_peer_index = 0;
            }
            SelectedHeader::Peer(_) => {
                if selected_torrent_peer_count > 0 {
                    app_state.ui.selected_peer_index = selected_torrent_peer_count - 1;
                }
            }
        },
        KeyCode::Left | KeyCode::Char('h') => {
            app_state.ui.selected_header = match app_state.ui.selected_header {
                SelectedHeader::Torrent(column_id) => {
                    let real_idx = torrent_column_index(column_id).unwrap_or(0);
                    let pos = visible_torrent_columns
                        .iter()
                        .position(|&idx| idx == real_idx)
                        .unwrap_or(0);
                    if pos > 0 {
                        torrent_column_id_for_index(visible_torrent_columns[pos - 1])
                            .map(SelectedHeader::Torrent)
                            .unwrap_or(SelectedHeader::Torrent(column_id))
                    } else {
                        SelectedHeader::Torrent(column_id)
                    }
                }
                SelectedHeader::Peer(column_id) => {
                    let real_idx = peer_column_index(column_id).unwrap_or(0);
                    let pos = visible_peer_columns
                        .iter()
                        .position(|&idx| idx == real_idx)
                        .unwrap_or(0);
                    if pos > 0 {
                        peer_column_id_for_index(visible_peer_columns[pos - 1])
                            .map(SelectedHeader::Peer)
                            .unwrap_or(SelectedHeader::Peer(column_id))
                    } else {
                        visible_torrent_columns
                            .last()
                            .copied()
                            .and_then(torrent_column_id_for_index)
                            .map(SelectedHeader::Torrent)
                            .unwrap_or(SelectedHeader::Torrent(ColumnId::Name))
                    }
                }
            };
        }
        KeyCode::Right | KeyCode::Char('l') => {
            app_state.ui.selected_header = match app_state.ui.selected_header {
                SelectedHeader::Torrent(column_id) => {
                    let real_idx = torrent_column_index(column_id).unwrap_or(0);
                    let pos = visible_torrent_columns
                        .iter()
                        .position(|&idx| idx == real_idx)
                        .unwrap_or(0);
                    if pos + 1 < visible_torrent_columns.len() {
                        torrent_column_id_for_index(visible_torrent_columns[pos + 1])
                            .map(SelectedHeader::Torrent)
                            .unwrap_or(SelectedHeader::Torrent(column_id))
                    } else if selected_torrent_has_peers {
                        visible_peer_columns
                            .first()
                            .copied()
                            .and_then(peer_column_id_for_index)
                            .map(SelectedHeader::Peer)
                            .unwrap_or(SelectedHeader::Torrent(column_id))
                    } else {
                        SelectedHeader::Torrent(column_id)
                    }
                }
                SelectedHeader::Peer(column_id) => {
                    let real_idx = peer_column_index(column_id).unwrap_or(0);
                    let pos = visible_peer_columns
                        .iter()
                        .position(|&idx| idx == real_idx)
                        .unwrap_or(0);
                    if pos + 1 < visible_peer_columns.len() {
                        peer_column_id_for_index(visible_peer_columns[pos + 1])
                            .map(SelectedHeader::Peer)
                            .unwrap_or(SelectedHeader::Peer(column_id))
                    } else {
                        SelectedHeader::Peer(column_id)
                    }
                }
            };
        }
        _ => {}
    }
}

fn normal_torrent_page_rows(area_height: u16) -> usize {
    area_height.saturating_sub(3).max(1) as usize
}

fn handle_search_key(key_code: KeyCode, app_state: &mut AppState) -> bool {
    if !matches!(app_state.mode, AppMode::Normal) || !app_state.ui.is_searching {
        return false;
    }

    match key_code {
        KeyCode::Esc => {
            app_state.ui.is_searching = false;
            app_state.ui.search_query.clear();
            sort_and_filter_torrent_list_state(app_state);
            app_state.ui.selected_torrent_index = 0;
        }
        KeyCode::Enter => {
            app_state.ui.is_searching = false;
        }
        KeyCode::Backspace => {
            app_state.ui.search_query.pop();
            sort_and_filter_torrent_list_state(app_state);
            app_state.ui.selected_torrent_index = 0;
        }
        KeyCode::Char(c) => {
            app_state.ui.search_query.push(c);
            sort_and_filter_torrent_list_state(app_state);
            app_state.ui.selected_torrent_index = 0;
        }
        _ => {}
    }

    true
}

#[derive(Default)]
pub struct NormalHandleResult {
    pub consumed: bool,
    pub redraw: bool,
    pub effects: Vec<UiEffect>,
}

pub fn handle_event(
    event: CrosstermEvent,
    app_state: &mut AppState,
    layout_mode: UiLayoutMode,
) -> NormalHandleResult {
    match event {
        CrosstermEvent::Key(key) if key.kind == KeyEventKind::Press => {
            handle_key_press(key, app_state, layout_mode)
        }
        CrosstermEvent::Paste(pasted_text) => reduce_normal_action(
            app_state,
            UiAction::PasteText(pasted_text.trim().to_string()),
            UiLayoutMode::Auto,
        ),
        _ => NormalHandleResult::default(),
    }
}

fn handle_key_press(
    key: KeyEvent,
    app_state: &mut AppState,
    layout_mode: UiLayoutMode,
) -> NormalHandleResult {
    let focus_changed = deactivate_visualization_focus_if_hidden(app_state, layout_mode);
    if focus_changed {
        app_state.ui.needs_redraw = true;
    }

    if app_state.ui.visualization_focus.active {
        if let Some(action) = map_visualization_focus_key(key) {
            return reduce_normal_action(app_state, action, layout_mode);
        }
        return NormalHandleResult {
            consumed: true,
            redraw: focus_changed,
            effects: Vec::new(),
        };
    }

    if handle_search_key(key.code, app_state) {
        app_state.ui.needs_redraw = true;
        return NormalHandleResult {
            consumed: true,
            redraw: true,
            effects: Vec::new(),
        };
    }

    let Some(action) = map_key_to_ui_action(key) else {
        return NormalHandleResult {
            consumed: false,
            redraw: focus_changed,
            effects: Vec::new(),
        };
    };
    reduce_normal_action(app_state, action, layout_mode)
}

fn reduce_normal_action(
    app_state: &mut AppState,
    action: UiAction,
    layout_mode: UiLayoutMode,
) -> NormalHandleResult {
    let result = if layout_mode == UiLayoutMode::Auto {
        reduce_ui_action(app_state, action)
    } else {
        reduce_ui_action_with_layout_mode(app_state, action, layout_mode)
    };
    if result.redraw {
        app_state.ui.needs_redraw = true;
    }
    NormalHandleResult {
        consumed: true,
        redraw: result.redraw,
        effects: result.effects,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{
        App, AppCommand, AppState, BrowserSearchState, DataRate, FileBrowserMode, PeerInfo,
        RssScreen, SelectedHeader, TorrentControlState, TorrentDisplayState, TorrentMetrics,
    };
    use crate::config::{PeerSortColumn, SortDirection, TorrentSortColumn};
    use crate::errors::StorageError;
    use crate::integrations::control::ControlRequest;
    use crate::theme::{Theme, ThemeContext, ThemeName};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn sort_direction_arrows_show_highest_first_rates_as_down() {
        assert_eq!(
            sort_direction_arrow_for_torrent_column(
                TorrentSortColumn::Down,
                SortDirection::Descending
            ),
            " ▼"
        );
        assert_eq!(
            sort_direction_arrow_for_torrent_column(
                TorrentSortColumn::Up,
                SortDirection::Descending
            ),
            " ▼"
        );
        assert_eq!(
            sort_direction_arrow_for_peer_column(PeerSortColumn::DL, SortDirection::Descending),
            " ▼"
        );
        assert_eq!(
            sort_direction_arrow_for_peer_column(PeerSortColumn::UL, SortDirection::Descending),
            " ▼"
        );
        assert_eq!(
            sort_direction_arrow_for_torrent_column(
                TorrentSortColumn::Name,
                SortDirection::Ascending
            ),
            " ▼"
        );
        assert_eq!(
            sort_direction_arrow_for_torrent_column(
                TorrentSortColumn::Name,
                SortDirection::Descending
            ),
            " ▲"
        );
    }

    fn create_mock_metrics(peer_count: usize) -> TorrentMetrics {
        let mut peers = Vec::new();
        for i in 0..peer_count {
            peers.push(PeerInfo {
                address: format!("127.0.0.1:{}", 6881 + i),
                ..Default::default()
            });
        }
        TorrentMetrics {
            data_available: true,
            is_complete: true,
            number_of_pieces_total: 1,
            number_of_pieces_completed: 1,
            peers,
            ..Default::default()
        }
    }

    fn create_mock_display_state(peer_count: usize) -> TorrentDisplayState {
        TorrentDisplayState {
            latest_state: create_mock_metrics(peer_count),
            ..Default::default()
        }
    }

    fn create_test_app_state() -> AppState {
        let mut app_state = AppState {
            screen_area: ratatui::layout::Rect::new(0, 0, 200, 100),
            ..Default::default()
        };

        let torrent_a = create_mock_display_state(2);
        let torrent_b = create_mock_display_state(0);

        app_state
            .torrents
            .insert("hash_a".as_bytes().to_vec(), torrent_a);
        app_state
            .torrents
            .insert("hash_b".as_bytes().to_vec(), torrent_b);
        app_state.torrent_list_order =
            vec!["hash_a".as_bytes().to_vec(), "hash_b".as_bytes().to_vec()];

        app_state
    }

    #[test]
    fn peer_address_formatter_omits_ipv6_brackets_in_table() {
        assert_eq!(
            format_peer_address_for_table("[2001:db8::1]:51413"),
            "2001:db8::1:51413"
        );
        assert_eq!(
            format_peer_address_for_table("127.0.0.1:6881"),
            "127.0.0.1:6881"
        );
    }

    #[test]
    fn reducer_start_search_sets_search_and_resets_selection() {
        let mut app_state = AppState::default();
        app_state.ui.is_searching = false;
        app_state.ui.selected_torrent_index = 7;

        let result = reduce_ui_action(&mut app_state, UiAction::StartSearch);

        assert!(result.redraw);
        assert!(app_state.ui.is_searching);
        assert_eq!(app_state.ui.selected_torrent_index, 0);
    }

    #[test]
    fn reducer_start_search_keeps_browser_search_state_intact() {
        let mut app_state = AppState::default();
        app_state.ui.file_browser.search_state = BrowserSearchState::Editing;
        app_state.ui.file_browser.search_query = "downloads".to_string();

        let result = reduce_ui_action(&mut app_state, UiAction::StartSearch);

        assert!(result.redraw);
        assert!(app_state.ui.is_searching);
        assert_eq!(
            app_state.ui.file_browser.search_state,
            BrowserSearchState::Editing
        );
        assert_eq!(app_state.ui.file_browser.search_query, "downloads");
    }

    #[test]
    fn reducer_clear_system_error_clears_error() {
        let mut app_state = AppState {
            system_error: Some("boom".to_string()),
            ..Default::default()
        };

        let result = reduce_ui_action(&mut app_state, UiAction::ClearSystemError);

        assert!(result.redraw);
        assert!(app_state.system_error.is_none());
    }

    #[test]
    fn visualization_focus_includes_visible_components_with_multiple_renderers() {
        let mut app_state = AppState {
            screen_area: Rect::new(0, 0, 200, 60),
            ..Default::default()
        };

        let plan = calculate_header_interaction_layout(&app_state, UiLayoutMode::Horizontal);
        let panels = visualization_focus_panels(&app_state, &plan);
        let eligible = panels
            .iter()
            .map(|(panel, _)| *panel)
            .collect::<HashSet<_>>();
        assert!(eligible.contains(&VisualizationFocusPanel::PeerStream));
        assert!(eligible.contains(&VisualizationFocusPanel::DhtWave));
        assert!(eligible.contains(&VisualizationFocusPanel::DiskHealth));
        assert_eq!(
            visualization_focus_selection_count(VisualizationFocusPanel::Chart),
            1
        );
        assert_eq!(
            visualization_focus_selection_count(VisualizationFocusPanel::PeerStream),
            PeerStreamVisualization::ALL.len()
        );
        assert_eq!(
            visualization_focus_selection_count(VisualizationFocusPanel::DiskHealth),
            DiskHealthVisualization::ALL.len()
        );
        assert_eq!(
            visualization_focus_selection_count(VisualizationFocusPanel::DhtWave),
            DhtVisualization::ALL.len()
        );

        reduce_ui_action_with_layout_mode(
            &mut app_state,
            UiAction::ToggleVisualizationFocus,
            UiLayoutMode::Horizontal,
        );
        assert!(app_state.ui.visualization_focus.active);
        assert_eq!(
            app_state.ui.visualization_focus.selected,
            VisualizationFocusPanel::PeerStream
        );
        assert!(app_state.system_error.is_none());

        let cycle_result = reduce_ui_action_with_layout_mode(
            &mut app_state,
            UiAction::VisualizationRendererNext,
            UiLayoutMode::Horizontal,
        );
        assert_eq!(
            cycle_result.effects,
            vec![UiEffect::PersistVisualizationSelections]
        );
        assert_eq!(
            app_state.ui.visualization_focus.peer_stream,
            PeerStreamVisualization::HelixExchange
        );
        let reset_result = reduce_ui_action_with_layout_mode(
            &mut app_state,
            UiAction::ResetVisualizationRenderer,
            UiLayoutMode::Horizontal,
        );
        assert_eq!(
            reset_result.effects,
            vec![UiEffect::PersistVisualizationSelections]
        );
        assert_eq!(
            app_state.ui.visualization_focus.peer_stream,
            PeerStreamVisualization::Classic
        );

        app_state.ui.visualization_focus.selected = VisualizationFocusPanel::DhtWave;
        reduce_ui_action_with_layout_mode(
            &mut app_state,
            UiAction::VisualizationRendererNext,
            UiLayoutMode::Horizontal,
        );
        assert_eq!(
            app_state.ui.visualization_focus.dht,
            DhtVisualization::RelayRibbon
        );
        reduce_ui_action_with_layout_mode(
            &mut app_state,
            UiAction::ResetVisualizationRenderer,
            UiLayoutMode::Horizontal,
        );
        assert_eq!(
            app_state.ui.visualization_focus.dht,
            DhtVisualization::Classic
        );
    }

    #[test]
    fn visualization_focus_does_not_activate_behind_unrelated_system_error() {
        let mut app_state = AppState {
            screen_area: Rect::new(0, 0, 200, 60),
            system_error: Some("runtime ingest failed".to_string()),
            ..Default::default()
        };

        reduce_ui_action_with_layout_mode(
            &mut app_state,
            UiAction::ToggleVisualizationFocus,
            UiLayoutMode::Horizontal,
        );

        assert!(!app_state.ui.visualization_focus.active);
        assert_eq!(
            app_state.system_error.as_deref(),
            Some("runtime ingest failed")
        );
    }

    #[test]
    fn visualization_focus_deactivates_when_selected_panel_becomes_hidden() {
        let mut app_state = AppState {
            screen_area: Rect::new(0, 0, 200, 60),
            ..Default::default()
        };
        app_state.ui.visualization_focus.active = true;
        app_state.ui.visualization_focus.selected = VisualizationFocusPanel::PeerStream;

        assert!(!deactivate_visualization_focus_if_hidden(
            &mut app_state,
            UiLayoutMode::Horizontal,
        ));
        assert!(app_state.ui.visualization_focus.active);

        app_state.screen_area = Rect::new(0, 0, 80, 60);
        assert!(deactivate_visualization_focus_if_hidden(
            &mut app_state,
            UiLayoutMode::Vertical,
        ));
        assert!(!app_state.ui.visualization_focus.active);
    }

    #[test]
    fn hidden_visualization_focus_does_not_consume_normal_key() {
        let mut app_state = AppState {
            screen_area: Rect::new(0, 0, 80, 60),
            ..AppState::default()
        };
        app_state.ui.visualization_focus.active = true;
        app_state.ui.visualization_focus.selected = VisualizationFocusPanel::PeerStream;

        let handled = handle_key_press(
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            &mut app_state,
            UiLayoutMode::Vertical,
        );

        assert!(handled.consumed);
        assert!(!app_state.ui.visualization_focus.active);
        assert!(app_state.ui.is_searching);
    }

    #[test]
    fn visualization_catalog_matches_retained_lab_slots() {
        assert_eq!(
            PeerStreamVisualization::ALL,
            [
                PeerStreamVisualization::Classic,
                PeerStreamVisualization::HelixExchange,
            ]
        );
        assert_eq!(
            DiskHealthVisualization::ALL,
            [
                DiskHealthVisualization::Classic,
                DiskHealthVisualization::SeekPendulum,
                DiskHealthVisualization::StorageDial,
            ]
        );
        assert_eq!(
            DhtVisualization::ALL,
            [
                DhtVisualization::Classic,
                DhtVisualization::RelayRibbon,
                DhtVisualization::PulseGrid,
                DhtVisualization::LookupVortex,
                DhtVisualization::PeerBloom,
            ]
        );
    }

    #[test]
    fn visualization_focus_registers_every_visible_dashboard_panel() {
        let app_state = AppState {
            screen_area: Rect::new(0, 0, 200, 60),
            ..Default::default()
        };
        let plan = calculate_header_interaction_layout(&app_state, UiLayoutMode::Horizontal);
        let candidates = visualization_focus_panel_candidates(&app_state, &plan);
        let registered = candidates
            .iter()
            .map(|(panel, _)| *panel)
            .collect::<HashSet<_>>();

        assert_eq!(registered.len(), 9);
        for panel in [
            VisualizationFocusPanel::TorrentList,
            VisualizationFocusPanel::TorrentDetails,
            VisualizationFocusPanel::PeerFiles,
            VisualizationFocusPanel::Chart,
            VisualizationFocusPanel::PeerStream,
            VisualizationFocusPanel::BlockStream,
            VisualizationFocusPanel::DhtWave,
            VisualizationFocusPanel::DiskHealth,
            VisualizationFocusPanel::Statistics,
        ] {
            assert!(registered.contains(&panel), "missing {panel:?}");
        }
    }

    #[test]
    fn visualization_focus_eligibility_is_independent_of_chart_metric_selection() {
        let app_state = AppState {
            screen_area: Rect::new(0, 0, 80, 60),
            ..Default::default()
        };
        let plan = calculate_header_interaction_layout(&app_state, UiLayoutMode::Vertical);

        assert!(visualization_focus_panel_candidates(&app_state, &plan)
            .iter()
            .any(|(panel, _)| *panel == VisualizationFocusPanel::Chart));
        let eligible = visualization_focus_panels(&app_state, &plan);
        assert!(!eligible
            .iter()
            .any(|(panel, _)| *panel == VisualizationFocusPanel::Chart));
        assert!(eligible
            .iter()
            .any(|(panel, _)| *panel == VisualizationFocusPanel::DiskHealth));
    }

    #[test]
    fn visualization_focus_reports_when_no_eligible_panel_is_visible() {
        let mut app_state = AppState {
            screen_area: Rect::new(0, 0, 80, 20),
            ..Default::default()
        };

        reduce_ui_action_with_layout_mode(
            &mut app_state,
            UiAction::ToggleVisualizationFocus,
            UiLayoutMode::Vertical,
        );

        assert!(!app_state.ui.visualization_focus.active);
        assert_eq!(
            app_state.system_error.as_deref(),
            Some(NO_VISIBLE_VISUALIZATION_FOCUS_PANEL_ERROR)
        );

        app_state.screen_area = Rect::new(0, 0, 200, 60);
        reduce_ui_action_with_layout_mode(
            &mut app_state,
            UiAction::ToggleVisualizationFocus,
            UiLayoutMode::Horizontal,
        );

        assert!(app_state.ui.visualization_focus.active);
        assert!(app_state.system_error.is_none());
    }

    #[test]
    fn reducer_navigate_updates_selection() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 0;
        app_state.ui.selected_header = SelectedHeader::Torrent(ColumnId::Name);

        let result = reduce_ui_action(&mut app_state, UiAction::Navigate(KeyCode::Down));

        assert!(result.redraw);
        assert_eq!(app_state.ui.selected_torrent_index, 1);
        assert_eq!(app_state.ui.selected_peer_index, 0);
    }

    #[test]
    fn reducer_sort_uses_forced_layout_visible_columns() {
        let mut app_state = create_test_app_state();
        app_state.screen_area = Rect::new(0, 0, 80, 70);
        app_state.ui.selected_header = SelectedHeader::Torrent(ColumnId::DownSpeed);
        app_state.torrent_sort = (TorrentSortColumn::Up, SortDirection::Descending);
        let selected = app_state.torrent_list_order[0].clone();
        if let Some(torrent) = app_state.torrents.get_mut(&selected) {
            torrent.smoothed_download_speed_bps = 32;
            torrent.smoothed_upload_speed_bps = 16;
        }

        let result = reduce_ui_action_with_layout_mode(
            &mut app_state,
            UiAction::SortBySelectedColumn,
            UiLayoutMode::Horizontal,
        );

        assert!(result.redraw);
        assert_eq!(
            app_state.ui.selected_header,
            SelectedHeader::Torrent(ColumnId::Name),
            "forced horizontal layout should normalize hidden speed columns using the drawn list width"
        );
        assert_eq!(app_state.torrent_sort.0, TorrentSortColumn::Name);
    }

    #[test]
    fn navigation_uses_forced_layout_visible_columns() {
        let mut app_state = create_test_app_state();
        app_state.screen_area = Rect::new(0, 0, 80, 70);
        app_state.ui.selected_torrent_index = 0;
        app_state.ui.selected_header = SelectedHeader::Torrent(ColumnId::DownSpeed);
        let selected = app_state.torrent_list_order[0].clone();
        if let Some(torrent) = app_state.torrents.get_mut(&selected) {
            torrent.smoothed_download_speed_bps = 32;
            torrent.smoothed_upload_speed_bps = 16;
        }

        handle_navigation_with_layout_mode(
            &mut app_state,
            KeyCode::Right,
            UiLayoutMode::Horizontal,
        );

        assert_eq!(
            app_state.ui.selected_header,
            SelectedHeader::Peer(PeerColumnId::Address),
            "forced horizontal layout should normalize the hidden torrent speed column before moving right"
        );
    }

    #[test]
    fn reducer_left_from_first_torrent_column_does_not_wrap_to_peer_table() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 0;
        app_state.ui.selected_header = SelectedHeader::Torrent(ColumnId::Name);

        let result = reduce_ui_action(&mut app_state, UiAction::Navigate(KeyCode::Left));

        assert!(result.redraw);
        assert_eq!(
            app_state.ui.selected_header,
            SelectedHeader::Torrent(ColumnId::Name)
        );
    }

    #[test]
    fn reducer_right_from_last_peer_column_does_not_wrap_to_torrent_list() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 0;
        app_state.ui.selected_header = SelectedHeader::Peer(PeerColumnId::Action);

        let result = reduce_ui_action(&mut app_state, UiAction::Navigate(KeyCode::Right));

        assert!(result.redraw);
        assert_eq!(
            app_state.ui.selected_header,
            SelectedHeader::Peer(PeerColumnId::Action)
        );
    }

    #[test]
    fn reducer_right_from_last_torrent_column_still_enters_peer_table() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 0;
        app_state.ui.selected_header = SelectedHeader::Torrent(ColumnId::Name);

        let result = reduce_ui_action(&mut app_state, UiAction::Navigate(KeyCode::Right));

        assert!(result.redraw);
        assert_eq!(
            app_state.ui.selected_header,
            SelectedHeader::Peer(PeerColumnId::Flags)
        );
    }

    #[test]
    fn reducer_left_from_first_peer_column_still_returns_to_torrent_list() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 0;
        app_state.ui.selected_header = SelectedHeader::Peer(PeerColumnId::Flags);

        let result = reduce_ui_action(&mut app_state, UiAction::Navigate(KeyCode::Left));

        assert!(result.redraw);
        assert_eq!(
            app_state.ui.selected_header,
            SelectedHeader::Torrent(ColumnId::Name)
        );
    }

    #[test]
    fn reducer_toggle_anonymize_names_flips_flag() {
        let mut app_state = AppState::default();
        assert!(!app_state.anonymize_torrent_names);

        reduce_ui_action(&mut app_state, UiAction::ToggleAnonymizeNames);
        assert!(app_state.anonymize_torrent_names);

        reduce_ui_action(&mut app_state, UiAction::ToggleAnonymizeNames);
        assert!(!app_state.anonymize_torrent_names);
    }

    #[test]
    fn peer_table_shows_more_inactive_peers_when_no_active_peers_exist() {
        let mut state = create_mock_metrics(12);
        for (idx, peer) in state.peers.iter_mut().enumerate() {
            peer.address = format!("127.0.0.1:{}", 7000 + idx);
            peer.download_speed_bps = 0;
            peer.upload_speed_bps = 0;
        }

        let peers =
            displayed_peers_for_table(&state, PeerSortColumn::Address, SortDirection::Ascending);

        assert_eq!(peers.len(), MAX_INACTIVE_ONLY_PEERS_IN_TABLE);
        assert!(peers.iter().all(|row| match row {
            PeerTableRow::Peer(peer) => peer_is_inactive_for_table(peer),
            PeerTableRow::InactiveSummary { .. } => false,
        }));
    }

    #[test]
    fn peer_table_keeps_active_peers_and_summarizes_inactive() {
        let mut state = create_mock_metrics(10);
        for (idx, peer) in state.peers.iter_mut().enumerate() {
            peer.address = format!("127.0.0.1:{}", 7000 + idx);
            if idx < 5 {
                peer.download_speed_bps = 1_000 + idx as u64;
            }
        }

        let peers =
            displayed_peers_for_table(&state, PeerSortColumn::DL, SortDirection::Descending);
        let active_count = peers
            .iter()
            .filter(|row| match row {
                PeerTableRow::Peer(peer) => !peer_is_inactive_for_table(peer),
                PeerTableRow::InactiveSummary { .. } => false,
            })
            .count();
        let inactive_peer_rows = peers
            .iter()
            .filter(|row| match row {
                PeerTableRow::Peer(peer) => peer_is_inactive_for_table(peer),
                PeerTableRow::InactiveSummary { .. } => false,
            })
            .count();
        let summary_count = peers.iter().find_map(|row| match row {
            PeerTableRow::InactiveSummary { count } => Some(*count),
            PeerTableRow::Peer(_) => None,
        });

        assert_eq!(active_count, 5);
        assert_eq!(inactive_peer_rows, 0);
        assert_eq!(summary_count, Some(5));
        assert_eq!(peers.len(), active_count + 1);
    }

    #[test]
    fn swarm_heatmap_uses_empty_and_scaled_levels() {
        assert_eq!(swarm_heatmap_level(0, 3), SwarmHeatmapLevel::Empty);
        assert_eq!(swarm_heatmap_level(1, 1), SwarmHeatmapLevel::High);
        assert_eq!(swarm_heatmap_level(1, 3), SwarmHeatmapLevel::Low);
        assert_eq!(swarm_heatmap_level(2, 3), SwarmHeatmapLevel::Medium);
        assert_eq!(swarm_heatmap_level(3, 3), SwarmHeatmapLevel::High);
    }

    #[test]
    fn swarm_heatmap_flash_tone_uses_regular_flash_for_non_empty_cells() {
        assert_eq!(
            swarm_heatmap_flash_tone(SwarmHeatmapLevel::Low, true),
            Some(SwarmHeatmapFlashTone::Regular)
        );
        assert_eq!(
            swarm_heatmap_flash_tone(SwarmHeatmapLevel::Medium, true),
            Some(SwarmHeatmapFlashTone::Regular)
        );
        assert_eq!(
            swarm_heatmap_flash_tone(SwarmHeatmapLevel::High, true),
            Some(SwarmHeatmapFlashTone::Regular)
        );
        assert_eq!(
            swarm_heatmap_flash_tone(SwarmHeatmapLevel::Low, false),
            None
        );
        assert_eq!(
            swarm_heatmap_flash_tone(SwarmHeatmapLevel::Empty, true),
            None
        );
    }

    #[test]
    fn swarm_heatmap_flash_peer_prefers_active_non_complete_peer_with_piece() {
        let peers = vec![
            PeerInfo {
                address: "127.0.0.1:7002".to_string(),
                bitfield: vec![true, true, true],
                upload_speed_bps: 8,
                ..Default::default()
            },
            PeerInfo {
                address: "127.0.0.1:7001".to_string(),
                bitfield: vec![false, true, false],
                ..Default::default()
            },
            PeerInfo {
                address: "127.0.0.1:7003".to_string(),
                bitfield: vec![false, true, false],
                download_speed_bps: 16,
                ..Default::default()
            },
        ];

        let peer = swarm_heatmap_flash_peer(&peers, 3, 1).expect("piece source");

        assert_eq!(peer.address, "127.0.0.1:7003");
    }

    #[test]
    fn swarm_heatmap_flash_peer_falls_back_to_stable_address_order() {
        let peers = vec![
            PeerInfo {
                address: "127.0.0.1:7002".to_string(),
                bitfield: vec![true, false],
                ..Default::default()
            },
            PeerInfo {
                address: "127.0.0.1:7001".to_string(),
                bitfield: vec![true, false],
                ..Default::default()
            },
        ];

        let peer = swarm_heatmap_flash_peer(&peers, 2, 0).expect("piece source");

        assert_eq!(peer.address, "127.0.0.1:7001");
    }

    #[test]
    fn swarm_heatmap_flash_color_uses_white_for_inactive_peer() {
        let ctx = ThemeContext::new(Theme::builtin(ThemeName::CatppuccinMocha), 0.0);
        let peers = vec![PeerInfo {
            address: "127.0.0.1:7001".to_string(),
            bitfield: vec![true, false],
            ..Default::default()
        }];

        let color = swarm_heatmap_flash_color(&ctx, &peers, 2, 0, ctx.theme.scale.heatmap.low);

        assert_eq!(color, ctx.theme.semantic.white);
    }

    #[test]
    fn swarm_heatmap_flash_color_uses_ip_color_for_active_peer() {
        let ctx = ThemeContext::new(Theme::builtin(ThemeName::CatppuccinMocha), 0.0);
        let peers = vec![PeerInfo {
            address: "127.0.0.1:7001".to_string(),
            bitfield: vec![true, false],
            download_speed_bps: 1,
            ..Default::default()
        }];

        let color = swarm_heatmap_flash_color(&ctx, &peers, 2, 0, ctx.theme.scale.heatmap.low);

        assert_eq!(color, ip_to_color(&ctx, "127.0.0.1:7001"));
    }

    #[test]
    fn swarm_heatmap_flash_color_uses_white_when_peer_matches_heatmap_block() {
        let ctx = ThemeContext::new(Theme::builtin(ThemeName::CatppuccinMocha), 0.0);
        let address = "127.0.0.1:7001";
        let peers = vec![PeerInfo {
            address: address.to_string(),
            bitfield: vec![true, false],
            download_speed_bps: 1,
            ..Default::default()
        }];
        let heatmap_block_color = ip_to_color(&ctx, address);

        let color = swarm_heatmap_flash_color(&ctx, &peers, 2, 0, heatmap_block_color);

        assert_eq!(color, ctx.theme.semantic.white);
    }

    #[test]
    fn swarm_heatmap_flashing_peer_addresses_tracks_active_piece_source() {
        let mut state = SwarmAvailabilityFlashState::default();
        let now = Instant::now();
        let duration = Duration::from_millis(200);
        let baseline_peers = vec![
            PeerInfo {
                address: "127.0.0.1:7001".to_string(),
                bitfield: vec![true, false, false],
                download_speed_bps: 1,
                ..Default::default()
            },
            PeerInfo {
                address: "127.0.0.1:7002".to_string(),
                bitfield: vec![false, false, false],
                download_speed_bps: 1,
                ..Default::default()
            },
        ];
        let current_peers = vec![
            baseline_peers[0].clone(),
            PeerInfo {
                bitfield: vec![false, true, false],
                ..baseline_peers[1].clone()
            },
        ];
        state.update_from_peers(b"torrent-a", &baseline_peers, 3, now, duration);
        state.update_from_peers(b"torrent-a", &current_peers, 3, now, duration);

        let flash = SwarmHeatmapFlash {
            info_hash: b"torrent-a",
            state: &state,
            now,
        };
        let addresses = swarm_heatmap_flashing_peer_addresses(Some(flash), &current_peers, 3);

        assert!(addresses.contains("127.0.0.1:7002"));
        assert!(!addresses.contains("127.0.0.1:7001"));
    }

    #[test]
    fn swarm_heatmap_ignores_complete_peers_for_display_levels() {
        let peers = vec![
            PeerInfo {
                bitfield: vec![true, true, true, true],
                ..Default::default()
            },
            PeerInfo {
                bitfield: vec![true, true, true, true],
                ..Default::default()
            },
            PeerInfo {
                bitfield: vec![true, true, true, false],
                ..Default::default()
            },
            PeerInfo {
                bitfield: vec![true, true, false, false],
                ..Default::default()
            },
            PeerInfo {
                bitfield: vec![true, false, false, false],
                ..Default::default()
            },
        ];

        let (availability, has_complete_peer) =
            swarm_heatmap_display_availability_counts(&peers, 4);
        let max_avail = availability.iter().max().copied().unwrap_or(0);

        assert!(has_complete_peer);
        assert_eq!(availability, vec![3, 2, 1, 0]);
        assert_eq!(
            swarm_heatmap_level(availability[0], max_avail),
            SwarmHeatmapLevel::High
        );
        assert_eq!(
            swarm_heatmap_level(availability[1], max_avail),
            SwarmHeatmapLevel::Medium
        );
        assert_eq!(
            swarm_heatmap_level(availability[2], max_avail),
            SwarmHeatmapLevel::Low
        );
        assert_eq!(
            swarm_heatmap_level(availability[3], max_avail),
            SwarmHeatmapLevel::Empty
        );
    }

    #[test]
    fn swarm_heatmap_only_complete_peers_stays_empty_for_display_levels() {
        let peers = vec![
            PeerInfo {
                bitfield: vec![true, true, true],
                ..Default::default()
            },
            PeerInfo {
                bitfield: vec![true, true, true],
                ..Default::default()
            },
        ];

        let (availability, has_complete_peer) =
            swarm_heatmap_display_availability_counts(&peers, 3);
        let max_avail = availability.iter().max().copied().unwrap_or(0);

        assert!(has_complete_peer);
        assert_eq!(availability, vec![0, 0, 0]);
        assert!(availability
            .iter()
            .all(|&count| swarm_heatmap_level(count, max_avail) == SwarmHeatmapLevel::Empty));
    }

    #[test]
    fn peer_files_layout_gives_extra_space_to_swarm_when_files_fit() {
        let mut app_state = create_test_app_state();
        let torrent = app_state
            .torrents
            .get_mut("hash_a".as_bytes())
            .expect("mock torrent exists");
        torrent.latest_state.torrent_name = "sample-tree".to_string();
        torrent.latest_state.download_path = Some(PathBuf::from(r"C:\data\sample-tree"));
        torrent.file_preview_tree = crate::app::build_torrent_preview_tree(
            (0..3)
                .map(|idx| (vec![format!("file_{idx:02}.bin")], 1_u64))
                .collect(),
            &Default::default(),
        );

        let layout = torrent_peer_files_layout(&app_state, Rect::new(0, 0, 80, 20))
            .expect("peers, files, and swarm should fit");
        let swarm = layout.swarm.expect("swarm visible");

        assert_eq!(layout.peer_table.expect("peer table visible").height, 4);
        assert_eq!(layout.files.height, 4);
        assert_eq!(swarm.y, layout.files.y + layout.files.height + 1);
        assert_eq!(swarm.height, 11);
    }

    #[test]
    fn peer_files_layout_keeps_adaptive_heatmap_when_files_are_limited() {
        let mut app_state = create_test_app_state();
        let torrent = app_state
            .torrents
            .get_mut("hash_a".as_bytes())
            .expect("mock torrent exists");
        torrent.latest_state.torrent_name = "sample-tree".to_string();
        torrent.latest_state.download_path = Some(PathBuf::from(r"C:\data\sample-tree"));
        torrent.file_preview_tree = crate::app::build_torrent_preview_tree(
            (0..30)
                .map(|idx| (vec![format!("file_{idx:02}.bin")], 1_u64))
                .collect(),
            &Default::default(),
        );

        let layout = torrent_peer_files_layout(&app_state, Rect::new(0, 0, 80, 20))
            .expect("peers, files, and swarm should fit");
        let swarm = layout.swarm.expect("swarm visible");

        assert_eq!(layout.peer_table.expect("peer table visible").height, 4);
        assert_eq!(layout.files.height, 14);
        assert_eq!(swarm.y, layout.files.y + layout.files.height + 1);
        assert_eq!(swarm.height, MIN_SWARM_AVAILABILITY_HEIGHT);
    }

    #[test]
    fn peer_files_layout_falls_back_when_files_would_not_fit() {
        let mut app_state = create_test_app_state();
        let torrent = app_state
            .torrents
            .get_mut("hash_a".as_bytes())
            .expect("mock torrent exists");
        torrent.latest_state.torrent_name = "sample-tree".to_string();
        torrent.latest_state.download_path = Some(PathBuf::from(r"C:\data\sample-tree"));
        torrent.file_preview_tree = crate::app::build_torrent_preview_tree(
            (0..3)
                .map(|idx| (vec![format!("file_{idx:02}.bin")], 1_u64))
                .collect(),
            &Default::default(),
        );

        assert_eq!(
            torrent_peer_files_layout(&app_state, Rect::new(0, 0, 80, 6)),
            None
        );
    }

    #[test]
    fn peer_files_layout_reserves_files_when_active_peers_fill_area() {
        let mut app_state = create_test_app_state();
        let torrent = app_state
            .torrents
            .get_mut("hash_a".as_bytes())
            .expect("mock torrent exists");
        torrent.latest_state.peers = (0..20)
            .map(|idx| PeerInfo {
                address: format!("127.0.0.1:{}", 7000 + idx),
                download_speed_bps: 1_000 + idx as u64,
                ..Default::default()
            })
            .collect();
        torrent.latest_state.torrent_name = "sample-tree".to_string();
        torrent.latest_state.download_path = Some(PathBuf::from(r"C:\data\sample-tree"));
        torrent.file_preview_tree = crate::app::build_torrent_preview_tree(
            (0..8)
                .map(|idx| (vec![format!("file_{idx:02}.bin")], 1_u64))
                .collect(),
            &Default::default(),
        );

        let layout = torrent_peer_files_layout(&app_state, Rect::new(0, 0, 80, 12))
            .expect("active peers should reserve a files strip");

        assert_eq!(layout.files_mode, TorrentFilesRenderMode::ActivitySorted);
        assert_eq!(layout.swarm, None);
        assert_eq!(layout.peer_table.expect("peer table visible").height, 7);
        assert_eq!(layout.files, Rect::new(0, 7, 80, 5));
    }

    #[test]
    fn peer_files_layout_skips_reserved_files_when_area_is_too_short() {
        let mut app_state = create_test_app_state();
        let torrent = app_state
            .torrents
            .get_mut("hash_a".as_bytes())
            .expect("mock torrent exists");
        torrent.latest_state.peers = (0..20)
            .map(|idx| PeerInfo {
                address: format!("127.0.0.1:{}", 7000 + idx),
                download_speed_bps: 1_000 + idx as u64,
                ..Default::default()
            })
            .collect();
        torrent.latest_state.torrent_name = "sample-tree".to_string();
        torrent.file_preview_tree = crate::app::build_torrent_preview_tree(
            (0..8)
                .map(|idx| (vec![format!("file_{idx:02}.bin")], 1_u64))
                .collect(),
            &Default::default(),
        );

        assert_eq!(
            torrent_peer_files_layout(&app_state, Rect::new(0, 0, 80, 11)),
            None
        );
    }

    #[test]
    fn peer_files_layout_reserves_only_existing_files_when_saturated() {
        let mut app_state = create_test_app_state();
        let torrent = app_state
            .torrents
            .get_mut("hash_a".as_bytes())
            .expect("mock torrent exists");
        torrent.latest_state.peers = (0..20)
            .map(|idx| PeerInfo {
                address: format!("127.0.0.1:{}", 7000 + idx),
                download_speed_bps: 1_000 + idx as u64,
                ..Default::default()
            })
            .collect();
        torrent.latest_state.torrent_name = "sample-tree".to_string();
        torrent.file_preview_tree = crate::app::build_torrent_preview_tree(
            (0..2)
                .map(|idx| (vec![format!("file_{idx:02}.bin")], 1_u64))
                .collect(),
            &Default::default(),
        );

        let layout = torrent_peer_files_layout(&app_state, Rect::new(0, 0, 80, 9))
            .expect("active peers should reserve only existing files");

        assert_eq!(layout.files_mode, TorrentFilesRenderMode::ActivitySorted);
        assert_eq!(layout.peer_table.expect("peer table visible").height, 7);
        assert_eq!(layout.files, Rect::new(0, 7, 80, 2));
    }

    #[test]
    fn peer_files_layout_can_show_files_without_peer_rows() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 1;
        let torrent = app_state
            .torrents
            .get_mut("hash_b".as_bytes())
            .expect("mock torrent exists");
        torrent.latest_state.torrent_name = "sample-tree".to_string();
        torrent.latest_state.download_path = Some(PathBuf::from(r"C:\data\sample-tree"));
        torrent.file_preview_tree = crate::app::build_torrent_preview_tree(
            vec![(vec!["single.bin".to_string()], 1_u64)],
            &Default::default(),
        );

        let layout = torrent_peer_files_layout(&app_state, Rect::new(0, 0, 80, 12))
            .expect("files and swarm should fit without peer rows");

        assert_eq!(layout.peer_table, None);
        assert_eq!(layout.files.height, 2);
        assert_eq!(layout.swarm.expect("swarm visible").height, 9);
    }

    #[test]
    fn torrent_files_body_area_uses_peer_table_horizontal_padding() {
        assert_eq!(
            torrent_files_body_area(Rect::new(10, 20, 80, 5)),
            Rect::new(11, 20, 78, 5)
        );
    }

    #[test]
    fn torrent_files_panel_height_uses_needed_rows_until_limited() {
        let mut torrent = create_mock_display_state(0);
        torrent.latest_state.torrent_name = "sample-tree".to_string();
        torrent.latest_state.download_path = Some(PathBuf::from(r"C:\data\sample-tree"));
        torrent.file_preview_tree = crate::app::build_torrent_preview_tree(
            (0..3)
                .map(|idx| (vec![format!("file_{idx:02}.bin")], 1_u64))
                .collect(),
            &Default::default(),
        );

        assert_eq!(
            torrent_files_panel_height_needed(&torrent, 80, false, 11),
            Some(4)
        );
        assert_eq!(
            torrent_files_panel_height_needed(&torrent, 80, false, 4),
            Some(4)
        );
        assert_eq!(
            torrent_files_panel_height_needed(&torrent, 80, false, 1),
            Some(1)
        );
    }

    #[test]
    fn split_path_components_handles_windows_paths() {
        assert_eq!(
            split_path_components(r"C:\Users\ExampleUser\Downloads\library"),
            vec!["C:", "Users", "ExampleUser", "Downloads", "library"]
        );
    }

    #[test]
    fn split_path_components_handles_posix_paths() {
        assert_eq!(
            split_path_components("/data/downloads/show"),
            vec!["data", "downloads", "show"]
        );
    }

    #[test]
    fn middle_ellipsize_path_preserves_path_ends() {
        let shaped = middle_ellipsize_path(r"C:\Users\ExampleUser\Downloads\library", 18);
        assert!(shaped.chars().count() <= 18, "{shaped}");
        assert!(shaped.starts_with("C:"), "{shaped}");
        assert!(shaped.ends_with("library"), "{shaped}");
        assert!(shaped.contains("..."), "{shaped}");
    }

    #[test]
    fn middle_ellipsize_path_preserves_posix_root_and_separator() {
        let shaped = middle_ellipsize_path("/data/downloads/show", 14);
        assert!(shaped.chars().count() <= 14, "{shaped}");
        assert!(shaped.starts_with('/'), "{shaped}");
        assert!(shaped.ends_with("show"), "{shaped}");
        assert!(shaped.contains("/.../"), "{shaped}");
    }

    #[test]
    fn torrent_root_path_label_uses_download_root_only() {
        let metrics = TorrentMetrics {
            download_path: Some(PathBuf::from(r"C:\Users\ExampleUser\Downloads\library")),
            container_name: Some("[team] sample release".to_string()),
            is_multi_file: true,
            torrent_name: "episode 01.mkv".to_string(),
            info_hash: vec![1, 2, 3, 4],
            ..Default::default()
        };

        assert_eq!(
            torrent_root_path_label(&metrics, false),
            r"C:\Users\ExampleUser\Downloads\library"
        );
    }

    #[test]
    fn anonymize_preserving_shape_keeps_path_separators() {
        let original = r"C:\Users\ExampleUser\Downloads\library\[Group] Episode_01.mkv";
        let anonymized = anonymize_preserving_shape(original);

        assert_eq!(
            anonymized.matches('\\').count(),
            original.matches('\\').count()
        );
        assert!(!anonymized.contains(':'));
        assert!(!anonymized.contains('.'));
        assert!(!anonymized.contains('_'));
        assert!(!anonymized.contains('['));
        assert!(!anonymized.contains(']'));
        assert!(!anonymized.chars().any(|ch| ch.is_ascii_digit()));
        assert!(!anonymized.contains("  "));
        assert!(anonymized.chars().all(|ch| {
            ch.is_ascii_lowercase() || ch.is_whitespace() || ch == '/' || ch == '\\'
        }));
        assert_ne!(anonymized, original);
    }

    #[test]
    fn anonymize_preserving_shape_hides_release_clues() {
        let original = "[Group] Episode 01_sample S7 - 99 (2097y) [17AC1A4Z].qfo (1.36 GB)";
        let anonymized = anonymize_preserving_shape(original);

        assert!(!anonymized.contains("  "));
        assert!(!anonymized.contains("Episode"));
        assert!(!anonymized.contains("2097"));
        assert!(!anonymized.contains("qfo"));
        assert!(!anonymized.contains("GB"));
        assert!(!anonymized.contains('['));
        assert!(!anonymized.contains('.'));
        assert!(!anonymized.chars().any(|ch| ch.is_ascii_digit()));
    }

    #[test]
    fn torrent_list_name_uses_file_panel_anonymizer() {
        let original = "Episode 01_sample.mkv";

        assert_eq!(
            anonymize_tree_name(original, false, true),
            anonymize_preserving_shape(original)
        );
        assert_eq!(
            anonymize_tree_name(original, false, false),
            original.to_string()
        );
    }

    #[test]
    fn torrent_root_path_label_anonymize_preserves_path_shape() {
        let metrics = TorrentMetrics {
            download_path: Some(PathBuf::from(r"C:\Users\ExampleUser\Downloads\library")),
            torrent_name: "episode 01.mkv".to_string(),
            ..Default::default()
        };

        let original = torrent_root_path_label(&metrics, false);
        let anonymized = torrent_root_path_label(&metrics, true);

        assert_eq!(
            anonymized.matches('\\').count(),
            original.matches('\\').count()
        );
        assert!(!anonymized.contains(':'));
        assert!(!anonymized.contains("  "));
        assert_ne!(anonymized, original);
    }

    #[test]
    fn shaped_row_start_offsets_account_for_hidden_path_separators() {
        let rows = vec![
            r"C:\Users".to_string(),
            "ExampleUser".to_string(),
            "library".to_string(),
        ];

        assert_eq!(shaped_row_start_offsets(&rows), vec![0, 9, 21]);
    }

    #[test]
    fn file_activity_wave_hits_can_continue_across_adjacent_path_slices() {
        let wave = FileActivityWaveProfile {
            band_width: 3,
            steps_per_second: 8.0,
        };
        let root_len = 9usize;
        let relative_path = "demo/file.bin";
        let total_len = root_len + 1 + relative_path.chars().count();
        let logical_hit_idx = 10usize;
        let mirrored_idx = total_len - 1 - logical_hit_idx;
        let step = mirrored_idx + 1;

        assert!(file_activity_wave_hits(
            relative_path,
            logical_hit_idx,
            root_len,
            wave,
            step,
            false,
        ));
        assert!(file_activity_wave_hits(
            relative_path,
            logical_hit_idx + 1,
            root_len,
            wave,
            step,
            false,
        ));
    }

    #[test]
    fn file_activity_visibility_lingers_for_one_wave_cycle() {
        let wave = FileActivityWaveProfile {
            band_width: 4,
            steps_per_second: 12.0,
        };
        let total_len = 24usize;
        let linger = file_activity_wave_cycle_duration(total_len, wave);
        let seen_at =
            Instant::now() - FILE_ACTIVITY_HIGHLIGHT_WINDOW - linger + Duration::from_millis(50);

        assert!(file_activity_is_visible(seen_at, total_len, wave));
    }

    #[test]
    fn file_activity_visibility_expires_after_wave_cycle_finishes() {
        let wave = FileActivityWaveProfile {
            band_width: 4,
            steps_per_second: 12.0,
        };
        let total_len = 24usize;
        let linger = file_activity_wave_cycle_duration(total_len, wave);
        let seen_at =
            Instant::now() - FILE_ACTIVITY_HIGHLIGHT_WINDOW - linger - Duration::from_millis(50);

        assert!(!file_activity_is_visible(seen_at, total_len, wave));
    }

    #[test]
    fn shape_root_path_for_viewport_keeps_single_line_when_it_fits() {
        let path = r"C:\Users\ExampleUser\Downloads";
        assert_eq!(
            shape_root_path_for_viewport(path, path.len(), 4),
            vec![path.to_string()]
        );
    }

    #[test]
    fn shape_root_path_for_viewport_uses_middle_ellipsis_when_only_one_row_is_available() {
        let rows = shape_root_path_for_viewport(r"C:\Users\ExampleUser\Downloads\library", 18, 1);
        assert_eq!(rows.len(), 1);
        assert!(rows_fit_in_box(&rows, 18, 1), "{rows:?}");
        assert!(rows[0].starts_with("C:"), "{rows:?}");
        assert!(rows[0].ends_with("library"), "{rows:?}");
        assert!(rows[0].contains("..."), "{rows:?}");
    }

    #[test]
    fn shape_root_path_for_viewport_splits_into_vertical_segments_when_narrow() {
        assert_eq!(
            shape_root_path_for_viewport(r"C:\Users\ExampleUser\Downloads\library", 10, 5),
            vec!["C:\\Users", "Example...", "Downloads", "library"]
        );
    }

    #[test]
    fn shape_root_path_for_viewport_preserves_posix_root_and_separator() {
        assert_eq!(
            shape_root_path_for_viewport("/data/downloads/show", 10, 5),
            vec!["/data", "downloads", "show"]
        );
    }

    #[test]
    fn shape_root_path_for_viewport_regroups_segments_to_match_height_budget() {
        assert_eq!(
            shape_root_path_for_viewport(r"C:\Users\ExampleUser\Downloads\library", 16, 3),
            vec!["C:\\Users", "ExampleUser", "Downloads"]
        );
    }

    #[test]
    fn shape_root_path_for_viewport_truncates_overwide_group_when_needed() {
        assert_eq!(
            shape_root_path_for_viewport(
                r"C:\Users\ExampleUser\[251226][longlonglonglong] release",
                12,
                2
            ),
            vec!["C:\\Users", "ExampleUser"]
        );
    }

    fn rows_fit_in_box(rows: &[String], width: usize, height: usize) -> bool {
        rows.len() <= height && rows.iter().all(|row| row.chars().count() <= width)
    }

    fn visible_signal(rows: &[String]) -> usize {
        rows.iter()
            .map(|row| row.replace("...", "").chars().count())
            .sum()
    }

    #[test]
    fn shaped_paths_fit_vertical_square_and_landscape_boxes() {
        let cases = [
            r"C:\Users\ExampleUser\Downloads\library",
            r"C:\Users\ExampleUser\Downloads\library\[251226][long-release-name] Episode 01.mkv",
            r"C:\seedbox\anime\season-01\episode-01.mkv",
            r"D:\dl\onefile.mkv",
            r"C:\very\deep\path\with\many\segments\and\a\long\final\component",
        ];
        let viewports = [
            (10, 8), // vertical
            (16, 4), // square-ish
            (40, 2), // landscape
            (12, 3),
            (20, 5),
        ];

        for path in cases {
            for (width, height) in viewports {
                let rows = shape_root_path_for_viewport(path, width, height);
                assert!(
                    rows_fit_in_box(&rows, width, height),
                    "rows should fit box for path={path:?} width={width} height={height}: {rows:?}"
                );
                assert!(
                    !rows.is_empty(),
                    "shape helper should produce at least one row for path={path:?}"
                );
            }
        }
    }

    #[test]
    fn wider_viewports_do_not_increase_row_count_or_truncation_for_same_height() {
        let path =
            r"C:\Users\ExampleUser\Downloads\library\[251226][long-release-name]\Episode 01.mkv";

        let narrow = shape_root_path_for_viewport(path, 12, 3);
        let medium = shape_root_path_for_viewport(path, 18, 3);
        let wide = shape_root_path_for_viewport(path, 28, 3);

        assert!(rows_fit_in_box(&narrow, 12, 3));
        assert!(rows_fit_in_box(&medium, 18, 3));
        assert!(rows_fit_in_box(&wide, 28, 3));
        assert!(
            visible_signal(&medium) >= visible_signal(&narrow),
            "{medium:?} vs {narrow:?}"
        );
        assert!(
            visible_signal(&wide) >= visible_signal(&medium),
            "{wide:?} vs {medium:?}"
        );
    }

    #[test]
    fn taller_viewports_do_not_increase_truncation_for_same_width() {
        let path = r"C:\Users\ExampleUser\Downloads\library\[251226][long-release-name]\subdir\Episode 01.mkv";

        let short = shape_root_path_for_viewport(path, 14, 2);
        let medium = shape_root_path_for_viewport(path, 14, 4);
        let tall = shape_root_path_for_viewport(path, 14, 8);

        assert!(
            visible_signal(&medium) >= visible_signal(&short),
            "{medium:?} vs {short:?}"
        );
        assert!(
            visible_signal(&tall) >= visible_signal(&medium),
            "{tall:?} vs {medium:?}"
        );
        assert!(rows_fit_in_box(&short, 14, 2));
        assert!(rows_fit_in_box(&medium, 14, 4));
        assert!(rows_fit_in_box(&tall, 14, 8));
    }

    #[test]
    fn shallow_paths_prefer_horizontal_layouts_when_space_allows() {
        let path = r"D:\dl\onefile.mkv";

        assert_eq!(
            shape_root_path_for_viewport(path, 40, 2),
            vec![path.to_string()]
        );
        assert_eq!(
            shape_root_path_for_viewport(path, 8, 3),
            vec!["D:\\dl", "onefi..."]
        );
    }

    #[test]
    fn deep_paths_prefer_vertical_layouts_when_width_is_constrained() {
        let path = r"C:\a\b\c\d\e\f\g\h\i";
        let rows = shape_root_path_for_viewport(path, 4, 9);
        assert!(rows_fit_in_box(&rows, 4, 9), "{rows:?}");
        assert_eq!(rows.len(), 5);
        assert!(
            rows.first().is_some_and(|row| row.starts_with("C:")),
            "{rows:?}"
        );
        assert!(
            rows.last().is_some_and(|row| row.ends_with('i')),
            "{rows:?}"
        );
    }

    #[test]
    fn root_path_shaping_peels_from_deepest_parent_first() {
        assert_eq!(
            shape_root_path_for_viewport(r"C:\Users\ExampleUser\Downloads\library", 24, 4),
            vec!["C:\\Users\\ExampleUser", "Downloads\\library"]
        );
        assert_eq!(
            shape_root_path_for_viewport(r"C:\Users\ExampleUser\Downloads\library", 18, 4),
            vec!["C:\\Users", "ExampleUser", "Downloads\\library"]
        );
    }

    #[test]
    fn long_single_component_paths_are_truncated_to_fit() {
        let path = r"C:\[251226][veryveryveryveryverylong-name]";
        let rows = shape_root_path_for_viewport(path, 10, 2);
        assert!(rows_fit_in_box(&rows, 10, 2));
        assert!(rows.iter().any(|row| row.contains("...")), "{rows:?}");
    }

    #[test]
    fn reducer_enter_power_saving_emits_mode_effect() {
        let mut app_state = AppState {
            mode: AppMode::Normal,
            ..Default::default()
        };

        let result = reduce_ui_action(&mut app_state, UiAction::EnterPowerSaving);

        assert_eq!(result.effects, vec![UiEffect::ToPowerSaving]);
        assert!(matches!(app_state.mode, AppMode::Normal));
    }

    #[test]
    fn reducer_request_quit_sets_flag() {
        let mut app_state = AppState::default();
        assert!(!app_state.should_quit);

        reduce_ui_action(&mut app_state, UiAction::RequestQuit);

        assert!(app_state.should_quit);
    }

    #[test]
    fn reducer_graph_actions_stop_at_boundaries() {
        let mut app_state = AppState::default();
        let initial = app_state.graph_mode;

        reduce_ui_action(&mut app_state, UiAction::GraphNext);
        assert_eq!(app_state.graph_mode, initial.next());

        reduce_ui_action(&mut app_state, UiAction::GraphPrev);
        assert_eq!(app_state.graph_mode, initial);

        app_state.graph_mode = GraphDisplayMode::OneYear;
        reduce_ui_action(&mut app_state, UiAction::GraphNext);
        assert_eq!(app_state.graph_mode, GraphDisplayMode::OneYear);

        app_state.graph_mode = GraphDisplayMode::OneMinute;
        reduce_ui_action(&mut app_state, UiAction::GraphPrev);
        assert_eq!(app_state.graph_mode, GraphDisplayMode::OneMinute);
    }

    #[test]
    fn reducer_chart_view_actions_stop_at_boundaries() {
        let mut app_state = AppState::default();
        let initial = app_state.chart_panel_view;

        reduce_ui_action(&mut app_state, UiAction::ChartViewNext);
        assert_eq!(app_state.chart_panel_view, initial.next());

        reduce_ui_action(&mut app_state, UiAction::ChartViewPrev);
        assert_eq!(app_state.chart_panel_view, initial);

        app_state.chart_panel_view = ChartPanelView::MultiTorrentOverlay;
        reduce_ui_action(&mut app_state, UiAction::ChartViewNext);
        assert_eq!(
            app_state.chart_panel_view,
            ChartPanelView::MultiTorrentOverlay
        );

        app_state.chart_panel_view = ChartPanelView::Network;
        reduce_ui_action(&mut app_state, UiAction::ChartViewPrev);
        assert_eq!(app_state.chart_panel_view, ChartPanelView::Network);
    }

    #[test]
    fn reducer_chart_view_navigation_includes_disk_mode() {
        assert_eq!(ChartPanelView::Ram.next(), ChartPanelView::Disk);
        assert_eq!(ChartPanelView::Disk.prev(), ChartPanelView::Ram);
        assert_eq!(ChartPanelView::Disk.next(), ChartPanelView::Tuning);
        assert_eq!(
            ChartPanelView::Tuning.next(),
            ChartPanelView::TorrentOverlay
        );
        assert_eq!(
            ChartPanelView::TorrentOverlay.next(),
            ChartPanelView::MultiTorrentOverlay
        );
        assert_eq!(
            ChartPanelView::MultiTorrentOverlay.prev(),
            ChartPanelView::TorrentOverlay
        );
        assert_eq!(
            ChartPanelView::MultiTorrentOverlay.next(),
            ChartPanelView::MultiTorrentOverlay
        );
        assert_eq!(ChartPanelView::Network.prev(), ChartPanelView::Network);
    }

    #[test]
    fn disk_series_draw_order_favors_more_recent_read_activity() {
        assert!(disk_series_draw_read_last(&[0, 12, 8, 0], &[0, 0, 0, 0]));
        assert!(!disk_series_draw_read_last(&[0, 0, 0, 0], &[0, 4, 3, 0]));
    }

    #[test]
    fn torrent_period_traffic_sums_download_and_upload_over_window() {
        let mut app_state = AppState::default();
        let info_hash = vec![9; 20];
        let key = hex::encode(&info_hash);
        app_state.activity_history_state.torrents.insert(
            key,
            ActivityHistorySeries {
                tiers: crate::persistence::activity_history::ActivityHistoryTiers {
                    second_1s: vec![
                        ActivityHistoryPoint {
                            ts_unix: 8,
                            primary: 100,
                            secondary: 50,
                        },
                        ActivityHistoryPoint {
                            ts_unix: 9,
                            primary: 25,
                            secondary: 5,
                        },
                    ],
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        assert_eq!(
            torrent_period_traffic(&app_state, &info_hash, HistoryTier::Second1s, 1, 4, 9),
            180
        );
    }

    #[test]
    fn torrent_current_traffic_uses_latest_point_only() {
        let mut app_state = AppState::default();
        let info_hash = vec![8; 20];
        let key = hex::encode(&info_hash);
        app_state.activity_history_state.torrents.insert(
            key,
            ActivityHistorySeries {
                tiers: crate::persistence::activity_history::ActivityHistoryTiers {
                    second_1s: vec![
                        ActivityHistoryPoint {
                            ts_unix: 8,
                            primary: 100,
                            secondary: 50,
                        },
                        ActivityHistoryPoint {
                            ts_unix: 9,
                            primary: 25,
                            secondary: 5,
                        },
                    ],
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        assert_eq!(
            torrent_current_traffic(
                &app_state,
                &info_hash,
                HistoryTier::Second1s,
                1,
                4,
                9,
                2.0 / 6.0
            ),
            43
        );
    }

    #[test]
    fn torrent_current_traffic_preserves_recent_activity_when_latest_bucket_is_zero() {
        let mut app_state = AppState::default();
        let info_hash = vec![7; 20];
        let key = hex::encode(&info_hash);
        app_state.activity_history_state.torrents.insert(
            key,
            ActivityHistorySeries {
                tiers: crate::persistence::activity_history::ActivityHistoryTiers {
                    second_1s: vec![ActivityHistoryPoint {
                        ts_unix: 8,
                        primary: 100,
                        secondary: 50,
                    }],
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        assert_eq!(
            torrent_current_traffic(
                &app_state,
                &info_hash,
                HistoryTier::Second1s,
                1,
                4,
                9,
                2.0 / 6.0
            ),
            33
        );
        assert_eq!(
            torrent_period_traffic(&app_state, &info_hash, HistoryTier::Second1s, 1, 4, 9),
            150
        );
    }

    #[test]
    fn details_eta_or_probe_text_uses_eta_for_incomplete_torrent() {
        let mut torrent = TorrentDisplayState::default();
        torrent.latest_state.number_of_pieces_total = 10;
        torrent.latest_state.number_of_pieces_completed = 4;
        torrent.latest_state.eta = Duration::from_secs(95);
        torrent.integrity_next_probe_in = Some(Duration::from_secs(30));

        assert_eq!(
            details_eta_or_probe_text(&torrent),
            ("ETA:      ", "1m 35s".to_string())
        );
    }

    #[test]
    fn details_eta_or_probe_text_uses_probe_for_completed_torrent() {
        let mut torrent = TorrentDisplayState::default();
        torrent.latest_state.number_of_pieces_total = 10;
        torrent.latest_state.number_of_pieces_completed = 10;
        torrent.latest_state.eta = Duration::ZERO;
        torrent.integrity_next_probe_in = Some(Duration::from_secs(125));

        assert_eq!(
            details_eta_or_probe_text(&torrent),
            ("Probe:    ", "2m 5s".to_string())
        );
    }

    #[test]
    fn torrent_overlay_legend_uses_full_chart_constraints() {
        assert_eq!(
            chart_hidden_legend_constraints(ChartPanelView::TorrentOverlay),
            (Constraint::Percentage(100), Constraint::Percentage(100))
        );
        assert_eq!(
            chart_hidden_legend_constraints(ChartPanelView::MultiTorrentOverlay),
            (Constraint::Percentage(100), Constraint::Percentage(100))
        );
        assert_eq!(
            chart_hidden_legend_constraints(ChartPanelView::Network),
            (Constraint::Ratio(1, 4), Constraint::Ratio(1, 4))
        );
    }

    #[test]
    fn torrent_overlay_legend_uses_top_left_position() {
        assert_eq!(
            chart_legend_position(ChartPanelView::TorrentOverlay),
            Some(ratatui::widgets::LegendPosition::TopLeft)
        );
        assert_eq!(
            chart_legend_position(ChartPanelView::MultiTorrentOverlay),
            Some(ratatui::widgets::LegendPosition::TopLeft)
        );
        assert_eq!(
            chart_legend_position(ChartPanelView::Network),
            Some(ratatui::widgets::LegendPosition::TopRight)
        );
    }

    #[test]
    fn speed_chart_upper_bound_adds_headroom_while_staying_near_peak() {
        assert_eq!(speed_chart_upper_bound(8_500_000), 10_000_000);
        assert_eq!(speed_chart_upper_bound(12_000_000), 14_000_000);
        assert_eq!(speed_chart_upper_bound(0), 10_000);
    }

    #[test]
    fn selector_window_returns_full_list_when_not_compact() {
        let labels = ["NET", "CPU", "RAM", "DISK"];
        assert_eq!(selector_window(&labels, 1, false), labels);
    }

    #[test]
    fn selector_window_centers_active_item_when_compact() {
        let labels = ["1m", "5m", "10m", "30m", "1h"];
        assert_eq!(selector_window(&labels, 2, true), vec!["5m", "10m", "30m"]);
    }

    #[test]
    fn selector_window_clamps_at_edges_in_compact_mode() {
        let labels = ["NET", "CPU", "RAM", "DISK", "TUNE", "TOR", "MULTI"];
        assert_eq!(selector_window(&labels, 0, true), vec!["NET", "CPU", "RAM"]);
        assert_eq!(
            selector_window(&labels, labels.len() - 1, true),
            vec!["TUNE", "TOR", "MULTI"]
        );
    }

    #[test]
    fn selector_active_position_clamps_to_visible_edge_slots() {
        let labels = ["1m", "5m", "10m", "30m", "1h"];
        assert_eq!(selector_active_position(labels.len(), 0, true), 0);
        assert_eq!(selector_active_position(labels.len(), 2, true), 1);
        assert_eq!(
            selector_active_position(labels.len(), labels.len() - 1, true),
            2
        );
    }
    #[test]
    fn keymap_includes_chart_view_controls() {
        assert_eq!(
            map_key_to_ui_action(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)),
            Some(UiAction::ChartViewNext)
        );
        assert_eq!(
            map_key_to_ui_action(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)),
            Some(UiAction::ChartViewPrev)
        );
        assert_eq!(
            map_key_to_ui_action(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            map_key_to_ui_action(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)),
            Some(UiAction::OpenSelectedTorrentFiles)
        );
        assert_eq!(
            map_key_to_ui_action(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE)),
            Some(UiAction::ClearManualSorting)
        );
        assert_eq!(
            map_key_to_ui_action(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::NONE)),
            Some(UiAction::OpenTorrentManagement)
        );
        assert_eq!(
            map_key_to_ui_action(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::NONE)),
            Some(UiAction::OpenPeerManagement)
        );
        assert_eq!(
            map_key_to_ui_action(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)),
            Some(UiAction::ToggleVisualizationFocus)
        );
    }

    #[test]
    fn visualization_focus_keymap_is_mode_local() {
        assert_eq!(
            map_visualization_focus_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Some(UiAction::VisualizationFocusNext)
        );
        assert_eq!(
            map_visualization_focus_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(UiAction::VisualizationFocusPrev)
        );
        assert_eq!(
            map_visualization_focus_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(UiAction::ExitVisualizationFocus)
        );
        assert_eq!(
            map_visualization_focus_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            Some(UiAction::VisualizationRendererNext)
        );
        assert_eq!(
            map_visualization_focus_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)),
            Some(UiAction::VisualizationRendererNext)
        );
        assert_eq!(
            map_visualization_focus_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            Some(UiAction::VisualizationRendererPrev)
        );
        assert_eq!(
            map_visualization_focus_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
            Some(UiAction::VisualizationRendererPrev)
        );
        assert_eq!(
            map_visualization_focus_key(KeyEvent::new(KeyCode::Char('<'), KeyModifiers::NONE)),
            Some(UiAction::VisualizationRendererPrev)
        );
        assert_eq!(
            map_visualization_focus_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)),
            Some(UiAction::ResetVisualizationRenderer)
        );
        assert_eq!(
            map_visualization_focus_key(KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn keymap_includes_vim_right_navigation() {
        assert_eq!(
            map_key_to_ui_action(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)),
            Some(UiAction::Navigate(KeyCode::Char('l')))
        );
    }

    #[test]
    fn keymap_ignores_control_modified_shortcuts() {
        assert_eq!(
            map_key_to_ui_action(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            map_key_to_ui_action(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn accepts_magnet_links_as_paste_candidates() {
        assert!(crate::tui::runtime::native_pasted_text_supported(
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567"
        ));
    }

    #[test]
    fn accepts_existing_torrent_files_as_paste_candidates() {
        let dir = tempdir().expect("temp dir");
        let torrent_path = dir.path().join("sample_fixture.torrent");
        fs::write(&torrent_path, b"sample torrent data").expect("write torrent fixture");

        assert!(crate::tui::runtime::native_pasted_text_supported(
            torrent_path.to_string_lossy().as_ref()
        ));
    }

    #[test]
    fn rejects_invalid_paste_candidates() {
        assert!(!crate::tui::runtime::native_pasted_text_supported("jj"));
    }
    #[test]
    fn build_time_aligned_window_snaps_unaligned_now_to_step_boundary() {
        let points = vec![
            NetworkHistoryPoint {
                ts_unix: 60,
                download_bps: 10,
                upload_bps: 20,
                backoff_ms_max: 1,
            },
            NetworkHistoryPoint {
                ts_unix: 120,
                download_bps: 30,
                upload_bps: 40,
                backoff_ms_max: 2,
            },
            NetworkHistoryPoint {
                ts_unix: 180,
                download_bps: 50,
                upload_bps: 60,
                backoff_ms_max: 3,
            },
        ];

        let (dl, ul, backoff) = build_time_aligned_window(&points, 60, 3, 190);

        assert_eq!(dl, vec![10, 30, 50]);
        assert_eq!(ul, vec![20, 40, 60]);
        assert_eq!(backoff, vec![1, 2, 3]);
    }

    #[test]
    fn reducer_open_add_torrent_browser_emits_effect() {
        let mut app_state = AppState::default();

        let result = reduce_ui_action(&mut app_state, UiAction::OpenAddTorrentBrowser);

        assert!(result.redraw);
        assert_eq!(result.effects, vec![UiEffect::OpenAddTorrentFileBrowser]);
    }

    #[test]
    fn reducer_open_selected_torrent_files_emits_selected_hash_effect() {
        let mut app_state = AppState::default();
        let first_hash = vec![1; 20];
        let second_hash = vec![2; 20];
        app_state.torrent_list_order = vec![first_hash, second_hash.clone()];
        app_state.ui.selected_torrent_index = 1;

        let result = reduce_ui_action(&mut app_state, UiAction::OpenSelectedTorrentFiles);

        assert!(result.redraw);
        assert_eq!(
            result.effects,
            vec![UiEffect::OpenExistingTorrentFileBrowser(second_hash)]
        );
    }

    #[test]
    fn reducer_open_delete_confirm_emits_mode_effect_and_sets_payload() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 1;

        let result = reduce_ui_action(
            &mut app_state,
            UiAction::OpenDeleteConfirm { with_files: true },
        );

        assert!(result.redraw);
        assert_eq!(result.effects, vec![UiEffect::ToDeleteConfirm]);
        assert_eq!(app_state.ui.delete_confirm.info_hash, b"hash_b".to_vec());
        assert!(app_state.ui.delete_confirm.with_files);
    }

    #[test]
    fn reducer_open_delete_confirm_is_noop_when_no_selection() {
        let mut app_state = AppState::default();
        app_state.ui.selected_torrent_index = 0;

        let result = reduce_ui_action(
            &mut app_state,
            UiAction::OpenDeleteConfirm { with_files: false },
        );

        assert!(result.redraw);
        assert!(result.effects.is_empty());
        assert!(matches!(app_state.mode, AppMode::Normal));
    }

    #[test]
    fn reducer_open_config_emits_effect() {
        let mut app_state = AppState::default();

        let result = reduce_ui_action(&mut app_state, UiAction::OpenConfig);

        assert!(result.redraw);
        assert_eq!(result.effects, vec![UiEffect::OpenConfigScreen]);
    }

    #[test]
    fn reducer_open_rss_emits_open_rss_effect() {
        let mut app_state = AppState::default();

        let result = reduce_ui_action(&mut app_state, UiAction::OpenRss);

        assert!(result.redraw);
        assert_eq!(result.effects, vec![UiEffect::OpenRssScreen]);
    }

    #[test]
    fn reducer_open_journal_emits_open_journal_effect() {
        let mut app_state = AppState::default();

        let result = reduce_ui_action(&mut app_state, UiAction::OpenJournal);

        assert!(result.redraw);
        assert_eq!(result.effects, vec![UiEffect::OpenJournalScreen]);
    }

    #[test]
    fn reducer_open_torrent_management_emits_effect() {
        let mut app_state = AppState::default();

        let result = reduce_ui_action(&mut app_state, UiAction::OpenTorrentManagement);

        assert!(result.redraw);
        assert_eq!(result.effects, vec![UiEffect::OpenTorrentManagementScreen]);
    }

    #[test]
    fn reducer_open_peer_management_emits_effect() {
        let mut app_state = AppState::default();

        let result = reduce_ui_action(&mut app_state, UiAction::OpenPeerManagement);

        assert!(result.redraw);
        assert_eq!(result.effects, vec![UiEffect::OpenPeerManagementScreen]);
    }

    #[test]
    fn reducer_data_rate_actions_update_rate_and_emit_effect() {
        let mut app_state = AppState {
            data_rate: DataRate::Rate1s,
            ..Default::default()
        };

        let slower = reduce_ui_action(&mut app_state, UiAction::DataRateSlower);
        assert_eq!(app_state.data_rate.as_ms(), DataRate::RateHalf.as_ms());
        assert_eq!(
            slower.effects,
            vec![UiEffect::BroadcastManagerDataRate(
                DataRate::RateHalf.as_ms()
            )]
        );

        let faster = reduce_ui_action(&mut app_state, UiAction::DataRateFaster);
        assert_eq!(app_state.data_rate.as_ms(), DataRate::Rate1s.as_ms());
        assert_eq!(
            faster.effects,
            vec![UiEffect::BroadcastManagerDataRate(DataRate::Rate1s.as_ms())]
        );
    }

    #[test]
    fn reducer_theme_actions_emit_effects() {
        let mut app_state = AppState::default();

        let prev = reduce_ui_action(&mut app_state, UiAction::ThemePrev);
        let next = reduce_ui_action(&mut app_state, UiAction::ThemeNext);

        assert_eq!(prev.effects, vec![UiEffect::ApplyThemePrev]);
        assert_eq!(next.effects, vec![UiEffect::ApplyThemeNext]);
    }

    #[test]
    fn reducer_toggle_pause_selected_toggles_state_and_emits_command_effect() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 0;
        let hash = b"hash_a".to_vec();

        if let Some(t) = app_state.torrents.get_mut(&hash) {
            t.latest_state.torrent_control_state = TorrentControlState::Running;
        }

        let paused = reduce_ui_action(&mut app_state, UiAction::TogglePauseSelected);
        assert_eq!(paused.effects, vec![UiEffect::SendPause(hash.clone())]);
        assert_eq!(
            app_state
                .torrents
                .get(&hash)
                .expect("selected torrent exists")
                .latest_state
                .torrent_control_state,
            TorrentControlState::Paused
        );

        let resumed = reduce_ui_action(&mut app_state, UiAction::TogglePauseSelected);
        assert_eq!(resumed.effects, vec![UiEffect::SendResume(hash.clone())]);
        assert_eq!(
            app_state
                .torrents
                .get(&hash)
                .expect("selected torrent exists")
                .latest_state
                .torrent_control_state,
            TorrentControlState::Running
        );
    }

    #[test]
    fn reducer_sort_by_selected_column_updates_torrent_sort() {
        let mut app_state = create_test_app_state();
        app_state.screen_area = Rect::new(0, 0, 220, 80);
        app_state.ui.selected_header = SelectedHeader::Torrent(ColumnId::Name);
        app_state.torrent_sort = (TorrentSortColumn::Down, SortDirection::Descending);

        if let Some(t) = app_state.torrents.get_mut("hash_a".as_bytes()) {
            t.latest_state.number_of_pieces_total = 10;
            t.latest_state.number_of_pieces_completed = 5;
            t.smoothed_download_speed_bps = 100;
            t.smoothed_upload_speed_bps = 50;
        }
        if let Some(t) = app_state.torrents.get_mut("hash_b".as_bytes()) {
            t.latest_state.number_of_pieces_total = 10;
            t.latest_state.number_of_pieces_completed = 10;
            t.smoothed_download_speed_bps = 200;
            t.smoothed_upload_speed_bps = 100;
        }

        let _ = reduce_ui_action(&mut app_state, UiAction::SortBySelectedColumn);

        assert_eq!(app_state.torrent_sort.0, TorrentSortColumn::Name);
        assert_eq!(app_state.torrent_sort.1, SortDirection::Ascending);
        assert!(app_state.torrent_sort_pinned);
    }

    #[test]
    fn reducer_sort_by_selected_column_keeps_dynamic_torrent_column_identity() {
        let mut app_state = create_test_app_state();
        app_state.screen_area = Rect::new(0, 0, 220, 80);
        app_state.ui.selected_header = SelectedHeader::Torrent(ColumnId::Status);
        app_state.torrent_sort = (TorrentSortColumn::Down, SortDirection::Descending);

        let _ = reduce_ui_action(&mut app_state, UiAction::SortBySelectedColumn);

        assert_eq!(
            app_state.ui.selected_header,
            SelectedHeader::Torrent(ColumnId::Name)
        );
        assert_eq!(app_state.torrent_sort.0, TorrentSortColumn::Name);

        for torrent in app_state.torrents.values_mut() {
            torrent.latest_state.number_of_pieces_total = 10;
            torrent.latest_state.number_of_pieces_completed = 5;
        }
        app_state.torrent_sort = (TorrentSortColumn::Down, SortDirection::Descending);

        let _ = reduce_ui_action(&mut app_state, UiAction::SortBySelectedColumn);

        assert_eq!(
            app_state.ui.selected_header,
            SelectedHeader::Torrent(ColumnId::Name)
        );
        assert_eq!(app_state.torrent_sort.0, TorrentSortColumn::Name);
    }

    #[test]
    fn reducer_sort_by_selected_column_sorts_visible_dynamic_download_column() {
        let mut app_state = create_test_app_state();
        app_state.screen_area = Rect::new(0, 0, 220, 80);
        app_state.ui.selected_header = SelectedHeader::Torrent(ColumnId::DownSpeed);

        if let Some(t) = app_state.torrents.get_mut("hash_a".as_bytes()) {
            t.smoothed_download_speed_bps = 100;
        }
        if let Some(t) = app_state.torrents.get_mut("hash_b".as_bytes()) {
            t.smoothed_download_speed_bps = 2_000;
        }

        let _ = reduce_ui_action(&mut app_state, UiAction::SortBySelectedColumn);

        assert_eq!(
            app_state.ui.selected_header,
            SelectedHeader::Torrent(ColumnId::DownSpeed)
        );
        assert_eq!(
            app_state.torrent_sort,
            (TorrentSortColumn::Down, SortDirection::Descending)
        );
        assert!(
            !app_state.torrent_sort_pinned,
            "DL/UL torrent sorting is autosort-managed, not a manual pin"
        );
        assert_eq!(
            app_state.torrent_list_order,
            vec!["hash_b".as_bytes().to_vec(), "hash_a".as_bytes().to_vec()]
        );
    }

    #[test]
    fn reducer_sort_by_selected_column_updates_peer_sort() {
        let mut app_state = create_test_app_state();
        app_state.screen_area = Rect::new(0, 0, 220, 80);
        app_state.ui.selected_torrent_index = 0;
        app_state.ui.selected_header = SelectedHeader::Peer(PeerColumnId::Flags);
        app_state.peer_sort = (PeerSortColumn::Address, SortDirection::Ascending);

        let _ = reduce_ui_action(&mut app_state, UiAction::SortBySelectedColumn);

        assert_eq!(app_state.peer_sort.0, PeerSortColumn::Flags);
        assert_eq!(app_state.peer_sort.1, SortDirection::Descending);
        assert!(app_state.peer_sort_pinned);
    }

    #[test]
    fn reducer_sort_by_selected_column_selects_visible_dynamic_peer_download_column() {
        let mut app_state = create_test_app_state();
        app_state.screen_area = Rect::new(0, 0, 220, 80);
        app_state.ui.selected_torrent_index = 0;
        app_state.ui.selected_header = SelectedHeader::Peer(PeerColumnId::DownSpeed);

        let torrent = app_state
            .torrents
            .get_mut("hash_a".as_bytes())
            .expect("test torrent exists");
        torrent.latest_state.peers[0].download_speed_bps = 2_000;

        let _ = reduce_ui_action(&mut app_state, UiAction::SortBySelectedColumn);

        assert_eq!(
            app_state.ui.selected_header,
            SelectedHeader::Peer(PeerColumnId::DownSpeed)
        );
        assert_eq!(
            app_state.peer_sort,
            (PeerSortColumn::DL, SortDirection::Descending)
        );
        assert!(
            !app_state.peer_sort_pinned,
            "DL/UL peer sorting is autosort-managed, not a manual pin"
        );
    }

    #[test]
    fn reducer_clear_manual_sorting_resumes_autosort() {
        let mut app_state = create_test_app_state();
        app_state.torrent_sort = (TorrentSortColumn::Name, SortDirection::Ascending);
        app_state.torrent_sort_pinned = true;
        app_state.peer_sort = (PeerSortColumn::Address, SortDirection::Ascending);
        app_state.peer_sort_pinned = true;

        let result = reduce_ui_action(&mut app_state, UiAction::ClearManualSorting);

        assert!(result.redraw);
        assert!(!app_state.torrent_sort_pinned);
        assert!(!app_state.peer_sort_pinned);
    }

    #[test]
    fn critical_details_panel_returns_simple_text_for_unavailable_data() {
        let mut torrent = create_mock_display_state(0);
        torrent.latest_state.data_available = false;
        torrent.integrity_next_probe_in = Some(Duration::from_secs(5));
        torrent.latest_state.download_path = Some("/downloads".into());
        torrent.latest_state.container_name = Some("sample".to_string());
        torrent.latest_file_probe_status = Some(TorrentFileProbeStatus::Files(vec![
            crate::app::torrent_manager_protocol::FileProbeEntry {
                relative_path: "missing.bin".into(),
                absolute_path: "/tmp/missing.bin".into(),
                error: StorageError::from(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "No such file or directory",
                )),
                expected_size: 10,
                observed_size: None,
            },
        ]));

        let panel = selected_torrent_critical_details(&torrent, false)
            .expect("critical panel should be present for unavailable data");
        let expected_path = PathBuf::from("/downloads")
            .join("sample")
            .join("missing.bin")
            .display()
            .to_string();
        assert_eq!(panel.title, "Critical");
        assert!(panel.text.contains("DATA UNAVAILABLE (1)"));
        assert!(panel.text.contains("Files Check: 5s"));
        assert!(panel.text.contains(&expected_path));
    }

    #[test]
    fn critical_details_panel_masks_path_when_anonymized() {
        let mut torrent = create_mock_display_state(0);
        torrent.latest_state.data_available = false;
        torrent.integrity_next_probe_in = Some(Duration::from_secs(5));
        torrent.latest_state.download_path = Some("/downloads".into());
        torrent.latest_state.container_name = Some("sample".to_string());
        torrent.latest_file_probe_status = Some(TorrentFileProbeStatus::Files(vec![
            crate::app::torrent_manager_protocol::FileProbeEntry {
                relative_path: "missing.bin".into(),
                absolute_path: "/tmp/missing.bin".into(),
                error: StorageError::from(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "No such file or directory",
                )),
                expected_size: 10,
                observed_size: None,
            },
        ]));

        let panel = selected_torrent_critical_details(&torrent, true)
            .expect("critical panel should be present for unavailable data");
        let unexpected_path = PathBuf::from("/downloads")
            .join("sample")
            .join("missing.bin")
            .display()
            .to_string();
        assert_eq!(panel.title, "Critical");
        assert!(panel.text.contains("DATA UNAVAILABLE (1)"));
        assert!(panel.text.contains("Files Check: 5s"));
        assert!(panel.text.contains("/path/to/torrent/file"));
        assert!(!panel.text.contains(&unexpected_path));
    }

    #[test]
    fn torrent_list_row_color_uses_error_when_data_is_unavailable() {
        let ctx = ThemeContext::new(Theme::builtin(ThemeName::CatppuccinMocha), 0.0);
        let mut torrent = create_mock_display_state(0);

        assert_eq!(
            torrent_list_row_color(&torrent, &ctx),
            ctx.theme.semantic.text
        );

        torrent.latest_state.data_available = false;
        assert_eq!(torrent_list_row_color(&torrent, &ctx), ctx.state_error());
    }

    #[test]
    fn torrent_status_cell_shows_metadata_pending() {
        let ctx = ThemeContext::new(Theme::builtin(ThemeName::CatppuccinMocha), 0.0);
        let mut torrent = create_mock_display_state(0);
        torrent.latest_state.is_complete = false;
        torrent.latest_state.number_of_pieces_total = 0;
        torrent.latest_state.number_of_pieces_completed = 0;

        assert_eq!(torrent_status_cell(&torrent, &ctx).text, "Meta");
    }

    #[test]
    fn torrent_status_cell_shows_file_probe_issue() {
        let ctx = ThemeContext::new(Theme::builtin(ThemeName::CatppuccinMocha), 0.0);
        let mut torrent = create_mock_display_state(0);
        torrent.latest_state.data_available = false;
        torrent.latest_file_probe_status = Some(TorrentFileProbeStatus::Files(Vec::new()));

        assert_eq!(torrent_status_cell(&torrent, &ctx).text, "Files");
    }

    #[test]
    fn reducer_open_help_emits_help_effect() {
        let mut app_state = create_test_app_state();
        let out = reduce_ui_action(&mut app_state, UiAction::OpenHelp);
        assert!(out.redraw);
        assert_eq!(out.effects, vec![UiEffect::OpenHelpScreen]);
    }

    #[test]
    fn reducer_paste_text_emits_paste_effect() {
        let mut app_state = create_test_app_state();
        let out = reduce_ui_action(
            &mut app_state,
            UiAction::PasteText("magnet:?xt=urn:btih:test".to_string()),
        );
        assert!(out.redraw);
        assert_eq!(
            out.effects,
            vec![UiEffect::HandlePastedText(
                "magnet:?xt=urn:btih:test".to_string()
            )]
        );
    }

    #[test]
    fn peer_stream_wave_amplitude_scales_with_activity() {
        let low = peer_stream_wave_amplitude(0.0);
        let mid = peer_stream_wave_amplitude(5.0);
        let high = peer_stream_wave_amplitude(20.0);

        assert!(low < mid);
        assert!(mid < high);
        assert!((low - 0.10).abs() < f64::EPSILON);
        assert!((high - 0.28).abs() < f64::EPSILON);
    }

    #[test]
    fn network_interruption_tracks_activation_until_listener_is_active() {
        let mut app_state = AppState {
            network_runtime_status: Some(crate::networking::NetworkRuntimeStatus {
                phase: NetworkRuntimePhase::Ready,
                mode: crate::networking::NetworkBindingMode::Interface,
                interface: Some("interface-test0".to_string()),
                interface_display_name: None,
                ipv4_interface_index: Some(7),
                ipv6_interface_index: None,
                enable_ipv4: true,
                enable_ipv6: false,
                configured_ipv4_address: None,
                configured_ipv6_address: None,
                selected_ipv4_address: Some("192.0.2.10".parse().unwrap()),
                selected_ipv6_address: None,
                interface_ipv4_addresses: vec!["192.0.2.10".parse().unwrap()],
                interface_ipv6_addresses: Vec::new(),
                ipv4_host_policy: Default::default(),
                ipv6_host_policy: Default::default(),
                dns_policy: crate::networking::DnsPolicy::System,
                dns_servers: Vec::new(),
                generation_id: Some(8),
                config_epoch: Some(8),
                blocked_reason: None,
                warning: None,
            }),
            network_activation_status: Some(NetworkActivationStatus::Pending {
                generation_id: Some(8),
            }),
            ..AppState::default()
        };

        let pending = network_interruption(&app_state).expect("pending activation warning");
        assert_eq!(pending.title, "Network Rebinding");
        let pending_footer = network_interruption_footer_text(&pending, 80, false);
        assert_eq!(pending_footer, "NETWORK REBINDING | interface-test0");
        assert_eq!(
            network_interruption_footer_text(&pending, 80, true),
            "REBINDING | interface-test0"
        );
        assert!(!pending_footer.contains('\n'));
        assert_eq!(pending.reason, "Switching network");

        app_state.network_activation_status = Some(NetworkActivationStatus::Blocked {
            reason: std::sync::Arc::from(
                "network binding configuration could not be activated: interface interface-test0 was not found: Device not configured (os error 6)",
            ),
        });
        let blocked = network_interruption(&app_state).expect("blocked activation warning");
        assert_eq!(blocked.title, "Network Blocked");
        assert_eq!(blocked.reason, "Interface unavailable");
        assert_eq!(
            network_interruption_footer_text(&blocked, 80, false),
            "NETWORK BLOCKED | interface-test0 | Interface unavailable"
        );
        assert_eq!(
            network_interruption_footer_text(&blocked, 80, true),
            "NETWORK BLOCKED | interface-test0 unavailable"
        );
        assert_eq!(
            network_interruption_footer_text(&blocked, 30, true),
            "NET BLOCKED | Interface una..."
        );
        assert_eq!(
            network_interruption_footer_text(&blocked, 10, true),
            "NET DOWN"
        );
        assert_eq!(network_interruption_footer_text(&blocked, 5, true), "NE...");

        app_state.anonymize_torrent_names = true;
        let anonymized = network_interruption(&app_state).expect("anonymized warning");
        assert_eq!(anonymized.interface, "Network interface");
        assert_eq!(anonymized.reason, "Interface unavailable");
        let anonymized_footer = network_interruption_footer_text(&anonymized, 80, false);
        assert!(!anonymized_footer.contains("interface-test0"));
        assert!(!anonymized_footer.contains("Device not configured"));

        app_state.network_activation_status = Some(NetworkActivationStatus::Active {
            generation_id: 8,
            listen_port: 6681,
        });
        assert!(network_interruption(&app_state).is_none());
    }

    #[test]
    fn peer_stream_smoothed_activity_blends_neighbors() {
        let data = [0_u64, 10, 0];
        let smoothed = peer_stream_smoothed_activity(&data, 1);
        assert!((smoothed - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn dht_wave_profile_responds_to_query_count() {
        let quiet = DhtStatus::default();
        let quiet_telemetry = DhtWaveTelemetry {
            inflight_ipv4_queries: 4,
            ..Default::default()
        };

        let busy = quiet.clone();
        let busy_telemetry = DhtWaveTelemetry {
            inflight_ipv4_queries: 40,
            inflight_ipv6_queries: 24,
            ..Default::default()
        };

        let quiet_profile = DhtWaveProfile::from_inputs(&quiet, &quiet_telemetry);
        let busy_profile = DhtWaveProfile::from_inputs(&busy, &busy_telemetry);

        assert!(busy_profile.amplitude > quiet_profile.amplitude);
        assert!(busy_profile.phase_speed > quiet_profile.phase_speed);
        assert!(busy_profile.frequency >= quiet_profile.frequency);
    }

    #[test]
    fn dht_wave_query_signal_uses_gentle_saturation() {
        let q10 = dht_wave_query_signal(&DhtWaveTelemetry {
            inflight_ipv4_queries: 10,
            ..Default::default()
        });
        let q48 = dht_wave_query_signal(&DhtWaveTelemetry {
            inflight_ipv4_queries: 48,
            ..Default::default()
        });
        let q96 = dht_wave_query_signal(&DhtWaveTelemetry {
            inflight_ipv4_queries: 96,
            ..Default::default()
        });

        assert!(q10 < 0.30);
        assert!(q48 > q10 + 0.30);
        assert!(q96 > q48);
    }

    #[test]
    fn dht_wave_title_is_query_count_without_multiplier() {
        let ctx = ThemeContext::new(Theme::builtin(ThemeName::CatppuccinMocha), 0.0);
        let spans = dht_wave_title_spans(42, 184, 2, &ctx);

        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "42");
        assert_eq!(spans[1].content, " ");
        assert_eq!(spans[2].content, "184");
    }

    #[test]
    fn dht_wave_title_colors_multiplier_prefix() {
        let ctx = ThemeContext::new(Theme::builtin(ThemeName::CatppuccinMocha), 0.0);
        let spans = dht_wave_title_spans(42, 184, 8, &ctx);

        assert_eq!(spans.len(), 6);
        assert_eq!(spans[0].content, "4x");
        assert_eq!(spans[1].content, "(");
        assert_eq!(spans[2].content, "42");
        assert_eq!(spans[3].content, " ");
        assert_eq!(spans[4].content, "184");
        assert_eq!(spans[5].content, ")");
        assert_eq!(
            spans[0].style,
            ctx.apply(
                Style::default()
                    .fg(ctx.accent_peach())
                    .add_modifier(Modifier::BOLD)
            )
        );
        assert_eq!(spans[1].style, spans[0].style);
        assert_eq!(spans[5].style, spans[0].style);
        assert_eq!(
            spans[4].style,
            ctx.apply(
                Style::default()
                    .fg(ctx.peer_connected())
                    .add_modifier(Modifier::BOLD)
            )
        );
    }

    #[test]
    fn dht_wave_title_can_show_half_power_cap() {
        let ctx = ThemeContext::new(Theme::builtin(ThemeName::CatppuccinMocha), 0.0);
        let spans = dht_wave_title_spans(42, 7, 1, &ctx);

        assert_eq!(spans.len(), 6);
        assert_eq!(spans[0].content, "0.5x");
        assert_eq!(spans[2].content, "42");
        assert_eq!(spans[4].content, "7");
    }

    #[test]
    fn dht_wave_title_hides_left_label_when_width_is_tight() {
        let ctx = ThemeContext::new(Theme::builtin(ThemeName::CatppuccinMocha), 0.0);
        let spans = dht_wave_title_spans(123, 1234, 4, &ctx);
        let right_title_width = dht_wave_title_width(&spans);

        assert_eq!(right_title_width, "2x(123 1234)".len());
        assert!(!dht_wave_should_show_left_title(17, right_title_width));
        assert!(dht_wave_should_show_left_title(18, right_title_width));
    }

    #[test]
    fn dht_peer_yield_wave_points_mirror_dht_wave_shape() {
        let empty = dht_peer_yield_wave_points(0.4, 0, 20, 1.0);
        let low = dht_peer_yield_wave_points(0.4, 12, 20, 1.0);
        let high = dht_peer_yield_wave_points(0.4, 384, 20, 1.0);

        assert!(empty.is_empty());
        assert_eq!(low.len(), 21);
        assert_eq!(high.len(), 21);
        assert_eq!(low[0].0, 0.0);
        assert_eq!(high[20].0, 20.0);
        let low_span = low.iter().map(|(_, y)| y.abs()).fold(0.0_f64, f64::max);
        let high_span = high.iter().map(|(_, y)| y.abs()).fold(0.0_f64, f64::max);
        assert!(high_span > low_span);
        assert!(high_span < 0.40);
    }

    #[test]
    fn dht_peer_yield_draw_order_uses_stronger_signal_on_top() {
        assert!(dht_peer_yield_draws_on_top(0.35, 0.60));
        assert!(!dht_peer_yield_draws_on_top(0.70, 0.30));
        assert!(dht_peer_yield_draws_on_top(0.50, 0.50));
    }

    #[test]
    fn dht_wave_profile_ignores_health_when_query_count_matches() {
        let mut healthy = DhtStatus::default();
        healthy.health.enabled = true;
        healthy.health.cached_ipv4_routes = 900;
        healthy.health.firewalled = Some(false);
        let healthy_telemetry = DhtWaveTelemetry {
            inflight_ipv4_queries: 12,
            ..Default::default()
        };

        let mut constrained = healthy.clone();
        constrained.health.enabled = false;
        constrained.health.firewalled = Some(true);
        let constrained_telemetry = healthy_telemetry.clone();

        let healthy_profile = DhtWaveProfile::from_inputs(&healthy, &healthy_telemetry);
        let constrained_profile = DhtWaveProfile::from_inputs(&constrained, &constrained_telemetry);

        assert_eq!(healthy_profile.amplitude, constrained_profile.amplitude);
        assert_eq!(healthy_profile.phase_speed, constrained_profile.phase_speed);
    }

    #[test]
    fn dht_wave_profile_stays_nearly_flat_when_only_routes_are_warm() {
        let mut route_warm = DhtStatus::default();
        route_warm.health.cached_ipv4_routes = 1_400;
        route_warm.health.cached_ipv6_routes = 260;
        let route_warm_telemetry = DhtWaveTelemetry::default();

        let active = route_warm.clone();
        let active_telemetry = DhtWaveTelemetry {
            inflight_ipv4_queries: 10,
            inflight_ipv6_queries: 4,
            ..Default::default()
        };

        let route_warm_profile = DhtWaveProfile::from_inputs(&route_warm, &route_warm_telemetry);
        let active_profile = DhtWaveProfile::from_inputs(&active, &active_telemetry);

        assert!(route_warm_profile.amplitude < 0.03);
        assert!(route_warm_profile.phase_speed < 0.08);
        assert!(active_profile.amplitude > route_warm_profile.amplitude);
        assert!(active_profile.phase_speed > route_warm_profile.phase_speed);
    }

    #[test]
    fn dht_wave_y_axis_bounds_scale_to_current_signal() {
        let small_points = [(0.0, -0.04), (1.0, 0.05)];
        let active_points = [(0.0, -0.24), (1.0, 0.28)];
        let saturated_points = [(0.0, -1.3), (1.0, 1.2)];

        let small_bounds = dht_wave_y_axis_bounds(&small_points);
        let active_bounds = dht_wave_y_axis_bounds(&active_points);
        let saturated_bounds = dht_wave_y_axis_bounds(&saturated_points);

        assert_eq!(small_bounds, [-0.18, 0.18]);
        assert!(active_bounds[0] < -0.30);
        assert!(active_bounds[1] > 0.30);
        assert_eq!(saturated_bounds, [-1.08, 1.08]);
    }

    #[test]
    fn file_activity_wave_profile_grows_with_speed_tiers() {
        let slow = file_activity_wave_profile(10_000, 24);
        let mid = file_activity_wave_profile(5_000_000, 24);
        let fast = file_activity_wave_profile(120_000_000, 24);

        assert!(slow.band_width <= mid.band_width);
        assert!(mid.band_width <= fast.band_width);
        assert!(slow.steps_per_second < mid.steps_per_second);
        assert!(mid.steps_per_second < fast.steps_per_second);
    }

    #[test]
    fn file_activity_wave_profile_clamps_band_width_to_text_length() {
        let profile = file_activity_wave_profile(120_000_000, 3);

        assert_eq!(profile.band_width, 3);
        assert_eq!(profile.steps_per_second, 23.0);
    }

    #[test]
    fn file_activity_wave_phase_can_continue_across_speed_changes() {
        let start_phase = 41.0;
        let dt = 0.25;
        let next_phase = start_phase + dt * file_activity_wave_steps_per_second(120_000_000);
        let later_phase = next_phase + dt * file_activity_wave_steps_per_second(10_000);

        assert!(next_phase > start_phase);
        assert!(later_phase > next_phase);
        assert!((later_phase - 49.5).abs() < f64::EPSILON);
    }

    #[test]
    fn file_activity_wave_uses_shared_phase_for_new_paths() {
        let wave = FileActivityWaveProfile {
            band_width: 4,
            steps_per_second: 12.0,
        };
        let root_len = 9usize;
        let step = 13usize;

        assert!(file_activity_wave_hits(
            "demo/one.bin",
            step,
            root_len,
            wave,
            step,
            true,
        ));
        assert!(file_activity_wave_hits(
            "demo/two.bin",
            step,
            root_len,
            wave,
            step,
            true,
        ));
    }

    #[test]
    fn file_activity_wave_head_stays_stable_across_band_width_changes() {
        let root_len = 9usize;
        let relative_path = "demo/file.bin";
        let total_len = root_len + 1 + relative_path.chars().count();
        let step = 87usize;
        let head = step % file_activity_wave_cycle_len(total_len);
        let slow_wave = FileActivityWaveProfile {
            band_width: 4,
            steps_per_second: 8.0,
        };
        let fast_wave = FileActivityWaveProfile {
            band_width: 9,
            steps_per_second: 23.0,
        };

        assert!(file_activity_wave_hits(
            relative_path,
            head,
            root_len,
            slow_wave,
            step,
            true,
        ));
        assert!(file_activity_wave_hits(
            relative_path,
            head,
            root_len,
            fast_wave,
            step,
            true,
        ));
    }

    #[test]
    fn render_file_tree_name_spans_mutes_inactive_rows() {
        let mut torrent = create_mock_display_state(0);
        torrent.latest_state.torrent_name = "sample-tree".to_string();
        let ctx = ThemeContext::new(Theme::builtin(ThemeName::CatppuccinMocha), 0.0);
        let base_style = Style::default()
            .fg(ctx.theme.semantic.text)
            .add_modifier(Modifier::BOLD);

        let spans = render_file_tree_name_spans(
            &torrent,
            "folder/file.bin",
            "file.bin",
            false,
            FileTreeNameRenderContext {
                download_phase: 0.0,
                upload_phase: 0.0,
                row_start_offset: 0,
                base_style,
                ctx: &ctx,
            },
        );

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "file.bin");
        assert_eq!(
            spans[0].style,
            ctx.apply(base_style.fg(ctx.theme.semantic.surface1))
        );
    }

    #[test]
    fn dashboard_file_name_style_ignores_priority_colors() {
        let ctx = ThemeContext::new(Theme::builtin(ThemeName::CatppuccinMocha), 0.0);

        assert_eq!(
            dashboard_file_name_style(false, &ctx),
            ctx.apply(Style::default().fg(ctx.theme.semantic.text))
        );
        assert_eq!(
            dashboard_file_name_style(true, &ctx),
            ctx.apply(Style::default().fg(ctx.state_info()))
        );
    }

    #[test]
    fn dashboard_files_panel_hides_priority_markers_in_tree_mode() {
        let mut torrent = create_mock_display_state(0);
        torrent.latest_state.torrent_name = "sample-tree".to_string();
        let priorities = HashMap::from([
            (0, crate::app::FilePriority::High),
            (1, crate::app::FilePriority::Skip),
        ]);
        torrent.file_preview_tree = crate::app::build_torrent_preview_tree(
            vec![
                (vec!["folder".to_string(), "high.bin".to_string()], 1_u64),
                (vec!["folder".to_string(), "skip.bin".to_string()], 1_u64),
            ],
            &priorities,
        );

        let ctx = ThemeContext::new(Theme::builtin(ThemeName::CatppuccinMocha), 0.0);
        let lines = render_list_item_plain_lines(
            build_torrent_file_list_items(
                &torrent,
                TorrentFilesListRenderOptions {
                    width: 60,
                    height: 10,
                    anonymize: false,
                    download_phase: 0.0,
                    upload_phase: 0.0,
                    mode: TorrentFilesRenderMode::Tree,
                },
                &ctx,
            ),
            60,
        );
        let rendered = lines.join("\n");

        assert!(rendered.contains("high.bin"));
        assert!(rendered.contains("skip.bin"));
        assert!(!rendered.contains("[H]"));
        assert!(!rendered.contains("[S]"));
        assert!(!rendered.contains("[*]"));
    }

    #[test]
    fn dashboard_files_panel_hides_priority_markers_in_activity_mode() {
        let mut torrent = create_mock_display_state(0);
        torrent.latest_state.torrent_name = "sample-tree".to_string();
        let priorities = HashMap::from([
            (0, crate::app::FilePriority::High),
            (1, crate::app::FilePriority::Skip),
        ]);
        torrent.file_preview_tree = crate::app::build_torrent_preview_tree(
            vec![
                (vec!["high.bin".to_string()], 1_u64),
                (vec!["skip.bin".to_string()], 1_u64),
            ],
            &priorities,
        );

        let ctx = ThemeContext::new(Theme::builtin(ThemeName::CatppuccinMocha), 0.0);
        let lines = render_list_item_plain_lines(
            build_torrent_file_list_items(
                &torrent,
                TorrentFilesListRenderOptions {
                    width: 60,
                    height: 10,
                    anonymize: false,
                    download_phase: 0.0,
                    upload_phase: 0.0,
                    mode: TorrentFilesRenderMode::ActivitySorted,
                },
                &ctx,
            ),
            60,
        );
        let rendered = lines.join("\n");

        assert!(rendered.contains("high.bin"));
        assert!(rendered.contains("skip.bin"));
        assert!(!rendered.contains("[H]"));
        assert!(!rendered.contains("[S]"));
        assert!(!rendered.contains("[*]"));
    }

    fn render_list_item_plain_lines(items: Vec<ListItem<'static>>, width: u16) -> Vec<String> {
        use ratatui::buffer::Buffer;
        use ratatui::widgets::Widget;

        let height = items.len() as u16;
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        List::new(items).render(area, &mut buffer);

        (0..height)
            .map(|y| {
                (0..width)
                    .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn build_torrent_file_list_items_limits_tree_rows_to_viewport_height() {
        let mut torrent = create_mock_display_state(0);
        torrent.latest_state.torrent_name = "sample-tree".to_string();
        torrent.file_preview_tree = crate::app::build_torrent_preview_tree(
            (0..20)
                .map(|idx| (vec![format!("file_{idx:02}.bin")], 1_u64))
                .collect(),
            &Default::default(),
        );

        let ctx = ThemeContext::new(Theme::builtin(ThemeName::CatppuccinMocha), 0.0);
        let items = build_torrent_file_list_items(
            &torrent,
            TorrentFilesListRenderOptions {
                width: 40,
                height: 3,
                anonymize: false,
                download_phase: 0.0,
                upload_phase: 0.0,
                mode: TorrentFilesRenderMode::Tree,
            },
            &ctx,
        );

        assert_eq!(items.len(), 3);
    }

    #[test]
    fn build_torrent_file_list_items_promotes_active_files_when_limited() {
        let mut torrent = create_mock_display_state(0);
        torrent.latest_state.torrent_name = "sample-tree".to_string();
        torrent.file_preview_tree = crate::app::build_torrent_preview_tree(
            (0..8)
                .map(|idx| (vec![format!("file_{idx:02}.bin")], 1_u64))
                .collect(),
            &Default::default(),
        );
        torrent.recent_file_activity.insert(
            "file_06.bin".to_string(),
            crate::app::RecentFileActivity {
                download_at: Some(Instant::now()),
                upload_at: None,
            },
        );

        let ctx = ThemeContext::new(Theme::builtin(ThemeName::CatppuccinMocha), 0.0);
        let items = build_torrent_file_list_items(
            &torrent,
            TorrentFilesListRenderOptions {
                width: 40,
                height: 3,
                anonymize: false,
                download_phase: 0.0,
                upload_phase: 0.0,
                mode: TorrentFilesRenderMode::Tree,
            },
            &ctx,
        );
        let lines = render_list_item_plain_lines(items, 40);

        assert_eq!(lines.len(), 3);
        assert!(lines[1].contains("file_06.bin"));
    }

    #[test]
    fn activity_sorted_file_list_orders_by_recent_activity_and_adds_overflow_row() {
        let mut torrent = create_mock_display_state(0);
        torrent.latest_state.torrent_name = "sample-tree".to_string();
        torrent.file_preview_tree = crate::app::build_torrent_preview_tree(
            (0..8)
                .map(|idx| (vec![format!("file_{idx:02}.bin")], 1_u64))
                .collect(),
            &Default::default(),
        );
        let now = Instant::now();
        torrent.recent_file_activity.insert(
            "file_03.bin".to_string(),
            crate::app::RecentFileActivity {
                download_at: Some(now - Duration::from_secs(10)),
                upload_at: None,
            },
        );
        torrent.recent_file_activity.insert(
            "file_06.bin".to_string(),
            crate::app::RecentFileActivity {
                download_at: Some(now),
                upload_at: None,
            },
        );

        let ctx = ThemeContext::new(Theme::builtin(ThemeName::CatppuccinMocha), 0.0);
        let items = build_torrent_file_list_items(
            &torrent,
            TorrentFilesListRenderOptions {
                width: 40,
                height: 5,
                anonymize: false,
                download_phase: 0.0,
                upload_phase: 0.0,
                mode: TorrentFilesRenderMode::ActivitySorted,
            },
            &ctx,
        );
        let lines = render_list_item_plain_lines(items, 40);

        assert_eq!(lines.len(), 5);
        assert!(lines[0].contains("file_06.bin"));
        assert!(lines[1].contains("file_03.bin"));
        assert!(lines[4].contains("+ 4 more files"));
    }

    #[test]
    fn build_torrent_file_list_items_keeps_tree_order_when_not_limited() {
        let mut torrent = create_mock_display_state(0);
        torrent.latest_state.torrent_name = "sample-tree".to_string();
        torrent.file_preview_tree = crate::app::build_torrent_preview_tree(
            (0..3)
                .map(|idx| (vec![format!("file_{idx:02}.bin")], 1_u64))
                .collect(),
            &Default::default(),
        );
        torrent.recent_file_activity.insert(
            "file_02.bin".to_string(),
            crate::app::RecentFileActivity {
                download_at: Some(Instant::now()),
                upload_at: None,
            },
        );

        let ctx = ThemeContext::new(Theme::builtin(ThemeName::CatppuccinMocha), 0.0);
        let items = build_torrent_file_list_items(
            &torrent,
            TorrentFilesListRenderOptions {
                width: 40,
                height: 5,
                anonymize: false,
                download_phase: 0.0,
                upload_phase: 0.0,
                mode: TorrentFilesRenderMode::Tree,
            },
            &ctx,
        );
        let lines = render_list_item_plain_lines(items, 40);

        assert!(lines[1].contains("file_00.bin"));
        assert!(lines[3].contains("file_02.bin"));
    }

    #[test]
    fn block_stream_and_disk_layout_uses_side_by_side_when_vertical_and_roomy() {
        let mode =
            block_stream_and_disk_layout_mode(Rect::new(0, 0, 90, 70), Rect::new(0, 0, 40, 18));
        assert_eq!(mode, BlockStreamDiskLayoutMode::SideBySide);
    }

    #[test]
    fn focus_geometry_uses_the_same_disk_rectangle_as_the_renderer() {
        let area = Rect::new(10, 20, 40, 18);
        let layout = block_stream_and_disk_panel_layout(Rect::new(0, 0, 90, 70), area);
        let block_stream = layout.block_stream.expect("block stream should be visible");

        assert_eq!(block_stream.x, area.x);
        assert_eq!(layout.disk_health.right(), area.right());
        assert_eq!(layout.disk_health.y, area.y);
        assert_eq!(layout.disk_health.height, area.height);
        assert_eq!(block_stream.right(), layout.disk_health.x);
    }

    #[test]
    fn block_stream_and_disk_layout_hides_blocks_when_vertical_stack_gets_too_narrow() {
        let mode =
            block_stream_and_disk_layout_mode(Rect::new(0, 0, 63, 90), Rect::new(0, 0, 33, 18));
        assert_eq!(mode, BlockStreamDiskLayoutMode::DiskOnly);
    }

    #[test]
    fn block_stream_and_disk_layout_keeps_stacked_mode_above_hide_breakpoint() {
        let mode =
            block_stream_and_disk_layout_mode(Rect::new(0, 0, 64, 90), Rect::new(0, 0, 33, 18));
        assert_eq!(mode, BlockStreamDiskLayoutMode::Stacked);
    }

    #[test]
    fn block_stream_and_disk_layout_uses_disk_only_when_stacked_area_is_too_short() {
        let mode =
            block_stream_and_disk_layout_mode(Rect::new(0, 0, 90, 70), Rect::new(0, 0, 17, 10));
        assert_eq!(mode, BlockStreamDiskLayoutMode::DiskOnly);
    }

    #[test]
    fn dht_inserts_between_blocks_and_disk_only_in_horizontal_mode() {
        assert!(should_insert_dht_between_blocks_and_disk(
            Rect::new(0, 0, 150, 60),
            Rect::new(0, 0, 17, 27)
        ));
        assert!(!should_insert_dht_between_blocks_and_disk(
            Rect::new(0, 0, 90, 70),
            Rect::new(0, 0, 40, 18)
        ));
    }

    #[test]
    fn block_stream_title_color_is_neutral_without_activity() {
        let app_state = create_test_app_state();
        let ctx = ThemeContext::new(app_state.theme, 0.0);
        assert_eq!(
            block_stream_title_color(&app_state, &ctx),
            ctx.theme.semantic.border
        );
    }

    #[test]
    fn block_stream_title_color_prefers_download_when_dominant() {
        let mut app_state = create_test_app_state();
        let selected = app_state.torrent_list_order[app_state.ui.selected_torrent_index].clone();
        if let Some(torrent) = app_state.torrents.get_mut(&selected) {
            torrent.latest_state.blocks_in_this_tick = 7;
            torrent.latest_state.blocks_out_this_tick = 2;
        }
        let ctx = ThemeContext::new(app_state.theme, 0.0);
        assert_eq!(
            block_stream_title_color(&app_state, &ctx),
            ctx.theme.scale.stream.inflow
        );
    }

    #[test]
    fn block_stream_title_color_prefers_upload_when_dominant() {
        let mut app_state = create_test_app_state();
        let selected = app_state.torrent_list_order[app_state.ui.selected_torrent_index].clone();
        if let Some(torrent) = app_state.torrents.get_mut(&selected) {
            torrent.latest_state.blocks_in_this_tick = 1;
            torrent.latest_state.blocks_out_this_tick = 9;
        }
        let ctx = ThemeContext::new(app_state.theme, 0.0);
        assert_eq!(
            block_stream_title_color(&app_state, &ctx),
            ctx.theme.scale.stream.outflow
        );
    }

    #[test]
    fn block_stream_title_color_uses_recent_download_history_when_tick_is_zero() {
        let mut app_state = create_test_app_state();
        let selected = app_state.torrent_list_order[app_state.ui.selected_torrent_index].clone();
        if let Some(torrent) = app_state.torrents.get_mut(&selected) {
            torrent.latest_state.blocks_in_history.push(8);
            torrent.latest_state.blocks_out_history.push(2);
            torrent.latest_state.blocks_in_this_tick = 0;
            torrent.latest_state.blocks_out_this_tick = 0;
        }
        let ctx = ThemeContext::new(app_state.theme, 0.0);
        assert_eq!(
            block_stream_title_color(&app_state, &ctx),
            ctx.theme.scale.stream.inflow
        );
    }

    #[test]
    fn block_stream_title_color_uses_recent_upload_history_when_tick_is_zero() {
        let mut app_state = create_test_app_state();
        let selected = app_state.torrent_list_order[app_state.ui.selected_torrent_index].clone();
        if let Some(torrent) = app_state.torrents.get_mut(&selected) {
            torrent.latest_state.blocks_in_history.push(1);
            torrent.latest_state.blocks_out_history.push(6);
            torrent.latest_state.blocks_in_this_tick = 0;
            torrent.latest_state.blocks_out_this_tick = 0;
        }
        let ctx = ThemeContext::new(app_state.theme, 0.0);
        assert_eq!(
            block_stream_title_color(&app_state, &ctx),
            ctx.theme.scale.stream.outflow
        );
    }

    #[test]
    fn block_stream_download_inflow_hidden_when_download_is_complete() {
        let metrics = TorrentMetrics {
            number_of_pieces_total: 10,
            number_of_pieces_completed: 10,
            ..Default::default()
        };
        assert!(!should_render_download_inflow(&metrics));
    }

    #[test]
    fn block_stream_download_inflow_visible_when_download_is_incomplete() {
        let metrics = TorrentMetrics {
            number_of_pieces_total: 10,
            number_of_pieces_completed: 9,
            ..Default::default()
        };
        assert!(should_render_download_inflow(&metrics));
    }

    #[test]
    fn disk_health_status_color_uses_state_slots_across_themes() {
        for theme_name in ThemeName::sorted_for_ui() {
            let ctx = ThemeContext::new(Theme::builtin(theme_name), 0.0);
            assert_eq!(
                disk_health_status_color(&ctx, 0),
                if theme_name == ThemeName::BlackHole {
                    ctx.theme.semantic.subtext1
                } else {
                    ctx.theme.semantic.subtext0
                }
            );
            assert_eq!(disk_health_status_color(&ctx, 1), ctx.state_info());
            assert_eq!(disk_health_status_color(&ctx, 2), ctx.state_warning());
            assert_eq!(disk_health_status_color(&ctx, 3), ctx.state_error());
            assert_eq!(disk_health_status_color(&ctx, 255), ctx.state_error());
        }
    }

    #[test]
    fn disk_health_title_color_keeps_stable_readable_and_maps_alerts() {
        for theme_name in ThemeName::sorted_for_ui() {
            let ctx = ThemeContext::new(Theme::builtin(theme_name), 0.0);
            assert_eq!(
                disk_health_title_color(&ctx, 0),
                if theme_name == ThemeName::BlackHole {
                    ctx.theme.semantic.subtext1
                } else {
                    ctx.theme.semantic.subtext0
                }
            );
            assert_eq!(disk_health_title_color(&ctx, 1), ctx.state_info());
            assert_eq!(disk_health_title_color(&ctx, 2), ctx.state_warning());
            assert_eq!(disk_health_title_color(&ctx, 3), ctx.state_error());
        }
    }

    #[test]
    fn disk_health_border_color_uses_normal_border_for_stable() {
        for theme_name in ThemeName::sorted_for_ui() {
            let ctx = ThemeContext::new(Theme::builtin(theme_name), 0.0);
            assert_eq!(disk_health_border_color(&ctx, 0), ctx.theme.semantic.border);
            assert_eq!(disk_health_border_color(&ctx, 1), ctx.state_info());
            assert_eq!(disk_health_border_color(&ctx, 2), ctx.state_warning());
            assert_eq!(disk_health_border_color(&ctx, 3), ctx.state_error());
        }
    }

    #[test]
    fn disk_health_state_word_maps_levels() {
        assert_eq!(disk_health_state_word(0), "Stable");
        assert_eq!(disk_health_state_word(1), "Busy");
        assert_eq!(disk_health_state_word(2), "Strain");
        assert_eq!(disk_health_state_word(3), "Chaos");
        assert_eq!(disk_health_state_word(9), "Chaos");
    }

    #[test]
    fn disk_gallery_name_only_appears_in_visualization_focus_mode() {
        let mut app_state = AppState {
            avg_disk_read_bps: 96 * 1024 * 1024,
            avg_disk_write_bps: 32 * 1024 * 1024,
            ..Default::default()
        };
        app_state.ui.visualization_focus.disk_health = DiskHealthVisualization::StorageDial;
        let ctx = ThemeContext::new(app_state.theme, 0.0);
        let backend = TestBackend::new(44, 7);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_disk_health_panel(frame, &app_state, frame.area(), &ctx))
            .expect("draw");
        let normal_bottom = terminal.backend().buffer().content()
            [terminal.backend().buffer().content().len() - 44..]
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!normal_bottom.contains("T20"));
        assert!(!normal_bottom.contains("Storage Dial"));

        app_state.ui.visualization_focus.active = true;
        terminal
            .draw(|frame| draw_disk_health_panel(frame, &app_state, frame.area(), &ctx))
            .expect("draw");
        let focused_buffer = terminal.backend().buffer();
        let focused_bottom = focused_buffer.content()[focused_buffer.content().len() - 44..]
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!focused_bottom.contains("T20"));
        assert!(!focused_bottom.contains("TEMP"));
        assert!(focused_bottom.contains("Storage Dial"));
    }

    #[test]
    fn disk_health_orb_layout_scales_box_without_exceeding_panel() {
        let layout =
            disk_health_orb_layout(Rect::new(10, 20, 28, 12)).expect("panel should fit the orb");

        assert_eq!(layout.area, Rect::new(14, 21, 20, 10));
        assert!((layout.visual_radius - 8.1).abs() < 0.000_001);
        assert_eq!(layout.center_y_offset_rows, 0.0);
    }

    #[test]
    fn disk_health_orb_layout_skips_tiny_panels() {
        assert_eq!(disk_health_orb_layout(Rect::new(0, 0, 2, 8)), None);
        assert_eq!(disk_health_orb_layout(Rect::new(0, 0, 8, 2)), None);
    }

    fn disk_health_orb_dot_points(rows: &[String]) -> Vec<(usize, usize)> {
        let mut points = Vec::new();

        for (cell_y, row) in rows.iter().enumerate() {
            for (cell_x, ch) in row.chars().enumerate() {
                let code = ch as u32;
                if !(0x2801..=0x28ff).contains(&code) {
                    continue;
                }

                let cell_bits = (code - 0x2800) as u8;
                for (dot_y, braille_row) in DISK_HEALTH_ORB_BRAILLE_BITS.iter().enumerate() {
                    for (dot_x, &bit) in braille_row.iter().enumerate() {
                        if cell_bits & bit != 0 {
                            points.push((cell_x * 2 + dot_x, cell_y * 4 + dot_y));
                        }
                    }
                }
            }
        }

        points
    }

    fn disk_health_orb_dot_bounds(points: &[(usize, usize)]) -> (usize, usize, usize, usize) {
        points.iter().fold(
            (usize::MAX, 0usize, usize::MAX, 0usize),
            |(min_x, max_x, min_y, max_y), &(x, y)| {
                (min_x.min(x), max_x.max(x), min_y.min(y), max_y.max(y))
            },
        )
    }

    #[test]
    fn disk_health_orb_layout_center_matches_panel_center() {
        let panel = Rect::new(10, 20, 28, 12);
        let layout = disk_health_orb_layout(panel).expect("panel should fit the orb");
        let geometry = disk_health_orb_geometry(layout);

        let absolute_center_x = f64::from(layout.area.x - panel.x) + geometry.visual_center_x;
        let absolute_center_y = f64::from(layout.area.y - panel.y) * DISK_HEALTH_ORB_CELL_Y_ASPECT
            + geometry.visual_center_y;

        assert_eq!(absolute_center_x, f64::from(panel.width) * 0.5);
        assert_eq!(
            absolute_center_y,
            f64::from(panel.height) * DISK_HEALTH_ORB_CELL_Y_ASPECT * 0.5
        );
    }

    #[test]
    fn disk_health_orb_stable_points_are_centered_and_not_clipped() {
        let panel = Rect::new(10, 20, 28, 12);
        let layout = disk_health_orb_layout(panel).expect("panel should fit the orb");
        let rows = build_disk_health_orb_rows(layout, 0.0, disk_health_deform_profile(0), 0.0, 0.0);
        let points = disk_health_orb_dot_points(&rows);
        assert!(!points.is_empty(), "stable orb should render dots");

        let (min_x, max_x, min_y, max_y) = disk_health_orb_dot_bounds(&points);
        assert!(min_x > 0, "left edge should have breathing room");
        assert!(min_y > 0, "top edge should have breathing room");
        assert!(
            max_x < layout.area.width as usize * 2 - 1,
            "right edge should not be clipped"
        );
        assert!(
            max_y < layout.area.height as usize * 4 - 1,
            "bottom edge should not be clipped"
        );

        let absolute_center_x_twice = (layout.area.x - panel.x) as usize * 4 + min_x + max_x + 1;
        let target_center_x_twice = panel.width as usize * 2;
        let absolute_center_y_twice = (layout.area.y - panel.y) as usize * 8 + min_y + max_y + 1;
        let target_center_y_twice = panel.height as usize * 4;

        assert!(
            absolute_center_x_twice.abs_diff(target_center_x_twice) <= 1,
            "horizontal dot bounds should center on calculated panel center"
        );
        assert!(
            absolute_center_y_twice.abs_diff(target_center_y_twice) <= 1,
            "vertical dot bounds should center on calculated panel center"
        );
    }

    #[test]
    fn peer_stream_legend_compacts_when_width_is_tight() {
        assert!(should_use_compact_peer_stream_legend(
            32,
            PeerStreamEventCounts {
                connected: 5,
                discovered: 182,
                disconnected: 104,
            }
        ));
    }

    #[test]
    fn peer_stream_legend_stays_verbose_when_width_allows() {
        assert!(!should_use_compact_peer_stream_legend(
            90,
            PeerStreamEventCounts {
                connected: 5,
                discovered: 182,
                disconnected: 104,
            }
        ));
    }

    #[tokio::test]
    async fn apply_open_add_torrent_browser_enters_file_browser_immediately() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..crate::config::Settings::default()
        };
        let mut app = App::new(settings, crate::app::AppRuntimeMode::Normal)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}
        app.app_state.mode = AppMode::Normal;
        app.app_state.ui.file_browser.search_state = BrowserSearchState::Applied;
        app.app_state.ui.file_browser.search_query = "stale search".to_string();
        let expected_path = app.get_initial_source_path();
        let previous_generation = app.app_state.ui.file_browser.browser_generation;

        crate::tui::runtime::execute_normal_effects(
            &mut app,
            vec![UiEffect::OpenAddTorrentFileBrowser],
        )
        .await;

        assert!(matches!(app.app_state.mode, AppMode::FileBrowser));
        assert_eq!(
            app.app_state.ui.file_browser.browser_generation,
            previous_generation.wrapping_add(1)
        );
        assert_eq!(
            app.app_state.ui.file_browser.state.current_path,
            expected_path
        );
        assert!(app.app_state.ui.file_browser.fetch_pending);
        assert!(matches!(
            &app.app_state.ui.file_browser.browser_mode,
            FileBrowserMode::File(extensions) if extensions == &[".torrent".to_string()]
        ));
        assert_eq!(
            app.app_state.ui.file_browser.search_state,
            BrowserSearchState::Closed
        );
        assert!(app.app_state.ui.file_browser.search_query.is_empty());
        while let Ok(command) = app.app_command_rx.try_recv() {
            assert!(!matches!(command, AppCommand::FetchFileTree { .. }));
        }
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn apply_open_rss_screen_sets_rss_mode_and_unified_screen() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..crate::config::Settings::default()
        };
        let mut app = App::new(settings, crate::app::AppRuntimeMode::Normal)
            .await
            .expect("build app");
        app.app_state.ui.rss.active_screen = RssScreen::History;

        crate::tui::runtime::execute_normal_effects(&mut app, vec![UiEffect::OpenRssScreen]).await;

        assert!(matches!(app.app_state.mode, AppMode::Rss));
        assert!(matches!(
            app.app_state.ui.rss.active_screen,
            RssScreen::Unified
        ));
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn apply_open_journal_screen_sets_journal_mode() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..crate::config::Settings::default()
        };
        let mut app = App::new(settings, crate::app::AppRuntimeMode::Normal)
            .await
            .expect("build app");
        app.app_state.ui.journal.selected_index = 9;
        app.app_state.ui.journal.scroll_offset = 7;

        crate::tui::runtime::execute_normal_effects(&mut app, vec![UiEffect::OpenJournalScreen])
            .await;

        assert!(matches!(app.app_state.mode, AppMode::Journal));
        assert_eq!(app.app_state.ui.journal.selected_index, 0);
        assert_eq!(app.app_state.ui.journal.scroll_offset, 0);
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn apply_open_peer_management_screen_sets_mode_and_resets_selection() {
        let settings = crate::config::Settings {
            client_port: 0,
            ..crate::config::Settings::default()
        };
        let mut app = App::new(settings, crate::app::AppRuntimeMode::Normal)
            .await
            .expect("build app");
        app.app_state.ui.peer_management.selected_index = 9;
        app.app_state.ui.peer_management.show_details = true;
        app.app_state.ui.peer_management.status_message = Some("stale status".to_string());

        crate::tui::runtime::execute_normal_effects(
            &mut app,
            vec![UiEffect::OpenPeerManagementScreen],
        )
        .await;

        assert!(matches!(app.app_state.mode, AppMode::PeerManagement));
        assert_eq!(app.app_state.ui.peer_management.selected_index, 0);
        assert!(!app.app_state.ui.peer_management.show_details);
        assert!(app.app_state.ui.peer_management.status_message.is_none());
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn pasted_magnet_on_shared_follower_queues_leader_request_without_browser() {
        let temp_dir = tempdir().expect("create tempdir");
        let download_folder = temp_dir.path().join("downloads");
        let magnet_link = "magnet:?xt=urn:btih:5555555555555555555555555555555555555555";
        let settings = crate::config::Settings {
            client_port: 0,
            default_download_folder: Some(download_folder.clone()),
            always_show_add_location_prompt: true,
            ..crate::config::Settings::default()
        };
        let mut app = App::new(settings, crate::app::AppRuntimeMode::SharedFollower)
            .await
            .expect("build app");
        while app.app_command_rx.try_recv().is_ok() {}

        crate::tui::runtime::execute_normal_effects(
            &mut app,
            vec![UiEffect::HandlePastedText(magnet_link.to_string())],
        )
        .await;

        let command = tokio::time::timeout(Duration::from_secs(1), app.app_command_rx.recv())
            .await
            .expect("queued pasted magnet request")
            .expect("app command channel open");
        let AppCommand::SubmitControlRequest(ControlRequest::AddMagnet {
            magnet_link: queued_link,
            download_path,
            container_name,
            ..
        }) = command
        else {
            panic!("expected add magnet control request");
        };
        assert_eq!(queued_link, magnet_link);
        assert_eq!(download_path, Some(download_folder));
        assert!(container_name.is_none());
        assert!(!matches!(app.app_state.mode, AppMode::FileBrowser));
        assert!(app.app_state.torrents.is_empty());
        assert!(app.torrent_manager_command_txs.is_empty());

        let _ = app.shutdown_tx.send(());
    }
}
