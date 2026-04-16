// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::file_activity_wave_steps_per_second;
use crate::app::sort_and_filter_torrent_list_state;
use crate::app::torrent_completion_percent;
use crate::app::AppCommand;
use crate::app::BrowserPane;
use crate::app::ChartPanelView;
use crate::app::FileBrowserMode;
use crate::app::FilePriority;
use crate::app::GraphDisplayMode;
use crate::app::PeerInfo;
use crate::app::{
    App, AppMode, AppState, ConfigItem, RssScreen, SelectedHeader, TorrentControlState,
    TorrentDisplayState,
};
use crate::config::Settings;
use crate::config::SortDirection;
use crate::integrations::control::ControlRequest;
use crate::persistence::activity_history::{ActivityHistoryPoint, ActivityHistorySeries};
use crate::persistence::network_history::NetworkHistoryPoint;
use crate::theme::{ThemeContext, ThemeName};
use crate::torrent_manager::{ManagerCommand, TorrentFileProbeStatus};
use crate::tui::formatters::{
    calculate_nice_upper_bound, format_bytes, format_countdown, format_duration, format_iops,
    format_latency, format_limit_bps, format_memory, format_speed, format_time,
    generate_x_axis_labels, ip_to_color, parse_peer_id, sanitize_text, speed_to_style,
    truncate_with_ellipsis,
};
use crate::tui::layout::common::compute_visible_peer_columns;
use crate::tui::layout::common::compute_visible_torrent_columns;
use crate::tui::layout::common::get_peer_columns;
use crate::tui::layout::common::get_torrent_columns;
use crate::tui::layout::common::ColumnId;
use crate::tui::layout::common::PeerColumnId;
use crate::tui::layout::normal::calculate_layout;
use crate::tui::layout::normal::LayoutContext;
use crate::tui::layout::normal::LayoutPlan;
use crate::tui::layout::normal::DEFAULT_SIDEBAR_PERCENT;
use crate::tui::screen_context::ScreenContext;
use crate::tui::tree::{TreeFilter, TreeMathHelper, TreeViewState};
use chrono::{DateTime, Utc};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::collections::HashMap;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::crossterm::event::{
    Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::layout::Layout;
use ratatui::prelude::{
    symbols, Alignment, Color, Constraint, Direction, Frame, Line, Modifier, Rect, Span, Style,
    Stylize,
};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Gauge, LineGauge, List, ListItem, Padding, Paragraph, Row, Table,
    TableState, Wrap,
};
use strum::IntoEnumIterator;
use tracing::{event as tracing_event, Level};

static APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const SECONDS_HISTORY_MAX: usize = 3600;
const MINUTES_HISTORY_MAX: usize = 48 * 60;
const TUNING_LABEL_WIDTH: usize = 14;
const ASCII_TREE_DIR_ICON: &str = "> ";
const ASCII_TREE_FILE_ICON: &str = "  ";
const FILE_ACTIVITY_HIGHLIGHT_WINDOW: Duration = Duration::from_millis(1800);

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
    StartSearch,
    Navigate(KeyCode),
    ToggleTorrentFiles,
    ToggleAnonymizeNames,
    EnterPowerSaving,
    RequestQuit,
    ChartViewNext,
    ChartViewPrev,
    GraphNext,
    GraphPrev,
    OpenAddTorrentBrowser,
    OpenDeleteConfirm { with_files: bool },
    OpenConfig,
    OpenRss,
    OpenJournal,
    DataRateSlower,
    DataRateFaster,
    ThemePrev,
    ThemeNext,
    TogglePauseSelected,
    SortBySelectedColumn,
    OpenHelp,
    PasteText(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum UiEffect {
    ToPowerSaving,
    ToDeleteConfirm,
    OpenAddTorrentFileBrowser,
    OpenConfigScreen,
    OpenRssScreen,
    OpenJournalScreen,
    BroadcastManagerDataRate(u64),
    ApplyThemePrev,
    ApplyThemeNext,
    SendPause(Vec<u8>),
    SendResume(Vec<u8>),
    OpenHelpScreen,
    HandlePastedText(String),
}

#[derive(Default)]
pub struct ReduceResult {
    pub redraw: bool,
    pub effects: Vec<UiEffect>,
}

pub fn reduce_ui_action(app_state: &mut AppState, action: UiAction) -> ReduceResult {
    match action {
        UiAction::ClearSystemError => {
            app_state.system_error = None;
            ReduceResult {
                redraw: true,
                effects: Vec::new(),
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
            handle_navigation(app_state, key_code);
            ReduceResult {
                redraw: true,
                effects: Vec::new(),
            }
        }
        UiAction::ToggleTorrentFiles => {
            app_state.ui.show_torrent_files = !app_state.ui.show_torrent_files;
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
            let layout_ctx =
                LayoutContext::new(app_state.screen_area, app_state, DEFAULT_SIDEBAR_PERCENT);
            let layout_plan = calculate_layout(app_state.screen_area, &layout_ctx);
            let (_, visible_torrent_columns) =
                compute_visible_torrent_columns(app_state, layout_plan.list.width);
            let (_, visible_peer_columns) =
                compute_visible_peer_columns(app_state, layout_plan.peers.width);

            match app_state.ui.selected_header {
                SelectedHeader::Torrent(i) => {
                    let cols = get_torrent_columns();
                    if let Some(def) = visible_torrent_columns
                        .get(i)
                        .and_then(|&real_idx| cols.get(real_idx))
                    {
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
                                app_state.torrent_sort.1 = SortDirection::Descending;
                            }
                            sort_and_filter_torrent_list_state(app_state);
                        }
                    }
                }
                SelectedHeader::Peer(i) => {
                    let cols = get_peer_columns();
                    if let Some(def) = visible_peer_columns
                        .get(i)
                        .and_then(|&real_idx| cols.get(real_idx))
                    {
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
                                app_state.peer_sort.1 = SortDirection::Descending;
                            }
                        }
                    }
                }
            };

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
        KeyCode::Char('/') => Some(UiAction::StartSearch),
        KeyCode::Char('f') => Some(UiAction::ToggleTorrentFiles),
        KeyCode::Char('x') => Some(UiAction::ToggleAnonymizeNames),
        KeyCode::Char('z') => Some(UiAction::EnterPowerSaving),
        KeyCode::Char('Q') => Some(UiAction::RequestQuit),
        KeyCode::Char('g') => Some(UiAction::ChartViewNext),
        KeyCode::Char('G') => Some(UiAction::ChartViewPrev),
        KeyCode::Char('t') => Some(UiAction::GraphNext),
        KeyCode::Char('T') => Some(UiAction::GraphPrev),
        KeyCode::Char('a') => Some(UiAction::OpenAddTorrentBrowser),
        KeyCode::Char('d') => Some(UiAction::OpenDeleteConfirm { with_files: false }),
        KeyCode::Char('D') => Some(UiAction::OpenDeleteConfirm { with_files: true }),
        KeyCode::Char('c') => Some(UiAction::OpenConfig),
        KeyCode::Char('r') => Some(UiAction::OpenRss),
        KeyCode::Char('J') => Some(UiAction::OpenJournal),
        KeyCode::Char('m') => Some(UiAction::OpenHelp),
        KeyCode::Char('[') | KeyCode::Char('{') => Some(UiAction::DataRateSlower),
        KeyCode::Char(']') | KeyCode::Char('}') => Some(UiAction::DataRateFaster),
        KeyCode::Char('<') => Some(UiAction::ThemePrev),
        KeyCode::Char('>') => Some(UiAction::ThemeNext),
        KeyCode::Char('p') => Some(UiAction::TogglePauseSelected),
        KeyCode::Char('s') => Some(UiAction::SortBySelectedColumn),
        KeyCode::Up
        | KeyCode::Char('k')
        | KeyCode::Down
        | KeyCode::Char('j')
        | KeyCode::Left
        | KeyCode::Char('h')
        | KeyCode::Right => Some(UiAction::Navigate(key.code)),
        _ => None,
    }
}

