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

use crate::app::{AppState, DhtVisualization};
use crate::dht_service::{DhtStatus, DhtWaveTelemetry};
use crate::theme::ThemeContext;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DiskHealthSignals {
    pub(crate) health: f64,
    pub(crate) throughput_gap: f64,
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
            state_level: app_state.disk_health_state_level.min(3),
            phase: app_state.disk_health_phase,
            active: disk_total_bps > 0,
        }
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

    fn query_amplitude(self) -> f64 {
        (self.amplitude + self.discovery_boost + self.query_surge).clamp(0.05, 0.82)
    }

    fn demand_signal(self) -> f64 {
        (f64::from(self.power_scale_halves.max(2)) / 8.0).clamp(0.25, 1.0)
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
    let full_title = format!("DHT · {}", view.label());
    let compact_title = format!("DHT {}", view.compact_label());
    let left_title = if full_title.chars().count() + 1 + metric_width <= available_width {
        Some(full_title)
    } else if compact_title.chars().count() + 1 + metric_width <= available_width {
        Some(compact_title)
    } else if "DHT".len() + 1 + metric_width <= available_width {
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
    let block = block
        .title_top(Line::from(metric_spans).alignment(Alignment::Right))
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
            DhtVisualization::LookupCore => draw_lookup_core(canvas, x_bounds, signals, palette),
        });
    frame.render_widget(canvas, area);
}

fn draw_lookup_core(
    canvas: &mut Context<'_>,
    bounds: [f64; 2],
    signals: DhtVisualSignals,
    palette: DhtPalette,
) {
    let span = bounds[1] - bounds[0];
    let query_amplitude = signals.query_amplitude();
    let demand = signals.demand_signal();
    let query_energy = (signals.query_signal * 0.42
        + signals.instant_query_signal * 0.58
        + signals.query_surge * 0.8)
        .clamp(0.0, 1.0);
    let activity = query_energy
        .max(signals.yield_signal)
        .max(signals.discovery_boost)
        .clamp(0.0, 1.0);
    let orbit_phase = signals.time * (0.78 + signals.frequency * 1.8);
    let core_x = span * (0.012 + query_energy * 0.055) * orbit_phase.sin();
    let core_y =
        signals.crest_bias + orbit_phase.cos() * (0.008 + signals.harmonic_amplitude * 0.22);
    let core_pulse = 1.0 + orbit_phase.sin() * (0.035 + activity * 0.10);
    draw_filled_ellipse(
        canvas,
        core_x,
        core_y,
        span * (0.038 + query_amplitude * 0.032 + demand * 0.010) * core_pulse,
        (0.15 + query_amplitude * 0.21 + demand * 0.035) * core_pulse,
        palette.core,
    );
    let rings = (2
        + (signals.yield_signal * 3.0).round() as usize
        + (signals.instant_query_signal * 2.0).round() as usize)
        .clamp(2, 7);
    for ring in 1..=rings {
        let ring_phase = orbit_phase * (0.54 + ring as f64 * 0.07) + ring as f64 * 1.13;
        let pulse = 1.0 + ring_phase.sin() * (0.025 + activity * 0.075);
        let color = if ring == 1 && signals.query_surge > 0.02 {
            palette.alert
        } else if signals.peer_yield > 0 && ring.is_multiple_of(3) {
            palette.peer_yield
        } else if ring.is_multiple_of(2) {
            palette.accent
        } else {
            palette.query
        };
        draw_ellipse(
            canvas,
            core_x,
            core_y,
            span * (0.055 + ring as f64 * (0.029 + query_energy * 0.007)) * pulse,
            (0.14 + ring as f64 * (0.105 + signals.yield_signal * 0.018)) * pulse,
            color,
        );
    }
    let beams = (8
        + (signals.instant_query_signal * 10.0).round() as usize
        + (signals.yield_signal * 6.0).round() as usize
        + (demand * 4.0).round() as usize)
        .clamp(9, 28);
    for beam in 0..beams {
        let spoke_phase = beam as f64 * TAU / beams as f64;
        let angle = orbit_phase + spoke_phase;
        let reach_pulse =
            1.0 + (orbit_phase * 1.7 + spoke_phase * 2.0).sin() * (0.025 + activity * 0.09);
        let outer_x = span * (0.24 + query_energy * 0.23 + demand * 0.08) * reach_pulse;
        let outer_y = (0.42
            + signals.yield_signal * 0.27
            + query_energy * 0.12
            + signals.bootstrap_ratio * 0.05)
            * reach_pulse;
        let color = if signals.query_surge > 0.02 && beam.is_multiple_of(4) {
            palette.alert
        } else if signals.peer_yield > 0 && beam.is_multiple_of(3) {
            palette.peer_yield
        } else if beam.is_multiple_of(2) {
            palette.query
        } else {
            palette.accent
        };
        canvas.draw(&CanvasLine {
            x1: core_x + angle.cos() * span * (0.052 + query_amplitude * 0.012),
            y1: core_y + angle.sin() * (0.13 + query_amplitude * 0.035),
            x2: core_x + angle.cos() * outer_x,
            y2: core_y + angle.sin() * outer_y,
            color,
        });
    }
}

