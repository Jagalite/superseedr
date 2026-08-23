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
    accent: Color,
    core: Color,
}

impl DiskPalette {
    fn from_theme(ctx: &ThemeContext, state_level: u8) -> Self {
        let state_color = disk_health_status_color(ctx, state_level);
        Self {
            read: state_color,
            write: state_color,
            pressure: state_color,
            accent: state_color,
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
    alert: Color,
    accent: Color,
    core: Color,
}

impl DhtPalette {
    fn from_theme(ctx: &ThemeContext) -> Self {
        Self {
            query: ctx.peer_discovered(),
            peer_yield: ctx.peer_connected(),
            alert: ctx.accent_peach(),
            accent: ctx.theme.scale.categorical.lavender,
            core: ctx.accent_sapphire(),
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
    amplitude: f64,
    harmonic_amplitude: f64,
    frequency: f64,
    crest_bias: f64,
    bootstrap_ratio: f64,
    discovery_boost: f64,
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
        let (amplitude, harmonic_amplitude, frequency, crest_bias, bootstrap_ratio, query_signal) =
            if wave.initialized {
                (
                    wave.amplitude,
                    wave.harmonic_amplitude,
                    wave.frequency,
                    wave.crest_bias,
                    wave.bootstrap_ratio,
                    wave.query_load,
                )
            } else {
                (
                    0.01 + raw_query_signal * 0.24,
                    0.004 + raw_query_signal * 0.13,
                    0.08 + raw_query_signal * 0.18,
                    ((raw_query_signal - 0.5) * 0.06).clamp(-0.22, 0.22),
                    1.0,
                    raw_query_signal,
                )
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
            amplitude: amplitude.clamp(0.0, 0.52),
            harmonic_amplitude: harmonic_amplitude.clamp(0.0, 0.20),
            frequency: frequency.clamp(0.06, 0.38),
            crest_bias,
            bootstrap_ratio: bootstrap_ratio.clamp(0.0, 1.0),
            discovery_boost: if wave.initialized {
                wave.discovery_boost
            } else {
                0.0
            },
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
    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds(x_bounds)
        .y_bounds(y_bounds)
        .paint(|canvas| match view {
            DiskHealthVisualization::Classic => {}
            DiskHealthVisualization::DiskPlatter => {
                draw_disk_platter(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::ReadHead => {
                draw_disk_read_head(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::SectorFan => {
                draw_disk_sector_fan(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::IoSpindle => {
                draw_disk_io_spindle(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::QueueStack => {
                draw_disk_queue_stack(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::ThroughputRails => {
                draw_disk_throughput_rails(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::PressureGauge => {
                draw_disk_pressure_gauge(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::BlockCascade => {
                draw_disk_block_cascade(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::CacheLattice => {
                draw_disk_cache_lattice(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::SeekRadar => {
                draw_disk_seek_radar(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::WriteFountain => {
                draw_disk_write_fountain(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::ReadRibbon => {
                draw_disk_read_ribbon(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::LatencyCanyon => {
                draw_disk_latency_canyon(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::BufferTower => {
                draw_disk_buffer_tower(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::TransferBridge => {
                draw_disk_transfer_bridge(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::LoadPrism => {
                draw_disk_load_prism(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::FlushVortex => {
                draw_disk_flush_vortex(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::SectorBloom => {
                draw_disk_sector_bloom(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::HeadLadder => {
                draw_disk_head_ladder(canvas, x_bounds, signals, palette)
            }
            DiskHealthVisualization::CircuitBoard => {
                draw_disk_circuit_board(canvas, x_bounds, signals, palette)
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
        let temporary_number = view.temporary_number().unwrap_or_default();
        let full_caption = format!("TEMP {temporary_number:02} · {}", view.label());
        let compact_caption = format!("T{temporary_number:02} {}", view.compact_label());
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
                        .fg(palette.accent)
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
            DhtVisualization::QueryWings => draw_query_wings(canvas, x_bounds, signals, palette),
            DhtVisualization::RelayRibbon => draw_relay_ribbon(canvas, x_bounds, signals, palette),
            DhtVisualization::PulseGrid => draw_pulse_grid(canvas, x_bounds, signals, palette),
            DhtVisualization::LookupVortex => {
                draw_lookup_vortex(canvas, x_bounds, signals, palette)
            }
            DhtVisualization::PeerBloom => draw_peer_bloom(canvas, x_bounds, signals, palette),
        });
    frame.render_widget(canvas, area);
}

fn dht_activity(signals: DhtVisualSignals) -> f64 {
    (signals.query_signal * 0.30
        + signals.instant_query_signal * 0.40
        + signals.yield_signal * 0.20
        + signals.amplitude * 0.18
        + signals.harmonic_amplitude * 0.30
        + signals.crest_bias.abs() * 0.10
        + (1.0 - signals.bootstrap_ratio) * 0.05
        + signals.discovery_boost * 0.35
        + signals.query_surge * 0.80)
        .clamp(0.0, 1.0)
}

fn dht_phase(signals: DhtVisualSignals, speed: f64) -> f64 {
    signals.time * speed * (0.75 + signals.frequency * 2.5)
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

fn draw_packet(
    canvas: &mut Context<'_>,
    start: (f64, f64),
    end: (f64, f64),
    progress: f64,
    span: f64,
    color: Color,
) {
    let progress = progress.rem_euclid(1.0);
    draw_filled_diamond(
        canvas,
        start.0 + (end.0 - start.0) * progress,
        start.1 + (end.1 - start.1) * progress,
        span * 0.009,
        0.035,
        color,
    );
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

fn disk_phase(signals: DiskHealthSignals, speed: f64) -> f64 {
    signals.phase * speed * (0.92 + disk_deformation(signals) * 0.22)
}

fn draw_loop(
    canvas: &mut Context<'_>,
    center: (f64, f64),
    radii: (f64, f64),
    segments: usize,
    phase: f64,
    deformation: f64,
    color: Color,
) {
    let mut points = Vec::with_capacity(segments + 1);
    for step in 0..=segments {
        let angle = step as f64 / segments as f64 * TAU + phase;
        let radial_warp = 1.0
            + deformation * 0.14 * (angle * 3.0 + phase).sin()
            + deformation * 0.07 * (angle * 5.0 - phase * 0.7).sin();
        points.push((
            center.0 + angle.cos() * radii.0 * radial_warp,
            center.1 + angle.sin() * radii.1 * radial_warp,
        ));
    }
    draw_polyline(canvas, &points, color);
}

fn draw_disk_platter(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let deformation = disk_deformation(signals);
    let phase = disk_phase(signals, 1.2);
    for ring in 1_usize..=4 {
        let unit = ring as f64 / 4.0;
        draw_loop(
            canvas,
            (0.0, 0.0),
            (
                span * unit * (0.09 + load * 0.01),
                unit * (0.27 + load * 0.035),
            ),
            24,
            phase * if ring.is_multiple_of(2) { 0.05 } else { -0.05 },
            deformation,
            if ring.is_multiple_of(2) {
                palette.read
            } else {
                palette.accent
            },
        );
    }
    let head = (phase.cos() * span * 0.22, phase.sin() * 0.58);
    draw_segment(canvas, (span * 0.38, -0.72), head, palette.write);
    draw_filled_diamond(canvas, head.0, head.1, span * 0.018, 0.07, palette.pressure);
    draw_filled_diamond(canvas, 0.0, 0.0, span * 0.025, 0.09, palette.core);
}

fn draw_disk_read_head(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let phase = disk_phase(signals, 1.45);
    let pivot = (-span * 0.34, -0.64);
    let tip = (
        phase.sin() * span * (0.22 + load * 0.08),
        0.18 + phase.cos() * (0.18 + signals.read_signal * 0.14),
    );
    draw_polyline(canvas, &[pivot, (-span * 0.12, -0.16), tip], palette.read);
    draw_diamond(canvas, tip.0, tip.1, span * 0.055, 0.14, palette.pressure);
    for track in 0_usize..5 {
        let y = -0.74 + track as f64 * 0.30;
        draw_segment(
            canvas,
            (-span * 0.42, y),
            (span * 0.42, y),
            if track.is_multiple_of(2) {
                palette.accent
            } else {
                palette.write
            },
        );
    }
    draw_filled_diamond(canvas, pivot.0, pivot.1, span * 0.022, 0.08, palette.core);
}

fn draw_disk_sector_fan(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let phase = disk_phase(signals, 0.9);
    let origin = (-span * 0.34, -0.68);
    let spokes = 7 + (load * 5.0).round() as usize;
    for spoke in 0..spokes {
        let unit = spoke as f64 / (spokes - 1) as f64;
        let angle = 0.22 + unit * 1.18 + phase.sin() * 0.08;
        let end = (
            origin.0 + angle.cos() * span * 0.66,
            origin.1 + angle.sin() * 1.42,
        );
        draw_segment(
            canvas,
            origin,
            end,
            if spoke.is_multiple_of(2) {
                palette.read
            } else {
                palette.write
            },
        );
    }
    draw_loop(
        canvas,
        origin,
        (span * 0.47, 1.0),
        24,
        0.18,
        disk_deformation(signals),
        palette.accent,
    );
    draw_filled_diamond(canvas, origin.0, origin.1, span * 0.025, 0.09, palette.core);
}

fn draw_disk_io_spindle(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let deformation = disk_deformation(signals);
    let phase = disk_phase(signals, 1.15);
    draw_segment(canvas, (0.0, -0.82), (0.0, 0.82), palette.core);
    let decks = 5 + (load * 3.0).round() as usize;
    for deck in 0..decks {
        let y = -0.64 + deck as f64 * 1.28 / (decks - 1) as f64;
        let pulse = 1.0 + (phase + deck as f64 * 0.7).sin() * 0.10;
        draw_loop(
            canvas,
            (0.0, y),
            (
                span * (0.18 + signals.throughput_gap * 0.035) * pulse,
                0.07 + load * 0.025,
            ),
            18,
            0.0,
            deformation,
            if deck.is_multiple_of(2) {
                palette.read
            } else {
                palette.write
            },
        );
    }
    draw_filled_diamond(
        canvas,
        0.0,
        phase.sin() * 0.58,
        span * 0.025,
        0.08,
        palette.pressure,
    );
}

fn draw_disk_queue_stack(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let deformation = disk_deformation(signals);
    let phase = disk_phase(signals, 1.3);
    let rows = 4 + (load * 5.0).round() as usize;
    for row in 0..rows {
        let y = -0.72 + row as f64 * 1.44 / (rows - 1) as f64;
        let shift = (phase + row as f64 * 0.9).sin() * span * (0.035 + deformation * 0.075);
        let half = span * (0.16 + (row as f64 / rows as f64) * 0.12);
        let color = if row.is_multiple_of(3) {
            palette.pressure
        } else if row.is_multiple_of(2) {
            palette.read
        } else {
            palette.write
        };
        draw_polyline(
            canvas,
            &[
                (shift - half, y - 0.07),
                (shift + half, y - 0.07),
                (shift + half, y + 0.07),
                (shift - half, y + 0.07),
                (shift - half, y - 0.07),
            ],
            color,
        );
    }
}

fn draw_disk_throughput_rails(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let phase = disk_phase(signals, 1.6);
    let deformation = disk_deformation(signals);
    let left = -span * 0.42;
    let right = span * 0.42;
    for (lane, y, color, speed) in [
        (0_usize, 0.42, palette.read, 1.0),
        (1, 0.14, palette.read, 1.3),
        (2, -0.14, palette.write, -1.15),
        (3, -0.42, palette.write, -1.45),
    ] {
        let warp = (phase * 0.55 + lane as f64 * 0.9).sin() * deformation * 0.16;
        let start = (left, y - warp);
        let end = (right, y + warp);
        draw_segment(canvas, start, end, color);
        let signal = if lane < 2 {
            signals.read_signal
        } else {
            signals.write_signal
        };
        draw_packet(
            canvas,
            start,
            end,
            phase * speed / TAU + signal,
            span,
            if signal > 0.45 {
                palette.pressure
            } else {
                color
            },
        );
    }
    draw_segment(canvas, (0.0, -0.68), (0.0, 0.68), palette.core);
}

fn draw_disk_pressure_gauge(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let deformation = disk_deformation(signals);
    let mut arc = Vec::with_capacity(17);
    for step in 0..=16 {
        let unit = step as f64 / 16.0;
        let angle = 0.12 + unit * (TAU * 0.46);
        let warp = 1.0 + (unit * TAU * 2.0).sin() * deformation * 0.10;
        arc.push((
            angle.cos() * span * 0.38 * warp,
            -0.48 + angle.sin() * 0.78 * warp,
        ));
    }
    draw_polyline(canvas, &arc, palette.accent);
    for tick in 0..=8 {
        let angle = 0.12 + tick as f64 / 8.0 * (TAU * 0.46);
        let outer = (angle.cos() * span * 0.38, -0.48 + angle.sin() * 0.78);
        let inner = (angle.cos() * span * 0.32, -0.48 + angle.sin() * 0.64);
        draw_segment(
            canvas,
            inner,
            outer,
            if tick <= signals.state_level as usize * 2 {
                palette.pressure
            } else {
                palette.read
            },
        );
    }
    let angle = 0.12 + load * (TAU * 0.46);
    draw_segment(
        canvas,
        (0.0, -0.48),
        (angle.cos() * span * 0.29, -0.48 + angle.sin() * 0.58),
        palette.write,
    );
    draw_filled_diamond(canvas, 0.0, -0.48, span * 0.026, 0.09, palette.core);
}

fn draw_disk_block_cascade(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let deformation = disk_deformation(signals);
    let phase = disk_phase(signals, 1.5);
    let blocks = 7 + (load * 5.0).round() as usize;
    for block in 0..blocks {
        let progress = (block as f64 / blocks as f64 + phase / TAU).rem_euclid(1.0);
        let y = 0.78 - progress * 1.56;
        let x = (progress * TAU * (1.5 + deformation * 0.7)).sin()
            * span
            * (0.12 + signals.throughput_gap * 0.08 + deformation * 0.06);
        let half = span * (0.025 + load * 0.012);
        draw_polyline(
            canvas,
            &[
                (x - half, y - 0.04),
                (x + half, y - 0.04),
                (x + half, y + 0.04),
                (x - half, y + 0.04),
                (x - half, y - 0.04),
            ],
            if block.is_multiple_of(2) {
                palette.read
            } else {
                palette.write
            },
        );
    }
    draw_segment(
        canvas,
        (-span * 0.28, -0.82),
        (span * 0.28, -0.82),
        palette.pressure,
    );
}

fn draw_disk_cache_lattice(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let phase = disk_phase(signals, 0.8);
    for diagonal in -4..=4 {
        let offset = f64::from(diagonal) * span * 0.12;
        draw_segment(
            canvas,
            (-span * 0.44 + offset, -0.78),
            (span * 0.12 + offset, 0.78),
            palette.read,
        );
        draw_segment(
            canvas,
            (-span * 0.12 + offset, 0.78),
            (span * 0.44 + offset, -0.78),
            palette.write,
        );
    }
    let pulse_x = phase.sin() * span * 0.30;
    let pulse_y = phase.cos() * (0.48 + load * 0.12);
    draw_filled_diamond(
        canvas,
        pulse_x,
        pulse_y,
        span * (0.025 + load * 0.012),
        0.08,
        palette.pressure,
    );
}

fn draw_disk_seek_radar(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let deformation = disk_deformation(signals);
    let phase = disk_phase(signals, 1.3);
    for ring in 1_usize..=3 {
        let unit = ring as f64 / 3.0;
        draw_loop(
            canvas,
            (0.0, 0.0),
            (span * 0.34 * unit, 0.68 * unit),
            24,
            0.0,
            deformation,
            if ring.is_multiple_of(2) {
                palette.accent
            } else {
                palette.read
            },
        );
    }
    let sweep = (phase.cos() * span * 0.36, phase.sin() * 0.72);
    draw_segment(canvas, (0.0, 0.0), sweep, palette.write);
    for echo in 0..(3 + (load * 4.0).round() as usize) {
        let angle = echo as f64 * 2.19 + phase * 0.12;
        let radius = 0.25 + (echo % 3) as f64 * 0.18;
        draw_filled_diamond(
            canvas,
            angle.cos() * span * radius * 0.42,
            angle.sin() * radius,
            span * 0.012,
            0.04,
            palette.pressure,
        );
    }
}

fn draw_disk_write_fountain(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let phase = disk_phase(signals, 1.4);
    let streams = 5 + (signals.write_signal * 5.0).round() as usize;
    for stream in 0..streams {
        let direction = stream as f64 / (streams - 1) as f64 * 2.0 - 1.0;
        let mut points = Vec::with_capacity(11);
        for step in 0..=10 {
            let unit = step as f64 / 10.0;
            points.push((
                direction * span * unit * (0.32 + load * 0.06),
                -0.70 + unit * 1.55 - unit * unit * (0.65 + direction.abs() * 0.35),
            ));
        }
        draw_polyline(
            canvas,
            &points,
            if stream.is_multiple_of(2) {
                palette.write
            } else {
                palette.pressure
            },
        );
        let packet = (phase / TAU + stream as f64 * 0.17).rem_euclid(1.0);
        let index = (packet * 9.99) as usize;
        draw_packet(
            canvas,
            points[index],
            points[index + 1],
            packet * 10.0 - index as f64,
            span,
            palette.read,
        );
    }
}

fn draw_disk_read_ribbon(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let phase = disk_phase(signals, 1.1);
    for ribbon in 0..4 {
        let mut points = Vec::with_capacity(19);
        for step in 0..=18 {
            let unit = step as f64 / 18.0;
            points.push((
                -span * 0.44 + unit * span * 0.88,
                (unit * TAU * 1.35 + phase + ribbon as f64 * 0.95).sin() * (0.22 + load * 0.12)
                    + (ribbon as f64 - 1.5) * 0.12,
            ));
        }
        draw_polyline(
            canvas,
            &points,
            [
                palette.read,
                palette.accent,
                palette.write,
                palette.pressure,
            ][ribbon],
        );
    }
}

fn draw_disk_latency_canyon(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let deformation = disk_deformation(signals);
    let phase = disk_phase(signals, 1.25);
    for side in [-1.0, 1.0] {
        let mut ridge = Vec::with_capacity(11);
        for step in 0..=10 {
            let unit = step as f64 / 10.0;
            let width = 0.12
                + (phase * 0.25 + unit * TAU * 1.4).sin().abs()
                    * (0.12 + signals.throughput_gap * 0.11 + deformation * 0.10);
            ridge.push((side * span * width, -0.78 + unit * 1.56));
        }
        draw_polyline(
            canvas,
            &ridge,
            if side < 0.0 {
                palette.read
            } else {
                palette.write
            },
        );
    }
    let lanes = 2 + (load * 3.0).round() as usize;
    for lane in 0..lanes {
        let x = (lane as f64 - (lanes - 1) as f64 / 2.0) * span * 0.025;
        draw_packet(
            canvas,
            (x, -0.78),
            (x, 0.78),
            phase / TAU + lane as f64 * 0.21,
            span,
            palette.pressure,
        );
    }
}

fn draw_disk_buffer_tower(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let phase = disk_phase(signals, 1.0);
    let deformation = disk_deformation(signals);
    let lean = phase.sin() * span * deformation * 0.06;
    draw_polyline(
        canvas,
        &[
            (-span * 0.22, -0.78),
            (lean - span * (0.10 + deformation * 0.025), 0.76),
            (lean + span * (0.10 + deformation * 0.025), 0.76),
            (span * 0.22, -0.78),
            (-span * 0.22, -0.78),
        ],
        palette.core,
    );
    let floors = 5 + (load * 5.0).round() as usize;
    for floor in 0..floors {
        let unit = floor as f64 / (floors - 1) as f64;
        let y = -0.62 + unit * 1.24;
        let half = span * (0.17 - unit * 0.07);
        draw_segment(
            canvas,
            (-half, y),
            (half, y),
            if floor.is_multiple_of(2) {
                palette.read
            } else {
                palette.write
            },
        );
    }
    draw_filled_diamond(
        canvas,
        0.0,
        -0.62 + (phase.sin() * 0.5 + 0.5) * 1.24,
        span * 0.02,
        0.07,
        palette.pressure,
    );
}

fn draw_disk_transfer_bridge(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let phase = disk_phase(signals, 1.45);
    let left = -span * 0.42;
    let right = span * 0.42;
    draw_segment(canvas, (left, -0.30), (right, -0.30), palette.core);
    let mut arch = Vec::with_capacity(13);
    for step in 0..=12 {
        let unit = step as f64 / 12.0;
        arch.push((
            left + unit * (right - left),
            -0.30 + unit * (1.0 - unit) * (2.7 + load),
        ));
    }
    draw_polyline(canvas, &arch, palette.accent);
    for (index, anchor) in arch.iter().enumerate().take(12).skip(1) {
        if index.is_multiple_of(2) {
            draw_segment(
                canvas,
                (anchor.0, -0.30),
                *anchor,
                if anchor.0 < 0.0 {
                    palette.read
                } else {
                    palette.write
                },
            );
        }
    }
    draw_packet(
        canvas,
        (left, -0.30),
        (right, -0.30),
        phase / TAU,
        span,
        palette.pressure,
    );
}

fn draw_disk_load_prism(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let phase = disk_phase(signals, 0.85);
    let triangle = [
        (-span * 0.18, -0.62),
        (0.0, 0.68 + phase.sin() * 0.05),
        (span * 0.18, -0.62),
        (-span * 0.18, -0.62),
    ];
    draw_polyline(canvas, &triangle, palette.core);
    draw_segment(
        canvas,
        (-span * 0.44, 0.08),
        (-span * 0.10, 0.08),
        palette.read,
    );
    draw_packet(
        canvas,
        (-span * 0.44, 0.08),
        (-span * 0.10, 0.08),
        phase / TAU,
        span,
        palette.pressure,
    );
    let rays = 3 + (load * 4.0).round() as usize;
    for ray in 0..rays {
        let unit = ray as f64 / (rays - 1) as f64;
        draw_segment(
            canvas,
            (span * 0.10, 0.08),
            (span * 0.44, -0.48 + unit * 0.96),
            if ray.is_multiple_of(2) {
                palette.write
            } else {
                palette.accent
            },
        );
    }
}

fn draw_disk_flush_vortex(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let phase = disk_phase(signals, 1.35);
    for arm in 0..3 {
        let mut points = Vec::with_capacity(23);
        for step in 0..=22 {
            let unit = step as f64 / 22.0;
            let angle = phase + arm as f64 * TAU / 3.0 + unit * TAU * 1.45;
            let radius = 0.05 + unit * (0.60 + load * 0.12);
            points.push((angle.cos() * radius * span * 0.48, angle.sin() * radius));
        }
        draw_polyline(
            canvas,
            &points,
            [palette.read, palette.write, palette.accent][arm],
        );
    }
    draw_filled_diamond(
        canvas,
        0.0,
        0.0,
        span * (0.025 + signals.throughput_gap * 0.025),
        0.09,
        palette.pressure,
    );
}

fn draw_disk_sector_bloom(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let phase = disk_phase(signals, 0.7);
    let petals = 6 + (load * 5.0).round() as usize;
    for petal in 0..petals {
        let angle = petal as f64 / petals as f64 * TAU + phase * 0.12;
        let center = (angle.cos() * span * 0.20, angle.sin() * 0.38);
        let tangent = (-angle.sin(), angle.cos());
        let tip = (
            angle.cos() * span * (0.36 + signals.write_signal * 0.04),
            angle.sin() * (0.70 + signals.read_signal * 0.06),
        );
        draw_polyline(
            canvas,
            &[
                (
                    center.0 + tangent.0 * span * 0.05,
                    center.1 + tangent.1 * 0.10,
                ),
                tip,
                (
                    center.0 - tangent.0 * span * 0.05,
                    center.1 - tangent.1 * 0.10,
                ),
                (
                    center.0 + tangent.0 * span * 0.05,
                    center.1 + tangent.1 * 0.10,
                ),
            ],
            if petal.is_multiple_of(2) {
                palette.read
            } else {
                palette.write
            },
        );
    }
    draw_filled_diamond(canvas, 0.0, 0.0, span * 0.05, 0.17, palette.core);
}

fn draw_disk_head_ladder(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let phase = disk_phase(signals, 1.55);
    let lower_left = (-span * 0.34, -0.76);
    let upper_left = (span * 0.10, 0.76);
    let lower_right = (-span * 0.10, -0.76);
    let upper_right = (span * 0.34, 0.76);
    draw_segment(canvas, lower_left, upper_left, palette.read);
    draw_segment(canvas, lower_right, upper_right, palette.write);
    let rungs = 6 + (load * 5.0).round() as usize;
    for rung in 0..rungs {
        let unit = rung as f64 / (rungs - 1) as f64;
        let left = (
            lower_left.0 + (upper_left.0 - lower_left.0) * unit,
            lower_left.1 + unit * 1.52,
        );
        let right = (
            lower_right.0 + (upper_right.0 - lower_right.0) * unit,
            lower_right.1 + unit * 1.52,
        );
        draw_segment(
            canvas,
            left,
            right,
            if rung.is_multiple_of(2) {
                palette.accent
            } else {
                palette.pressure
            },
        );
    }
    draw_packet(
        canvas,
        lower_left,
        upper_left,
        phase / TAU,
        span,
        palette.core,
    );
}

fn draw_disk_circuit_board(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DiskHealthSignals,
    palette: DiskPalette,
) {
    let span = bounds[1] - bounds[0];
    let load = disk_load(signals);
    let phase = disk_phase(signals, 1.65);
    let traces = [
        [
            (-0.43, 0.58),
            (-0.20, 0.58),
            (-0.20, 0.18),
            (0.10, 0.18),
            (0.10, 0.46),
            (0.43, 0.46),
        ],
        [
            (-0.43, 0.06),
            (-0.30, 0.06),
            (-0.30, -0.30),
            (0.18, -0.30),
            (0.18, -0.04),
            (0.43, -0.04),
        ],
        [
            (-0.43, -0.58),
            (-0.08, -0.58),
            (-0.08, -0.08),
            (0.30, -0.08),
            (0.30, -0.48),
            (0.43, -0.48),
        ],
    ];
    for (index, trace) in traces.into_iter().enumerate() {
        let points = trace.map(|(x, y)| (x * span, y));
        let color = [palette.read, palette.write, palette.accent][index];
        draw_polyline(canvas, &points, color);
        let segment = ((phase / TAU + index as f64 * 0.29).rem_euclid(1.0) * 4.99) as usize;
        draw_packet(
            canvas,
            points[segment],
            points[segment + 1],
            phase / TAU + load,
            span,
            palette.pressure,
        );
        draw_filled_diamond(
            canvas,
            points[5].0,
            points[5].1,
            span * (0.014 + load * 0.008),
            0.05,
            palette.core,
        );
    }
}

fn draw_query_wings(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DhtVisualSignals,
    palette: DhtPalette,
) {
    let span = bounds[1] - bounds[0];
    let activity = dht_activity(signals);
    let phase = dht_phase(signals, 1.08);
    let flap = phase.sin() * (0.05 + activity * 0.10);
    for side in [-1.0, 1.0] {
        for feather in 0_usize..6 {
            let unit = feather as f64 / 5.0;
            let root = (side * span * 0.035, -0.38 + unit * 0.14);
            let elbow = (
                side * span * (0.16 + unit * 0.10),
                0.05 + unit * 0.48 + flap,
            );
            let tip = (
                side * span * (0.42 - unit * 0.04),
                -0.48 + unit * 0.24 - flap,
            );
            draw_polyline(
                canvas,
                &[root, elbow, tip],
                if feather.is_multiple_of(2) {
                    palette.query
                } else {
                    palette.accent
                },
            );
        }
    }
    draw_filled_diamond(canvas, 0.0, -0.10, span * 0.045, 0.18, palette.peer_yield);
}

fn draw_relay_ribbon(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DhtVisualSignals,
    palette: DhtPalette,
) {
    let span = bounds[1] - bounds[0];
    let activity = dht_activity(signals);
    let phase = dht_phase(signals, 1.15);
    for ribbon in 0..4 {
        let mut points = Vec::with_capacity(17);
        for step in 0..=16 {
            let unit = step as f64 / 16.0;
            let x = -span * 0.44 + unit * span * 0.88;
            let y = (unit * TAU * 1.5 + phase + ribbon as f64 * 0.85).sin()
                * (0.22 + activity * 0.12)
                + (ribbon as f64 - 1.5) * 0.12;
            points.push((x, y));
        }
        draw_polyline(
            canvas,
            &points,
            [
                palette.query,
                palette.peer_yield,
                palette.accent,
                palette.alert,
            ][ribbon],
        );
    }
}

fn draw_pulse_grid(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DhtVisualSignals,
    palette: DhtPalette,
) {
    let span = bounds[1] - bounds[0];
    let activity = dht_activity(signals);
    let phase = dht_phase(signals, 1.30);
    let left = -span * 0.38;
    let right = span * 0.38;
    for column in 0..=6 {
        let x = left + column as f64 / 6.0 * (right - left);
        draw_segment(canvas, (x, -0.70), (x, 0.70), palette.accent);
    }
    for row in 0..=5 {
        let y = -0.70 + row as f64 / 5.0 * 1.40;
        draw_segment(canvas, (left, y), (right, y), palette.query);
    }
    let scan = ((phase.sin() * 0.5 + 0.5) * 5.0).round() as usize;
    let active_cells = 3 + (activity * 8.0).round() as usize;
    for cell in 0..active_cells {
        let column = (cell * 5 + scan * 3) % 6;
        let row = (cell * 3 + scan) % 5;
        let x = left + (column as f64 + 0.5) / 6.0 * (right - left);
        let y = -0.70 + (row as f64 + 0.5) / 5.0 * 1.40;
        draw_filled_diamond(
            canvas,
            x,
            y,
            span * 0.018,
            0.06,
            if cell.is_multiple_of(2) {
                palette.alert
            } else {
                palette.peer_yield
            },
        );
    }
}

fn draw_lookup_vortex(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DhtVisualSignals,
    palette: DhtPalette,
) {
    let span = bounds[1] - bounds[0];
    let activity = dht_activity(signals);
    let phase = dht_phase(signals, 1.22);
    for arm in 0..3 {
        let mut points = Vec::with_capacity(25);
        for step in 0..=24 {
            let unit = step as f64 / 24.0;
            let angle = unit * TAU * 1.55 + phase + arm as f64 * TAU / 3.0;
            let radius = 0.05 + unit * (0.64 + activity * 0.10);
            points.push((angle.cos() * radius * span * 0.48, angle.sin() * radius));
        }
        draw_polyline(
            canvas,
            &points,
            [palette.query, palette.peer_yield, palette.accent][arm],
        );
    }
    draw_filled_diamond(
        canvas,
        0.0,
        0.0,
        span * (0.025 + activity * 0.02),
        0.10,
        palette.alert,
    );
}

fn draw_peer_bloom(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DhtVisualSignals,
    palette: DhtPalette,
) {
    let span = bounds[1] - bounds[0];
    let activity = dht_activity(signals);
    let phase = dht_phase(signals, 0.72);
    let petals = 6 + (signals.yield_signal * 4.0).round() as usize;
    for petal in 0..petals {
        let angle = petal as f64 / petals as f64 * TAU + phase * 0.18;
        let tangent = (-angle.sin(), angle.cos());
        let center = (angle.cos() * span * 0.22, angle.sin() * 0.42);
        let half = span * (0.05 + activity * 0.025);
        let tip = (
            angle.cos() * span * (0.37 + activity * 0.04),
            angle.sin() * (0.72 + activity * 0.05),
        );
        draw_polyline(
            canvas,
            &[
                (center.0 + tangent.0 * half, center.1 + tangent.1 * 0.10),
                tip,
                (center.0 - tangent.0 * half, center.1 - tangent.1 * 0.10),
                (center.0 + tangent.0 * half, center.1 + tangent.1 * 0.10),
            ],
            if petal.is_multiple_of(2) {
                palette.peer_yield
            } else {
                palette.query
            },
        );
    }
    draw_filled_diamond(canvas, 0.0, 0.0, span * 0.055, 0.18, palette.core);
}

fn canvas_bounds(area: Rect) -> ([f64; 2], [f64; 2]) {
    let width = f64::from(area.width.saturating_sub(2).max(1));
    let height = f64::from(area.height.saturating_sub(2).max(1));
    let half = (width / (height * 2.0)).max(0.65);
    ([-half, half], [-1.0, 1.0])
}

fn draw_diamond(
    canvas: &mut Context<'_>,
    center_x: f64,
    center_y: f64,
    radius_x: f64,
    radius_y: f64,
    color: Color,
) {
    let points = [
        (center_x, center_y - radius_y),
        (center_x + radius_x, center_y),
        (center_x, center_y + radius_y),
        (center_x - radius_x, center_y),
    ];
    for index in 0..points.len() {
        let next = (index + 1) % points.len();
        canvas.draw(&CanvasLine {
            x1: points[index].0,
            y1: points[index].1,
            x2: points[next].0,
            y2: points[next].1,
            color,
        });
    }
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
    fn retained_dht_gallery_keeps_selected_ids_and_distinguishes_every_candidate() {
        let candidates = DhtVisualization::ALL
            .into_iter()
            .filter(|view| *view != DhtVisualization::Classic)
            .collect::<Vec<_>>();
        assert_eq!(candidates.len(), 5);
        assert_eq!(
            candidates
                .iter()
                .filter_map(|view| view.temporary_number())
                .collect::<Vec<_>>(),
            [10, 13, 14, 15, 16]
        );

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
    fn dht_temporary_name_only_appears_in_visualization_focus_mode() {
        let (mut state, status, telemetry) = sample_dht_inputs();
        let normal =
            render_dht_inputs(DhtVisualization::PulseGrid, 52, &state, &status, &telemetry);
        let normal_text = normal
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!normal_text.contains("TEMP 14"));
        assert!(!normal_text.contains("Pulse Grid"));

        state.ui.visualization_focus.active = true;
        let focused =
            render_dht_inputs(DhtVisualization::PulseGrid, 52, &state, &status, &telemetry);
        let focused_text = focused
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(focused_text.contains("TEMP 14"));
        assert!(focused_text.contains("Pulse Grid"));
    }

    #[test]
    fn every_temporary_candidate_reacts_to_current_metrics() {
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
    fn disk_gallery_numbers_and_distinguishes_all_twenty_candidates() {
        let state = sample_disk_state();
        let candidates = DiskHealthVisualization::ALL
            .into_iter()
            .filter(|view| *view != DiskHealthVisualization::Classic)
            .collect::<Vec<_>>();
        assert_eq!(candidates.len(), 20);

        let mut interiors = Vec::with_capacity(candidates.len());
        for (index, view) in candidates.into_iter().enumerate() {
            assert_eq!(view.temporary_number(), Some(index as u8 + 1));
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
