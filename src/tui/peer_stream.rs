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

use crate::app::{PeerInfo, PeerStreamVisualization, TorrentDisplayState};
use crate::theme::ThemeContext;

#[derive(Clone, Copy)]
struct PeerStreamPalette {
    discovered: Color,
    connected: Color,
    disconnected: Color,
    grid: Color,
    text: Color,
    border: Color,
}

impl PeerStreamPalette {
    fn from_theme(ctx: &ThemeContext) -> Self {
        Self {
            discovered: ctx.peer_discovered(),
            connected: ctx.peer_connected(),
            disconnected: ctx.peer_disconnected(),
            grid: ctx.theme.semantic.surface2,
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
    activity: f64,
    quality: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PeerStreamEventCounts {
    pub(crate) connected: u64,
    pub(crate) discovered: u64,
    pub(crate) disconnected: u64,
}

impl PeerStreamEventCounts {
    fn total(self) -> u64 {
        self.connected
            .saturating_add(self.discovered)
            .saturating_add(self.disconnected)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PeerStreamEventSample {
    connected: u64,
    discovered: u64,
    disconnected: u64,
}

fn peer_stream_event_samples(
    torrent: &TorrentDisplayState,
    visible_columns: usize,
) -> Vec<PeerStreamEventSample> {
    let len = torrent
        .peer_discovery_history
        .len()
        .max(torrent.peer_connection_history.len())
        .max(torrent.peer_disconnect_history.len());
    let start = len.saturating_sub(visible_columns);
    let history_value = |history: &[u64], index: usize| {
        let offset = len.saturating_sub(history.len());
        index
            .checked_sub(offset)
            .and_then(|local| history.get(local))
            .copied()
            .unwrap_or(0)
    };

    (start..len)
        .map(|index| PeerStreamEventSample {
            connected: history_value(&torrent.peer_connection_history, index),
            discovered: history_value(&torrent.peer_discovery_history, index),
            disconnected: history_value(&torrent.peer_disconnect_history, index),
        })
        .collect()
}

fn peer_stream_event_counts_from_samples(
    samples: &[PeerStreamEventSample],
) -> PeerStreamEventCounts {
    samples
        .iter()
        .fold(PeerStreamEventCounts::default(), |mut counts, sample| {
            counts.connected = counts.connected.saturating_add(sample.connected);
            counts.discovered = counts.discovered.saturating_add(sample.discovered);
            counts.disconnected = counts.disconnected.saturating_add(sample.disconnected);
            counts
        })
}

pub(crate) fn peer_stream_event_counts(
    torrent: &TorrentDisplayState,
    visible_columns: usize,
) -> PeerStreamEventCounts {
    peer_stream_event_counts_from_samples(&peer_stream_event_samples(torrent, visible_columns))
}

pub(crate) fn should_use_compact_peer_stream_legend(
    available_width: usize,
    counts: PeerStreamEventCounts,
) -> bool {
    let full = format!(
        "Connected: {}  Discovered: {}  Disconnected: {}",
        counts.connected, counts.discovered, counts.disconnected
    );
    full.len() > available_width
}

#[derive(Debug, Default)]
struct PeerStreamData {
    buckets: Vec<HistoryBucket>,
    peers: Vec<VisualPeer>,
    recent_events: Vec<PeerStreamEventSample>,
    event_counts: PeerStreamEventCounts,
    legend_width: usize,
    time: f64,
}

impl PeerStreamData {
    fn from_torrent(
        torrent: &TorrentDisplayState,
        columns: usize,
        legend_width: usize,
        time: f64,
    ) -> Self {
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
        let recent_events = peer_stream_event_samples(torrent, legend_width);
        let event_counts = peer_stream_event_counts_from_samples(&recent_events);
        Self {
            buckets,
            peers,
            recent_events,
            event_counts,
            legend_width,
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
    let legend_width = usize::from(area.width.saturating_sub(2).max(1));
    let data = PeerStreamData::from_torrent(
        torrent,
        canvas_sample_columns(area).clamp(20, 120),
        legend_width,
        time,
    );
    match view {
        PeerStreamVisualization::Classic => {}
        PeerStreamVisualization::HelixExchange => {
            draw_helix_exchange(frame, area, &data, palette, ctx)
        }
    }
}

fn panel_block<'a>(
    data: &PeerStreamData,
    palette: PeerStreamPalette,
    ctx: &ThemeContext,
) -> Block<'a> {
    let title = Line::from(Span::styled(
        " Peer Stream ",
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
    ));
    let counts = data.event_counts;
    let compact = should_use_compact_peer_stream_legend(data.legend_width, counts);
    let connected_label = if compact { "C" } else { "Connected" };
    let discovered_label = if compact { "D" } else { "Discovered" };
    let disconnected_label = if compact { "X" } else { "Disconnected" };
    let legend_style = |count: u64, color: Color| {
        if count > 0 {
            ctx.apply(Style::default().fg(color))
        } else {
            ctx.apply(Style::default().fg(ctx.theme.semantic.surface1))
        }
    };
    let detail = Line::from(vec![
        Span::styled(
            format!("{}:", connected_label),
            legend_style(counts.connected, palette.connected),
        ),
        Span::styled(
            format!(" {} ", counts.connected),
            legend_style(counts.connected, palette.connected).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{}:", discovered_label),
            legend_style(counts.discovered, palette.discovered),
        ),
        Span::styled(
            format!(" {} ", counts.discovered),
            legend_style(counts.discovered, palette.discovered).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{}:", disconnected_label),
            legend_style(counts.disconnected, palette.disconnected),
        ),
        Span::styled(
            format!(" {} ", counts.disconnected),
            legend_style(counts.disconnected, palette.disconnected).add_modifier(Modifier::BOLD),
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

fn lerp(start: f64, end: f64, progress: f64) -> f64 {
    start + (end - start) * progress.clamp(0.0, 1.0)
}

fn draw_helix_exchange(
    frame: &mut Frame,
    area: Rect,
    data: &PeerStreamData,
    palette: PeerStreamPalette,
    ctx: &ThemeContext,
) {
    let (x_bounds, y_bounds) = canvas_bounds(area);
    let event_counts = data.event_counts;
    let event_density = (event_counts.total() as f64 / data.recent_events.len().max(1) as f64)
        .sqrt()
        .clamp(0.0, 1.0);
    let amplitude = 0.32 + event_density * 0.24;
    let turns = 2.0 + event_density * 1.4;
    let scroll_phase = data.time * 0.035;
    let block = panel_block(data, palette, ctx);
    let canvas = Canvas::default()
        .block(block)
        .marker(Marker::Braille)
        .x_bounds(x_bounds)
        .y_bounds(y_bounds)
        .paint(|canvas| {
            let samples = canvas_sample_columns(area).clamp(36, 180);
            let modulation_profile = (0..samples)
                .map(|index| {
                    let unit = index as f64 / samples.saturating_sub(1).max(1) as f64;
                    peer_stream_activity_modulation(&data.recent_events, unit, data.time)
                })
                .collect::<Vec<_>>();
            let modulation_at = |unit: f64| {
                let position =
                    unit.clamp(0.0, 1.0) * modulation_profile.len().saturating_sub(1) as f64;
                let left = position.floor() as usize;
                let right = (left + 1).min(modulation_profile.len().saturating_sub(1));
                let progress = position - left as f64;
                let before = modulation_profile[left];
                let after = modulation_profile[right];
                PeerStreamActivityModulation {
                    strength: lerp(before.strength, after.strength, progress),
                    phase_offset: lerp(before.phase_offset, after.phase_offset, progress),
                }
            };
            let strand_y = |unit: f64, side: f64| {
                let modulation = modulation_at(unit);
                let local_amplitude = amplitude * (0.68 + modulation.strength * 0.32);
                side * local_amplitude
                    * helix_phase(unit, side, turns, scroll_phase + modulation.phase_offset).sin()
            };
            let strand_depth = |unit: f64, side: f64| {
                let modulation = modulation_at(unit);
                helix_depth(unit, side, turns, scroll_phase + modulation.phase_offset)
            };
            for foreground in [false, true] {
                for index in 1..samples {
                    let u1 = (index - 1) as f64 / (samples - 1) as f64;
                    let u2 = index as f64 / (samples - 1) as f64;
                    for side in [1.0, -1.0] {
                        let depth = (strand_depth(u1, side) + strand_depth(u2, side)) * 0.5;
                        if (depth >= 0.0) != foreground {
                            continue;
                        }
                        draw_helix_strand_segment(
                            canvas,
                            plot_x(u1, x_bounds),
                            strand_y(u1, side),
                            plot_x(u2, x_bounds),
                            strand_y(u2, side),
                            palette.grid,
                            helix_depth_scale(depth),
                        );
                    }
                }
            }

            let rung_count = (usize::from(area.width.saturating_sub(2)) / 5).clamp(8, 20);
            for rung in 0..rung_count {
                let unit = rung as f64 / rung_count.saturating_sub(1).max(1) as f64;
                let x = plot_x(unit, x_bounds);
                canvas.draw(&CanvasLine {
                    x1: x,
                    y1: strand_y(unit, 1.0),
                    x2: x,
                    y2: strand_y(unit, -1.0),
                    color: palette.grid,
                });
            }

            for (index, bucket) in data.buckets.iter().copied().enumerate() {
                if bucket.flow <= 0.0 {
                    continue;
                }
                let unit = index as f64 / data.buckets.len().saturating_sub(1).max(1) as f64;
                let x = plot_x(unit, x_bounds);
                canvas.draw(&CanvasLine {
                    x1: x,
                    y1: strand_y(unit, 1.0),
                    x2: x,
                    y2: strand_y(unit, -1.0),
                    color: helix_history_color(bucket, event_counts, palette),
                });
            }

            let carrier_count = helix_metric_carrier_count(event_counts);
            for carrier in 0..carrier_count {
                let carrier_id = 0xca22_1e00_u64 ^ carrier as u64;
                let direction = if carrier.is_multiple_of(2) {
                    HelixDirection::RightToLeft
                } else {
                    HelixDirection::LeftToRight
                };
                let unit = helix_exchange_unit(carrier_id, data.time, direction);
                let x = plot_x(unit, x_bounds);
                let upper = strand_y(unit, 1.0);
                let lower = strand_y(unit, -1.0);
                let color =
                    helix_metric_carrier_color(carrier, carrier_count, event_counts, palette);
                canvas.draw(&CanvasLine {
                    x1: x,
                    y1: upper,
                    x2: x,
                    y2: lower,
                    color,
                });
                draw_helix_particle(canvas, x, upper, color, 0.72, strand_depth(unit, 1.0));
                draw_helix_particle(canvas, x, lower, color, 0.72, strand_depth(unit, -1.0));
            }

            for peer in sampled_peers(data, area.width, 3) {
                let direction = helix_direction_for_peer(peer.id);
                let unit = helix_exchange_unit(peer.id, data.time, direction);
                let x = plot_x(unit, x_bounds);
                let upper = strand_y(unit, 1.0);
                let lower = strand_y(unit, -1.0);
                let upper_depth = strand_depth(unit, 1.0);
                let lower_depth = strand_depth(unit, -1.0);
                let color = helix_peer_color(peer.state, event_counts, palette);
                match peer.state {
                    VisualPeerState::Discovered => {
                        draw_helix_particle(canvas, x, upper, color, peer.activity, upper_depth)
                    }
                    VisualPeerState::Connecting => {
                        let exchange = smoothstep(oscillating_unit(peer.progress));
                        let end = upper + (lower - upper) * exchange;
                        canvas.draw(&CanvasLine {
                            x1: x,
                            y1: upper,
                            x2: x,
                            y2: end,
                            color,
                        });
                        draw_helix_particle(
                            canvas,
                            x,
                            end,
                            color,
                            peer.activity,
                            lerp(upper_depth, lower_depth, exchange),
                        );
                    }
                    VisualPeerState::Connected => {
                        canvas.draw(&CanvasLine {
                            x1: x,
                            y1: upper,
                            x2: x,
                            y2: lower,
                            color,
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
                            color,
                        });
                        canvas.draw(&CanvasLine {
                            x1: x,
                            y1: lower,
                            x2: x,
                            y2: midpoint - gap * direction,
                            color,
                        });
                    }
                }
            }
        });
    frame.render_widget(canvas, area);
}

const HELIX_STROKE_HALF_WIDTH: f64 = 0.035;
const PEER_STREAM_ACTIVITY_HALF_LIFE_SECONDS: f64 = 8.0;
const PEER_STREAM_ACTIVITY_SPREAD: f64 = 0.18;
const PEER_STREAM_ACTIVITY_PHASE_TURNS: f64 = 0.14;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HelixDirection {
    RightToLeft,
    LeftToRight,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PeerStreamActivityModulation {
    strength: f64,
    phase_offset: f64,
}

fn peer_stream_event_recency(index: usize, sample_count: usize) -> f64 {
    let newest = sample_count.saturating_sub(1);
    let age_seconds = newest.saturating_sub(index) as f64;
    0.5_f64.powf(age_seconds / PEER_STREAM_ACTIVITY_HALF_LIFE_SECONDS)
}

fn peer_stream_activity_modulation(
    samples: &[PeerStreamEventSample],
    unit: f64,
    time: f64,
) -> PeerStreamActivityModulation {
    if samples.is_empty() {
        return PeerStreamActivityModulation::default();
    }

    let newest = samples.len().saturating_sub(1);
    let denominator = newest.max(1) as f64;
    let mut strength = 0.0;
    let mut phase = 0.0;
    for (index, sample) in samples.iter().copied().enumerate() {
        let total = sample
            .connected
            .saturating_add(sample.discovered)
            .saturating_add(sample.disconnected);
        if total == 0 {
            continue;
        }

        let event_unit = index as f64 / denominator;
        let recency = peer_stream_event_recency(index, samples.len());
        let distance = (unit.clamp(0.0, 1.0) - event_unit) / PEER_STREAM_ACTIVITY_SPREAD;
        let proximity = (-0.5 * distance * distance).exp();
        let magnitude = ((total as f64).ln_1p() / 4.0_f64.ln()).clamp(0.0, 1.0);
        let influence = recency * proximity * magnitude;

        let connected_weight = (sample.connected as f64).sqrt();
        let discovered_weight = (sample.discovered as f64).sqrt();
        let disconnected_weight = (sample.disconnected as f64).sqrt();
        let weight = connected_weight + discovered_weight + disconnected_weight;
        let metric_wave = if weight > 0.0 {
            (connected_weight * (TAU * (time * 0.11 + event_unit * 0.40)).sin()
                + discovered_weight * (TAU * (time * 0.16 + event_unit * 0.25 + 0.33)).sin()
                + disconnected_weight * (TAU * (-time * 0.13 + event_unit * 0.45 + 0.66)).sin())
                / weight
        } else {
            0.0
        };
        strength += influence;
        phase += influence * metric_wave;
    }

    PeerStreamActivityModulation {
        strength: strength.clamp(0.0, 1.0),
        phase_offset: phase.clamp(-1.0, 1.0) * PEER_STREAM_ACTIVITY_PHASE_TURNS,
    }
}

fn helix_phase(unit: f64, side: f64, turns: f64, scroll_phase: f64) -> f64 {
    let phase_direction = if side > 0.0 { 1.0 } else { -1.0 };
    TAU * (turns * unit + scroll_phase * phase_direction)
}

fn helix_depth(unit: f64, side: f64, turns: f64, scroll_phase: f64) -> f64 {
    side * helix_phase(unit, side, turns, scroll_phase).cos()
}

fn helix_depth_scale(depth: f64) -> f64 {
    0.55 + (depth.clamp(-1.0, 1.0) + 1.0) * 0.45
}

fn helix_metric_carrier_count(counts: PeerStreamEventCounts) -> usize {
    let total = counts.total();
    if total == 0 {
        return 0;
    }
    total.min(10) as usize
}

fn helix_metric_carrier_color(
    slot: usize,
    slot_count: usize,
    counts: PeerStreamEventCounts,
    palette: PeerStreamPalette,
) -> Color {
    let total = counts.total();
    if total == 0 || slot_count == 0 {
        return palette.grid;
    }

    let target = (slot as u128 * 2 + 1) * u128::from(total);
    let denominator = slot_count as u128 * 2;
    let connected_end = denominator * u128::from(counts.connected);
    let discovered_end = connected_end + denominator * u128::from(counts.discovered);
    if target < connected_end {
        palette.connected
    } else if target < discovered_end {
        palette.discovered
    } else {
        palette.disconnected
    }
}

fn helix_history_color(
    bucket: HistoryBucket,
    counts: PeerStreamEventCounts,
    palette: PeerStreamPalette,
) -> Color {
    let mut visible = bucket;
    if counts.connected == 0 {
        visible.connected = 0;
    }
    if counts.discovered == 0 {
        visible.discovered = 0;
    }
    if counts.disconnected == 0 {
        visible.disconnected = 0;
    }
    dominant_history_color(visible, palette)
}

fn helix_peer_color(
    state: VisualPeerState,
    counts: PeerStreamEventCounts,
    palette: PeerStreamPalette,
) -> Color {
    match state {
        VisualPeerState::Discovered if counts.discovered > 0 => palette.discovered,
        VisualPeerState::Connected if counts.connected > 0 => palette.connected,
        VisualPeerState::Leaving if counts.disconnected > 0 => palette.disconnected,
        VisualPeerState::Discovered
        | VisualPeerState::Connecting
        | VisualPeerState::Connected
        | VisualPeerState::Leaving => palette.grid,
    }
}

fn draw_helix_strand_segment(
    canvas: &mut Context<'_>,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    color: Color,
    depth_scale: f64,
) {
    let half_width = HELIX_STROKE_HALF_WIDTH * depth_scale;
    for offset in [-half_width, 0.0, half_width] {
        canvas.draw(&CanvasLine {
            x1,
            y1: y1 + offset,
            x2,
            y2: y2 + offset,
            color,
        });
    }
}

fn draw_helix_particle(
    canvas: &mut Context<'_>,
    x: f64,
    y: f64,
    color: Color,
    activity: f64,
    depth: f64,
) {
    let scale = helix_depth_scale(depth);
    let spread = (0.008 + activity.clamp(0.0, 1.0) * 0.010) * scale;
    let coords = [
        (x, y),
        (x + spread, y),
        (x - spread, y),
        (x, y + spread),
        (x, y - spread),
        (x + spread, y + spread),
        (x - spread, y + spread),
        (x + spread, y - spread),
        (x - spread, y - spread),
    ];
    let count = if scale >= 1.20 {
        9
    } else if scale >= 0.82 {
        5
    } else {
        1
    };
    canvas.draw(&Points {
        coords: &coords[..count],
        color,
    });
}

fn helix_direction_for_peer(peer_id: u64) -> HelixDirection {
    if peer_id.is_multiple_of(2) {
        HelixDirection::RightToLeft
    } else {
        HelixDirection::LeftToRight
    }
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

fn dominant_history_color(bucket: HistoryBucket, palette: PeerStreamPalette) -> Color {
    if bucket.disconnected >= bucket.connected.max(bucket.discovered) && bucket.disconnected > 0 {
        palette.disconnected
    } else if bucket.connected >= bucket.discovered && bucket.connected > 0 {
        palette.connected
    } else if bucket.discovered > 0 {
        palette.discovered
    } else {
        palette.grid
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

fn plot_x(unit: f64, bounds: [f64; 2]) -> f64 {
    bounds[0] + (bounds[1] - bounds[0]) * unit.clamp(0.0, 1.0)
}

fn oscillating_unit(unit: f64) -> f64 {
    0.5 - 0.5 * (TAU * wrap01(unit)).cos()
}

fn helix_exchange_unit(peer_id: u64, time: f64, direction: HelixDirection) -> f64 {
    let travel = time * 0.032;
    match direction {
        HelixDirection::RightToLeft => wrap01(visual_unit(peer_id) - travel),
        HelixDirection::LeftToRight => wrap01(visual_unit(peer_id) + travel),
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
                bitfield: vec![true, index.is_multiple_of(2), true, false].into(),
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
        render_torrent_view_at(torrent, view, width, 5.0)
    }

    fn render_torrent_view_at(
        torrent: &TorrentDisplayState,
        view: PeerStreamVisualization,
        width: u16,
        time: f64,
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
                    time,
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
        let data = PeerStreamData::from_torrent(&torrent, 8, 8, 5.0);

        assert_eq!(
            data.event_counts,
            PeerStreamEventCounts {
                connected: 8,
                discovered: 13,
                disconnected: 4,
            }
        );
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

        let data = PeerStreamData::from_torrent(&torrent, 8, 8, 5.0);

        assert_eq!(data.buckets[0].useful, 5.0);
        assert_eq!(
            data.peers
                .iter()
                .filter(|peer| peer.state == VisualPeerState::Connected)
                .count(),
            9
        );
    }

    #[test]
    fn peer_stream_event_counts_match_the_classic_visible_window() {
        let torrent = sample_torrent();

        assert_eq!(
            peer_stream_event_counts(&torrent, 3),
            PeerStreamEventCounts {
                connected: 4,
                discovered: 6,
                disconnected: 3,
            }
        );
    }

    #[test]
    fn alternate_views_use_the_classic_legend_without_a_visualization_name() {
        for view in PeerStreamVisualization::ALL
            .into_iter()
            .filter(|view| *view != PeerStreamVisualization::Classic)
        {
            let buffer = render_view(view, 120);
            let title = (0..buffer.area.width)
                .filter_map(|x| buffer.cell((x, 0)).map(|cell| cell.symbol()))
                .collect::<String>();

            assert!(title.contains("Peer Stream"));
            assert!(title.contains("Connected: 8"));
            assert!(title.contains("Discovered: 13"));
            assert!(title.contains("Disconnected: 4"));
            assert!(!title.contains("A6 U"));
            assert!(!title.contains("Helix Exchange"));
        }
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
                "{view:?} hides older peer events"
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
    fn helix_exchange_scales_foreground_larger_than_background() {
        let background = helix_depth_scale(-1.0);
        let midpoint = helix_depth_scale(0.0);
        let foreground = helix_depth_scale(1.0);

        assert!(background < midpoint);
        assert!(midpoint < foreground);
        assert!((background - 0.55).abs() < 1.0e-12);
        assert!((foreground - 1.45).abs() < 1.0e-12);
    }

    #[test]
    fn helix_exchange_opposite_strands_have_opposite_depth() {
        let upper_depth = helix_depth(0.0, 1.0, 2.0, 0.0);
        let lower_depth = helix_depth(0.0, -1.0, 2.0, 0.0);

        assert_eq!(upper_depth, 1.0);
        assert_eq!(lower_depth, -1.0);
    }

    #[test]
    fn peer_stream_recent_activity_moves_more_than_old_activity() {
        let event = PeerStreamEventSample {
            connected: 1,
            ..Default::default()
        };
        let mut old_samples = vec![PeerStreamEventSample::default(); 9];
        old_samples[0] = event;
        let mut recent_samples = vec![PeerStreamEventSample::default(); 9];
        recent_samples[8] = event;

        let old = peer_stream_activity_modulation(&old_samples, 0.0, 5.0);
        let recent = peer_stream_activity_modulation(&recent_samples, 1.0, 5.0);

        assert!(recent.strength > old.strength);
        assert!((recent.strength - 0.5).abs() < 1.0e-12);
        assert!((old.strength - 0.25).abs() < 1.0e-12);
    }

    #[test]
    fn peer_stream_recent_activity_phase_changes_with_current_time() {
        let samples = vec![PeerStreamEventSample {
            connected: 1,
            discovered: 1,
            disconnected: 1,
        }];

        let before = peer_stream_activity_modulation(&samples, 0.0, 5.0);
        let after = peer_stream_activity_modulation(&samples, 0.0, 5.25);

        assert!((after.phase_offset - before.phase_offset).abs() > 1.0e-4);
    }

    #[test]
    fn helix_exchange_carrier_colors_follow_classic_metrics() {
        let ctx = ThemeContext::new(crate::theme::Theme::default(), 0.0);
        let palette = PeerStreamPalette::from_theme(&ctx);
        let counts = PeerStreamEventCounts {
            connected: 2,
            discovered: 1,
            disconnected: 1,
        };

        let carrier_count = helix_metric_carrier_count(counts);
        let colors = (0..carrier_count)
            .map(|slot| helix_metric_carrier_color(slot, carrier_count, counts, palette))
            .collect::<Vec<_>>();

        assert_eq!(carrier_count, 4);
        assert_eq!(
            colors,
            vec![
                palette.connected,
                palette.connected,
                palette.discovered,
                palette.disconnected,
            ]
        );
    }

    #[test]
    fn helix_exchange_is_neutral_when_classic_metrics_are_zero() {
        let ctx = ThemeContext::new(crate::theme::Theme::default(), 0.0);
        let buffer = render_torrent_view(
            &TorrentDisplayState::default(),
            PeerStreamVisualization::HelixExchange,
            80,
        );
        let buffer_ref = &buffer;
        let interior_colors = (1..buffer.area.height.saturating_sub(1))
            .flat_map(|y| {
                (1..buffer.area.width.saturating_sub(1))
                    .filter_map(move |x| buffer_ref.cell((x, y)).map(|cell| cell.fg))
            })
            .collect::<Vec<_>>();

        assert!(interior_colors.contains(&ctx.theme.semantic.surface2));
        assert!(!interior_colors.contains(&ctx.peer_connected()));
        assert!(!interior_colors.contains(&ctx.peer_discovered()));
        assert!(!interior_colors.contains(&ctx.peer_disconnected()));
    }

    #[test]
    fn helix_exchange_has_stable_counterflow() {
        assert_eq!(helix_direction_for_peer(42), HelixDirection::RightToLeft);
        assert_eq!(helix_direction_for_peer(43), HelixDirection::LeftToRight);

        let right_to_left_before = helix_exchange_unit(42, 10.0, HelixDirection::RightToLeft);
        let right_to_left_after = helix_exchange_unit(42, 10.25, HelixDirection::RightToLeft);
        let right_to_left_delta =
            (right_to_left_after - right_to_left_before + 0.5).rem_euclid(1.0) - 0.5;

        let left_to_right_before = helix_exchange_unit(43, 10.0, HelixDirection::LeftToRight);
        let left_to_right_after = helix_exchange_unit(43, 10.25, HelixDirection::LeftToRight);
        let left_to_right_delta =
            (left_to_right_after - left_to_right_before + 0.5).rem_euclid(1.0) - 0.5;

        assert!(right_to_left_delta < 0.0);
        assert!(left_to_right_delta > 0.0);
    }

    #[test]
    fn helix_exchange_has_visible_frame_motion() {
        let torrent = sample_torrent();
        let before =
            render_torrent_view_at(&torrent, PeerStreamVisualization::HelixExchange, 80, 5.0);
        let after =
            render_torrent_view_at(&torrent, PeerStreamVisualization::HelixExchange, 80, 5.25);
        let changed_cells = before
            .content()
            .iter()
            .zip(after.content())
            .filter(|(before, after)| before != after)
            .count();

        assert!(changed_cells >= 10, "only {changed_cells} cells changed");
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
            let occupied_bins = (0..8)
                .map(|bin| {
                    let start = bin * inner_width / 8;
                    let end = (bin + 1) * inner_width / 8;
                    (0..inner_height).any(|y| (start..end).any(|x| occupancy[y * inner_width + x]))
                })
                .collect::<Vec<_>>();
            for (bin, has_mark) in occupied_bins.into_iter().enumerate() {
                assert!(has_mark, "{view:?} leaves horizontal bin {bin} empty");
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
                    "{left_view:?} and {right_view:?} are too similar ({distance:.2})"
                );
            }
        }
    }
}
