// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::f64::consts::{PI, TAU};

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Context, Line as CanvasLine, Points};
use ratatui::widgets::{Block, Borders};
use ratatui::Frame;

use crate::app::{PeerInfo, PeerStreamVisualization, TorrentDisplayState};
use crate::theme::ThemeContext;

#[derive(Clone, Copy)]
struct PeerStreamPalette {
    discovered: Color,
    connecting: Color,
    connected: Color,
    disconnected: Color,
    grid: Color,
    core: Color,
    text: Color,
    border: Color,
}

impl PeerStreamPalette {
    fn from_theme(ctx: &ThemeContext) -> Self {
        Self {
            discovered: ctx.peer_discovered(),
            connecting: ctx.state_info(),
            connected: ctx.peer_connected(),
            disconnected: ctx.peer_disconnected(),
            grid: ctx.theme.semantic.surface2,
            core: ctx.accent_teal(),
            text: ctx.theme.semantic.text,
            border: ctx.theme.semantic.border,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct HistoryBucket {
    discovered: u64,
    connected: u64,
    disconnected: u64,
    active: f64,
    useful: f64,
    flow: f64,
    samples: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VisualPeerState {
    Discovered,
    Connecting,
    Connected,
    Leaving,
}

#[derive(Clone, Copy, Debug)]
struct VisualPeer {
    id: u64,
    state: VisualPeerState,
    progress: f64,
    phase: f64,
    activity: f64,
    quality: f64,
}

#[derive(Debug, Default)]
struct PeerStreamData {
    buckets: Vec<HistoryBucket>,
    peers: Vec<VisualPeer>,
    active_count: usize,
    useful_count: usize,
    discovered_recent: u64,
    connected_recent: u64,
    disconnected_recent: u64,
    time: f64,
}

impl PeerStreamData {
    fn from_torrent(torrent: &TorrentDisplayState, columns: usize, time: f64) -> Self {
        let live_peers = &torrent.latest_state.peers;
        let inferred_useful_count = live_peers
            .iter()
            .filter(|peer| peer_is_useful(peer))
            .count();
        let reported_useful_count = torrent
            .latest_state
            .beneficial_tcp_peer_count
            .saturating_add(torrent.latest_state.beneficial_utp_peer_count);
        let active_count = live_peers
            .len()
            .max(torrent.latest_state.number_of_successfully_connected_peers);
        let useful_count = inferred_useful_count
            .max(reported_useful_count)
            .min(active_count);
        let buckets = aggregate_histories(torrent, columns, active_count, useful_count);
        let peers = build_visual_peers(torrent, time);
        let recent_window = 8;
        let discovered_recent = torrent
            .peer_discovery_history
            .iter()
            .rev()
            .take(recent_window)
            .sum();
        let connected_recent = torrent
            .peer_connection_history
            .iter()
            .rev()
            .take(recent_window)
            .sum();
        let disconnected_recent = torrent
            .peer_disconnect_history
            .iter()
            .rev()
            .take(recent_window)
            .sum();

        Self {
            buckets,
            peers,
            active_count,
            useful_count,
            discovered_recent,
            connected_recent,
            disconnected_recent,
            time,
        }
    }
}

pub fn draw_peer_stream_visualization(
    frame: &mut Frame,
    torrent: &TorrentDisplayState,
    area: Rect,
    view: PeerStreamVisualization,
    ctx: &ThemeContext,
    time: f64,
) {
    if area.width < 10 || area.height < 3 {
        return;
    }

    let palette = PeerStreamPalette::from_theme(ctx);
    let data =
        PeerStreamData::from_torrent(torrent, canvas_sample_columns(area).clamp(20, 120), time);
    match view {
        PeerStreamVisualization::Classic => {}
        PeerStreamVisualization::AccretionLens => {
            draw_accretion_lens(frame, area, view, &data, palette, ctx)
        }
        PeerStreamVisualization::PrismSplit => {
            draw_prism_split(frame, area, view, &data, palette, ctx)
        }
        PeerStreamVisualization::InOut => draw_in_out(frame, area, view, &data, palette, ctx),
        PeerStreamVisualization::HelixExchange => {
            draw_helix_exchange(frame, area, view, &data, palette, ctx)
        }
        PeerStreamVisualization::MagSlalom => {
            draw_mag_slalom(frame, area, view, &data, palette, ctx)
        }
    }
}

fn panel_block<'a>(
    view: PeerStreamVisualization,
    data: &PeerStreamData,
    palette: PeerStreamPalette,
    ctx: &ThemeContext,
) -> Block<'a> {
    let title = Line::from(vec![
        Span::styled(
            " Peer Stream ",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
        ),
        Span::styled(
            format!("· {} ", view.label()),
            ctx.apply(
                Style::default()
                    .fg(ctx.state_selected())
                    .add_modifier(Modifier::BOLD),
            ),
        ),
    ]);
    let detail = Line::from(vec![
        Span::styled(
            format!("A{} U{} ", data.active_count, data.useful_count),
            ctx.apply(Style::default().fg(palette.connected)),
        ),
        Span::styled(
            format!("D{} ", data.discovered_recent),
            ctx.apply(Style::default().fg(palette.discovered)),
        ),
        Span::styled(
            format!("C{} ", data.connected_recent),
            ctx.apply(Style::default().fg(palette.connecting)),
        ),
        Span::styled(
            format!("X{} ", data.disconnected_recent),
            ctx.apply(Style::default().fg(palette.disconnected)),
        ),
    ])
    .alignment(Alignment::Right);

    Block::default()
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(palette.border)))
        .title_top(title)
        .title_top(detail)
}

