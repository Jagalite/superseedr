// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::f64::consts::TAU;

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Context, Line as CanvasLine};
use ratatui::widgets::{Block, Borders};
use ratatui::Frame;

use crate::app::{AppState, DhtVisualization, DiskHealthVisualization};
use crate::dht_service::{DhtStatus, DhtWaveTelemetry};
use crate::theme::{ThemeContext, ThemeName};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DiskHealthSignals {
    pub(crate) health: f64,
    pub(crate) throughput_gap: f64,
    pub(crate) read_signal: f64,
    pub(crate) write_signal: f64,
    pub(crate) state_level: u8,
    pub(crate) phase: f64,
    pub(crate) active: bool,
}

impl DiskHealthSignals {
    pub(crate) fn from_app(app_state: &AppState) -> Self {
        let net_total_bps = app_state.avg_download_history.last().copied().unwrap_or(0)
            + app_state.avg_upload_history.last().copied().unwrap_or(0);
        let disk_total_bps = app_state.avg_disk_read_bps + app_state.avg_disk_write_bps;
        let throughput_gap = if net_total_bps == 0 {
            0.0
        } else {
            (net_total_bps.saturating_sub(disk_total_bps) as f64 / net_total_bps as f64)
                .clamp(0.0, 1.0)
        };
        Self {
            health: app_state
                .disk_health_ema
                .max(app_state.disk_health_peak_hold)
                .clamp(0.0, 1.0),
            throughput_gap,
            read_signal: normalize_disk_rate(app_state.avg_disk_read_bps),
            write_signal: normalize_disk_rate(app_state.avg_disk_write_bps),
            state_level: app_state.disk_health_state_level.min(3),
            phase: app_state.disk_health_phase,
            active: disk_total_bps > 0,
        }
    }
}

fn normalize_disk_rate(bytes_per_second: u64) -> f64 {
    const RESPONSE_MIDPOINT_BYTES_PER_SECOND: f64 = 64.0 * 1024.0 * 1024.0;
    let rate = bytes_per_second as f64;
    if rate <= 0.0 {
        0.0
    } else {
        (rate / (rate + RESPONSE_MIDPOINT_BYTES_PER_SECOND)).clamp(0.0, 1.0)
    }
}

#[derive(Clone, Copy)]
struct DiskPalette {
    read: Color,
    write: Color,
    pressure: Color,
    core: Color,
}

impl DiskPalette {
    fn from_theme(ctx: &ThemeContext, state_level: u8) -> Self {
        let state_color = disk_health_status_color(ctx, state_level);
        Self {
            read: state_color,
            write: state_color,
            pressure: state_color,
            core: state_color,
        }
    }
}