fn canvas_bounds(area: Rect) -> ([f64; 2], [f64; 2]) {
    let width = f64::from(area.width.saturating_sub(2).max(1));
    let height = f64::from(area.height.saturating_sub(2).max(1));
    let half = (width / (height * 2.0)).max(0.65);
    ([-half, half], [-1.0, 1.0])
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

    #[test]
    fn lookup_core_is_labeled_and_draws_a_dense_signal() {
        let buffer = render_dht(DhtVisualization::LookupCore);
        let text = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(text.contains("Lookup Core"));
        assert!(occupied(&buffer) > 40);
    }

    #[test]
    fn dht_renderer_prioritizes_classic_metrics_in_a_narrow_title() {
        let buffer = render_dht_with_width(DhtVisualization::LookupCore, 17);
        let top_row = buffer.content()[..17]
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(top_row.contains("DHT"));
        assert!(top_row.contains("2x(23 37)"));
        assert!(!top_row.contains("Lookup Core"));
        assert!(!top_row.contains("Core"));
    }

    #[test]
    fn lookup_core_tracks_the_classic_smoothed_wave_state() {
        let (mut quiet, status, mut telemetry) = sample_dht_inputs();
        quiet.ui.dht_wave.amplitude = 0.01;
        quiet.ui.dht_wave.harmonic_amplitude = 0.004;
        quiet.ui.dht_wave.query_load = 0.0;
        quiet.ui.dht_wave.query_surge = 0.0;
        quiet.ui.dht_wave.discovery_boost = 0.0;
        telemetry.inflight_ipv4_queries = 0;
        telemetry.inflight_ipv6_queries = 0;
        telemetry.unique_peers_found_last_10s = 0;

        let (active, _, active_telemetry) = sample_dht_inputs();
        let quiet_buffer = render_dht_inputs(
            DhtVisualization::LookupCore,
            52,
            &quiet,
            &status,
            &telemetry,
        );
        let active_buffer = render_dht_inputs(
            DhtVisualization::LookupCore,
            52,
            &active,
            &status,
            &active_telemetry,
        );
        assert_ne!(
            interior_cells(&quiet_buffer),
            interior_cells(&active_buffer)
        );
    }

    #[test]
    fn lookup_core_reacts_immediately_to_current_query_and_yield_metrics() {
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

        let quiet = render_dht_inputs(
            DhtVisualization::LookupCore,
            52,
            &state,
            &status,
            &quiet_telemetry,
        );
        let busy = render_dht_inputs(
            DhtVisualization::LookupCore,
            52,
            &state,
            &status,
            &busy_telemetry,
        );

        assert_ne!(interior_cells(&quiet), interior_cells(&busy));
        assert!(occupied(&busy) > occupied(&quiet));
    }

    #[test]
    fn lookup_core_keeps_live_color_when_dht_is_idle() {
        let mut state = AppState::default();
        state.ui.dht_wave.initialized = true;
        let status = DhtStatus::default();
        let telemetry = DhtWaveTelemetry::default();
        let ctx = ThemeContext::new(state.theme, 0.0);
        let buffer = render_dht_inputs(
            DhtVisualization::LookupCore,
            52,
            &state,
            &status,
            &telemetry,
        );
        let colors = interior_cells(&buffer)
            .into_iter()
            .filter(|(symbol, _)| symbol != " ")
            .map(|(_, color)| color)
            .collect::<Vec<_>>();

        assert!(colors.contains(&ctx.peer_discovered()));
        assert!(colors.contains(&ctx.accent_sapphire()));
        assert!(colors.contains(&ctx.theme.scale.categorical.lavender));
    }
}