fn aggregate_histories(
    torrent: &TorrentDisplayState,
    requested_columns: usize,
    active_count: usize,
    useful_count: usize,
) -> Vec<HistoryBucket> {
    let len = torrent
        .peer_discovery_history
        .len()
        .max(torrent.peer_connection_history.len())
        .max(torrent.peer_disconnect_history.len());
    if len == 0 || requested_columns == 0 {
        return vec![HistoryBucket {
            active: active_count as f64,
            useful: useful_count as f64,
            ..Default::default()
        }];
    }

    let history_value = |history: &[u64], index: usize| {
        let offset = len.saturating_sub(history.len());
        index
            .checked_sub(offset)
            .and_then(|local| history.get(local))
            .copied()
            .unwrap_or(0)
    };
    let connected_total: i128 = (0..len)
        .map(|index| i128::from(history_value(&torrent.peer_connection_history, index)))
        .sum();
    let disconnected_total: i128 = (0..len)
        .map(|index| i128::from(history_value(&torrent.peer_disconnect_history, index)))
        .sum();
    let mut running_active = (active_count as i128 - connected_total + disconnected_total).max(0);
    let useful_ratio = useful_count as f64 / active_count.max(1) as f64;
    let columns = requested_columns.min(len).max(1);
    let mut buckets = vec![HistoryBucket::default(); columns];

    for index in 0..len {
        let discovered = history_value(&torrent.peer_discovery_history, index);
        let connected = history_value(&torrent.peer_connection_history, index);
        let disconnected = history_value(&torrent.peer_disconnect_history, index);
        running_active = (running_active + i128::from(connected) - i128::from(disconnected)).max(0);
        let target_index = (index * columns / len).min(columns - 1);
        let target = &mut buckets[target_index];
        target.discovered += discovered;
        target.connected += connected;
        target.disconnected += disconnected;
        target.active += running_active as f64;
        target.useful += running_active as f64 * useful_ratio;
        target.flow += (discovered + connected + disconnected) as f64;
        target.samples += 1;
    }

    for bucket in &mut buckets {
        let samples = bucket.samples.max(1) as f64;
        bucket.active /= samples;
        bucket.useful /= samples;
        bucket.flow /= samples;
    }
    buckets
}

fn build_visual_peers(torrent: &TorrentDisplayState, time: f64) -> Vec<VisualPeer> {
    const PEER_LIMIT: usize = 64;
    const EVENT_BUCKETS: usize = 12;
    let peers = &torrent.latest_state.peers;
    let max_speed = peers
        .iter()
        .map(|peer| {
            peer.download_speed_bps
                .saturating_add(peer.upload_speed_bps)
        })
        .max()
        .unwrap_or(1)
        .max(1) as f64;
    let step = peers.len().div_ceil(PEER_LIMIT).max(1);
    let mut visual = peers
        .iter()
        .enumerate()
        .step_by(step)
        .take(PEER_LIMIT)
        .map(|(index, peer)| {
            let id = stable_peer_id(peer, index);
            let speed = peer
                .download_speed_bps
                .saturating_add(peer.upload_speed_bps) as f64;
            let activity = (speed / max_speed).sqrt().clamp(0.08, 1.0);
            let piece_ratio = peer.bitfield.iter().filter(|has_piece| **has_piece).count() as f64
                / peer.bitfield.len().max(1) as f64;
            let responsiveness = if peer.peer_choking { 0.12 } else { 0.34 }
                + if peer.peer_interested { 0.20 } else { 0.0 }
                + if peer.am_interested { 0.12 } else { 0.0 };
            VisualPeer {
                id,
                state: VisualPeerState::Connected,
                progress: wrap01(time * 0.05 + visual_unit(id)),
                phase: visual_unit(id ^ 0x006f_7262_6974) * TAU,
                activity,
                quality: (piece_ratio * 0.45 + responsiveness).clamp(0.05, 1.0),
            }
        })
        .collect::<Vec<_>>();

    let reported_count = torrent.latest_state.number_of_successfully_connected_peers;
    let missing_count = reported_count
        .saturating_sub(peers.len())
        .min(PEER_LIMIT.saturating_sub(visual.len()));
    let aggregate_speed = torrent
        .latest_state
        .download_speed_bps
        .saturating_add(torrent.latest_state.upload_speed_bps);
    let aggregate_activity = if aggregate_speed == 0 { 0.12 } else { 0.55 };
    let beneficial_count = torrent
        .latest_state
        .beneficial_tcp_peer_count
        .saturating_add(torrent.latest_state.beneficial_utp_peer_count);
    let reported_quality = beneficial_count as f64 / reported_count.max(1) as f64;
    for index in 0..missing_count {
        let id = 0xa11c_e000_u64 ^ index as u64;
        visual.push(VisualPeer {
            id,
            state: VisualPeerState::Connected,
            progress: wrap01(time * 0.05 + visual_unit(id)),
            phase: visual_unit(id ^ 0x006f_7262_6974) * TAU,
            activity: aggregate_activity,
            quality: reported_quality.clamp(0.12, 1.0),
        });
    }

    let len = torrent.peer_discovery_history.len();
    for age in 0..EVENT_BUCKETS.min(len) {
        let index = len - 1 - age;
        for (state, count, salt) in [
            (
                VisualPeerState::Discovered,
                torrent.peer_discovery_history[index],
                0xd15c_0000_u64,
            ),
            (
                VisualPeerState::Connecting,
                torrent
                    .peer_connection_history
                    .get(index)
                    .copied()
                    .unwrap_or(0),
                0xc011_0000_u64,
            ),
            (
                VisualPeerState::Leaving,
                torrent
                    .peer_disconnect_history
                    .get(index)
                    .copied()
                    .unwrap_or(0),
                0xde1e_0000_u64,
            ),
        ] {
            let marks = ((count as f64).sqrt().ceil() as usize).min(3);
            for mark in 0..marks {
                let id = salt ^ ((index as u64) << 12) ^ mark as u64;
                visual.push(VisualPeer {
                    id,
                    state,
                    progress: wrap01(time * 0.18 + age as f64 * 0.11 + mark as f64 * 0.23),
                    phase: visual_unit(id) * TAU,
                    activity: (0.28 + (count as f64).ln_1p() * 0.22).clamp(0.28, 1.0),
                    quality: 0.35 + visual_unit(id ^ 0x5155) * 0.5,
                });
            }
        }
    }
    visual
}