pub(crate) fn disk_health_status_color(ctx: &ThemeContext, state_level: u8) -> Color {
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

#[derive(Clone, Copy)]
struct DhtPalette {
    query: Color,
    peer_yield: Color,
    power_scale: Color,
    neutral: Color,
}

impl DhtPalette {
    fn from_theme(ctx: &ThemeContext) -> Self {
        Self {
            query: ctx.peer_discovered(),
            peer_yield: ctx.peer_connected(),
            power_scale: ctx.accent_peach(),
            neutral: ctx.theme.semantic.surface2,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DhtVisualSignals {
    queries: usize,
    peer_yield: usize,
    query_signal: f64,
    instant_query_signal: f64,
    yield_signal: f64,
    frequency: f64,
    query_surge: f64,
    power_scale_halves: u8,
    time: f64,
}

impl DhtVisualSignals {
    pub(crate) fn from_live(
        app_state: &AppState,
        _status: &DhtStatus,
        telemetry: &DhtWaveTelemetry,
    ) -> Self {
        let queries = telemetry.inflight_ipv4_queries + telemetry.inflight_ipv6_queries;
        let peer_yield = telemetry.unique_peers_found_last_10s;
        let raw_query_signal = normalize_dht_query_signal(queries);
        let raw_yield_signal = normalize_dht_peer_yield(peer_yield);
        let wave = &app_state.ui.dht_wave;
        let (frequency, query_signal) = if wave.initialized {
            (wave.frequency, wave.query_load)
        } else {
            (0.08 + raw_query_signal * 0.18, raw_query_signal)
        };
        let time = if wave.initialized {
            wave.phase
        } else {
            let phase_speed = 0.03 + raw_query_signal * (0.85 + raw_query_signal * 0.75);
            app_state.ui.effects_phase_time * phase_speed
        };
        Self {
            queries,
            peer_yield,
            query_signal: query_signal.clamp(0.0, 1.0),
            instant_query_signal: raw_query_signal,
            yield_signal: raw_yield_signal,
            frequency: frequency.clamp(0.06, 0.38),
            query_surge: if wave.initialized {
                wave.query_surge
            } else {
                0.0
            },
            power_scale_halves: telemetry.demand_power_scale_halves,
            time,
        }
    }
}

pub(crate) fn normalize_dht_query_signal(total_queries: usize) -> f64 {
    let queries = total_queries as f64;
    if queries <= 0.0 {
        0.0
    } else {
        (queries / (queries + 40.0)).clamp(0.0, 1.0)
    }
}

pub(crate) fn normalize_dht_peer_yield(unique_peers_found_last_10s: usize) -> f64 {
    let peers = unique_peers_found_last_10s as f64;
    if peers <= 0.0 {
        0.0
    } else {
        (peers / (peers + 256.0)).clamp(0.0, 1.0)
    }
}

fn dht_power_scale_label(scale_halves: u8) -> String {
    if scale_halves.is_multiple_of(2) {
        format!("{}x", scale_halves / 2)
    } else {
        format!("{}.5x", scale_halves / 2)
    }
}

fn dht_metric_title_spans(signals: DhtVisualSignals, ctx: &ThemeContext) -> Vec<Span<'static>> {
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
    let scale_halves = if signals.power_scale_halves == 0 {
        2
    } else {
        signals.power_scale_halves
    };
    let mut spans = Vec::new();
    if scale_halves != 2 {
        spans.extend([
            Span::styled(dht_power_scale_label(scale_halves), multiplier_style),
            Span::styled("(", multiplier_style),
        ]);
    }
    spans.extend([
        Span::styled(signals.queries.to_string(), query_style),
        Span::raw(" "),
        Span::styled(signals.peer_yield.to_string(), peer_yield_style),
    ]);
    if scale_halves != 2 {
        spans.push(Span::styled(")", multiplier_style));
    }
    spans
}

fn title_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.chars().count()).sum()
}

pub fn draw_disk_health_visualization(
    frame: &mut Frame,
    app_state: &AppState,
    area: Rect,
    view: DiskHealthVisualization,
    ctx: &ThemeContext,
) {
    if area.width < 3 || area.height < 2 || view == DiskHealthVisualization::Classic {
        return;
    }
    let signals = DiskHealthSignals::from_app(app_state);
    let palette = DiskPalette::from_theme(ctx, signals.state_level);
    let (x_bounds, y_bounds) = canvas_bounds(area);
    let center_y = canvas_center_y(area, y_bounds);
    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds(x_bounds)
        .y_bounds(y_bounds)
        .paint(|canvas| match view {
            DiskHealthVisualization::Classic => {}
            DiskHealthVisualization::SeekPendulum => {
                draw_seek_pendulum(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::StorageDial => {
                draw_storage_dial(canvas, x_bounds, center_y, signals, palette)
            }
        });
    frame.render_widget(canvas, area);
}

pub fn draw_dht_visualization(
    frame: &mut Frame,
    app_state: &AppState,
    status: &DhtStatus,
    telemetry: &DhtWaveTelemetry,
    area: Rect,
    view: DhtVisualization,
    ctx: &ThemeContext,
) {
    if area.width < 10 || area.height < 3 || view == DhtVisualization::Classic {
        return;
    }
    let signals = DhtVisualSignals::from_live(app_state, status, telemetry);
    let palette = DhtPalette::from_theme(ctx);
    let available_width = usize::from(area.width.saturating_sub(2));
    let metric_spans = dht_metric_title_spans(signals, ctx);
    let metric_width = title_width(&metric_spans);
    let left_title = if "DHT".len() + 1 + metric_width <= available_width {
        Some("DHT".to_owned())
    } else {
        None
    };
    let mut block = Block::default();
    if let Some(left_title) = left_title {
        block = block.title_top(Line::from(Span::styled(
            left_title,
            ctx.apply(Style::default().fg(palette.query)),
        )));
    }
    block = block.title_top(Line::from(metric_spans).alignment(Alignment::Right));
    if app_state.ui.visualization_focus.active {
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
                        .fg(ctx.state_selected())
                        .add_modifier(Modifier::BOLD),
                ),
            ))
            .alignment(Alignment::Center),
        );
    }
    let block = block
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)));
    let (x_bounds, y_bounds) = canvas_bounds(area);
    let canvas = Canvas::default()
        .block(block)
        .marker(Marker::Braille)
        .x_bounds(x_bounds)
        .y_bounds(y_bounds)
        .paint(|canvas| match view {
            DhtVisualization::Classic => {}
            DhtVisualization::RelayRibbon => draw_relay_ribbon(canvas, x_bounds, signals, palette),
            DhtVisualization::PulseGrid => draw_pulse_grid(canvas, x_bounds, signals, palette),
            DhtVisualization::LookupVortex => {
                draw_lookup_vortex(canvas, x_bounds, signals, palette)
            }
            DhtVisualization::PeerBloom => draw_peer_bloom(canvas, x_bounds, signals, palette),
        });
    frame.render_widget(canvas, area);
}

fn dht_phase(signals: DhtVisualSignals, speed: f64) -> f64 {
    signals.time * speed * (0.75 + signals.frequency * 2.5)
}

