// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::f64::consts::TAU;

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Context, Line as CanvasLine, Points};
use ratatui::widgets::{Block, Borders};
use ratatui::Frame;

use crate::app::{AppState, DhtVisualization, DiskHealthVisualization};
use crate::dht_service::{DhtStatus, DhtWaveTelemetry};
use crate::theme::ThemeContext;

#[derive(Clone, Copy)]
struct DiskPalette {
    read: Color,
    write: Color,
    pressure: Color,
    accent: Color,
    grid: Color,
}

impl DiskPalette {
    fn from_theme(ctx: &ThemeContext) -> Self {
        Self {
            read: ctx.metric_download(),
            write: ctx.metric_upload(),
            pressure: ctx.state_warning(),
            accent: ctx.accent_peach(),
            grid: ctx.theme.semantic.surface2,
        }
    }
}

#[derive(Debug)]
struct DiskSignals {
    reads: Vec<f64>,
    writes: Vec<f64>,
    pressure: f64,
    backoff: f64,
    iops: f64,
    time: f64,
}

impl DiskSignals {
    fn from_app(app_state: &AppState, sample_count: usize) -> Self {
        let sample_count = sample_count.clamp(8, 120);
        let reads = resample_history(
            &app_state.disk_read_history,
            app_state.avg_disk_read_bps,
            sample_count,
        );
        let writes = resample_history(
            &app_state.disk_write_history,
            app_state.avg_disk_write_bps,
            sample_count,
        );
        let shared_max = reads.iter().chain(&writes).copied().fold(1.0_f64, f64::max);
        let normalize = |values: Vec<f64>| {
            values
                .into_iter()
                .map(|value| (value / shared_max).clamp(0.0, 1.0))
                .collect()
        };

        let total_iops = f64::from(app_state.read_iops) + f64::from(app_state.write_iops);
        Self {
            reads: normalize(reads),
            writes: normalize(writes),
            pressure: app_state
                .disk_health_ema
                .max(app_state.disk_health_peak_hold)
                .clamp(0.0, 1.0),
            backoff: (app_state.max_disk_backoff_this_tick_ms as f64 / 250.0).clamp(0.0, 1.0),
            iops: (total_iops / (total_iops + 2_000.0)).clamp(0.0, 1.0),
            time: app_state.disk_health_phase,
        }
    }
}

fn resample_history(history: &[u64], fallback: u64, requested: usize) -> Vec<f64> {
    if history.is_empty() {
        return vec![fallback as f64; requested];
    }
    let samples = requested.max(2);
    (0..samples)
        .map(|index| {
            let source = index * history.len().saturating_sub(1) / samples.saturating_sub(1);
            history[source] as f64
        })
        .collect()
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

    let sample_count = usize::from(area.width).saturating_mul(2);
    let signals = DiskSignals::from_app(app_state, sample_count);
    let palette = DiskPalette::from_theme(ctx);
    let (x_bounds, y_bounds) = canvas_bounds(area);
    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds(x_bounds)
        .y_bounds(y_bounds)
        .paint(|canvas| match view {
            DiskHealthVisualization::Classic => {}
            DiskHealthVisualization::IoBraid => draw_io_braid(canvas, x_bounds, &signals, palette),
            DiskHealthVisualization::PressureFan => {
                draw_pressure_fan(canvas, x_bounds, &signals, palette)
            }
        });
    frame.render_widget(canvas, area);
}

fn draw_io_braid(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: &DiskSignals,
    palette: DiskPalette,
) {
    canvas.draw(&CanvasLine {
        x1: bounds[0],
        y1: 0.0,
        x2: bounds[1],
        y2: 0.0,
        color: palette.grid,
    });
    let mut read_previous = None;
    let mut write_previous = None;
    let mut pressure_previous = None;
    for index in 0..signals.reads.len() {
        let unit = index as f64 / signals.reads.len().saturating_sub(1).max(1) as f64;
        let x = plot_x(unit, bounds);
        let carrier = unit * TAU * 1.6 + signals.time * 0.5;
        let read_y = carrier.sin() * (0.12 + signals.reads[index] * 0.52);
        let write_y = (carrier + TAU / 3.0).sin() * (0.12 + signals.writes[index] * 0.52);
        let pressure_y = (carrier + TAU * 2.0 / 3.0).sin()
            * (0.10 + signals.pressure * 0.36 + signals.backoff * 0.16);
        segment(canvas, &mut read_previous, x, read_y, palette.read);
        segment(canvas, &mut write_previous, x, write_y, palette.write);
        segment(
            canvas,
            &mut pressure_previous,
            x,
            pressure_y,
            if signals.pressure > 0.55 || signals.backoff > 0.20 {
                palette.pressure
            } else {
                palette.accent
            },
        );
    }
}