fn peer_is_useful(peer: &PeerInfo) -> bool {
    !peer.peer_choking
        && (peer.download_speed_bps > 0
            || peer.upload_speed_bps > 0
            || (peer.peer_interested && peer.am_interested))
}

fn stable_peer_id(peer: &PeerInfo, fallback_index: usize) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let bytes: &[u8] = if !peer.peer_id.is_empty() {
        &peer.peer_id
    } else if !peer.address.is_empty() {
        peer.address.as_bytes()
    } else {
        return fallback_index as u64;
    };
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

fn draw_accretion_lens(
    frame: &mut Frame,
    area: Rect,
    view: PeerStreamVisualization,
    data: &PeerStreamData,
    palette: PeerStreamPalette,
    ctx: &ThemeContext,
) {
    let (x_bounds, y_bounds) = canvas_bounds(area);
    let active_max = max_active(&data.buckets);
    let event_max = max_event(&data.buckets);
    let span = x_bounds[1] - x_bounds[0];
    let block = panel_block(view, data, palette, ctx);
    let canvas = Canvas::default()
        .block(block)
        .marker(Marker::Braille)
        .x_bounds(x_bounds)
        .y_bounds(y_bounds)
        .paint(|canvas| {
            for band in 0..5 {
                let sign = if band < 3 { 1.0 } else { -1.0 };
                let layer = if band < 3 { band } else { band - 3 };
                let color = [
                    palette.discovered,
                    palette.connected,
                    palette.core,
                    palette.text,
                    palette.disconnected,
                ][band];
                let mut previous = None;
                for (index, bucket) in data.buckets.iter().copied().enumerate() {
                    let x = time_x(index, data.buckets.len(), x_bounds);
                    let center = (x / (span * 0.5)).clamp(-1.0, 1.0);
                    let lens = (-(center * center) * 5.5).exp();
                    let energy = match band {
                        0 => bucket.discovered as f64 / event_max,
                        1 => bucket.connected as f64 / event_max,
                        2 => bucket.active / active_max,
                        3 => bucket.useful / active_max,
                        _ => bucket.disconnected as f64 / event_max,
                    };
                    let base = 0.045 + layer as f64 * 0.075;
                    let y = sign * (base + lens * (0.25 + layer as f64 * 0.055) + energy * 0.12)
                        + (data.time * 0.42 + index as f64 * 0.22 + band as f64).sin() * 0.012;
                    draw_segment_from_previous(canvas, &mut previous, x, y, color);
                }
            }

            canvas.draw(&CanvasLine {
                x1: x_bounds[0],
                y1: -0.015,
                x2: x_bounds[1],
                y2: -0.015,
                color: palette.discovered,
            });
            draw_filled_ellipse(canvas, 0.0, -0.015, span * 0.065, 0.30, Color::Black);
            draw_ellipse(canvas, 0.0, -0.015, span * 0.065, 0.30, palette.core);
            draw_ellipse(canvas, 0.0, -0.015, span * 0.084, 0.36, palette.discovered);

            for peer in sampled_peers(data, area.width, 2) {
                let side = if peer.id.is_multiple_of(2) { -1.0 } else { 1.0 };
                let progress = smoothstep(peer.progress);
                let (x, y) = match peer.state {
                    VisualPeerState::Discovered => (
                        side * span * (0.47 - progress * 0.10),
                        side * 0.08 + (peer.phase + data.time).sin() * 0.05,
                    ),
                    VisualPeerState::Connecting => {
                        let x = side * span * (0.37 - progress * 0.26);
                        let arch = (-(x / (span * 0.24)).powi(2)).exp();
                        (x, side * (0.10 + arch * 0.30))
                    }
                    VisualPeerState::Connected => {
                        let theta = peer.phase + data.time * (0.8 + peer.activity);
                        (
                            theta.cos() * span * (0.070 + peer.quality * 0.025),
                            theta.sin() * (0.29 + peer.quality * 0.09),
                        )
                    }
                    VisualPeerState::Leaving => (
                        side * span * (0.08 + progress * 0.42),
                        -side * 0.12 - progress * 0.48,
                    ),
                };
                draw_particle(canvas, x, y, peer_color(peer.state, palette), peer.activity);
                draw_useful_flare(canvas, peer, x, y, palette);
            }
        });
    frame.render_widget(canvas, area);
}