fn dht_power_scale_color(signals: DhtVisualSignals, palette: DhtPalette) -> Color {
    let scale_halves = if signals.power_scale_halves == 0 {
        2
    } else {
        signals.power_scale_halves
    };
    if scale_halves == 2 {
        palette.neutral
    } else {
        palette.power_scale
    }
}

fn dht_activity_core_color(signals: DhtVisualSignals, palette: DhtPalette) -> Color {
    if signals.queries > 0 {
        palette.query
    } else if signals.peer_yield > 0 {
        palette.peer_yield
    } else {
        palette.neutral
    }
}

fn draw_segment(canvas: &mut Context<'_>, start: (f64, f64), end: (f64, f64), color: Color) {
    canvas.draw(&CanvasLine {
        x1: start.0,
        y1: start.1,
        x2: end.0,
        y2: end.1,
        color,
    });
}

fn draw_polyline(canvas: &mut Context<'_>, points: &[(f64, f64)], color: Color) {
    for pair in points.windows(2) {
        draw_segment(canvas, pair[0], pair[1], color);
    }
}

fn disk_load(signals: DiskHealthSignals) -> f64 {
    signals
        .health
        .max(signals.read_signal)
        .max(signals.write_signal)
        .max(f64::from(signals.state_level) / 3.0)
        .clamp(0.0, 1.0)
}

fn disk_deformation(signals: DiskHealthSignals) -> f64 {
    let state_floor = match signals.state_level {
        0 => 0.03,
        1 => 0.18,
        2 => 0.38,
        _ => 0.68,
    };
    (state_floor
        + signals.health * 0.22
        + signals.throughput_gap * 0.16
        + signals.read_signal.max(signals.write_signal) * 0.08)
        .clamp(0.03, 1.0)
}

fn draw_seek_pendulum(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let deform = disk_deformation(signals);
    let pivot = (0.0, 0.68);
    let weight = seek_pendulum_weight(span, signals.phase);
    draw_polyline(
        canvas,
        &[(-span * 0.22, 0.78), pivot, (span * 0.22, 0.78)],
        palette.core,
    );
    draw_segment(canvas, pivot, weight, palette.read);
    draw_filled_diamond(
        canvas,
        weight.0,
        weight.1,
        span * (0.035 + load * 0.018),
        0.12 + deform * 0.05,
        palette.pressure,
    );
    for tick in -4_i32..=4 {
        let x = f64::from(tick) * span * 0.075;
        let height = if tick % 2 == 0 { 0.12 } else { 0.07 };
        draw_segment(canvas, (x, -0.75), (x, -0.75 + height), palette.write);
    }
}

fn seek_pendulum_weight(span: f64, phase: f64) -> (f64, f64) {
    let pivot_y = 0.68;
    let angle = phase.sin() * 0.68;
    (angle.sin() * span * 0.28, pivot_y - angle.cos())
}

fn draw_storage_dial(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    center_y: f64,
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let center = (0.0, center_y);

    let mut arc = Vec::with_capacity(41);
    for step in 0..=40 {
        let unit = step as f64 / 40.0;
        let angle = storage_dial_angle(unit);
        arc.push((
            center.0 + angle.cos() * span * 0.36,
            center.1 + angle.sin() * 0.74,
        ));
    }
    draw_polyline(canvas, &arc, palette.core);

    let progress_steps = (load * 40.0).round() as usize;
    if progress_steps > 0 {
        let mut progress = Vec::with_capacity(progress_steps + 1);
        for step in 0..=progress_steps {
            let unit = step as f64 / 40.0;
            let angle = storage_dial_angle(unit);
            progress.push((
                center.0 + angle.cos() * span * 0.325,
                center.1 + angle.sin() * 0.66,
            ));
        }
        draw_polyline(canvas, &progress, palette.pressure);
    }

    for tick in 0_usize..=20 {
        let unit = tick as f64 / 20.0;
        let angle = storage_dial_angle(unit);
        let major = tick.is_multiple_of(2);
        let outer = (
            center.0 + angle.cos() * span * 0.36,
            center.1 + angle.sin() * 0.74,
        );
        let (inner_radius_x, inner_radius_y) = if major {
            (span * 0.275, 0.54)
        } else {
            (span * 0.305, 0.61)
        };
        let inner = (
            center.0 + angle.cos() * inner_radius_x,
            center.1 + angle.sin() * inner_radius_y,
        );
        draw_segment(canvas, inner, outer, palette.read);
    }

    let needle_angle = storage_dial_needle_angle(load, signals);
    let needle_tip = (
        center.0 + needle_angle.cos() * span * 0.285,
        center.1 + needle_angle.sin() * 0.57,
    );
    let counterweight = (
        center.0 - needle_angle.cos() * span * 0.065,
        center.1 - needle_angle.sin() * 0.13,
    );
    draw_segment(canvas, counterweight, center, palette.pressure);
    draw_segment(canvas, center, needle_tip, palette.pressure);
    draw_filled_diamond(
        canvas,
        center.0,
        center.1,
        span * 0.026,
        0.09,
        palette.write,
    );
}