fn draw_pressure_fan(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: &DiskSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let latest_read = signals.reads.last().copied().unwrap_or(0.0);
    let latest_write = signals.writes.last().copied().unwrap_or(0.0);
    let total = latest_read + latest_write;
    let balance = if total > 0.0 {
        (latest_read - latest_write) / total
    } else {
        0.0
    };
    let origin_x = balance * span * 0.08;
    let origin_y = -0.88;
    for ray in 0..11 {
        let unit = ray as f64 / 10.0;
        let sample = ray * signals.reads.len().saturating_sub(1) / 10;
        let activity = signals.reads[sample].max(signals.writes[sample]);
        let sway = (signals.time * (0.30 + signals.iops * 0.55) + ray as f64 * 0.31).sin()
            * (0.04 + signals.pressure * 0.08);
        let endpoint_y = 0.48 + activity * 0.28 - signals.backoff * 0.10 + sway;
        let color = if signals.pressure > 0.60 && ray.is_multiple_of(4) {
            palette.pressure
        } else if signals.reads[sample] >= signals.writes[sample] {
            palette.read
        } else {
            palette.write
        };
        canvas.draw(&CanvasLine {
            x1: origin_x,
            y1: origin_y,
            x2: plot_x(unit, bounds),
            y2: endpoint_y,
            color,
        });
    }
    canvas.draw(&Points {
        coords: &[(origin_x, origin_y)],
        color: palette.accent,
    });
}

#[derive(Clone, Copy)]
struct DhtPalette {
    query: Color,
    peer_yield: Color,
    alert: Color,
    accent: Color,
    grid: Color,
    faint: Color,
    core: Color,
}