fn draw_prism_split(
    frame: &mut Frame,
    area: Rect,
    view: PeerStreamVisualization,
    data: &PeerStreamData,
    palette: PeerStreamPalette,
    ctx: &ThemeContext,
) {
    let (x_bounds, y_bounds) = canvas_bounds(area);
    let span = x_bounds[1] - x_bounds[0];
    let prism_x = plot_x(0.39, x_bounds);
    let prism_left = prism_x - span * 0.065;
    let prism_right = prism_x + span * 0.065;
    let output_y = [0.62, 0.22, -0.22, -0.62];
    let output_colors = [
        palette.discovered,
        palette.connecting,
        palette.connected,
        palette.disconnected,
    ];
    let block = panel_block(view, data, palette, ctx);
    let canvas = Canvas::default()
        .block(block)
        .marker(Marker::Braille)
        .x_bounds(x_bounds)
        .y_bounds(y_bounds)
        .paint(|canvas| {
            draw_temporal_history(canvas, data, x_bounds, palette);
            for (lane, y) in [-0.12, 0.0, 0.12].into_iter().enumerate() {
                canvas.draw(&CanvasLine {
                    x1: x_bounds[0],
                    y1: y,
                    x2: prism_left,
                    y2: y * 0.28,
                    color: [palette.discovered, palette.text, palette.connecting][lane],
                });
            }
            let triangle = [
                (prism_left, -0.48),
                (prism_left, 0.48),
                (prism_right, 0.0),
                (prism_left, -0.48),
            ];
            for pair in triangle.windows(2) {
                canvas.draw(&CanvasLine {
                    x1: pair[0].0,
                    y1: pair[0].1,
                    x2: pair[1].0,
                    y2: pair[1].1,
                    color: palette.text,
                });
            }
            for (target_y, color) in output_y.into_iter().zip(output_colors) {
                canvas.draw(&CanvasLine {
                    x1: prism_right,
                    y1: 0.0,
                    x2: x_bounds[1],
                    y2: target_y,
                    color,
                });
            }
            for flare in 0..6 {
                let progress = wrap01(
                    data.time * (0.10 + data.discovered_recent as f64 * 0.006) + flare as f64 / 6.0,
                );
                let x = x_bounds[0] + (prism_left - x_bounds[0]) * progress;
                canvas.draw(&CanvasLine {
                    x1: x,
                    y1: -0.17,
                    x2: x,
                    y2: 0.17,
                    color: palette.discovered,
                });
            }

            for peer in sampled_peers(data, area.width, 2) {
                let speed = 0.05 + peer.activity * 0.09;
                let travel = wrap01(visual_unit(peer.id) + data.time * speed);
                let state_index = peer_state_index(peer.state);
                let (x, y) = if travel < 0.42 {
                    let progress = travel / 0.42;
                    (
                        x_bounds[0] + (prism_left - x_bounds[0]) * progress,
                        (visual_unit(peer.id ^ 0x9911) - 0.5) * 0.20 * (1.0 - progress),
                    )
                } else {
                    let progress = (travel - 0.42) / 0.58;
                    (
                        prism_right + (x_bounds[1] - prism_right) * progress,
                        output_y[state_index] * progress,
                    )
                };
                draw_particle(canvas, x, y, peer_color(peer.state, palette), peer.activity);
                draw_useful_flare(canvas, peer, x, y, palette);
            }
        });
    frame.render_widget(canvas, area);
}

fn draw_in_out(
    frame: &mut Frame,
    area: Rect,
    view: PeerStreamVisualization,
    data: &PeerStreamData,
    palette: PeerStreamPalette,
    ctx: &ThemeContext,
) {
    let (x_bounds, y_bounds) = canvas_bounds(area);
    let core_edge = (x_bounds[1] - x_bounds[0]) * 0.075;
    let event_max = max_event(&data.buckets);
    let active_max = max_active(&data.buckets);
    let block = panel_block(view, data, palette, ctx);
    let canvas = Canvas::default()
        .block(block)
        .marker(Marker::Braille)
        .x_bounds(x_bounds)
        .y_bounds(y_bounds)
        .paint(|canvas| {
            for lane in 0..3 {
                let lane_y = -0.34 + lane as f64 * 0.34;
                let mut previous_left = None;
                let mut previous_right = None;
                for (age, bucket) in data.buckets.iter().rev().copied().enumerate() {
                    let age = age as f64 / (data.buckets.len() - 1).max(1) as f64;
                    let left_x = -core_edge - age * (x_bounds[1] * 0.96 - core_edge);
                    let right_x = core_edge + age * (x_bounds[1] * 0.96 - core_edge);
                    let inbound =
                        ((bucket.discovered + bucket.connected) as f64 / event_max).clamp(0.0, 1.0);
                    let outbound = (bucket.disconnected as f64 / event_max).clamp(0.0, 1.0);
                    let density = bucket.active / active_max;
                    let left_y = lane_y * age
                        + (age * 9.0 + lane as f64).sin() * 0.12 * inbound
                        + (1.0 - age) * 0.10 * density;
                    let right_y = lane_y * age + (age * 8.0 + lane as f64).cos() * 0.14 * outbound
                        - (1.0 - age) * 0.10 * density;
                    draw_segment_from_previous(
                        canvas,
                        &mut previous_left,
                        left_x,
                        left_y,
                        if lane == 0 {
                            palette.discovered
                        } else if lane == 1 {
                            palette.connected
                        } else {
                            palette.core
                        },
                    );
                    draw_segment_from_previous(
                        canvas,
                        &mut previous_right,
                        right_x,
                        right_y,
                        if lane == 2 {
                            palette.disconnected
                        } else {
                            palette.grid
                        },
                    );
                }
            }

            let radius = 0.10 + (data.active_count as f64).sqrt() * 0.009;
            draw_filled_ellipse(canvas, 0.0, 0.0, radius, radius * 0.76, palette.core);
            draw_ellipse(canvas, 0.0, 0.0, radius * 1.3, radius, palette.connected);
            for ring in 0..3 {
                let rx = core_edge * (1.0 + ring as f64 * 0.32);
                let ry = 0.30 + ring as f64 * 0.16;
                draw_ellipse(
                    canvas,
                    0.0,
                    0.0,
                    rx,
                    ry,
                    [palette.discovered, palette.connected, palette.disconnected][ring],
                );
            }

            for peer in sampled_peers(data, area.width, 2) {
                let side = if peer.state == VisualPeerState::Leaving {
                    1.0
                } else {
                    -1.0
                };
                let progress = if peer.state == VisualPeerState::Connected {
                    wrap01(visual_unit(peer.id) + data.time * (0.05 + peer.activity * 0.08))
                } else {
                    smoothstep(peer.progress)
                };
                let x = side * (core_edge + progress * (x_bounds[1] * 0.88 - core_edge));
                let y = (peer.phase + progress * TAU).sin() * (0.12 + progress * 0.42);
                draw_particle(canvas, x, y, peer_color(peer.state, palette), peer.activity);
            }
        });
    frame.render_widget(canvas, area);
}