fn storage_dial_angle(load: f64) -> f64 {
    const START_ANGLE: f64 = TAU * 0.58;
    const SWEEP_ANGLE: f64 = TAU * 0.66;
    START_ANGLE - load.clamp(0.0, 1.0) * SWEEP_ANGLE
}

fn storage_dial_needle_angle(load: f64, signals: DiskHealthSignals) -> f64 {
    let activity = signals.read_signal.max(signals.write_signal);
    let flutter = if signals.active {
        let amplitude = 0.008 + activity * 0.012;
        let wave = signals.phase.sin() * 0.72 + (signals.phase * 2.3 + 0.4).sin() * 0.28;
        wave * amplitude
    } else {
        0.0
    };

    storage_dial_angle(load + flutter)
}

fn draw_relay_ribbon(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DhtVisualSignals,
    palette: DhtPalette,
) {
    let span = bounds[1] - bounds[0];
    let phase = dht_phase(signals, 1.15);
    for ribbon in 0..4 {
        let query_ribbon = ribbon < 2;
        let strength = if query_ribbon {
            signals
                .query_signal
                .max(signals.instant_query_signal)
                .max(signals.query_surge)
        } else {
            signals.yield_signal
        };
        let color = if query_ribbon {
            palette.query
        } else if signals.peer_yield > 0 {
            palette.peer_yield
        } else {
            palette.neutral
        };
        let mut points = Vec::with_capacity(17);
        for step in 0..=16 {
            let unit = step as f64 / 16.0;
            let x = -span * 0.44 + unit * span * 0.88;
            let direction = if query_ribbon { 1.0 } else { -1.0 };
            let y = (unit * TAU * 1.5 + direction * phase + ribbon as f64 * 0.85).sin()
                * (0.10 + strength * 0.24)
                + (ribbon as f64 - 1.5) * 0.14;
            points.push((x, y));
        }
        draw_polyline(canvas, &points, color);
    }
    let scale_width = span * (0.04 + f64::from(signals.power_scale_halves.clamp(2, 8)) * 0.008);
    draw_segment(
        canvas,
        (-scale_width, 0.0),
        (scale_width, 0.0),
        dht_power_scale_color(signals, palette),
    );
}

fn draw_pulse_grid(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DhtVisualSignals,
    palette: DhtPalette,
) {
    let span = bounds[1] - bounds[0];
    let phase = dht_phase(signals, 1.30);
    let left = -span * 0.38;
    let right = span * 0.38;
    for column in 0..=6 {
        let x = left + column as f64 / 6.0 * (right - left);
        draw_segment(canvas, (x, -0.70), (x, 0.70), palette.neutral);
    }
    for row in 0..=5 {
        let y = -0.70 + row as f64 / 5.0 * 1.40;
        draw_segment(canvas, (left, y), (right, y), palette.neutral);
    }
    let scan = ((phase.sin() * 0.5 + 0.5) * 5.0).round() as usize;
    let scan_x = left + (scan as f64 + 0.5) / 6.0 * (right - left);
    draw_segment(
        canvas,
        (scan_x, -0.70),
        (scan_x, 0.70),
        dht_power_scale_color(signals, palette),
    );
    let query_cells = if signals.queries == 0 {
        0
    } else {
        1 + (signals.query_signal.max(signals.instant_query_signal) * 8.0).round() as usize
    };
    for cell in 0..query_cells {
        let column = (cell * 5 + scan * 3) % 6;
        let row = (cell * 3 + scan) % 5;
        let x = left + (column as f64 + 0.5) / 6.0 * (right - left);
        let y = -0.70 + (row as f64 + 0.5) / 5.0 * 1.40;
        draw_filled_diamond(canvas, x, y, span * 0.018, 0.06, palette.query);
    }
    let yield_cells = if signals.peer_yield == 0 {
        0
    } else {
        1 + (signals.yield_signal * 8.0).round() as usize
    };
    for cell in 0..yield_cells {
        let column = (cell * 4 + scan + 2) % 6;
        let row = (cell * 2 + scan + 3) % 5;
        let x = left + (column as f64 + 0.5) / 6.0 * (right - left);
        let y = -0.70 + (row as f64 + 0.5) / 5.0 * 1.40;
        draw_filled_diamond(canvas, x, y, span * 0.018, 0.06, palette.peer_yield);
    }
}