impl DhtPalette {
    fn from_theme(ctx: &ThemeContext) -> Self {
        Self {
            query: ctx.peer_discovered(),
            peer_yield: ctx.peer_connected(),
            alert: ctx.accent_peach(),
            accent: ctx.theme.scale.categorical.lavender,
            grid: ctx.theme.semantic.surface2,
            faint: ctx.theme.semantic.surface0,
            core: ctx.accent_sapphire(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct DhtSignals {
    queries: usize,
    peer_yield: usize,
    lookups: usize,
    query_signal: f64,
    yield_signal: f64,
    lookup_signal: f64,
    route_signal: f64,
    demand_signal: f64,
    time: f64,
}

impl DhtSignals {
    fn from_live(app_state: &AppState, status: &DhtStatus, telemetry: &DhtWaveTelemetry) -> Self {
        let queries = telemetry.inflight_ipv4_queries + telemetry.inflight_ipv6_queries;
        let peer_yield = telemetry.unique_peers_found_last_10s;
        let lookups = telemetry.active_lookups.max(status.health.inflight_lookups);
        let routes = status.health.cached_ipv4_routes + status.health.cached_ipv6_routes;
        let phase = if app_state.ui.dht_wave.initialized {
            app_state.ui.dht_wave.phase
        } else {
            app_state.ui.effects_phase_time * 0.7
        };
        let query_signal = (queries as f64 / (queries as f64 + 40.0)).clamp(0.0, 1.0);
        let yield_signal = (peer_yield as f64 / (peer_yield as f64 + 256.0)).clamp(0.0, 1.0);
        let lookup_signal = (lookups as f64 / (lookups as f64 + 8.0)).clamp(0.0, 1.0);
        let route_signal = (routes as f64 / (routes as f64 + 256.0)).clamp(0.0, 1.0);
        let demand_signal = (f64::from(telemetry.demand_power_scale_halves) / 8.0).clamp(0.0, 1.0);
        Self {
            queries,
            peer_yield,
            lookups,
            query_signal,
            yield_signal,
            lookup_signal,
            route_signal,
            demand_signal,
            time: phase,
        }
    }
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
    let signals = DhtSignals::from_live(app_state, status, telemetry);
    let palette = DhtPalette::from_theme(ctx);
    let detail_text = format!("Q{} Y{}", signals.queries, signals.peer_yield);
    let available_width = usize::from(area.width.saturating_sub(2));
    let full_title = format!("DHT · {}", view.label());
    let compact_title = format!("DHT {}", view.compact_label());
    let (title_text, show_detail) =
        if full_title.chars().count() + 1 + detail_text.len() <= available_width {
            (full_title, true)
        } else if compact_title.len() + 1 + detail_text.len() <= available_width {
            (compact_title, true)
        } else if compact_title.len() <= available_width {
            (compact_title, false)
        } else {
            (
                "DHT".to_owned(),
                "DHT".len() + 1 + detail_text.len() <= available_width,
            )
        };
    let title = Line::from(vec![
        Span::styled("DHT ", ctx.apply(Style::default().fg(palette.query))),
        Span::styled(
            title_text.strip_prefix("DHT ").unwrap_or_default(),
            ctx.apply(
                Style::default()
                    .fg(ctx.state_selected())
                    .add_modifier(Modifier::BOLD),
            ),
        ),
    ]);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)))
        .title_top(title);
    if show_detail {
        let detail = Line::from(vec![
            Span::styled(
                format!("Q{} ", signals.queries),
                ctx.apply(Style::default().fg(palette.query)),
            ),
            Span::styled(
                format!("Y{}", signals.peer_yield),
                ctx.apply(Style::default().fg(palette.peer_yield)),
            ),
        ])
        .alignment(Alignment::Right);
        block = block.title_top(detail);
    }
    let (x_bounds, y_bounds) = canvas_bounds(area);
    let canvas = Canvas::default()
        .block(block)
        .marker(Marker::Braille)
        .x_bounds(x_bounds)
        .y_bounds(y_bounds)
        .paint(|canvas| match view {
            DhtVisualization::Classic => {}
            DhtVisualization::QueryTide => draw_query_tide(canvas, x_bounds, signals, palette),
            DhtVisualization::NodeWeb => draw_node_web(canvas, x_bounds, signals, palette),
            DhtVisualization::QueryPulse => draw_query_pulse(canvas, x_bounds, signals, palette),
            DhtVisualization::LookupCore => draw_lookup_core(canvas, x_bounds, signals, palette),
        });
    frame.render_widget(canvas, area);
}

fn draw_query_tide(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DhtSignals,
    palette: DhtPalette,
) {
    let samples: usize = 30;
    let mut surface = None;
    for index in 0..samples {
        let unit = index as f64 / (samples - 1) as f64;
        let x = plot_x(unit, bounds);
        let energy = signals.query_signal.max(0.04);
        let crest = -0.10
            + (signals.time * 0.52 + index as f64 * 0.19).sin()
                * (0.10 + energy * 0.35 + signals.demand_signal * 0.10);
        canvas.draw(&CanvasLine {
            x1: x,
            y1: -0.92,
            x2: x,
            y2: crest,
            color: if index.is_multiple_of(3) && signals.yield_signal > 0.0 {
                palette.peer_yield
            } else {
                palette.query
            },
        });
        segment(canvas, &mut surface, x, crest, palette.accent);
    }
}

fn draw_node_web(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DhtSignals,
    palette: DhtPalette,
) {
    let nodes = (8 + (signals.route_signal * 8.0).round() as usize).clamp(8, 16);
    let points = (0..nodes)
        .map(|index| {
            let unit = index as f64 / (nodes - 1) as f64;
            let x = plot_x(unit, bounds);
            let y = (index as f64 * 1.73 + signals.time * (0.20 + signals.lookup_signal * 0.45))
                .sin()
                * (0.34 + signals.query_signal * 0.25);
            (x, y)
        })
        .collect::<Vec<_>>();
    for index in 0..nodes {
        for jump in [1, 3] {
            if let Some(target) = points.get(index + jump) {
                canvas.draw(&CanvasLine {
                    x1: points[index].0,
                    y1: points[index].1,
                    x2: target.0,
                    y2: target.1,
                    color: if jump == 1 {
                        palette.grid
                    } else {
                        palette.faint
                    },
                });
            }
        }
        canvas.draw(&Points {
            coords: &[points[index]],
            color: if index.is_multiple_of(3) && signals.lookups > 0 {
                palette.alert
            } else if signals.peer_yield > 0 {
                palette.peer_yield
            } else {
                palette.query
            },
        });
    }
    let cursor = (signals.time * (0.05 + signals.lookup_signal * 0.12)).rem_euclid(1.0);
    canvas.draw(&Points {
        coords: &[(plot_x(cursor, bounds), 0.82 - signals.demand_signal * 0.24)],
        color: palette.accent,
    });
}

fn draw_query_pulse(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DhtSignals,
    palette: DhtPalette,
) {
    let pulses: usize = 24;
    for pulse in 0..pulses {
        let unit = pulse as f64 / (pulses - 1) as f64;
        let x = plot_x(unit, bounds);
        let carrier = (signals.time * (0.55 + signals.demand_signal) + pulse as f64 * 0.51)
            .sin()
            .abs();
        let height =
            0.06 + signals.query_signal * (0.28 + carrier * 0.42) + signals.lookup_signal * 0.10;
        let wobble = (signals.time * 0.8 + pulse as f64 * 0.5).sin() * 0.04;
        canvas.draw(&CanvasLine {
            x1: x,
            y1: -height + wobble,
            x2: x,
            y2: height + wobble,
            color: if carrier > 0.78 && signals.demand_signal > 0.35 {
                palette.alert
            } else if pulse.is_multiple_of(4) && signals.peer_yield > 0 {
                palette.peer_yield
            } else {
                palette.query
            },
        });
    }
}

fn draw_lookup_core(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DhtSignals,
    palette: DhtPalette,
) {
    let span = bounds[1] - bounds[0];
    let core_x = span * 0.04 * (signals.time * 0.31).sin() * signals.lookup_signal;
    draw_filled_ellipse(
        canvas,
        core_x,
        0.0,
        span * (0.035 + signals.lookup_signal * 0.025),
        0.18 + signals.query_signal * 0.16,
        palette.core,
    );
    let rings = (2 + (signals.route_signal * 3.0).round() as usize).clamp(2, 5);
    for ring in 1..=rings {
        draw_ellipse(
            canvas,
            core_x,
            0.0,
            span * (0.06 + ring as f64 * 0.035),
            0.16 + ring as f64 * 0.12,
            if ring.is_multiple_of(2) {
                palette.peer_yield
            } else {
                palette.grid
            },
        );
    }
    let beams = (8 + signals.lookups.min(8)).max(8);
    for beam in 0..beams {
        let angle =
            signals.time * (0.18 + signals.demand_signal * 0.22) + beam as f64 * TAU / beams as f64;
        canvas.draw(&CanvasLine {
            x1: core_x + angle.cos() * span * 0.06,
            y1: angle.sin() * 0.14,
            x2: core_x + angle.cos() * span * (0.36 + signals.query_signal * 0.13),
            y2: angle.sin() * (0.58 + signals.yield_signal * 0.20),
            color: if beam.is_multiple_of(3) && signals.lookups > 0 {
                palette.alert
            } else {
                palette.faint
            },
        });
    }
}

fn canvas_bounds(area: Rect) -> ([f64; 2], [f64; 2]) {
    let width = f64::from(area.width.saturating_sub(2).max(1));
    let height = f64::from(area.height.saturating_sub(2).max(1));
    let half = (width / (height * 2.0)).max(0.65);
    ([-half, half], [-1.0, 1.0])
}

fn plot_x(unit: f64, bounds: [f64; 2]) -> f64 {
    bounds[0] + (bounds[1] - bounds[0]) * unit.clamp(0.0, 1.0)
}

fn segment(
    canvas: &mut Context<'_>,
    previous: &mut Option<(f64, f64)>,
    x: f64,
    y: f64,
    color: Color,
) {
    if let Some((previous_x, previous_y)) = *previous {
        canvas.draw(&CanvasLine {
            x1: previous_x,
            y1: previous_y,
            x2: x,
            y2: y,
            color,
        });
    }
    *previous = Some((x, y));
}

fn draw_ellipse(
    canvas: &mut Context<'_>,
    center_x: f64,
    center_y: f64,
    radius_x: f64,
    radius_y: f64,
    color: Color,
) {
    const SEGMENTS: usize = 48;
    for index in 0..SEGMENTS {
        let a = index as f64 / SEGMENTS as f64 * TAU;
        let b = (index + 1) as f64 / SEGMENTS as f64 * TAU;
        canvas.draw(&CanvasLine {
            x1: center_x + radius_x * a.cos(),
            y1: center_y + radius_y * a.sin(),
            x2: center_x + radius_x * b.cos(),
            y2: center_y + radius_y * b.sin(),
            color,
        });
    }
}

fn draw_filled_ellipse(
    canvas: &mut Context<'_>,
    center_x: f64,
    center_y: f64,
    radius_x: f64,
    radius_y: f64,
    color: Color,
) {
    const STEPS: i32 = 24;
    for step in -STEPS..=STEPS {
        let unit_y = f64::from(step) / f64::from(STEPS);
        let half = radius_x * (1.0 - unit_y * unit_y).max(0.0).sqrt();
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

    fn render_disk(view: DiskHealthVisualization) -> Buffer {
        let mut state = AppState {
            disk_read_history: vec![8_000_000, 20_000_000, 14_000_000, 28_000_000],
            disk_write_history: vec![5_000_000, 11_000_000, 19_000_000, 9_000_000],
            avg_disk_read_bps: 28_000_000,
            avg_disk_write_bps: 9_000_000,
            read_iops: 440,
            write_iops: 210,
            disk_health_ema: 0.48,
            disk_health_phase: 1.7,
            ..Default::default()
        };
        state.disk_health_peak_hold = 0.61;
        let backend = TestBackend::new(42, 9);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let ctx = ThemeContext::new(state.theme, 0.0);
        terminal
            .draw(|frame| draw_disk_health_visualization(frame, &state, frame.area(), view, &ctx))
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    fn render_dht_with_width(view: DhtVisualization, width: u16) -> Buffer {
        let mut state = AppState::default();
        state.ui.dht_wave.initialized = true;
        state.ui.dht_wave.phase = 1.4;
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
        let backend = TestBackend::new(width, 9);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let ctx = ThemeContext::new(state.theme, 0.0);
        terminal
            .draw(|frame| {
                draw_dht_visualization(frame, &state, &status, &telemetry, frame.area(), view, &ctx)
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    fn render_dht(view: DhtVisualization) -> Buffer {
        render_dht_with_width(view, 52)
    }

    fn occupied(buffer: &Buffer) -> usize {
        buffer
            .content()
            .iter()
            .filter(|cell| cell.symbol() != " ")
            .count()
    }

    #[test]
    fn retained_disk_renderers_draw_distinct_production_signals() {
        let braid = render_disk(DiskHealthVisualization::IoBraid);
        let fan = render_disk(DiskHealthVisualization::PressureFan);
        assert!(occupied(&braid) > 20);
        assert!(occupied(&fan) > 20);
        assert_ne!(braid, fan);
    }

    #[test]
    fn retained_dht_renderers_are_distinct_and_labeled() {
        let views = [
            DhtVisualization::QueryTide,
            DhtVisualization::NodeWeb,
            DhtVisualization::QueryPulse,
            DhtVisualization::LookupCore,
        ];
        let buffers = views.map(render_dht);
        for (view, buffer) in views.into_iter().zip(&buffers) {
            let text = buffer
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains(view.label()), "missing label for {view:?}");
            assert!(occupied(buffer) > 40);
        }
        for left in 0..buffers.len() {
            for right in left + 1..buffers.len() {
                assert_ne!(buffers[left], buffers[right]);
            }
        }
    }

    #[test]
    fn dht_renderer_uses_compact_title_without_narrow_panel_collision() {
        let buffer = render_dht_with_width(DhtVisualization::QueryTide, 17);
        let top_row = buffer.content()[..17]
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(top_row.contains("DHT Tide"));
        assert!(!top_row.contains("Query Tide"));
        assert!(!top_row.contains("Q23"));
    }

    #[test]
    fn disk_history_uses_one_shared_scale_for_read_write_comparison() {
        let state = AppState {
            disk_read_history: vec![100, 200],
            disk_write_history: vec![25, 50],
            ..Default::default()
        };
        let signals = DiskSignals::from_app(&state, 8);
        assert_eq!(signals.reads.last().copied(), Some(1.0));
        assert_eq!(signals.writes.last().copied(), Some(0.25));
    }
}