fn draw_helix_exchange(
    frame: &mut Frame,
    area: Rect,
    view: PeerStreamVisualization,
    data: &PeerStreamData,
    palette: PeerStreamPalette,
    ctx: &ThemeContext,
) {
    let (x_bounds, y_bounds) = canvas_bounds(area);
    let active_density = (data.active_count as f64 / 120.0).clamp(0.0, 1.0);
    let amplitude = 0.32 + active_density * 0.17;
    let turns = 2.0 + active_density * 1.4;
    // The shared effects clock already integrates the activity speed. Multiplying its absolute
    // value by a live activity-derived rate makes the phase discontinuous whenever rates change.
    let scroll_phase = data.time * 0.10;
    let block = panel_block(view, data, palette, ctx);
    let canvas = Canvas::default()
        .block(block)
        .marker(Marker::Braille)
        .x_bounds(x_bounds)
        .y_bounds(y_bounds)
        .paint(|canvas| {
            draw_temporal_history(canvas, data, x_bounds, palette);
            let samples = canvas_sample_columns(area).clamp(36, 180);
            let strand_y = |unit: f64, side: f64| {
                side * amplitude * (TAU * (turns * unit + scroll_phase)).sin()
            };
            for index in 1..samples {
                let u1 = (index - 1) as f64 / (samples - 1) as f64;
                let u2 = index as f64 / (samples - 1) as f64;
                for (side, color) in [(1.0, palette.connecting), (-1.0, palette.grid)] {
                    canvas.draw(&CanvasLine {
                        x1: plot_x(u1, x_bounds),
                        y1: strand_y(u1, side),
                        x2: plot_x(u2, x_bounds),
                        y2: strand_y(u2, side),
                        color,
                    });
                }
            }

            for peer in sampled_peers(data, area.width, 3) {
                let unit = helix_exchange_unit(peer.id, data.time);
                let x = plot_x(unit, x_bounds);
                let upper = strand_y(unit, 1.0);
                let lower = strand_y(unit, -1.0);
                match peer.state {
                    VisualPeerState::Discovered => {
                        draw_particle(canvas, x, upper, palette.discovered, peer.activity)
                    }
                    VisualPeerState::Connecting => {
                        let exchange = smoothstep(oscillating_unit(peer.progress));
                        let end = upper + (lower - upper) * exchange;
                        canvas.draw(&CanvasLine {
                            x1: x,
                            y1: upper,
                            x2: x,
                            y2: end,
                            color: palette.connecting,
                        });
                        draw_particle(canvas, x, end, palette.connecting, peer.activity);
                    }
                    VisualPeerState::Connected => {
                        canvas.draw(&CanvasLine {
                            x1: x,
                            y1: upper,
                            x2: x,
                            y2: lower,
                            color: palette.connected,
                        });
                        draw_useful_flare(canvas, peer, x, (upper + lower) * 0.5, palette);
                    }
                    VisualPeerState::Leaving => {
                        let gap = 0.04 + smoothstep(oscillating_unit(peer.progress)) * 0.18;
                        let midpoint = (upper + lower) * 0.5;
                        let direction = (upper - lower).signum();
                        canvas.draw(&CanvasLine {
                            x1: x,
                            y1: upper,
                            x2: x,
                            y2: midpoint + gap * direction,
                            color: palette.disconnected,
                        });
                        canvas.draw(&CanvasLine {
                            x1: x,
                            y1: lower,
                            x2: x,
                            y2: midpoint - gap * direction,
                            color: palette.disconnected,
                        });
                    }
                }
            }
        });
    frame.render_widget(canvas, area);
}