fn draw_lookup_vortex(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DhtVisualSignals,
    palette: DhtPalette,
) {
    let span = bounds[1] - bounds[0];
    let phase = dht_phase(signals, 1.22);
    for arm in 0..3 {
        let yield_arm = arm == 2;
        let strength = if yield_arm {
            signals.yield_signal
        } else {
            signals
                .query_signal
                .max(signals.instant_query_signal)
                .max(signals.query_surge)
        };
        let color = if yield_arm {
            if signals.peer_yield > 0 {
                palette.peer_yield
            } else {
                palette.neutral
            }
        } else {
            palette.query
        };
        let mut points = Vec::with_capacity(25);
        for step in 0..=24 {
            let unit = step as f64 / 24.0;
            let direction = if yield_arm { -1.0 } else { 1.0 };
            let angle = unit * TAU * 1.55 + direction * phase + arm as f64 * TAU / 3.0;
            let radius = 0.05 + unit * (0.54 + strength * 0.20);
            points.push((angle.cos() * radius * span * 0.48, angle.sin() * radius));
        }
        draw_polyline(canvas, &points, color);
    }
    draw_filled_diamond(
        canvas,
        0.0,
        0.0,
        span * (0.025 + signals.query_signal.max(signals.yield_signal) * 0.02),
        0.10,
        dht_activity_core_color(signals, palette),
    );
}

fn draw_peer_bloom(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DhtVisualSignals,
    palette: DhtPalette,
) {
    let span = bounds[1] - bounds[0];
    let phase = dht_phase(signals, 0.72);
    let petals = 6 + (signals.yield_signal * 4.0).round() as usize;
    for petal in 0..petals {
        let angle = petal as f64 / petals as f64 * TAU + phase * 0.18;
        let tangent = (-angle.sin(), angle.cos());
        let center = (angle.cos() * span * 0.22, angle.sin() * 0.42);
        let half = span * (0.05 + signals.yield_signal * 0.035);
        let tip = (
            angle.cos() * span * (0.37 + signals.yield_signal * 0.06),
            angle.sin() * (0.72 + signals.yield_signal * 0.08),
        );
        draw_segment(
            canvas,
            (0.0, 0.0),
            center,
            if signals.queries > 0 {
                palette.query
            } else {
                palette.neutral
            },
        );
        draw_polyline(
            canvas,
            &[
                (center.0 + tangent.0 * half, center.1 + tangent.1 * 0.10),
                tip,
                (center.0 - tangent.0 * half, center.1 - tangent.1 * 0.10),
                (center.0 + tangent.0 * half, center.1 + tangent.1 * 0.10),
            ],
            if signals.peer_yield > 0 {
                palette.peer_yield
            } else {
                palette.neutral
            },
        );
    }
    draw_filled_diamond(
        canvas,
        0.0,
        0.0,
        span * 0.055,
        0.18,
        dht_activity_core_color(signals, palette),
    );
}

fn canvas_bounds(area: Rect) -> ([f64; 2], [f64; 2]) {
    let width = f64::from(area.width.saturating_sub(2).max(1));
    let height = f64::from(area.height.saturating_sub(2).max(1));
    let half = (width / (height * 2.0)).max(0.65);
    ([-half, half], [-1.0, 1.0])
}

fn canvas_center_y(area: Rect, y_bounds: [f64; 2]) -> f64 {
    let row_height = (y_bounds[1] - y_bounds[0]) / f64::from(area.height.max(1));
    (y_bounds[0] + y_bounds[1]) * 0.5 - row_height * 0.5
}