pub fn draw(f: &mut Frame, screen: &ScreenContext<'_>, plan: &LayoutPlan) {
    let app_state = screen.app.state;
    let settings = screen.settings;
    let ctx = screen.theme;

    draw_torrent_list(f, app_state, plan.list, ctx);
    draw_footer(f, app_state, settings, plan.footer, ctx);
    draw_details_panel(f, app_state, plan.details, ctx);
    if app_state.ui.show_torrent_files {
        draw_torrent_files_panel(f, app_state, plan.peers, ctx);
    } else {
        draw_peers_table(f, app_state, plan.peers, ctx);
    }

    if let Some(r) = plan.chart {
        draw_network_chart(f, app_state, r, ctx);
    }
    if let Some(r) = plan.peer_stream {
        draw_peer_stream(f, app_state, r, ctx);
    }
    if let Some(r) = plan.block_stream {
        draw_block_stream_and_disk_orb(f, app_state, r, ctx);
    }
    if let Some(r) = plan.stats {
        draw_stats_panel(f, app_state, settings, r, ctx);
    }
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

fn estimate_footer_left_content_width(app_state: &AppState, ctx: &ThemeContext) -> u16 {
    let fx_enabled = ctx.theme.effects.enabled();
    let theme_label = if fx_enabled {
        format!("{} [FX]", ctx.theme.name)
    } else {
        ctx.theme.name.to_string()
    };

    let content = if let Some(new_version) = &app_state.update_available {
        format!(
            "UPDATE AVAILABLE: v{} -> v{} | {} | {}",
            APP_VERSION,
            new_version,
            app_state.data_rate.to_string(),
            theme_label
        )
    } else {
        #[cfg(all(feature = "dht", feature = "pex"))]
        {
            format!(
                "superseedr v{} | {} | {}",
                APP_VERSION,
                app_state.data_rate.to_string(),
                theme_label
            )
        }
        #[cfg(not(all(feature = "dht", feature = "pex")))]
        {
            format!(
                "superseedr [PRIVATE] v{} | {} | {}",
                APP_VERSION,
                app_state.data_rate.to_string(),
                theme_label
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
    let status_width = format!(
        "Port {} | IPv4/IPv6 | {}",
        settings.client_port, overall_port_status
    )
    .len() as u16;

    let is_update = app_state.update_available.is_some();
    let (left_constraint, right_constraint) = if show_branding {
        let min_left = if is_update { 52u16 } else { 40u16 };
        let min_commands = 18u16;
        let desired_left = compute_footer_left_width(footer_chunk.width, is_update);
        let content_left = estimate_footer_left_content_width(app_state, ctx);
        let left_target = desired_left.min(content_left.max(min_left));
        let symmetric_left_cap = footer_chunk.width.saturating_sub(min_commands) / 2;

        if symmetric_left_cap >= min_left {
            let symmetric_left = left_target.min(symmetric_left_cap);
            (
                Constraint::Length(symmetric_left),
                Constraint::Length(symmetric_left),
            )
        } else {
            (
                Constraint::Length(left_target),
                Constraint::Length(status_width),
            )
        }
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
        let current_dl_speed = *app_state.avg_download_history.last().unwrap_or(&0);
        let current_ul_speed = *app_state.avg_upload_history.last().unwrap_or(&0);
        let fx_enabled = ctx.theme.effects.enabled();
        let theme_name = ctx.theme.name.to_string();
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
                APP_VERSION,
                new_version,
                app_state.data_rate.to_string()
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
                    app_state.data_rate.to_string(),
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
                let theme_display = fit_theme_label(&format!(
                    "superseedr v{} | {} | ",
                    APP_VERSION,
                    app_state.data_rate.to_string()
                ));
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
                        app_state.data_rate.to_string(),
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
                    APP_VERSION,
                    app_state.data_rate.to_string()
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
                        app_state.data_rate.to_string(),
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
        ctx.apply(Style::default().fg(ctx.state_info())),
    );
    push_if_fits(
        "[Q]",
        "uit",
        ctx.apply(Style::default().fg(ctx.state_error())),
    );
    push_if_fits(
        "[Paste]",
        "paste",
        ctx.apply(Style::default().fg(ctx.accent_teal())),
    );
    push_if_fits(
        "[p]",
        "ause",
        ctx.apply(Style::default().fg(ctx.state_success())),
    );
    push_if_fits(
        "[a]",
        "dd",
        ctx.apply(Style::default().fg(ctx.state_success())),
    );
    push_if_fits(
        "[f]",
        "iles",
        ctx.apply(Style::default().fg(ctx.accent_teal())),
    );
    push_if_fits(
        "[d]",
        "elete",
        ctx.apply(Style::default().fg(ctx.state_warning())),
    );
    push_if_fits(
        "[s]",
        "ort",
        ctx.apply(Style::default().fg(ctx.state_selected())),
    );
    push_if_fits(
        "[t]",
        "ime",
        ctx.apply(Style::default().fg(ctx.accent_sapphire())),
    );
    push_if_fits(
        "[g]",
        "raph",
        ctx.apply(Style::default().fg(ctx.state_warning())),
    );
    push_if_fits(
        "[<]theme[>]",
        "",
        ctx.apply(Style::default().fg(ctx.state_selected())),
    );
    push_if_fits(
        "[/]",
        "search",
        ctx.apply(Style::default().fg(ctx.state_warning())),
    );
    push_if_fits(
        "[c]",
        "onfig",
        ctx.apply(Style::default().fg(ctx.state_complete())),
    );
    push_if_fits(
        "[r]",
        "ss",
        ctx.apply(Style::default().fg(ctx.accent_sapphire())),
    );
    push_if_fits(
        "[d]",
        "elete",
        ctx.apply(Style::default().fg(ctx.state_error())),
    );
    push_if_fits(
        "[x]",
        "anon",
        ctx.apply(Style::default().fg(ctx.accent_sapphire())),
    );
    push_if_fits(
        "[z]",
        "power",
        ctx.apply(Style::default().fg(ctx.state_warning())),
    );
    push_if_fits(
        "[T]",
        "time++",
        ctx.apply(Style::default().fg(ctx.accent_sapphire())),
    );
    push_if_fits(
        "[[]",
        "slower",
        ctx.apply(Style::default().fg(ctx.state_info())),
    );
    push_if_fits(
        "[]]",
        "faster",
        ctx.apply(Style::default().fg(ctx.state_success())),
    );

    if !try_push_footer_command(
        &mut spans,
        &mut used_width,
        max_width,
        manual_key,
        manual_suffix,
        ctx.apply(Style::default().fg(ctx.accent_teal())),
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
    let v4_port_style = if app_state.externally_accessable_port_v4 {
        let style = Style::default().fg(ctx.state_success());
        if v4_highlight_active {
            ctx.apply(style.bold())
        } else {
            ctx.apply(style)
        }
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))
    };
    let v6_port_style = if app_state.externally_accessable_port_v6 {
        let style = Style::default().fg(ctx.state_success());
        if v6_highlight_active {
            ctx.apply(style.bold())
        } else {
            ctx.apply(style)
        }
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
        .enumerate()
        .map(|(visual_idx, &real_idx)| {
            let def = &all_cols[real_idx];
            let is_selected = app_state.ui.selected_header == SelectedHeader::Torrent(visual_idx);
            let is_sorting = def.sort_enum == Some(sort_col);

            let mut style = ctx.apply(Style::default().fg(ctx.state_warning()));
            if is_sorting {
                style = style.fg(ctx.state_selected());
            }
            style = ctx.apply(style);

            let mut spans = vec![];
            let mut text_span = Span::styled(def.header, style);
            if is_selected {
                text_span = text_span.underlined().bold();
            }
            spans.push(text_span);

            if is_sorting {
                let arrow = if sort_dir == SortDirection::Ascending {
                    " ▲"
                } else {
                    " ▼"
                };
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
                                    let display_pct = torrent_completion_percent(state);
                                    Cell::from(format!("{:.1}%", display_pct))
                                        .style(ctx.apply(Style::default().fg(row_color)))
                                }
                                ColumnId::Name => {
                                    let name = if app_state.anonymize_torrent_names {
                                        format!("Torrent {}", i + 1)
                                    } else {
                                        sanitize_text(&state.torrent_name)
                                    };
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
        let custom_line_set = symbols::line::Set {
            horizontal: "⣿",
            ..symbols::line::THICK
        };
        let line_gauge = LineGauge::default()
            .ratio(progress_ratio)
            .label(progress_label_text)
            .line_set(custom_line_set)
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
    let dl_limit = settings.global_download_limit_bps;

    let mut dl_spans = vec![
        Span::styled(
            "DL Speed: ",
            ctx.apply(Style::default().fg(ctx.metric_download()).bold()),
        ),
        Span::styled(
            format_speed(dl_speed),
            ctx.apply(Style::default().fg(ctx.metric_download()).bold()),
        ),
        Span::raw(" / "),
    ];
    if dl_limit > 0 && dl_speed >= dl_limit {
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

    let ul_speed = *app_state.avg_upload_history.last().unwrap_or(&0);
    let ul_limit = settings.global_upload_limit_bps;

    let mut ul_spans = vec![
        Span::styled(
            "UL Speed: ",
            ctx.apply(Style::default().fg(ctx.metric_upload()).bold()),
        ),
        Span::styled(
            format_speed(ul_speed),
            ctx.apply(Style::default().fg(ctx.metric_upload()).bold()),
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

    let tune_delta_pct = if app_state.last_tuning_score > 0 {
        let best = app_state.last_tuning_score as f64;
        let current = app_state.current_tuning_score as f64;
        Some(((current - best) / best) * 100.0)
    } else {
        Some(0.0)
    };
    let tune_header = format!("Self-Tune({}s): ", app_state.tuning_countdown);
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
            Span::styled(
                app_state.current_tuning_score.to_string(),
                ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
            ),
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
            app_state.limits.reserve_permits,
            app_state.last_tuning_limits.reserve_permits,
            ctx.accent_teal(),
        ),
        build_tuning_peer_line(
            ctx,
            total_peers,
            app_state.limits.max_connected_peers,
            app_state.last_tuning_limits.max_connected_peers,
        ),
        build_tuning_numeric_line(
            ctx,
            "Read Slots:",
            app_state.limits.disk_read_permits,
            app_state.last_tuning_limits.disk_read_permits,
            ctx.state_success(),
        ),
        build_tuning_numeric_line(
            ctx,
            "Write Slots:",
            app_state.limits.disk_write_permits,
            app_state.last_tuning_limits.disk_write_permits,
            ctx.accent_sky(),
        ),
    ];

    let (lvl, progress) = crate::tui::view::calculate_player_stats(app_state);
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
) -> Line<'static> {
    let delta = current as isize - last as isize;
    let delta_style = if delta > 0 {
        ctx.apply(Style::default().fg(ctx.state_success()))
    } else if delta < 0 {
        ctx.apply(Style::default().fg(ctx.state_error()))
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))
    };
    let delta_text = if delta > 0 {
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

fn build_tuning_peer_line(
    ctx: &ThemeContext,
    used: usize,
    current_limit: usize,
    last_limit: usize,
) -> Line<'static> {
    let delta = current_limit as isize - last_limit as isize;
    let delta_style = if delta > 0 {
        ctx.apply(Style::default().fg(ctx.state_success()))
    } else if delta < 0 {
        ctx.apply(Style::default().fg(ctx.state_error()))
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))
    };
    let delta_text = if delta > 0 {
        format!(" (+{})", delta)
    } else if delta < 0 {
        format!(" ({})", delta)
    } else {
        String::new()
    };
    Line::from(vec![
        Span::styled(
            format!("{:<TUNING_LABEL_WIDTH$}", "Peer Slots:"),
            ctx.apply(Style::default().fg(ctx.state_selected())),
        ),
        Span::raw(" "),
        Span::raw(format!("{} / {}", used, current_limit)),
        Span::styled(delta_text, delta_style),
    ])
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

pub fn draw_peer_stream(f: &mut Frame, app_state: &AppState, area: Rect, ctx: &ThemeContext) {
    if area.height < 3 || area.width < 10 {
        return;
    }

    let selected_torrent = app_state
        .torrent_list_order
        .get(app_state.ui.selected_torrent_index)
        .and_then(|info_hash| app_state.torrents.get(info_hash));

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

    let discovered_count: u64 = disc_slice.iter().sum();
    let connected_count: u64 = conn_slice.iter().sum();
    let disconnected_count: u64 = disconn_slice.iter().sum();

    let legend_style_fn = |count: u64, color: Color| {
        if selected_torrent.is_some() && count > 0 {
            ctx.apply(Style::default().fg(color))
        } else {
            ctx.apply(Style::default().fg(ctx.theme.semantic.surface1))
        }
    };
    let use_compact_legend = should_use_compact_peer_stream_legend(
        area.width.saturating_sub(2) as usize,
        connected_count,
        discovered_count,
        disconnected_count,
    );
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

fn should_use_compact_peer_stream_legend(
    available_width: usize,
    connected: u64,
    discovered: u64,
    disconnected: u64,
) -> bool {
    let full = format!(
        "Connected: {}  Discovered: {}  Disconnected: {}",
        connected, discovered, disconnected
    );
    full.len() > available_width
}

pub fn draw_block_stream_and_disk_orb(
    f: &mut Frame,
    app_state: &AppState,
    area: Rect,
    ctx: &ThemeContext,
) {
    if area.width < 2 || area.height < 2 {
        return;
    }

    match block_stream_and_disk_layout_mode(app_state.screen_area, area) {
        BlockStreamDiskLayoutMode::SideBySide => {
            let split =
                Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
                    .split(area);
            draw_vertical_block_stream_panel(f, app_state, split[0], ctx);
            draw_disk_health_panel(f, app_state, split[1], ctx);
        }
        BlockStreamDiskLayoutMode::Stacked => {
            let split = Layout::vertical([Constraint::Percentage(70), Constraint::Percentage(30)])
                .split(area);
            draw_vertical_block_stream_panel(f, app_state, split[0], ctx);
            draw_disk_health_panel(f, app_state, split[1], ctx);
        }
        BlockStreamDiskLayoutMode::DiskOnly => {
            draw_disk_health_panel(f, app_state, area, ctx);
        }
    }
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

    // Decide split shape using the local pane geometry first; global screen mode can be too coarse
    // and causes unreadable side-by-side micro-panels at transition widths.
    let force_stacked =
        area.width < FORCE_STACKED_WIDTH || area.height > area.width.saturating_mul(2);
    let is_vertical_mode =
        screen_area.width < 100 || (screen_area.height as f32 > screen_area.width as f32 * 0.6);

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
    let block = Block::default()
        .title_top(Span::styled(
            "Disk",
            ctx.apply(Style::default().fg(title_color).bold()),
        ))
        .title_top(
            Line::from(Span::styled(
                disk_state_word,
                ctx.apply(Style::default().fg(title_color).bold()),
            ))
            .alignment(Alignment::Right),
        )
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(border_color)));
    let inner = block.inner(area);
    f.render_widget(block, area);
    draw_disk_health_orb(f, app_state, inner, ctx);
}

fn disk_health_state_word(state_level: u8) -> &'static str {
    match state_level {
        0 => "Stable",
        1 => "Busy",
        2 => "Strain",
        _ => "Chaos",
    }
}

fn disk_health_status_color(ctx: &ThemeContext, state_level: u8) -> Color {
    match state_level {
        0 => {
            if ctx.theme.name == ThemeName::BlackHole {
                ctx.theme.semantic.subtext1
            } else {
                ctx.theme.semantic.subtext0
            }
        }
        1 => ctx.state_info(),
        2 => ctx.state_warning(),
        _ => ctx.state_error(),
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

fn compute_throughput_gap(app_state: &AppState) -> f64 {
    let net_total_bps = app_state.avg_download_history.last().copied().unwrap_or(0)
        + app_state.avg_upload_history.last().copied().unwrap_or(0);
    if net_total_bps == 0 {
        return 0.0;
    }
    let disk_total_bps = app_state.avg_disk_read_bps + app_state.avg_disk_write_bps;
    (net_total_bps.saturating_sub(disk_total_bps) as f64 / net_total_bps as f64).clamp(0.0, 1.0)
}

fn draw_disk_health_orb(f: &mut Frame, app_state: &AppState, area: Rect, ctx: &ThemeContext) {
    if area.width < 2 || area.height < 2 {
        return;
    }

    let health = app_state
        .disk_health_ema
        .max(app_state.disk_health_peak_hold)
        .clamp(0.0, 1.0);
    let deform_profile = disk_health_deform_profile(app_state.disk_health_state_level);
    let gap = compute_throughput_gap(app_state);
    let phase = app_state.disk_health_phase;

    let orb_color = disk_health_status_color(ctx, app_state.disk_health_state_level);
    let has_disk_speed_activity =
        app_state.avg_disk_read_bps > 0 || app_state.avg_disk_write_bps > 0;
    let orb_style = if has_disk_speed_activity {
        ctx.apply(Style::default().fg(orb_color))
    } else {
        ctx.apply(Style::default().fg(orb_color).dim())
    };

    let max_square = area.width.min(area.height);
    if max_square < 3 {
        return;
    }
    let side = ((max_square as f32) * 1.0).round() as u16;
    let side = side.clamp(3, max_square);
    let orb_area = Rect::new(
        area.x + (area.width.saturating_sub(side)) / 2,
        area.y + (area.height.saturating_sub(side)) / 2,
        side,
        side,
    );

    let cells_w = orb_area.width as usize;
    let cells_h = orb_area.height as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(cells_h);

    const BRAILLE_BITS: [[u8; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];

    for cy in 0..cells_h {
        let mut row = String::with_capacity(cells_w);
        for cx in 0..cells_w {
            let mut bits: u8 = 0;
            for (sy, braille_row) in BRAILLE_BITS.iter().enumerate() {
                for (sx, &bit) in braille_row.iter().enumerate() {
                    let px = cx as f64 + (sx as f64 + 0.5) / 2.0;
                    let py = cy as f64 + (sy as f64 + 0.5) / 4.0;

                    let nx = ((px / cells_w as f64) - 0.5) * 2.0;
                    let ny = ((py / cells_h as f64) - 0.5) * 2.0;

                    // Keep gap-driven deformation centered by applying horizontal squeeze symmetrically.
                    let squeeze = (1.0 - (0.22 * gap)).max(0.35);
                    let x = nx / squeeze;
                    // Terminal cells are usually taller than they are wide; compensate to keep a round shape.
                    let y = ny * (cells_w as f64 / cells_h as f64).clamp(0.6, 1.8) * 2.0;
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
        lines.push(Line::from(Span::styled(row, orb_style)));
    }

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
    if peers_chunk.height < 2 || peers_chunk.width < 2 {
        return;
    }

    if let Some(info_hash) = app_state
        .torrent_list_order
        .get(app_state.ui.selected_torrent_index)
    {
        if let Some(torrent) = app_state.torrents.get(info_hash) {
            let state = &torrent.latest_state;

            if peers_chunk.height > 0 {
                let has_established_peers =
                    state.peers.iter().any(|p| p.last_action != "Connecting...");

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

                let (sort_by, sort_direction) = app_state.peer_sort;
                peers_to_display.sort_by(|a, b| {
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
                });

                let all_peer_cols = get_peer_columns();
                let (constraints, visible_indices) =
                    compute_visible_peer_columns(app_state, peers_chunk.width);

                let peer_border_style =
                    if matches!(app_state.ui.selected_header, SelectedHeader::Peer(_)) {
                        ctx.apply(Style::default().fg(ctx.state_selected()))
                    } else {
                        ctx.apply(Style::default().fg(ctx.theme.semantic.surface2))
                    };

                if peers_to_display.is_empty() {
                    draw_swarm_heatmap(
                        f,
                        ctx,
                        &state.peers,
                        state.number_of_pieces_total,
                        peers_chunk,
                    );
                } else {
                    let header_cells: Vec<Cell> = visible_indices
                        .iter()
                        .enumerate()
                        .map(|(visual_idx, &real_idx)| {
                            let def = &all_peer_cols[real_idx];

                            let is_selected =
                                app_state.ui.selected_header == SelectedHeader::Peer(visual_idx);
                            let is_sorting = def.sort_enum == Some(sort_by);

                            let mut style = ctx.apply(Style::default().fg(ctx.state_warning()));
                            if is_sorting {
                                style = style.fg(ctx.state_selected());
                            }
                            style = ctx.apply(style);

                            let mut text = def.header.to_string();
                            if is_sorting {
                                text.push_str(if sort_direction == SortDirection::Ascending {
                                    " ▲"
                                } else {
                                    " ▼"
                                });
                            }

                            let mut span = Span::styled(text, style);
                            if is_selected {
                                span = span.underlined().bold();
                            }
                            Cell::from(Line::from(vec![span]))
                        })
                        .collect();

                    let peer_header = Row::new(header_cells).height(1);

                    let peer_rows = peers_to_display.iter().map(|peer| {
                        let row_color =
                            if peer.download_speed_bps == 0 && peer.upload_speed_bps == 0 {
                                ctx.theme.semantic.surface1
                            } else {
                                ip_to_color(ctx, &peer.address)
                            };

                        let cells: Vec<Cell> = visible_indices
                            .iter()
                            .map(|&real_idx| {
                                let def = &all_peer_cols[real_idx];
                                match def.id {
                                    PeerColumnId::Flags => Line::from(vec![
                                        Span::styled(
                                            "■",
                                            ctx.apply(Style::default().fg(if peer.am_interested {
                                                ctx.accent_sapphire()
                                            } else {
                                                ctx.theme.semantic.surface1
                                            })),
                                        ),
                                        Span::styled(
                                            "■",
                                            ctx.apply(Style::default().fg(if peer.peer_choking {
                                                ctx.accent_maroon()
                                            } else {
                                                ctx.theme.semantic.surface1
                                            })),
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
                                            ctx.apply(Style::default().fg(if peer.am_choking {
                                                ctx.accent_peach()
                                            } else {
                                                ctx.theme.semantic.surface1
                                            })),
                                        ),
                                    ])
                                    .into(),
                                    PeerColumnId::Address => {
                                        let display = if app_state.anonymize_torrent_names {
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
                                    PeerColumnId::Action => Cell::from(peer.last_action.clone()),
                                    PeerColumnId::Progress => {
                                        let total = state.number_of_pieces_total as usize;
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
                                        Cell::from(format!("{:.0}%", pct))
                                    }
                                    PeerColumnId::DownSpeed => {
                                        if peers_chunk.width > 120 {
                                            Cell::from(format!(
                                                "{} ({})",
                                                format_speed(peer.download_speed_bps),
                                                format_bytes(peer.total_downloaded)
                                            ))
                                        } else {
                                            Cell::from(format_speed(peer.download_speed_bps))
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
                                            Cell::from(format_speed(peer.upload_speed_bps))
                                        }
                                    }
                                }
                            })
                            .collect();
                        Row::new(cells).style(ctx.apply(Style::default().fg(row_color)))
                    });

                    let peers_table = Table::new(peer_rows, constraints)
                        .header(peer_header)
                        .block(Block::default());

                    let table_rows_needed: u16 = 1 + peers_to_display.len() as u16;
                    let peer_block_height_needed: u16 = table_rows_needed + 1;
                    let remaining_height =
                        peers_chunk.height.saturating_sub(peer_block_height_needed);
                    const MIN_HEATMAP_HEIGHT: u16 = 4;

                    let peers_block = Block::default()
                        .padding(Padding::new(1, 1, 0, 0))
                        .border_style(peer_border_style);

                    if remaining_height >= MIN_HEATMAP_HEIGHT {
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
                        );
                    } else {
                        let inner_peers_area = peers_block.inner(peers_chunk);
                        f.render_widget(peers_block, peers_chunk);
                        f.render_widget(peers_table, inner_peers_area);
                    }
                }
            }
        }
    } else {
        draw_swarm_heatmap(f, ctx, &[], 0, peers_chunk);
    }
}

pub fn draw_torrent_files_panel(
    f: &mut Frame,
    app_state: &AppState,
    area: Rect,
    ctx: &ThemeContext,
) {
    const MIN_HEATMAP_HEIGHT: u16 = 4;

    if area.height < 2 || area.width < 2 {
        return;
    }

    let selected_torrent = app_state
        .torrent_list_order
        .get(app_state.ui.selected_torrent_index)
        .and_then(|info_hash| app_state.torrents.get(info_hash));

    let block = Block::default()
        .title(Span::styled(
            "Files",
            ctx.apply(Style::default().fg(ctx.state_selected())),
        ))
        .borders(Borders::ALL)
        .padding(Padding::new(1, 1, 0, 0))
        .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)));

    let Some(torrent) = selected_torrent else {
        let inner_area = block.inner(area);
        f.render_widget(block, area);
        let empty = Paragraph::new("No torrent selected")
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true });
        f.render_widget(empty, inner_area);
        return;
    };

    let list_items = build_torrent_file_list_items(
        torrent,
        area.width,
        area.height.saturating_sub(2),
        app_state.anonymize_torrent_names,
        app_state.ui.file_activity_download_phase,
        app_state.ui.file_activity_upload_phase,
        ctx,
    );
    let file_block_height_needed = (list_items.len() as u16).saturating_add(2);
    let remaining_height = area.height.saturating_sub(file_block_height_needed);

    if remaining_height >= MIN_HEATMAP_HEIGHT {
        let layout_chunks = Layout::vertical([
            Constraint::Length(file_block_height_needed),
            Constraint::Min(0),
        ])
        .split(area);
        let inner_area = block.inner(layout_chunks[0]);
        f.render_widget(block, layout_chunks[0]);
        f.render_widget(List::new(list_items), inner_area);
        draw_swarm_heatmap(
            f,
            ctx,
            &torrent.latest_state.peers,
            torrent.latest_state.number_of_pieces_total,
            layout_chunks[1],
        );
    } else {
        let inner_area = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(List::new(list_items), inner_area);
    }
}

fn build_torrent_file_list_items(
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

    let visible_rows = TreeMathHelper::get_visible_slice(
        &torrent.file_preview_tree,
        &expanded_state,
        TreeFilter::default(),
        visible_tree_height,
    );

    list_items.extend(visible_rows.iter().map(|item| {
        let indent = "  ".repeat(item.depth + root_depth);
        let icon = if item.node.is_dir {
            ASCII_TREE_DIR_ICON
        } else {
            ASCII_TREE_FILE_ICON
        };
        let relative_path = normalize_tree_relative_path(item.path.as_path());

        let (name_style, suffix) = match item.node.payload.priority {
            FilePriority::Skip => (
                ctx.apply(
                    Style::default()
                        .fg(ctx.theme.semantic.surface1)
                        .add_modifier(Modifier::CROSSED_OUT),
                ),
                Some(" [S]".to_string()),
            ),
            FilePriority::High => (
                ctx.apply(
                    Style::default()
                        .fg(ctx.state_success())
                        .add_modifier(Modifier::BOLD),
                ),
                Some(" [H]".to_string()),
            ),
            FilePriority::Mixed => (
                ctx.apply(
                    Style::default()
                        .fg(ctx.state_warning())
                        .add_modifier(Modifier::ITALIC),
                ),
                Some(" [*]".to_string()),
            ),
            FilePriority::Normal => (
                ctx.apply(Style::default().fg(if item.node.is_dir {
                    ctx.state_info()
                } else {
                    ctx.theme.semantic.text
                })),
                None,
            ),
        };
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

        if let Some(suffix) = suffix {
            spans.push(Span::styled(
                suffix,
                ctx.apply(Style::default().fg(ctx.theme.semantic.surface1)),
            ));
        }

        ListItem::new(Line::from(spans))
    }));

    list_items
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

    if !row_active {
        return vec![Span::styled(display_name.to_string(), active_base_style)];
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
    Duration::from_secs_f64((total_len + wave.band_width) as f64 / wave.steps_per_second.max(1.0))
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
    let cycle_len = total_len + wave.band_width;
    let phase_offset = file_activity_wave_phase_offset(relative_path, left_to_right, cycle_len);
    let head = (step + phase_offset) % cycle_len;
    let logical_idx = if left_to_right {
        global_char_idx
    } else {
        total_len.saturating_sub(1).saturating_sub(global_char_idx)
    };

    (head as isize - logical_idx as isize) >= 0
        && (head as isize - logical_idx as isize) < wave.band_width as isize
}

fn file_activity_wave_phase_offset(
    relative_path: &str,
    left_to_right: bool,
    cycle_len: usize,
) -> usize {
    if cycle_len == 0 {
        return 0;
    }

    let mut hash = 1469598103934665603_u64;
    for byte in relative_path.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(1099511628211);
    }
    hash ^= u64::from(left_to_right);
    hash = hash.wrapping_mul(1099511628211);
    (hash % cycle_len as u64) as usize
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
        return name.to_string();
    }

    let _ = is_dir;
    anonymize_preserving_shape(name)
}

fn anonymize_preserving_shape(input: &str) -> String {
    let seed = stable_string_seed(input);
    input
        .chars()
        .enumerate()
        .map(|(idx, ch)| anonymized_shape_char(seed, idx, ch))
        .collect()
}

fn stable_string_seed(input: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn anonymized_shape_char(seed: u64, idx: usize, ch: char) -> char {
    let mut state = seed ^ ((idx as u64 + 1).wrapping_mul(0x9e3779b97f4a7c15));
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58476d1ce4e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d049bb133111eb);
    state ^= state >> 31;

    if ch.is_ascii_lowercase() {
        (b'a' + (state % 26) as u8) as char
    } else if ch.is_ascii_uppercase() {
        (b'A' + (state % 26) as u8) as char
    } else if ch.is_ascii_digit() {
        (b'0' + (state % 10) as u8) as char
    } else if ch.is_alphabetic() {
        (b'a' + (state % 26) as u8) as char
    } else {
        ch
    }
}

fn draw_swarm_heatmap(
    f: &mut Frame,
    ctx: &ThemeContext,
    peers: &[PeerInfo],
    total_pieces: u32,
    area: Rect,
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

    let total_pieces_usize = total_pieces as usize;
    let mut availability: Vec<u32> = vec![0; total_pieces_usize];
    if total_pieces_usize > 0 {
        for peer in peers {
            for (i, has_piece) in peer.bitfield.iter().enumerate().take(total_pieces_usize) {
                if *has_piece {
                    availability[i] += 1;
                }
            }
        }
    }

    let max_avail = availability.iter().max().copied().unwrap_or(0);
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

    let max_avail_f64 = max_avail.max(5) as f64;
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
            let count = availability[piece_index];
            let (piece_char, color) = if count == 0 {
                (shade_light, color_heatmap_empty)
            } else {
                let norm_val = count as f64 / max_avail_f64;
                if norm_val < 0.20 {
                    (shade_light, color_heatmap_low)
                } else if norm_val < 0.80 {
                    (shade_medium, color_heatmap_medium)
                } else {
                    (shade_dark, color_heatmap_high)
                }
            };
            spans.push(Span::styled(
                piece_char.to_string(),
                ctx.apply(Style::default().fg(color)),
            ));
        }
        lines.push(Line::from(spans));
    }
    let heatmap = Paragraph::new(lines);
    f.render_widget(heatmap, inner_area);
}

pub(crate) fn handle_navigation(app_state: &mut AppState, key_code: KeyCode) {
    let selected_torrent = app_state
        .torrent_list_order
        .get(app_state.ui.selected_torrent_index)
        .and_then(|info_hash| app_state.torrents.get(info_hash));

    let selected_torrent_has_peers =
        selected_torrent.is_some_and(|torrent| !torrent.latest_state.peers.is_empty());

    let selected_torrent_peer_count =
        selected_torrent.map_or(0, |torrent| torrent.latest_state.peers.len());

    let layout_ctx = LayoutContext::new(app_state.screen_area, app_state, DEFAULT_SIDEBAR_PERCENT);
    let layout_plan = calculate_layout(app_state.screen_area, &layout_ctx);
    let (_, visible_torrent_columns) =
        compute_visible_torrent_columns(app_state, layout_plan.list.width);
    let (_, visible_peer_columns) =
        compute_visible_peer_columns(app_state, layout_plan.peers.width);
    let torrent_col_count = visible_torrent_columns.len();
    let peer_col_count = visible_peer_columns.len();

    app_state.ui.selected_header = match app_state.ui.selected_header {
        SelectedHeader::Torrent(i) => {
            if torrent_col_count == 0 {
                SelectedHeader::Torrent(0)
            } else {
                SelectedHeader::Torrent(i.min(torrent_col_count - 1))
            }
        }
        SelectedHeader::Peer(i) => {
            if !selected_torrent_has_peers || peer_col_count == 0 {
                SelectedHeader::Torrent(torrent_col_count.saturating_sub(1))
            } else {
                SelectedHeader::Peer(i.min(peer_col_count - 1))
            }
        }
    };

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
        KeyCode::Left | KeyCode::Char('h') => {
            app_state.ui.selected_header = match app_state.ui.selected_header {
                SelectedHeader::Torrent(0) => {
                    if selected_torrent_has_peers && peer_col_count > 0 {
                        SelectedHeader::Peer(peer_col_count - 1)
                    } else {
                        SelectedHeader::Torrent(0)
                    }
                }
                SelectedHeader::Torrent(i) => SelectedHeader::Torrent(i - 1),
                SelectedHeader::Peer(0) => {
                    SelectedHeader::Torrent(torrent_col_count.saturating_sub(1))
                }
                SelectedHeader::Peer(i) => SelectedHeader::Peer(i - 1),
            };
        }
        KeyCode::Right | KeyCode::Char('l') => {
            app_state.ui.selected_header = match app_state.ui.selected_header {
                SelectedHeader::Torrent(i) => {
                    if i < torrent_col_count.saturating_sub(1) {
                        SelectedHeader::Torrent(i + 1)
                    } else if selected_torrent_has_peers && peer_col_count > 0 {
                        SelectedHeader::Peer(0)
                    } else {
                        SelectedHeader::Torrent(i)
                    }
                }
                SelectedHeader::Peer(i) => {
                    if i < peer_col_count.saturating_sub(1) {
                        SelectedHeader::Peer(i + 1)
                    } else {
                        SelectedHeader::Torrent(0)
                    }
                }
            };
        }
        _ => {}
    }
}

fn handle_search_key(key_code: KeyCode, app: &mut App) -> bool {
    if !matches!(app.app_state.mode, AppMode::Normal) || !app.app_state.ui.is_searching {
        return false;
    }

    match key_code {
        KeyCode::Esc => {
            app.app_state.ui.is_searching = false;
            app.app_state.ui.search_query.clear();
            app.sort_and_filter_torrent_list();
            app.app_state.ui.selected_torrent_index = 0;
        }
        KeyCode::Enter => {
            app.app_state.ui.is_searching = false;
        }
        KeyCode::Backspace => {
            app.app_state.ui.search_query.pop();
            app.sort_and_filter_torrent_list();
            app.app_state.ui.selected_torrent_index = 0;
        }
        KeyCode::Char(c) => {
            app.app_state.ui.search_query.push(c);
            app.sort_and_filter_torrent_list();
            app.app_state.ui.selected_torrent_index = 0;
        }
        _ => {}
    }

    true
}

enum PastedContent<'a> {
    Magnet(&'a str),
    TorrentFile(&'a Path),
    Unsupported,
}

fn classify_pasted_text(pasted_text: &str) -> PastedContent<'_> {
    let pasted_text = pasted_text.trim();
    if pasted_text.starts_with("magnet:") {
        return PastedContent::Magnet(pasted_text);
    }

    let path = Path::new(pasted_text);
    if path.is_file() && path.extension().is_some_and(|ext| ext == "torrent") {
        return PastedContent::TorrentFile(path);
    }

    PastedContent::Unsupported
}

pub fn accepts_pasted_text(pasted_text: &str) -> bool {
    !matches!(
        classify_pasted_text(pasted_text),
        PastedContent::Unsupported
    )
}

async fn handle_pasted_text(app: &mut App, pasted_text: &str) {
    match classify_pasted_text(pasted_text) {
        PastedContent::Magnet(magnet_link) => {
            let download_path = app.client_configs.default_download_folder.clone();

            if let Some(download_path) = download_path {
                let request = app.prepare_add_magnet_request(
                    magnet_link.to_string(),
                    Some(download_path),
                    None,
                    HashMap::new(),
                );
                let _ = app
                    .app_command_tx
                    .send(AppCommand::SubmitControlRequest(request))
                    .await;
            } else {
                app.app_state.pending_torrent_link = magnet_link.to_string();
                let initial_path = app.get_initial_destination_path();
                let _ = app.app_command_tx.try_send(AppCommand::FetchFileTree {
                    path: initial_path,
                    browser_mode: FileBrowserMode::DownloadLocSelection {
                        torrent_files: vec![],
                        container_name: String::new(),
                        use_container: false,
                        is_editing_name: false,
                        focused_pane: BrowserPane::FileSystem,
                        preview_tree: Vec::new(),
                        preview_state: TreeViewState::default(),
                        cursor_pos: 0,
                        original_name_backup: "Magnet Download".to_string(),
                    },
                    highlight_path: None,
                });
            }
        }
        PastedContent::TorrentFile(path) => {
            if let Some(download_path) = app.client_configs.default_download_folder.clone() {
                match app.prepare_add_torrent_file_request(
                    path.to_path_buf(),
                    Some(download_path),
                    None,
                    HashMap::new(),
                ) {
                    Ok(request) => {
                        let _ = app
                            .app_command_tx
                            .send(AppCommand::SubmitControlRequest(request))
                            .await;
                    }
                    Err(error) => {
                        app.app_state.system_error = Some(error);
                    }
                }
            } else {
                let _ = app
                    .app_command_tx
                    .try_send(AppCommand::AddTorrentFromFile(path.to_path_buf()));
            }
        }
        PastedContent::Unsupported => {
            let pasted_text = pasted_text.trim();
            tracing_event!(
                Level::WARN,
                "Pasted content not recognized as magnet link or torrent file: {}",
                pasted_text
            );
            app.app_state.system_error =
                Some("Pasted content not recognized as magnet link or torrent file.".to_string());
        }
    }
}
pub async fn handle_event(event: CrosstermEvent, app: &mut App) {
    match event {
        CrosstermEvent::Key(key) if key.kind == KeyEventKind::Press => {
            let _ = handle_key_press(key, app).await;
        }
        CrosstermEvent::Paste(pasted_text) => {
            let _ = handle_paste_text(pasted_text.trim().to_string(), app).await;
        }
        _ => {}
    };
}
async fn handle_key_press(key: KeyEvent, app: &mut App) -> bool {
    if handle_search_key(key.code, app) {
        app.app_state.ui.needs_redraw = true;
        return true;
    }

    if handle_reducer_key(key, app).await {
        return true;
    }

    false
}
async fn handle_reducer_key(key: KeyEvent, app: &mut App) -> bool {
    let Some(action) = map_key_to_ui_action(key) else {
        return false;
    };

    let result = reduce_ui_action(&mut app.app_state, action);
    if result.redraw {
        app.app_state.ui.needs_redraw = true;
    }
    execute_ui_effects(app, result.effects).await;
    true
}
async fn handle_paste_text(text: String, app: &mut App) -> bool {
    let result = reduce_ui_action(&mut app.app_state, UiAction::PasteText(text));
    if result.redraw {
        app.app_state.ui.needs_redraw = true;
    }
    execute_ui_effects(app, result.effects).await;
    true
}

async fn execute_ui_effects(app: &mut App, effects: Vec<UiEffect>) {
    for effect in effects {
        execute_ui_effect(app, effect).await;
    }
}

async fn execute_ui_effect(app: &mut App, effect: UiEffect) {
    match effect {
        UiEffect::ToPowerSaving => {
            app.app_state.mode = AppMode::PowerSaving;
        }
        UiEffect::ToDeleteConfirm => {
            app.app_state.mode = AppMode::DeleteConfirm;
        }
        UiEffect::OpenAddTorrentFileBrowser => {
            let initial_path = app.get_initial_source_path();
            let _ = app.app_command_tx.try_send(AppCommand::FetchFileTree {
                path: initial_path,
                browser_mode: FileBrowserMode::File(vec![".torrent".to_string()]),
                highlight_path: None,
            });
        }
        UiEffect::OpenConfigScreen => {
            *app.app_state.ui.config.settings_edit = app.client_configs.clone();
            app.app_state.ui.config.selected_index = 0;
            app.app_state.ui.config.items = ConfigItem::iter().collect::<Vec<_>>();
            app.app_state.ui.config.editing = None;
            app.app_state.mode = AppMode::Config;
        }
        UiEffect::BroadcastManagerDataRate(new_rate) => {
            for manager_tx in app.torrent_manager_command_txs.values() {
                let _ = manager_tx.try_send(ManagerCommand::SetDataRate(new_rate));
            }
        }
        UiEffect::ApplyThemePrev => {
            if app.is_current_shared_follower() {
                app.app_state.system_error = Some(
                    "Shared theme changes are leader-only while this node is a follower."
                        .to_string(),
                );
                return;
            }
            let themes = crate::theme::ThemeName::sorted_for_ui();
            let current_idx = themes
                .iter()
                .position(|&t| t == app.client_configs.ui_theme)
                .unwrap_or(0);
            let new_idx = if current_idx == 0 {
                themes.len() - 1
            } else {
                current_idx - 1
            };
            app.client_configs.ui_theme = themes[new_idx];
            app.app_state.theme = crate::theme::Theme::builtin(themes[new_idx]);
            let _ = app
                .app_command_tx
                .try_send(AppCommand::UpdateConfig(app.client_configs.clone()));
        }
        UiEffect::ApplyThemeNext => {
            if app.is_current_shared_follower() {
                app.app_state.system_error = Some(
                    "Shared theme changes are leader-only while this node is a follower."
                        .to_string(),
                );
                return;
            }
            let themes = crate::theme::ThemeName::sorted_for_ui();
            let current_idx = themes
                .iter()
                .position(|&t| t == app.client_configs.ui_theme)
                .unwrap_or(0);
            let new_idx = (current_idx + 1) % themes.len();
            app.client_configs.ui_theme = themes[new_idx];
            app.app_state.theme = crate::theme::Theme::builtin(themes[new_idx]);
            let _ = app
                .app_command_tx
                .try_send(AppCommand::UpdateConfig(app.client_configs.clone()));
        }
        UiEffect::SendPause(info_hash) => {
            let _ = app
                .app_command_tx
                .try_send(AppCommand::SubmitControlRequest(ControlRequest::Pause {
                    info_hash_hex: hex::encode(info_hash),
                }));
        }
        UiEffect::SendResume(info_hash) => {
            let _ = app
                .app_command_tx
                .try_send(AppCommand::SubmitControlRequest(ControlRequest::Resume {
                    info_hash_hex: hex::encode(info_hash),
                }));
        }
        UiEffect::OpenHelpScreen => {
            app.app_state.mode = AppMode::Help;
        }
        UiEffect::OpenRssScreen => {
            app.app_state.ui.rss.active_screen = RssScreen::Unified;
            app.app_state.mode = AppMode::Rss;
        }
        UiEffect::OpenJournalScreen => {
            app.app_state.ui.journal.selected_index = 0;
            app.app_state.mode = AppMode::Journal;
        }
        UiEffect::HandlePastedText(text) => {
            handle_pasted_text(app, &text).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{
        AppState, DataRate, PeerInfo, SelectedHeader, TorrentControlState, TorrentDisplayState,
        TorrentMetrics,
    };
    use crate::config::{PeerSortColumn, SortDirection, TorrentSortColumn};
    use crate::errors::StorageError;
    use crate::theme::{Theme, ThemeContext, ThemeName};
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;
    use tempfile::tempdir;

    fn create_mock_metrics(peer_count: usize) -> TorrentMetrics {
        let mut metrics = TorrentMetrics::default();
        let mut peers = Vec::new();
        for i in 0..peer_count {
            peers.push(PeerInfo {
                address: format!("127.0.0.1:{}", 6881 + i),
                ..Default::default()
            });
        }
        metrics.peers = peers;
        metrics
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
        app_state.ui.file_browser.is_searching = true;
        app_state.ui.file_browser.search_query = "downloads".to_string();

        let result = reduce_ui_action(&mut app_state, UiAction::StartSearch);

        assert!(result.redraw);
        assert!(app_state.ui.is_searching);
        assert!(app_state.ui.file_browser.is_searching);
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
    fn reducer_navigate_updates_selection() {
        let mut app_state = create_test_app_state();
        app_state.ui.selected_torrent_index = 0;
        app_state.ui.selected_header = SelectedHeader::Torrent(0);

        let result = reduce_ui_action(&mut app_state, UiAction::Navigate(KeyCode::Down));

        assert!(result.redraw);
        assert_eq!(app_state.ui.selected_torrent_index, 1);
        assert_eq!(app_state.ui.selected_peer_index, 0);
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
    fn reducer_toggle_torrent_files_flips_flag() {
        let mut app_state = AppState::default();
        assert!(!app_state.ui.show_torrent_files);

        reduce_ui_action(&mut app_state, UiAction::ToggleTorrentFiles);
        assert!(app_state.ui.show_torrent_files);

        reduce_ui_action(&mut app_state, UiAction::ToggleTorrentFiles);
        assert!(!app_state.ui.show_torrent_files);
    }

    #[test]
    fn split_path_components_handles_windows_paths() {
        assert_eq!(
            split_path_components(r"C:\Users\jagat\Documents\seedbox"),
            vec!["C:", "Users", "jagat", "Documents", "seedbox"]
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
        let shaped = middle_ellipsize_path(r"C:\Users\jagat\Documents\seedbox", 18);
        assert!(shaped.chars().count() <= 18, "{shaped}");
        assert!(shaped.starts_with("C:"), "{shaped}");
        assert!(shaped.ends_with("seedbox"), "{shaped}");
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
            download_path: Some(PathBuf::from(r"C:\Users\jagat\Documents\seedbox")),
            container_name: Some("[team] sample release".to_string()),
            is_multi_file: true,
            torrent_name: "episode 01.mkv".to_string(),
            info_hash: vec![1, 2, 3, 4],
            ..Default::default()
        };

        assert_eq!(
            torrent_root_path_label(&metrics, false),
            r"C:\Users\jagat\Documents\seedbox"
        );
    }

    #[test]
    fn anonymize_preserving_shape_keeps_length_and_structure() {
        let original = r"C:\Users\jagat\Documents\seedbox\episode_01.mkv";
        let anonymized = anonymize_preserving_shape(original);

        assert_eq!(anonymized.chars().count(), original.chars().count());
        assert_eq!(
            anonymized.matches('\\').count(),
            original.matches('\\').count()
        );
        assert_eq!(
            anonymized.matches(':').count(),
            original.matches(':').count()
        );
        assert_eq!(
            anonymized.matches('.').count(),
            original.matches('.').count()
        );
        assert_eq!(
            anonymized.matches('_').count(),
            original.matches('_').count()
        );
        assert_ne!(anonymized, original);
    }

    #[test]
    fn torrent_root_path_label_anonymize_preserves_path_shape() {
        let metrics = TorrentMetrics {
            download_path: Some(PathBuf::from(r"C:\Users\jagat\Documents\seedbox")),
            torrent_name: "episode 01.mkv".to_string(),
            ..Default::default()
        };

        let original = torrent_root_path_label(&metrics, false);
        let anonymized = torrent_root_path_label(&metrics, true);

        assert_eq!(anonymized.chars().count(), original.chars().count());
        assert_eq!(
            anonymized.matches('\\').count(),
            original.matches('\\').count()
        );
        assert_eq!(
            anonymized.matches(':').count(),
            original.matches(':').count()
        );
        assert_ne!(anonymized, original);
    }

    #[test]
    fn shaped_row_start_offsets_account_for_hidden_path_separators() {
        let rows = vec![
            r"C:\Users".to_string(),
            "jagat".to_string(),
            "seedbox".to_string(),
        ];

        assert_eq!(shaped_row_start_offsets(&rows), vec![0, 9, 15]);
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
        let cycle_len = total_len + wave.band_width;
        let phase_offset = file_activity_wave_phase_offset(relative_path, false, cycle_len);
        let step = (mirrored_idx + 1 + cycle_len - phase_offset) % cycle_len;

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
        let path = r"C:\Users\jagat\Documents";
        assert_eq!(
            shape_root_path_for_viewport(path, path.len(), 4),
            vec![path.to_string()]
        );
    }

    #[test]
    fn shape_root_path_for_viewport_uses_middle_ellipsis_when_only_one_row_is_available() {
        let rows = shape_root_path_for_viewport(r"C:\Users\jagat\Documents\seedbox", 18, 1);
        assert_eq!(rows.len(), 1);
        assert!(rows_fit_in_box(&rows, 18, 1), "{rows:?}");
        assert!(rows[0].starts_with("C:"), "{rows:?}");
        assert!(rows[0].ends_with("seedbox"), "{rows:?}");
        assert!(rows[0].contains("..."), "{rows:?}");
    }

    #[test]
    fn shape_root_path_for_viewport_splits_into_vertical_segments_when_narrow() {
        assert_eq!(
            shape_root_path_for_viewport(r"C:\Users\jagat\Documents\seedbox", 10, 5),
            vec!["C:\\Users", "jagat", "Documents", "seedbox"]
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
            shape_root_path_for_viewport(r"C:\Users\jagat\Documents\seedbox", 16, 3),
            vec!["C:\\Users\\jagat", "Documents", "seedbox"]
        );
    }

    #[test]
    fn shape_root_path_for_viewport_truncates_overwide_group_when_needed() {
        assert_eq!(
            shape_root_path_for_viewport(
                r"C:\Users\jagat\[251226][longlonglonglong] release",
                12,
                2
            ),
            vec!["C:\\Users", "jagat"]
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
            r"C:\Users\jagat\Documents\seedbox",
            r"C:\Users\jagat\Documents\seedbox\[251226][long-release-name] Episode 01.mkv",
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
        let path = r"C:\Users\jagat\Documents\seedbox\[251226][long-release-name]\Episode 01.mkv";

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
        let path =
            r"C:\Users\jagat\Documents\seedbox\[251226][long-release-name]\subdir\Episode 01.mkv";

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
            shape_root_path_for_viewport(r"C:\Users\jagat\Documents\seedbox", 24, 4),
            vec!["C:\\Users\\jagat\\Documents", "seedbox"]
        );
        assert_eq!(
            shape_root_path_for_viewport(r"C:\Users\jagat\Documents\seedbox", 18, 4),
            vec!["C:\\Users\\jagat", "Documents\\seedbox"]
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
            Some(UiAction::ToggleTorrentFiles)
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
        assert!(accepts_pasted_text(
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567"
        ));
    }

    #[test]
    fn accepts_existing_torrent_files_as_paste_candidates() {
        let dir = tempdir().expect("temp dir");
        let torrent_path = dir.path().join("sample_fixture.torrent");
        fs::write(&torrent_path, b"sample torrent data").expect("write torrent fixture");

        assert!(accepts_pasted_text(torrent_path.to_string_lossy().as_ref()));
    }

    #[test]
    fn rejects_invalid_paste_candidates() {
        assert!(!accepts_pasted_text("jj"));
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
        app_state.ui.selected_header = SelectedHeader::Torrent(1);
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
        assert_eq!(app_state.torrent_sort.1, SortDirection::Descending);
    }

    #[test]
    fn reducer_sort_by_selected_column_updates_peer_sort() {
        let mut app_state = create_test_app_state();
        app_state.screen_area = Rect::new(0, 0, 220, 80);
        app_state.ui.selected_torrent_index = 0;
        app_state.ui.selected_header = SelectedHeader::Peer(0);
        app_state.peer_sort = (PeerSortColumn::Address, SortDirection::Ascending);

        let _ = reduce_ui_action(&mut app_state, UiAction::SortBySelectedColumn);

        assert_eq!(app_state.peer_sort.0, PeerSortColumn::Flags);
        assert_eq!(app_state.peer_sort.1, SortDirection::Descending);
    }

    #[test]
    fn critical_details_panel_returns_simple_text_for_unavailable_data() {
        let mut torrent = create_mock_display_state(0);
        torrent.latest_state.data_available = false;
        torrent.integrity_next_probe_in = Some(Duration::from_secs(5));
        torrent.latest_state.download_path = Some("/downloads".into());
        torrent.latest_state.container_name = Some("sample".to_string());
        torrent.latest_file_probe_status = Some(TorrentFileProbeStatus::Files(vec![
            crate::torrent_manager::FileProbeEntry {
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
            crate::torrent_manager::FileProbeEntry {
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
    fn peer_stream_smoothed_activity_blends_neighbors() {
        let data = [0_u64, 10, 0];
        let smoothed = peer_stream_smoothed_activity(&data, 1);
        assert!((smoothed - 5.0).abs() < f64::EPSILON);
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
    fn file_activity_wave_phase_offset_is_stable_per_path() {
        let cycle_len = 23;
        let alpha = file_activity_wave_phase_offset("folder/alpha.bin", false, cycle_len);
        let alpha_again = file_activity_wave_phase_offset("folder/alpha.bin", false, cycle_len);
        let beta = file_activity_wave_phase_offset("folder/beta.bin", false, cycle_len);

        assert_eq!(alpha, alpha_again);
        assert!(alpha < cycle_len);
        assert!(beta < cycle_len);
        assert_ne!(alpha, beta);
    }

    #[test]
    fn render_file_tree_name_spans_keeps_inactive_rows_at_base_style() {
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
        assert_eq!(spans[0].style, ctx.apply(base_style));
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
        let items = build_torrent_file_list_items(&torrent, 40, 3, false, 0.0, 0.0, &ctx);

        assert_eq!(items.len(), 3);
    }

    #[test]
    fn block_stream_and_disk_layout_uses_side_by_side_when_vertical_and_roomy() {
        let mode =
            block_stream_and_disk_layout_mode(Rect::new(0, 0, 90, 70), Rect::new(0, 0, 40, 18));
        assert_eq!(mode, BlockStreamDiskLayoutMode::SideBySide);
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
    fn peer_stream_legend_compacts_when_width_is_tight() {
        assert!(should_use_compact_peer_stream_legend(32, 5, 182, 104));
    }

    #[test]
    fn peer_stream_legend_stays_verbose_when_width_allows() {
        assert!(!should_use_compact_peer_stream_legend(90, 5, 182, 104));
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

        execute_ui_effect(&mut app, UiEffect::OpenRssScreen).await;

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

        execute_ui_effect(&mut app, UiEffect::OpenJournalScreen).await;

        assert!(matches!(app.app_state.mode, AppMode::Journal));
        assert_eq!(app.app_state.ui.journal.selected_index, 0);
        let _ = app.shutdown_tx.send(());
    }
}