fn draw_mag_slalom(
    frame: &mut Frame,
    area: Rect,
    view: PeerStreamVisualization,
    data: &PeerStreamData,
    palette: PeerStreamPalette,
    ctx: &ThemeContext,
) {
    let (x_bounds, y_bounds) = canvas_bounds(area);
    let block = panel_block(view, data, palette, ctx);
    let canvas = Canvas::default()
        .block(block)
        .marker(Marker::Braille)
        .x_bounds(x_bounds)
        .y_bounds(y_bounds)
        .paint(|canvas| {
            draw_temporal_history(canvas, data, x_bounds, palette);
            for post in 0_usize..7 {
                let unit = post as f64 / 6.0;
                let x = plot_x(unit, x_bounds);
                let y = if post.is_multiple_of(2) { 0.34 } else { -0.34 };
                draw_filled_ellipse(canvas, x, y, 0.035, 0.08, palette.core);
                draw_ellipse(canvas, x, y, 0.07, 0.12, palette.connecting);
            }

            for peer in sampled_peers(data, area.width, 3) {
                let speed = 0.045 + peer.activity * 0.09;
                let unit = mag_slalom_unit(peer.id, data.time, speed);
                let previous_unit = mag_slalom_unit(peer.id, data.time - 0.08, speed);
                let path = |position: f64| {
                    (position * PI * 6.0 + visual_unit(peer.id ^ 0x44) * 0.8).sin()
                        * (0.27 + peer.quality * 0.14)
                };
                let x = plot_x(unit, x_bounds);
                let mut y = path(unit);
                if peer.state == VisualPeerState::Leaving {
                    y += y.signum() * peer.progress * 0.40;
                }
                let previous_x = plot_x(previous_unit, x_bounds);
                if (x - previous_x).abs() < (x_bounds[1] - x_bounds[0]) * 0.45 {
                    canvas.draw(&CanvasLine {
                        x1: previous_x,
                        y1: path(previous_unit),
                        x2: x,
                        y2: y,
                        color: peer_color(peer.state, palette),
                    });
                }
                draw_particle(canvas, x, y, peer_color(peer.state, palette), peer.activity);
            }
        });
    frame.render_widget(canvas, area);
}

fn sampled_peers(
    data: &PeerStreamData,
    width: u16,
    divisor: usize,
) -> impl Iterator<Item = VisualPeer> + '_ {
    let limit = (usize::from(width.saturating_sub(2)) / divisor).clamp(10, 44);
    let step = data.peers.len().div_ceil(limit).max(1);
    data.peers.iter().copied().step_by(step).take(limit)
}

fn draw_temporal_history(
    canvas: &mut Context<'_>,
    data: &PeerStreamData,
    bounds: [f64; 2],
    palette: PeerStreamPalette,
) {
    if data.buckets.len() < 2 {
        return;
    }

    let event_max = max_event(&data.buckets);
    let baselines = [0.74, 0.0, -0.74];
    let directions = [-1.0, 1.0, 1.0];
    let colors = [palette.discovered, palette.connected, palette.disconnected];
    let mut previous = [None, None, None];

    for (index, bucket) in data.buckets.iter().copied().enumerate() {
        let x = time_x(index, data.buckets.len(), bounds);
        let values = [bucket.discovered, bucket.connected, bucket.disconnected];
        for lane in 0..values.len() {
            let signal = (values[lane] as f64 / event_max).sqrt().clamp(0.0, 1.0);
            let y = baselines[lane] + directions[lane] * signal * 0.18;
            draw_segment_from_previous(
                canvas,
                &mut previous[lane],
                x,
                y,
                if values[lane] > 0 {
                    colors[lane]
                } else {
                    palette.grid
                },
            );
            if values[lane] > 0 {
                canvas.draw(&CanvasLine {
                    x1: x,
                    y1: baselines[lane],
                    x2: x,
                    y2: y,
                    color: colors[lane],
                });
            }
        }
    }
}

fn canvas_bounds(area: Rect) -> ([f64; 2], [f64; 2]) {
    let width = f64::from(area.width.saturating_sub(2).max(1));
    let height = f64::from(area.height.saturating_sub(2).max(1));
    let x_half = (width / (height * 2.0)).max(0.65);
    ([-x_half, x_half], [-1.0, 1.0])
}

fn canvas_sample_columns(area: Rect) -> usize {
    usize::from(area.width.saturating_sub(2).max(1)) * 2
}

fn time_x(index: usize, len: usize, bounds: [f64; 2]) -> f64 {
    if len <= 1 {
        return bounds[1];
    }
    bounds[0] + (bounds[1] - bounds[0]) * index as f64 / (len - 1) as f64
}

fn plot_x(unit: f64, bounds: [f64; 2]) -> f64 {
    bounds[0] + (bounds[1] - bounds[0]) * unit.clamp(0.0, 1.0)
}

fn oscillating_unit(unit: f64) -> f64 {
    0.5 - 0.5 * (TAU * wrap01(unit)).cos()
}

fn helix_exchange_unit(peer_id: u64, time: f64) -> f64 {
    wrap01(visual_unit(peer_id) - time * 0.018)
}

fn mag_slalom_unit(peer_id: u64, time: f64, speed: f64) -> f64 {
    wrap01(visual_unit(peer_id) - time * speed)
}

fn max_event(buckets: &[HistoryBucket]) -> f64 {
    buckets
        .iter()
        .map(|bucket| {
            bucket
                .discovered
                .max(bucket.connected)
                .max(bucket.disconnected)
        })
        .max()
        .unwrap_or(1)
        .max(1) as f64
}

fn max_active(buckets: &[HistoryBucket]) -> f64 {
    buckets
        .iter()
        .map(|bucket| bucket.active)
        .fold(1.0, f64::max)
}