fn draw_filled_diamond(
    canvas: &mut Context<'_>,
    center_x: f64,
    center_y: f64,
    radius_x: f64,
    radius_y: f64,
    color: Color,
) {
    const STEPS: i32 = 20;
    for step in -STEPS..=STEPS {
        let unit_y = f64::from(step) / f64::from(STEPS);
        let half = radius_x * (1.0 - unit_y.abs());
        let y = center_y + unit_y * radius_y;
        canvas.draw(&CanvasLine {
            x1: center_x - half,
            y1: y,
            x2: center_x + half,
            y2: y,
            color,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    fn sample_dht_inputs() -> (AppState, DhtStatus, DhtWaveTelemetry) {
        let mut state = AppState::default();
        state.ui.dht_wave.initialized = true;
        state.ui.dht_wave.phase = 1.4;
        state.ui.dht_wave.amplitude = 0.31;
        state.ui.dht_wave.harmonic_amplitude = 0.09;
        state.ui.dht_wave.frequency = 0.19;
        state.ui.dht_wave.crest_bias = 0.04;
        state.ui.dht_wave.bootstrap_ratio = 0.78;
        state.ui.dht_wave.discovery_boost = 0.06;
        state.ui.dht_wave.query_load = normalize_dht_query_signal(23);
        state.ui.dht_wave.query_surge = 0.03;
        let status = DhtStatus {
            health: crate::dht_service::DhtHealthSnapshot {
                cached_ipv4_routes: 92,
                cached_ipv6_routes: 28,
                inflight_lookups: 3,
                ..Default::default()
            },
            ..Default::default()
        };
        let telemetry = DhtWaveTelemetry {
            active_lookups: 4,
            inflight_ipv4_queries: 18,
            inflight_ipv6_queries: 5,
            unique_peers_found_last_10s: 37,
            demand_power_scale_halves: 4,
            ..Default::default()
        };
        (state, status, telemetry)
    }

    fn render_dht_inputs(
        view: DhtVisualization,
        width: u16,
        state: &AppState,
        status: &DhtStatus,
        telemetry: &DhtWaveTelemetry,
    ) -> Buffer {
        let backend = TestBackend::new(width, 9);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let ctx = ThemeContext::new(state.theme, 0.0);
        terminal
            .draw(|frame| {
                draw_dht_visualization(frame, state, status, telemetry, frame.area(), view, &ctx)
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    fn render_dht_with_width(view: DhtVisualization, width: u16) -> Buffer {
        let (state, status, telemetry) = sample_dht_inputs();
        render_dht_inputs(view, width, &state, &status, &telemetry)
    }

    fn sample_disk_state() -> AppState {
        let mut state = AppState {
            avg_disk_read_bps: 128 * 1024 * 1024,
            avg_disk_write_bps: 48 * 1024 * 1024,
            disk_health_ema: 0.58,
            disk_health_peak_hold: 0.66,
            disk_health_state_level: 2,
            disk_health_phase: 1.37,
            ..Default::default()
        };
        state.avg_download_history.push(320 * 1024 * 1024);
        state.avg_upload_history.push(24 * 1024 * 1024);
        state
    }

    fn render_disk(view: DiskHealthVisualization, state: &AppState) -> Buffer {
        let backend = TestBackend::new(64, 7);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let ctx = ThemeContext::new(state.theme, 0.0);
        terminal
            .draw(|frame| draw_disk_health_visualization(frame, state, frame.area(), view, &ctx))
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    fn occupied(buffer: &Buffer) -> usize {
        buffer
            .content()
            .iter()
            .filter(|cell| cell.symbol() != " ")
            .count()
    }

    fn interior_cells(buffer: &Buffer) -> Vec<(String, Color)> {
        let area = buffer.area;
        (1..area.height.saturating_sub(1))
            .flat_map(|y| {
                (1..area.width.saturating_sub(1)).filter_map(move |x| {
                    buffer
                        .cell((x, y))
                        .map(|cell| (cell.symbol().to_owned(), cell.fg))
                })
            })
            .collect()
    }

    fn interior_symbols(buffer: &Buffer) -> Vec<String> {
        interior_cells(buffer)
            .into_iter()
            .map(|(symbol, _)| symbol)
            .collect()
    }

    #[test]
    fn retained_dht_gallery_distinguishes_every_candidate() {
        let candidates = DhtVisualization::ALL
            .into_iter()
            .filter(|view| *view != DhtVisualization::Classic)
            .collect::<Vec<_>>();
        assert_eq!(candidates.len(), 4);

        let mut interiors = Vec::with_capacity(candidates.len());
        for view in candidates {
            let buffer = render_dht_with_width(view, 64);
            assert!(
                occupied(&buffer) > 30,
                "{} should draw a visible concept",
                view.label()
            );
            interiors.push(interior_cells(&buffer));
        }

        for left in 0..interiors.len() {
            for right in left + 1..interiors.len() {
                assert_ne!(interiors[left], interiors[right]);
            }
        }
    }

    #[test]
    fn dht_name_without_numeric_prefix_only_appears_in_visualization_focus_mode() {
        let (mut state, status, telemetry) = sample_dht_inputs();
        let normal =
            render_dht_inputs(DhtVisualization::PulseGrid, 52, &state, &status, &telemetry);
        let normal_text = normal
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!normal_text.contains("T14"));
        assert!(!normal_text.contains("Pulse Grid"));

        state.ui.visualization_focus.active = true;
        let focused =
            render_dht_inputs(DhtVisualization::PulseGrid, 52, &state, &status, &telemetry);
        let focused_text = focused
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!focused_text.contains("T14"));
        assert!(!focused_text.contains("TEMP"));
        assert!(focused_text.contains("Pulse Grid"));
    }

    #[test]
    fn every_candidate_reacts_to_current_metrics() {
        let (state, status, mut quiet_telemetry) = sample_dht_inputs();
        quiet_telemetry.inflight_ipv4_queries = 0;
        quiet_telemetry.inflight_ipv6_queries = 0;
        quiet_telemetry.unique_peers_found_last_10s = 0;
        quiet_telemetry.demand_power_scale_halves = 2;

        let mut busy_telemetry = quiet_telemetry.clone();
        busy_telemetry.inflight_ipv4_queries = 48;
        busy_telemetry.inflight_ipv6_queries = 16;
        busy_telemetry.unique_peers_found_last_10s = 320;
        busy_telemetry.demand_power_scale_halves = 6;

        for view in DhtVisualization::ALL
            .into_iter()
            .filter(|view| *view != DhtVisualization::Classic)
        {
            let quiet = render_dht_inputs(view, 64, &state, &status, &quiet_telemetry);
            let busy = render_dht_inputs(view, 64, &state, &status, &busy_telemetry);
            assert_ne!(
                interior_cells(&quiet),
                interior_cells(&busy),
                "{} should react to live DHT metrics",
                view.label()
            );
        }
    }

    #[test]
    fn dht_palette_matches_the_classic_metric_colors() {
        let state = AppState::default();
        let ctx = ThemeContext::new(state.theme, 0.0);
        let palette = DhtPalette::from_theme(&ctx);

        assert_eq!(palette.query, ctx.peer_discovered());
        assert_eq!(palette.peer_yield, ctx.peer_connected());
        assert_eq!(palette.power_scale, ctx.accent_peach());
        assert_eq!(palette.neutral, ctx.theme.semantic.surface2);
    }

    #[test]
    fn vortex_and_bloom_core_color_tracks_activity_instead_of_power_scale() {
        let (state, status, mut telemetry) = sample_dht_inputs();
        telemetry.demand_power_scale_halves = 2;
        let ctx = ThemeContext::new(state.theme, 0.0);
        let palette = DhtPalette::from_theme(&ctx);

        let querying = DhtVisualSignals::from_live(&state, &status, &telemetry);
        assert_eq!(dht_activity_core_color(querying, palette), palette.query);

        telemetry.inflight_ipv4_queries = 0;
        telemetry.inflight_ipv6_queries = 0;
        let yielding = DhtVisualSignals::from_live(&state, &status, &telemetry);
        assert_eq!(
            dht_activity_core_color(yielding, palette),
            palette.peer_yield
        );

        telemetry.unique_peers_found_last_10s = 0;
        let idle = DhtVisualSignals::from_live(&state, &status, &telemetry);
        assert_eq!(dht_activity_core_color(idle, palette), palette.neutral);
    }

    #[test]
    fn every_dht_candidate_uses_only_metric_semantic_colors() {
        let (state, status, telemetry) = sample_dht_inputs();
        let ctx = ThemeContext::new(state.theme, 0.0);
        let palette = DhtPalette::from_theme(&ctx);
        let allowed = [
            palette.query,
            palette.peer_yield,
            palette.power_scale,
            palette.neutral,
        ];

        for view in DhtVisualization::ALL
            .into_iter()
            .filter(|view| *view != DhtVisualization::Classic)
        {
            let colors = interior_cells(&render_dht_inputs(view, 64, &state, &status, &telemetry))
                .into_iter()
                .filter(|(symbol, _)| symbol != " ")
                .map(|(_, color)| color)
                .collect::<Vec<_>>();

            assert!(
                !colors.is_empty(),
                "{} should draw colored cells",
                view.label()
            );
            assert!(
                colors.iter().all(|color| allowed.contains(color)),
                "{} should only use DHT metric semantic colors",
                view.label()
            );
            assert!(
                colors.contains(&palette.query),
                "{} should show query activity with the discovered-peer color",
                view.label()
            );
            assert!(
                colors.contains(&palette.peer_yield),
                "{} should show peer yield with the connected-peer color",
                view.label()
            );
        }
    }

    #[test]
    fn disk_gallery_keeps_and_distinguishes_t09_and_t20() {
        let state = sample_disk_state();
        let candidates = DiskHealthVisualization::ALL
            .into_iter()
            .filter(|view| *view != DiskHealthVisualization::Classic)
            .collect::<Vec<_>>();
        assert_eq!(
            candidates,
            [
                DiskHealthVisualization::SeekPendulum,
                DiskHealthVisualization::StorageDial,
            ]
        );

        let mut interiors = Vec::with_capacity(candidates.len());
        for view in candidates {
            let buffer = render_disk(view, &state);
            assert!(
                occupied(&buffer) > 8,
                "{} should draw a visible concept",
                view.label()
            );
            interiors.push(interior_cells(&buffer));
        }

        for left in 0..interiors.len() {
            for right in left + 1..interiors.len() {
                assert_ne!(interiors[left], interiors[right]);
            }
        }
    }

    #[test]
    fn storage_dial_sweeps_from_low_left_through_top_to_high_right() {
        let low = storage_dial_angle(0.0);
        let midpoint = storage_dial_angle(0.5);
        let high = storage_dial_angle(1.0);

        assert!(low.cos() < 0.0);
        assert!(midpoint.cos().abs() < 1.0e-9);
        assert!(high.cos() > 0.0);
        assert!(low > midpoint && midpoint > high);
    }

    #[test]
    fn storage_dial_center_uses_a_height_scaled_half_row_offset() {
        let short = canvas_center_y(Rect::new(0, 0, 40, 5), [-1.0, 1.0]);
        let tall = canvas_center_y(Rect::new(0, 0, 40, 10), [-1.0, 1.0]);

        assert!((short + 0.2).abs() < 1.0e-9);
        assert!((tall + 0.1).abs() < 1.0e-9);
    }

    #[test]
    fn storage_dial_needle_flutters_around_a_stable_metric_reading() {
        let mut first = sample_disk_state();
        first.disk_health_phase = 0.0;
        let mut later = sample_disk_state();
        later.disk_health_phase = 4.2;
        let first_signals = DiskHealthSignals::from_app(&first);
        let later_signals = DiskHealthSignals::from_app(&later);
        let load = disk_load(first_signals);
        let base_angle = storage_dial_angle(load);
        let first_angle = storage_dial_needle_angle(load, first_signals);
        let later_angle = storage_dial_needle_angle(load, later_signals);

        assert_ne!(first_angle, later_angle);
        assert!((first_angle - base_angle).abs() < 0.09);
        assert!((later_angle - base_angle).abs() < 0.09);
        assert_ne!(
            interior_cells(&render_disk(DiskHealthVisualization::StorageDial, &first)),
            interior_cells(&render_disk(DiskHealthVisualization::StorageDial, &later))
        );
    }

    #[test]
    fn storage_dial_needle_does_not_flutter_when_disk_is_idle() {
        let idle = AppState {
            disk_health_phase: 1.7,
            ..Default::default()
        };
        let signals = DiskHealthSignals::from_app(&idle);
        let load = disk_load(signals);

        assert_eq!(
            storage_dial_needle_angle(load, signals),
            storage_dial_angle(load)
        );
    }

    #[test]
    fn seek_pendulum_is_continuous_across_the_phase_wrap() {
        let epsilon = 1.0e-6;
        let before_wrap = seek_pendulum_weight(10.0, TAU - epsilon);
        let after_wrap = seek_pendulum_weight(10.0, epsilon);

        assert!((before_wrap.0 - after_wrap.0).abs() < 4.0e-6);
        assert!((before_wrap.1 - after_wrap.1).abs() < 1.0e-9);
    }

    #[test]
    fn every_disk_candidate_reacts_to_live_read_write_and_pressure_metrics() {
        let quiet = AppState {
            disk_health_phase: 1.37,
            ..Default::default()
        };
        let busy = sample_disk_state();

        for view in DiskHealthVisualization::ALL
            .into_iter()
            .filter(|view| *view != DiskHealthVisualization::Classic)
        {
            let quiet_buffer = render_disk(view, &quiet);
            let busy_buffer = render_disk(view, &busy);
            assert_ne!(
                interior_cells(&quiet_buffer),
                interior_cells(&busy_buffer),
                "{} should react to live disk metrics",
                view.label()
            );
        }
    }

    #[test]
    fn every_disk_candidate_uses_the_classic_state_color() {
        for state_level in 0..=3 {
            let state = AppState {
                avg_disk_read_bps: 96 * 1024 * 1024,
                avg_disk_write_bps: 32 * 1024 * 1024,
                disk_health_ema: 0.52,
                disk_health_peak_hold: 0.60,
                disk_health_state_level: state_level,
                disk_health_phase: 1.37,
                ..Default::default()
            };
            let ctx = ThemeContext::new(state.theme, 0.0);
            let expected = disk_health_status_color(&ctx, state_level);

            for view in DiskHealthVisualization::ALL
                .into_iter()
                .filter(|view| *view != DiskHealthVisualization::Classic)
            {
                let colors = interior_cells(&render_disk(view, &state))
                    .into_iter()
                    .filter(|(symbol, _)| symbol != " ")
                    .map(|(_, color)| color)
                    .collect::<Vec<_>>();
                assert!(
                    !colors.is_empty(),
                    "{} should draw colored cells",
                    view.label()
                );
                assert!(
                    colors.iter().all(|color| *color == expected),
                    "{} should use the state color at level {state_level}",
                    view.label()
                );
            }
        }
    }

    #[test]
    fn every_disk_candidate_deforms_as_the_disk_state_escalates() {
        let stable = AppState {
            avg_disk_read_bps: 48 * 1024 * 1024,
            avg_disk_write_bps: 16 * 1024 * 1024,
            disk_health_ema: 0.18,
            disk_health_peak_hold: 0.22,
            disk_health_state_level: 0,
            disk_health_phase: 1.37,
            ..Default::default()
        };
        let chaos = AppState {
            avg_disk_read_bps: 48 * 1024 * 1024,
            avg_disk_write_bps: 16 * 1024 * 1024,
            disk_health_ema: 0.18,
            disk_health_peak_hold: 0.22,
            disk_health_state_level: 3,
            disk_health_phase: 1.37,
            ..Default::default()
        };

        for view in DiskHealthVisualization::ALL
            .into_iter()
            .filter(|view| *view != DiskHealthVisualization::Classic)
        {
            assert_ne!(
                interior_symbols(&render_disk(view, &stable)),
                interior_symbols(&render_disk(view, &chaos)),
                "{} should geometrically deform as state escalates",
                view.label()
            );
        }
    }
}