fn peer_color(state: VisualPeerState, palette: PeerStreamPalette) -> Color {
    match state {
        VisualPeerState::Discovered => palette.discovered,
        VisualPeerState::Connecting => palette.connecting,
        VisualPeerState::Connected => palette.connected,
        VisualPeerState::Leaving => palette.disconnected,
    }
}

fn peer_state_index(state: VisualPeerState) -> usize {
    match state {
        VisualPeerState::Discovered => 0,
        VisualPeerState::Connecting => 1,
        VisualPeerState::Connected => 2,
        VisualPeerState::Leaving => 3,
    }
}

fn draw_useful_flare(
    canvas: &mut Context<'_>,
    peer: VisualPeer,
    x: f64,
    y: f64,
    palette: PeerStreamPalette,
) {
    if peer.state == VisualPeerState::Connected && peer.activity >= 0.48 && peer.quality >= 0.45 {
        canvas.draw(&Points {
            coords: &[(x, y)],
            color: palette.text,
        });
    }
}

fn draw_segment_from_previous(
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
        let theta_a = index as f64 / SEGMENTS as f64 * TAU;
        let theta_b = (index + 1) as f64 / SEGMENTS as f64 * TAU;
        canvas.draw(&CanvasLine {
            x1: center_x + radius_x * theta_a.cos(),
            y1: center_y + radius_y * theta_a.sin(),
            x2: center_x + radius_x * theta_b.cos(),
            y2: center_y + radius_y * theta_b.sin(),
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
        let half_width = radius_x * (1.0 - unit_y * unit_y).max(0.0).sqrt();
        let y = center_y + unit_y * radius_y;
        canvas.draw(&CanvasLine {
            x1: center_x - half_width,
            y1: y,
            x2: center_x + half_width,
            y2: y,
            color,
        });
    }
}

fn draw_particle(canvas: &mut Context<'_>, x: f64, y: f64, color: Color, activity: f64) {
    let spread = 0.006 + activity.clamp(0.0, 1.0) * 0.007;
    let coords = [(x, y), (x + spread, y), (x - spread, y), (x, y + spread)];
    let count = if activity >= 0.62 { 4 } else { 1 };
    canvas.draw(&Points {
        coords: &coords[..count],
        color,
    });
}

fn visual_unit(value: u64) -> f64 {
    let mut mixed = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    let bits = mixed ^ (mixed >> 31);
    (bits >> 11) as f64 / ((1_u64 << 53) - 1) as f64
}

fn wrap01(value: f64) -> f64 {
    value.rem_euclid(1.0)
}

fn smoothstep(value: f64) -> f64 {
    let value = value.clamp(0.0, 1.0);
    value * value * (3.0 - 2.0 * value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    fn sample_torrent() -> TorrentDisplayState {
        let mut torrent = TorrentDisplayState {
            peer_discovery_history: vec![1, 3, 0, 2, 1, 4, 0, 2],
            peer_connection_history: vec![0, 1, 2, 0, 1, 2, 1, 1],
            peer_disconnect_history: vec![0, 0, 1, 0, 0, 1, 0, 2],
            ..Default::default()
        };
        torrent.latest_state.peers = (0_usize..6)
            .map(|index| PeerInfo {
                address: format!("10.0.0.{}:6881", index + 1),
                peer_interested: true,
                am_interested: index.is_multiple_of(2),
                bitfield: vec![true, index.is_multiple_of(2), true, false],
                download_speed_bps: 10_000 * (index + 1) as u64,
                upload_speed_bps: 2_000 * index as u64,
                ..Default::default()
            })
            .collect();
        torrent
    }

    fn render_torrent_view(
        torrent: &TorrentDisplayState,
        view: PeerStreamVisualization,
        width: u16,
    ) -> Buffer {
        let ctx = ThemeContext::new(crate::theme::Theme::default(), 0.0);
        let backend = TestBackend::new(width, 9);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                draw_peer_stream_visualization(
                    frame,
                    torrent,
                    Rect::new(0, 0, width, 9),
                    view,
                    &ctx,
                    5.0,
                );
            })
            .expect("render");
        terminal.backend().buffer().clone()
    }

    fn render_view(view: PeerStreamVisualization, width: u16) -> Buffer {
        render_torrent_view(&sample_torrent(), view, width)
    }

    fn sample_torrent_with_older_discovery(count: u64) -> TorrentDisplayState {
        let mut torrent = sample_torrent();
        let mut discovery = vec![0; 16];
        discovery.extend_from_slice(&torrent.peer_discovery_history);
        torrent.peer_discovery_history = discovery;

        let mut connections = vec![0; 16];
        connections.extend_from_slice(&torrent.peer_connection_history);
        torrent.peer_connection_history = connections;

        let mut disconnects = vec![0; 16];
        disconnects.extend_from_slice(&torrent.peer_disconnect_history);
        torrent.peer_disconnect_history = disconnects;
        torrent.peer_discovery_history[2] = count;
        torrent
    }

    fn interior_occupancy(buffer: &Buffer) -> Vec<bool> {
        let area = buffer.area;
        (1..area.height.saturating_sub(1))
            .flat_map(|y| {
                (1..area.width.saturating_sub(1)).map(move |x| {
                    buffer
                        .cell((x, y))
                        .is_some_and(|cell| !matches!(cell.symbol(), " " | "⠀"))
                })
            })
            .collect()
    }

    #[test]
    fn production_snapshot_uses_real_peer_and_event_metrics() {
        let torrent = sample_torrent();
        let data = PeerStreamData::from_torrent(&torrent, 8, 5.0);

        assert_eq!(data.active_count, 6);
        assert_eq!(data.discovered_recent, 13);
        assert_eq!(data.connected_recent, 8);
        assert_eq!(data.disconnected_recent, 4);
        assert!(data
            .peers
            .iter()
            .any(|peer| peer.state == VisualPeerState::Connected));
        assert!(data
            .peers
            .iter()
            .any(|peer| peer.state == VisualPeerState::Discovered));
        assert!(data
            .peers
            .iter()
            .any(|peer| peer.state == VisualPeerState::Leaving));
    }

    #[test]
    fn production_snapshot_falls_back_to_reported_peer_counters() {
        let mut torrent = TorrentDisplayState::default();
        torrent.latest_state.number_of_successfully_connected_peers = 9;
        torrent.latest_state.beneficial_tcp_peer_count = 3;
        torrent.latest_state.beneficial_utp_peer_count = 2;
        torrent.latest_state.download_speed_bps = 800_000;

        let data = PeerStreamData::from_torrent(&torrent, 8, 5.0);

        assert_eq!(data.active_count, 9);
        assert_eq!(data.useful_count, 5);
        assert_eq!(
            data.peers
                .iter()
                .filter(|peer| peer.state == VisualPeerState::Connected)
                .count(),
            9
        );
    }

    #[test]
    fn aggregated_history_preserves_all_event_totals() {
        let torrent = sample_torrent();
        let buckets = aggregate_histories(&torrent, 3, 6, 4);

        assert_eq!(
            buckets.iter().map(|bucket| bucket.discovered).sum::<u64>(),
            13
        );
        assert_eq!(
            buckets.iter().map(|bucket| bucket.connected).sum::<u64>(),
            8
        );
        assert_eq!(
            buckets
                .iter()
                .map(|bucket| bucket.disconnected)
                .sum::<u64>(),
            4
        );
    }

    #[test]
    fn retained_views_preserve_events_outside_the_recent_summary_window() {
        let quiet_history = sample_torrent_with_older_discovery(0);
        let active_history = sample_torrent_with_older_discovery(12);

        for view in PeerStreamVisualization::ALL
            .into_iter()
            .filter(|view| *view != PeerStreamVisualization::Classic)
        {
            assert_ne!(
                render_torrent_view(&quiet_history, view, 80),
                render_torrent_view(&active_history, view, 80),
                "{} hides older peer events",
                view.label()
            );
        }
    }

    #[test]
    fn helix_exchange_vertical_motion_is_continuous_at_cycle_boundary() {
        assert!((oscillating_unit(0.0) - 0.0).abs() < f64::EPSILON);
        assert!((oscillating_unit(0.5) - 1.0).abs() < f64::EPSILON);
        assert!((oscillating_unit(1.0) - 0.0).abs() < f64::EPSILON);
        assert!((oscillating_unit(0.999) - oscillating_unit(0.001)).abs() < 1.0e-9);
    }

    #[test]
    fn helix_exchange_travels_from_right_to_left() {
        let before = helix_exchange_unit(42, 10.0);
        let after = helix_exchange_unit(42, 10.25);
        let circular_delta = (after - before + 0.5).rem_euclid(1.0) - 0.5;

        assert!(circular_delta < 0.0);
    }

    #[test]
    fn mag_slalom_travels_from_right_to_left() {
        let before = mag_slalom_unit(42, 10.0, 0.1);
        let after = mag_slalom_unit(42, 10.25, 0.1);
        let circular_delta = (after - before + 0.5).rem_euclid(1.0) - 0.5;

        assert!(circular_delta < 0.0);
    }

    #[test]
    fn all_imported_views_render_at_production_height() {
        for view in PeerStreamVisualization::ALL
            .into_iter()
            .filter(|view| *view != PeerStreamVisualization::Classic)
        {
            for width in [20, 31, 48, 80] {
                let buffer = render_view(view, width);
                assert!(buffer.content().iter().any(|cell| cell.symbol() != " "));
            }
        }
    }

    #[test]
    fn imported_views_fill_the_wide_peer_stream_and_remain_distinct() {
        let views = PeerStreamVisualization::ALL
            .into_iter()
            .filter(|view| *view != PeerStreamVisualization::Classic)
            .map(|view| (view, interior_occupancy(&render_view(view, 80))))
            .collect::<Vec<_>>();
        let inner_width = 78_usize;
        let inner_height = 7_usize;

        for (view, occupancy) in &views {
            for bin in 0..8 {
                let start = bin * inner_width / 8;
                let end = (bin + 1) * inner_width / 8;
                let has_mark =
                    (0..inner_height).any(|y| (start..end).any(|x| occupancy[y * inner_width + x]));
                assert!(
                    has_mark,
                    "{} leaves horizontal bin {bin} empty",
                    view.label()
                );
            }
        }

        for left in 0..views.len() {
            for right in left + 1..views.len() {
                let (left_view, left_mask) = &views[left];
                let (right_view, right_mask) = &views[right];
                let union = left_mask
                    .iter()
                    .zip(right_mask)
                    .filter(|(left, right)| **left || **right)
                    .count();
                let different = left_mask
                    .iter()
                    .zip(right_mask)
                    .filter(|(left, right)| left != right)
                    .count();
                let distance = different as f64 / union.max(1) as f64;
                assert!(
                    distance >= 0.12,
                    "{} and {} are too similar ({distance:.2})",
                    left_view.label(),
                    right_view.label()
                );
            }
        }
    }
}
