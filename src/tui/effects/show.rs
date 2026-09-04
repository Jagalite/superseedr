// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Show's deterministic score: paired color fields, geometric tracers, background
//! texture and typography share one clock. Occupied text and protected surfaces
//! retain their content; texture is confined to clear space between UI elements.

use std::f64::consts::{PI, TAU};

use ratatui::{
    buffer::{Buffer, CellDiffOption},
    layout::Rect,
    style::{Color, Modifier},
    text::Span,
};

use crate::theme::{blend_colors, color_to_rgb, ThemeContext};

const STEP_SECONDS: f64 = 0.4;
const SCENE_STEPS: usize = 32;
const SCENE_SECONDS: f64 = STEP_SECONDS * SCENE_STEPS as f64;
const CHASE: [f64; 8] = [0.0, 1.0, 2.0, 3.0, 3.0, 2.0, 1.0, 0.0];

type Rgb = (u8, u8, u8);

// Saturated light sources; text gets a separate white lift for legibility.
const CYAN_PINK: [Rgb; 2] = [(12, 235, 255), (255, 40, 177)];
const ACID_VIOLET: [Rgb; 2] = [(202, 255, 24), (159, 70, 255)];
const ICE_BLUE: [Rgb; 2] = [(30, 246, 255), (65, 105, 255)];
const AMBER_ROSE: [Rgb; 2] = [(255, 163, 15), (255, 49, 108)];
const MINT_LILAC: [Rgb; 2] = [(35, 255, 140), (199, 88, 255)];

#[derive(Clone, Copy, Debug)]
enum Pattern {
    PrismChase,
    PulseTunnel,
    MirrorShards,
    EchoChamber,
    CheckerSwitch,
    SpiralDrive,
    WaveInterference,
    Honeycomb,
    RadarSweep,
    DiamondLattice,
    SignalRain,
    WarpGrid,
    SineRibbons,
    MoireWeave,
    StarAperture,
    DiamondEcho,
    BinaryWeave,
    Pinwheel,
    RipplePool,
    CircuitTraces,
    ZigzagLadder,
    Hourglass,
    WovenRings,
    Rosette,
    Crosshatch,
    SteppedTerraces,
    PolarChecker,
    SplitScan,
    OrbitInterference,
    ShutterFan,
}

#[derive(Clone, Copy, Debug)]
enum Pulse {
    Snap,
    Double,
    Flicker,
    Swell,
    Hold,
}

#[derive(Clone, Copy, Debug)]
enum FontFlow {
    Unified,
    Alternating,
    FollowField,
    ColumnChase,
    Split,
    Radial,
}

#[derive(Clone, Copy)]
struct Scene {
    pattern: Pattern,
    palette: [Rgb; 2],
    pulse: Pulse,
    font: FontFlow,
}

const fn scene(pattern: Pattern, palette: [Rgb; 2], pulse: Pulse, font: FontFlow) -> Scene {
    Scene {
        pattern,
        palette,
        pulse,
        font,
    }
}

// Ordering alternates hard geometric hits with calmer, more fluid phrases.
const SCENES: [Scene; 30] = [
    scene(
        Pattern::PrismChase,
        CYAN_PINK,
        Pulse::Double,
        FontFlow::FollowField,
    ),
    scene(
        Pattern::PulseTunnel,
        ACID_VIOLET,
        Pulse::Snap,
        FontFlow::Radial,
    ),
    scene(
        Pattern::MirrorShards,
        ICE_BLUE,
        Pulse::Flicker,
        FontFlow::Alternating,
    ),
    scene(
        Pattern::EchoChamber,
        CYAN_PINK,
        Pulse::Double,
        FontFlow::Radial,
    ),
    scene(
        Pattern::CheckerSwitch,
        ACID_VIOLET,
        Pulse::Hold,
        FontFlow::ColumnChase,
    ),
    scene(
        Pattern::SpiralDrive,
        MINT_LILAC,
        Pulse::Swell,
        FontFlow::FollowField,
    ),
    scene(
        Pattern::WaveInterference,
        ICE_BLUE,
        Pulse::Swell,
        FontFlow::Unified,
    ),
    scene(
        Pattern::Honeycomb,
        AMBER_ROSE,
        Pulse::Double,
        FontFlow::FollowField,
    ),
    scene(
        Pattern::RadarSweep,
        MINT_LILAC,
        Pulse::Snap,
        FontFlow::Radial,
    ),
    scene(
        Pattern::DiamondLattice,
        CYAN_PINK,
        Pulse::Flicker,
        FontFlow::Alternating,
    ),
    scene(
        Pattern::SignalRain,
        ICE_BLUE,
        Pulse::Double,
        FontFlow::ColumnChase,
    ),
    scene(Pattern::WarpGrid, ACID_VIOLET, Pulse::Snap, FontFlow::Split),
    scene(
        Pattern::SineRibbons,
        AMBER_ROSE,
        Pulse::Swell,
        FontFlow::FollowField,
    ),
    scene(
        Pattern::MoireWeave,
        MINT_LILAC,
        Pulse::Swell,
        FontFlow::Alternating,
    ),
    scene(
        Pattern::StarAperture,
        CYAN_PINK,
        Pulse::Snap,
        FontFlow::Radial,
    ),
    scene(
        Pattern::DiamondEcho,
        ACID_VIOLET,
        Pulse::Double,
        FontFlow::Radial,
    ),
    scene(
        Pattern::BinaryWeave,
        ICE_BLUE,
        Pulse::Flicker,
        FontFlow::ColumnChase,
    ),
    scene(
        Pattern::Pinwheel,
        AMBER_ROSE,
        Pulse::Hold,
        FontFlow::FollowField,
    ),
    scene(
        Pattern::RipplePool,
        MINT_LILAC,
        Pulse::Swell,
        FontFlow::Unified,
    ),
    scene(
        Pattern::CircuitTraces,
        CYAN_PINK,
        Pulse::Flicker,
        FontFlow::FollowField,
    ),
    scene(
        Pattern::ZigzagLadder,
        ACID_VIOLET,
        Pulse::Double,
        FontFlow::Alternating,
    ),
    scene(Pattern::Hourglass, ICE_BLUE, Pulse::Snap, FontFlow::Split),
    scene(
        Pattern::WovenRings,
        AMBER_ROSE,
        Pulse::Swell,
        FontFlow::FollowField,
    ),
    scene(
        Pattern::Rosette,
        MINT_LILAC,
        Pulse::Double,
        FontFlow::FollowField,
    ),
    scene(
        Pattern::Crosshatch,
        CYAN_PINK,
        Pulse::Flicker,
        FontFlow::Alternating,
    ),
    scene(
        Pattern::SteppedTerraces,
        ACID_VIOLET,
        Pulse::Hold,
        FontFlow::FollowField,
    ),
    scene(
        Pattern::PolarChecker,
        ICE_BLUE,
        Pulse::Double,
        FontFlow::Radial,
    ),
    scene(Pattern::SplitScan, AMBER_ROSE, Pulse::Snap, FontFlow::Split),
    scene(
        Pattern::OrbitInterference,
        MINT_LILAC,
        Pulse::Swell,
        FontFlow::Radial,
    ),
    scene(
        Pattern::ShutterFan,
        CYAN_PINK,
        Pulse::Flicker,
        FontFlow::Alternating,
    ),
];

#[derive(Clone, Copy)]
struct Score {
    scene_index: usize,
    step: usize,
    travel: f64,
    turn: f64,
    energy: f64,
    phrase: usize,
    phase: f64,
    gain: f64,
}

impl Score {
    fn at(time: f64) -> Self {
        // Bound long-running clocks and make invalid/rewound timestamps harmless.
        let time = if time.is_finite() { time.max(0.0) } else { 0.0 };
        let time = time.rem_euclid(SCENE_SECONDS * SCENES.len() as f64);
        let scene_index = (time / SCENE_SECONDS) as usize;
        let position = (time / STEP_SECONDS).rem_euclid(8.0);
        let step = position.floor() as usize;
        let phase = position.fract();
        let scene = SCENES[scene_index];
        let phrase = ((time / STEP_SECONDS) as usize / 8) % 4;
        // Build, peak, break, return: each eight-step phrase changes the layer
        // balance. The break strips the field back to its structural tracers.
        let gain = [0.70, 1.0, 0.28, 0.90][phrase];
        let energy = envelope(scene.pulse, phase);
        Self {
            scene_index,
            step,
            travel: position / 8.0,
            turn: position / 8.0 * TAU,
            energy,
            phrase,
            phase,
            gain,
        }
    }

    fn sample(self, x: f64, y: f64, aspect: f64) -> Sample {
        let scene = SCENES[self.scene_index];
        let dx = (x - 0.5) * aspect;
        let dy = y - 0.5;
        let radius = dx.hypot(dy);
        let angle = dy.atan2(dx);
        let t = self.travel;
        let turn = self.turn;
        let coordinate = match scene.pattern {
            Pattern::PrismChase => x * 8.0 + dy.abs() * 8.0,
            Pattern::PulseTunnel => radius * 15.0 - t * 8.0,
            Pattern::MirrorShards => (dx.abs() + dy.abs()) * 13.0 + angle.abs() * 2.0,
            Pattern::EchoChamber => dx.abs().max(dy.abs()) * 18.0 - t * 4.0,
            Pattern::CheckerSwitch => (x * 12.0).floor() + (y * 8.0).floor() * 2.0,
            Pattern::SpiralDrive => angle * 8.0 / PI + radius * 22.0 - t * 8.0,
            Pattern::WaveInterference => {
                ((dx * 23.0 + turn).sin() + (dy * 26.0 - turn).sin()) * 3.0
            }
            Pattern::Honeycomb => hex_band(dx, dy),
            Pattern::RadarSweep => (angle - turn) * 8.0 / PI + (radius * 12.0).floor() * 0.5,
            Pattern::DiamondLattice => {
                ((dx + dy) * 10.0).floor() + ((dx - dy) * 10.0).floor() * 2.0
            }
            Pattern::SignalRain => {
                (x * 20.0).floor().rem_euclid(4.0) + (y * 16.0 - t * 16.0).floor()
            }
            Pattern::WarpGrid => {
                let depth = dy.abs().max(0.055);
                (dx / depth * 3.0).floor() + (1.0 / depth - t * 8.0).floor()
            }
            Pattern::SineRibbons => dy * 20.0 + (dx * 9.0 + turn).sin() * 3.0,
            Pattern::MoireWeave => (dx * 31.0 + turn).sin() * (dy * 29.0 - turn).cos() * 7.0,
            Pattern::StarAperture => radius * (18.0 + (angle * 5.0 + turn).cos() * 6.0) - t * 4.0,
            Pattern::DiamondEcho => (dx.abs() + dy.abs()) * 24.0 - t * 12.0,
            Pattern::BinaryWeave => ((x * 24.0).floor() as i32 ^ (y * 16.0).floor() as i32) as f64,
            Pattern::Pinwheel => (angle + turn) * 12.0 / TAU + radius * 4.0,
            Pattern::RipplePool => {
                let a = (dx - 0.24).hypot(dy) * 24.0 - turn;
                let b = (dx + 0.24).hypot(dy) * 24.0 - turn;
                (a.sin() + b.sin()) * 3.5
            }
            Pattern::CircuitTraces => {
                let column = (x * 16.0).floor();
                y * 22.0 + column.rem_euclid(3.0) * 2.0 + (dx * 16.0).fract().abs() * 0.6
            }
            Pattern::ZigzagLadder => dy * 24.0 + ((dx * 6.0 + t).rem_euclid(2.0) - 1.0).abs() * 8.0,
            Pattern::Hourglass => dx / (dy.abs() + 0.07) * 4.0 + dy * 8.0 - t * 8.0,
            Pattern::WovenRings => ((radius * 28.0 - turn).sin() + (angle * 8.0).cos()) * 3.0,
            Pattern::Rosette => radius * 24.0 + (angle * 7.0 - turn).sin() * 4.0,
            Pattern::Crosshatch => ((dx * 14.0 + t * 4.0).floor().rem_euclid(4.0)
                + (dy * 18.0 - t * 4.0).floor().rem_euclid(4.0))
            .rem_euclid(4.0),
            Pattern::SteppedTerraces => {
                (dx * 6.0 + (dy * 7.0 + turn).sin()).floor() * 2.0 + (dy * 16.0).floor()
            }
            Pattern::PolarChecker => {
                (angle * 12.0 / TAU).floor() + (radius * 18.0 - t * 4.0).floor() * 2.0
            }
            Pattern::SplitScan => {
                if dx < 0.0 {
                    y * 24.0 - t * 8.0
                } else {
                    (1.0 - y) * 24.0 + t * 8.0
                }
            }
            Pattern::OrbitInterference => {
                let cx = turn.cos() * 0.22;
                let cy = turn.sin() * 0.22;
                ((dx - cx).hypot(dy - cy) * 25.0).sin() * 3.0
                    + ((dx + cx).hypot(dy + cy) * 19.0).cos() * 3.0
            }
            Pattern::ShutterFan => ((angle - turn) * 16.0 / TAU).floor() + radius * 6.0,
        };
        let band = coordinate.floor().rem_euclid(4.0);
        let active = band == CHASE[self.step];
        let pair = (self.step / 2 + self.phrase / 2) % 2;
        let color = scene.palette[pair];
        let counter_color = scene.palette[1 - pair];
        let echo = band == CHASE[(self.step + 7) % 8];
        let (structure, glyph) = tracer(scene.pattern, dx, dy, radius, angle, t, turn);
        // The tracer has its own counter-chase, aligned to the same step grid.
        let counter = 0.35 + 0.65 * envelope(Pulse::Double, (self.phase + 0.5).rem_euclid(1.0));
        let trace = structure.min(1.0) * counter * [0.65, 1.0, 0.40, 1.0][self.phrase];
        let light = if active { self.energy } else { 0.0 };
        let text_color = match scene.font {
            FontFlow::Unified => color,
            FontFlow::Alternating => scene.palette[(pair + (y * 8.0).floor() as usize) % 2],
            FontFlow::FollowField => scene.palette[(pair + usize::from(!active)) % 2],
            FontFlow::ColumnChase => scene.palette[(pair + (x * 8.0) as usize) % 2],
            FontFlow::Split => scene.palette[pair ^ usize::from(x > 0.5)],
            FontFlow::Radial => scene.palette[(pair + (radius * 12.0) as usize) % 2],
        };
        let font_hit = match scene.font {
            FontFlow::Unified => self.energy * 0.65 + light * 0.35,
            FontFlow::Alternating => {
                self.energy
                    * f64::from(u8::from(((y * 8.0) as usize + self.step).is_multiple_of(2)))
            }
            FontFlow::FollowField => light,
            FontFlow::ColumnChase => {
                self.energy * f64::from(u8::from((x * 8.0) as usize % 4 == self.step % 4))
            }
            FontFlow::Split => {
                self.energy * f64::from(u8::from((x > 0.5) == self.step.is_multiple_of(2)))
            }
            FontFlow::Radial => {
                self.energy * f64::from(u8::from((radius * 12.0) as usize % 4 == self.step % 4))
            }
        };
        Sample {
            color,
            counter_color,
            text_color,
            light,
            wash: (light + if echo { (1.0 - self.phase) * 0.25 } else { 0.0 }) * self.gain,
            trace,
            glyph,
            font_hit: (font_hit * self.gain + trace * 0.22).min(1.0),
        }
    }
}

/// Thin structural geometry supports each scene's broad color field. These are
/// sampled shapes, not independently moving particles or a simulated data chart.
fn tracer(
    pattern: Pattern,
    x: f64,
    y: f64,
    r: f64,
    a: f64,
    t: f64,
    turn: f64,
) -> (f64, &'static str) {
    let line = |v: f64, width: f64| (1.0 - (v - v.round()).abs() / width).max(0.0);
    let diagonal = if x * y > 0.0 { "╲" } else { "╱" };
    let tangent = if y.abs() > x.abs() { "─" } else { "│" };
    match pattern {
        Pattern::PrismChase | Pattern::MirrorShards | Pattern::DiamondLattice => (
            line((x.abs() + y.abs()) * 7.0 + t * 2.0, 0.15)
                .max(line((x.abs() - y.abs()) * 5.0 - t, 0.09) * 0.65),
            diagonal,
        ),
        Pattern::PulseTunnel | Pattern::DiamondEcho | Pattern::EchoChamber => {
            let depth = match pattern {
                Pattern::DiamondEcho => x.abs() + y.abs(),
                Pattern::EchoChamber => x.abs().max(y.abs()),
                _ => r,
            };
            (
                line(depth * 9.0 + t * 2.0, 0.18) * (0.65 + 0.35 * (a * 3.0 - turn).cos()),
                tangent,
            )
        }
        Pattern::CheckerSwitch | Pattern::BinaryWeave => (
            line(x * 12.0, 0.10).max(line(y * 12.0, 0.12))
                * (0.5 + 0.5 * (turn + x * 8.0 - y * 8.0).cos()),
            "▪",
        ),
        Pattern::Honeycomb => {
            let (_, _, hx, hy) = hex_cell(x, y);
            let distance = hx
                .abs()
                .max(hx.abs() * 0.5 + hy.abs() * 3.0_f64.sqrt() / 2.0)
                / (0.065 * 3.0_f64.sqrt() / 2.0);
            (
                (1.0 - (1.0 - distance).abs() / 0.22).max(0.0),
                if hy.abs() < 0.02 {
                    "│"
                } else if hx * hy > 0.0 {
                    "╱"
                } else {
                    "╲"
                },
            )
        }
        Pattern::SpiralDrive | Pattern::StarAperture | Pattern::Pinwheel | Pattern::Rosette => (
            line(a * 3.0 / PI - r * 5.0 - t * 2.0, 0.14).max(line(r * 8.0 + t, 0.08) * 0.5),
            diagonal,
        ),
        Pattern::RadarSweep | Pattern::PolarChecker | Pattern::ShutterFan => (
            line((a + turn) * 6.0 / PI, 0.12).max(line(r * 10.0 + t * 2.0, 0.10) * 0.6),
            tangent,
        ),
        Pattern::WaveInterference | Pattern::SineRibbons | Pattern::MoireWeave => (
            line(y * 10.0 + (x * 8.0 - turn).sin() * 1.5, 0.20)
                .max(line(x * 9.0 + (y * 6.0 + turn).sin(), 0.09) * 0.5),
            "~",
        ),
        Pattern::SignalRain | Pattern::CircuitTraces => {
            let column = (x * 18.0).round();
            let head = (y * 8.0 + t * 8.0 + column.rem_euclid(4.0)).rem_euclid(4.0);
            (
                line(x * 18.0, 0.13) * (1.0 - head / 4.0)
                    + line(y * 12.0 + column.rem_euclid(3.0), 0.10) * 0.25,
                if head < 0.35 { "┼" } else { "│" },
            )
        }
        Pattern::WarpGrid | Pattern::Hourglass => {
            let depth = y.abs().max(0.06);
            (
                line(x / depth * 2.0, 0.11).max(line(0.6 / depth + t * 4.0, 0.16)),
                diagonal,
            )
        }
        Pattern::RipplePool | Pattern::OrbitInterference | Pattern::WovenRings => (
            line(
                (x - turn.cos() * 0.2).hypot(y - turn.sin() * 0.2) * 10.0 + t * 3.0,
                0.20,
            ),
            "·",
        ),
        Pattern::ZigzagLadder | Pattern::SteppedTerraces => (
            line(
                y * 12.0 + ((x * 5.0 - t).rem_euclid(2.0) - 1.0).abs() * 3.0,
                0.18,
            ),
            diagonal,
        ),
        Pattern::Crosshatch => (
            line((x + y) * 10.0 + t * 3.0, 0.13).max(line((x - y) * 10.0 - t * 3.0, 0.13)),
            "╳",
        ),
        Pattern::SplitScan => (
            line(y * 10.0 + if x < 0.0 { t * 4.0 } else { -t * 4.0 }, 0.18)
                * (0.6 + 0.4 * (x * 12.0 + turn).cos()),
            "─",
        ),
    }
}

fn envelope(pulse: Pulse, phase: f64) -> f64 {
    let hit = |start: f64, duration: f64| {
        let age = phase - start;
        if age < 0.0 || age >= duration {
            return 0.0;
        }
        (age / 0.08).min(1.0) * (1.0 - age / duration).powf(1.3)
    };
    match pulse {
        Pulse::Snap => hit(0.0, 0.78),
        Pulse::Double => hit(0.0, 0.42).max(0.8 * hit(0.5, 0.28)),
        // A modest dropout in a local band, not full-screen black/white flashes.
        Pulse::Flicker => {
            hit(0.0, 0.78)
                * if (0.22..0.32).contains(&phase) {
                    0.35
                } else {
                    1.0
                }
        }
        Pulse::Swell => {
            if phase < 0.75 {
                (PI * phase / 0.75).sin().powi(2)
            } else {
                0.0
            }
        }
        Pulse::Hold => {
            if phase < 0.7 {
                (phase / 0.12).min(1.0) * 0.65
            } else {
                0.0
            }
        }
    }
}

fn hex_band(x: f64, y: f64) -> f64 {
    let (hx, hz, _, _) = hex_cell(x, y);
    hx + hz * 2.0
}

fn hex_cell(x: f64, y: f64) -> (f64, f64, f64, f64) {
    let q = (3.0_f64.sqrt() / 3.0 * x - y / 3.0) / 0.065;
    let r = (2.0 / 3.0 * y) / 0.065;
    let mut hx = q.round();
    let mut hz = r.round();
    let hy = (-q - r).round();
    let ex = (hx - q).abs();
    let ez = (hz - r).abs();
    let ey = (hy + q + r).abs();
    if ex > ey && ex > ez {
        hx = -hy - hz;
    } else if ez > ey {
        hz = -hx - hy;
    }
    (
        hx,
        hz,
        x - 0.065 * 3.0_f64.sqrt() * (hx + hz / 2.0),
        y - 0.065 * 1.5 * hz,
    )
}

#[derive(Clone, Copy)]
struct Sample {
    color: Rgb,
    counter_color: Rgb,
    text_color: Rgb,
    light: f64,
    wash: f64,
    trace: f64,
    glyph: &'static str,
    font_hit: f64,
}

fn exposed_background(cell: &ratatui::buffer::Cell, base: Color) -> bool {
    (matches!(cell.bg, Color::Reset | Color::Black) || cell.bg == base)
        && !cell
            .modifier
            .intersects(Modifier::REVERSED | Modifier::HIDDEN)
        && cell.diff_option != CellDiffOption::Skip
}

/// Reserve a cell of breathing room at both ends of every blank run. Account
/// for wide graphemes: ratatui resets their continuation cells to plain spaces.
fn texture_mask(buf: &Buffer, area: Rect, base: Color) -> Vec<bool> {
    let width = usize::from(area.width);
    let mut mask = vec![false; width * usize::from(area.height)];
    for y in area.top()..area.bottom() {
        let mut covered_until = area.left();
        let mut run_start = area.left();
        for x in area.left()..=area.right() {
            let clear = if x < area.right() {
                let cell = &buf[(x, y)];
                let clear = x >= covered_until
                    && cell.symbol() == " "
                    && cell.modifier.is_empty()
                    && exposed_background(cell, base);
                if cell.symbol() != " " {
                    covered_until = x.saturating_add(match cell.diff_option {
                        CellDiffOption::ForcedWidth(width) => width.get(),
                        _ => Span::raw(cell.symbol()).width().max(1) as u16,
                    });
                }
                clear
            } else {
                false
            };
            if !clear {
                if x.saturating_sub(run_start) >= 4 {
                    let row = usize::from(y - area.y) * width;
                    for column in run_start + 1..x - 1 {
                        mask[row + usize::from(column - area.x)] = true;
                    }
                }
                run_start = x.saturating_add(1);
            }
        }
    }
    mask
}

pub(super) fn apply(buf: &mut Buffer, area: Rect, ctx: &ThemeContext) {
    let area = area.intersection(buf.area);
    let score = Score::at(ctx.frame_time);
    let semantic = ctx.theme.semantic;
    let base = color_to_rgb(semantic.surface0);
    let texture = texture_mask(buf, area, semantic.surface0);
    let state = ctx.theme.role_slots().state;
    let neutral = [
        semantic.text,
        semantic.subtext0,
        semantic.subtext1,
        semantic.overlay0,
        semantic.surface2,
    ];
    // Approximate terminal character aspect: cells are twice as tall as wide.
    let aspect = (f64::from(area.width) / (f64::from(area.height.max(1)) * 2.0)).clamp(0.25, 4.0);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let Some(cell) = buf.cell_mut((x, y)) else {
                continue;
            };
            if !exposed_background(cell, semantic.surface0) {
                continue;
            }
            let nx = f64::from(x - area.x) / f64::from(area.width.max(1));
            let ny = f64::from(y - area.y) / f64::from(area.height.max(1));
            let sample = score.sample(nx, ny, aspect);
            let wash = blend_colors(base, sample.color, 0.025 + sample.wash * 0.20);
            cell.bg = blend_colors(
                color_to_rgb(wash),
                sample.counter_color,
                0.012 + sample.trace * 0.075,
            );
            let index = usize::from(y - area.y) * usize::from(area.width) + usize::from(x - area.x);
            if texture[index] && sample.trace > 0.07 {
                cell.set_symbol(sample.glyph);
                cell.fg = blend_colors(
                    color_to_rgb(cell.bg),
                    sample.counter_color,
                    0.10 + sample.trace * 0.36,
                );
            } else if cell.fg == semantic.border {
                // Outlines carry a much stronger saturated chase than body text.
                let color = blend_colors(sample.color, sample.counter_color, sample.trace);
                let hit = sample.font_hit * 0.65 + sample.light * score.gain * 0.35;
                cell.fg = blend_colors(base, color_to_rgb(color), 0.48 + hit * 0.52);
            } else if neutral.contains(&cell.fg) {
                let white = if cell.fg == semantic.text { 0.48 } else { 0.38 };
                cell.fg = blend_colors(
                    sample.text_color,
                    (255, 255, 255),
                    white + sample.font_hit * 0.32,
                );
            } else if cell.fg != Color::Reset
                && ![state.error, state.warning, state.success].contains(&cell.fg)
            {
                // Metrics and chart accents participate through light, keeping
                // their categorical hue. Critical status colors stay exact.
                cell.fg = blend_colors(
                    color_to_rgb(cell.fg),
                    (255, 255, 255),
                    sample.font_hit * 0.28,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Theme, ThemeEffects, ThemeName};
    use ratatui::{backend::TestBackend, style::Style, Terminal};
    use std::collections::HashSet;

    #[test]
    fn all_thirty_scenes_have_distinct_spatial_patterns() {
        let mut fingerprints = HashSet::new();
        for (scene_index, scene) in SCENES.iter().enumerate() {
            let mut field = Vec::new();
            for time in [0.06, 0.57, 1.29] {
                let mut score = Score::at(time);
                score.scene_index = scene_index;
                score.energy = 1.0;
                for y in 0..40 {
                    for x in 0..120 {
                        field.push(u8::from(
                            score.sample(x as f64 / 120.0, y as f64 / 40.0, 1.5).light > 0.0,
                        ));
                    }
                }
            }
            let occupied = field.iter().filter(|&&v| v != 0).count();
            assert!(
                occupied > field.len() / 20 && occupied < field.len() / 2,
                "{:?}: pattern must have both active and quiet regions",
                scene.pattern
            );
            assert!(
                fingerprints.insert(field),
                "duplicate pattern: {:?}",
                scene.pattern
            );
        }
        assert_eq!(fingerprints.len(), 30);
    }

    #[test]
    fn score_rotates_every_scene_and_repeats_without_frame_history() {
        for i in 0..30 {
            let time = i as f64 * SCENE_SECONDS + 0.07;
            let a = Score::at(time);
            let b = Score::at(time + SCENE_SECONDS * 30.0);
            assert_eq!(a.scene_index, i);
            assert_eq!(a.scene_index, b.scene_index);
            assert_eq!(a.step, b.step);
            assert!((a.energy - b.energy).abs() < 1e-10);
            assert_eq!(
                Score::at((i + 1) as f64 * SCENE_SECONDS + 1e-6).scene_index,
                (i + 1) % 30
            );
        }
        for time in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            assert_eq!(Score::at(time).scene_index, 0);
        }
        assert!(Score::at(1e12).energy.is_finite());
    }

    #[test]
    fn each_pulse_has_quiet_time_and_a_bounded_attack() {
        let mut fingerprints = HashSet::new();
        for pulse in [
            Pulse::Snap,
            Pulse::Double,
            Pulse::Flicker,
            Pulse::Swell,
            Pulse::Hold,
        ] {
            let values: Vec<_> = (0..100)
                .map(|i| envelope(pulse, i as f64 / 100.0))
                .collect();
            assert!(values.iter().all(|v| (0.0..=1.0).contains(v)));
            assert!(values.iter().any(|&v| v > 0.5));
            assert!(values[80..].iter().all(|&v| v == 0.0));
            assert!(fingerprints.insert(
                values
                    .iter()
                    .map(|v| (v * 1000.0) as u16)
                    .collect::<Vec<_>>()
            ));
        }
    }

    #[test]
    fn postpass_preserves_symbols_semantics_selection_and_modifiers() {
        let theme = Theme::show();
        let area = Rect::new(7, 11, 16, 2);
        let mut original = Buffer::empty(area);
        original.set_string(
            7,
            11,
            "界 e\u{301} demo",
            Style::default().fg(theme.semantic.text),
        );
        original[(7, 12)]
            .set_symbol("!")
            .set_fg(theme.role_slots().state.error);
        original[(8, 12)]
            .set_symbol("?")
            .set_fg(theme.role_slots().state.warning);
        original[(9, 12)]
            .set_symbol("+")
            .set_fg(theme.role_slots().state.success);
        original[(10, 12)].set_symbol("S").set_style(
            Style::default()
                .fg(theme.semantic.text)
                .bg(theme.semantic.surface1),
        );
        original[(11, 12)].set_symbol("R").set_style(
            Style::default()
                .fg(theme.semantic.text)
                .add_modifier(Modifier::REVERSED),
        );
        original[(12, 12)].set_symbol("H").set_style(
            Style::default()
                .fg(theme.semantic.text)
                .add_modifier(Modifier::HIDDEN),
        );
        original[(15, 12)].set_diff_option(CellDiffOption::Skip);
        let mask = texture_mask(&original, area, theme.semantic.surface0);
        // The second column of the wide grapheme and spaces inside text are
        // reserved, even though ratatui represents them as ordinary blank cells.
        assert!(!mask[1]);
        assert!(!mask[2]);
        assert!(!mask[4]);
        for scene in 0..30 {
            let mut result = original.clone();
            apply(
                &mut result,
                area,
                &ThemeContext::new(theme, scene as f64 * SCENE_SECONDS + 0.07),
            );
            for (i, (before, after)) in original.content.iter().zip(&result.content).enumerate() {
                if !mask[i] {
                    assert_eq!(before.symbol(), after.symbol());
                }
                assert_eq!(before.modifier, after.modifier);
                assert_eq!(before.diff_option, after.diff_option);
            }
            for x in 7..10 {
                assert_eq!(result[(x, 12)].fg, original[(x, 12)].fg);
            }
            for x in 10..13 {
                assert_eq!(result[(x, 12)], original[(x, 12)]);
            }
            assert_eq!(result[(15, 12)], original[(15, 12)]);
            assert_ne!(original[(7, 11)].fg, result[(7, 11)].fg);
        }
    }

    #[test]
    fn every_scene_has_layered_texture_and_a_quieter_break() {
        let theme = Theme::show();
        let area = Rect::new(0, 0, 120, 40);
        for scene in 0..30 {
            let mut phase_colors = Vec::new();
            let mut levels = Vec::new();
            for phrase in 0..4 {
                let mut colors = Vec::new();
                let mut textured = 0;
                let mut level = 0.0;
                // Sample attacks and decay, rather than treating an intentional
                // quiet frame as the brightness of the entire phrase.
                for phase in [0.04, 0.15, 0.24] {
                    let time =
                        scene as f64 * SCENE_SECONDS + phrase as f64 * 8.0 * STEP_SECONDS + phase;
                    let mut buffer = Buffer::empty(area);
                    apply(&mut buffer, area, &ThemeContext::new(theme, time));
                    textured += buffer.content.iter().filter(|c| c.symbol() != " ").count();
                    level += buffer.content.iter().map(|c| luminance(c.bg)).sum::<f64>();
                    colors.extend(buffer.content.iter().map(|c| c.bg));
                }
                assert!(
                    textured > 30 && textured < colors.len() / 2,
                    "scene {scene}, phrase {phrase}: texture {textured}"
                );
                assert!(
                    colors.iter().copied().collect::<HashSet<_>>().len() > 8,
                    "scene {scene}: needs depth beyond an on/off wash"
                );
                levels.push(level);
                phase_colors.push(colors);
            }
            assert!(
                levels[2] < levels[1] * 0.8,
                "scene {scene}: break must relieve the peak"
            );
            assert!(phase_colors.windows(2).all(|pair| pair[0] != pair[1]));
        }
    }

    #[test]
    fn typography_and_borders_pulse_with_the_score() {
        let theme = Theme::show();
        let area = Rect::new(0, 0, 40, 8);
        let mut variations: [HashSet<_>; 3] = std::array::from_fn(|_| HashSet::new());
        for time in [0.04, 0.15, 0.3, 0.55, 0.95, 3.35, 6.55, 9.75] {
            let mut buffer = Buffer::empty(area);
            for (i, cell) in buffer.content.iter_mut().enumerate() {
                cell.set_symbol("x").set_fg(
                    [
                        theme.semantic.text,
                        theme.semantic.border,
                        Color::Rgb(70, 170, 230),
                    ][i % 3],
                );
            }
            apply(&mut buffer, area, &ThemeContext::new(theme, time));
            for (i, cell) in buffer.content.iter().enumerate() {
                variations[i % 3].insert(cell.fg);
            }
        }
        assert!(variations.iter().all(|colors| colors.len() > 12));
    }

    #[test]
    fn frame_origins_small_sizes_and_frozen_time_are_stable() {
        let theme = Theme::show();
        let ctx = ThemeContext::new(theme, 1.07);
        for (width, height) in [(0, 0), (1, 1), (2, 1), (80, 24), (160, 60)] {
            let a = Rect::new(0, 0, width, height);
            let b = Rect::new(9, 17, width, height);
            let mut first = Buffer::empty(a);
            let mut shifted = Buffer::empty(b);
            apply(&mut first, a, &ctx);
            apply(&mut shifted, b, &ctx);
            assert_eq!(first.content, shifted.content);
            let mut repeated = Buffer::empty(a);
            apply(&mut repeated, a, &ctx);
            assert_eq!(first, repeated);
        }
    }

    fn luminance(color: Color) -> f64 {
        let (r, g, b) = color_to_rgb(color);
        let linear = |c: u8| {
            let v = f64::from(c) / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
    }

    #[test]
    fn animated_text_remains_readable_in_every_scene() {
        let theme = Theme::show();
        let area = Rect::new(0, 0, 48, 16);
        for scene in 0..30 {
            for phase in [0.0, 0.06, 0.12, 0.24, 0.35, 0.59, 3.24, 3.35, 6.55, 9.75] {
                let mut buffer = Buffer::empty(area);
                for (i, cell) in buffer.content.iter_mut().enumerate() {
                    cell.fg = [
                        theme.semantic.text,
                        theme.semantic.subtext0,
                        theme.semantic.subtext1,
                        theme.semantic.overlay0,
                        theme.semantic.surface2,
                    ][i % 5];
                    cell.set_symbol("x");
                }
                apply(
                    &mut buffer,
                    area,
                    &ThemeContext::new(theme, scene as f64 * SCENE_SECONDS + phase),
                );
                for cell in &buffer.content {
                    let ratio = (luminance(cell.fg) + 0.05) / (luminance(cell.bg) + 0.05);
                    assert!(
                        ratio >= 4.5,
                        "scene {scene}, time {phase}: contrast {ratio}"
                    );
                }
            }
        }
    }

    #[test]
    fn show_serializes_and_remains_selectable() {
        assert_eq!(serde_json::to_string(&ThemeName::Show).unwrap(), "\"show\"");
        assert_eq!(
            serde_json::from_str::<ThemeName>("\"Show\"").unwrap(),
            ThemeName::Show
        );
        assert!(ThemeName::sorted_for_ui().contains(&ThemeName::Show));
        assert!(Theme::show().effects.enabled());
        assert!(!Theme::show().effects.particle.enabled);
    }

    #[test]
    fn real_screens_render_with_show_without_losing_text() {
        use crate::{
            app::{AppMode, AppState},
            config::Settings,
            dht_service::{DhtStatus, DhtWaveTelemetry},
        };
        let settings = Settings::default();
        for mode in [
            AppMode::Normal,
            AppMode::Help,
            AppMode::Journal,
            AppMode::PeerManagement,
            AppMode::TorrentManagement,
            AppMode::Config,
            AppMode::FileBrowser,
            AppMode::Rss,
            AppMode::DeleteConfirm,
            AppMode::PowerSaving,
            AppMode::Welcome,
        ] {
            let mut state = AppState {
                mode,
                theme: Theme::show(),
                ..AppState::default()
            };
            state.ui.effects_phase_time = 0.07;
            state.theme.effects = ThemeEffects::default();
            let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal
                .draw(|f| {
                    crate::tui::view::draw(
                        f,
                        &state,
                        &DhtStatus::default(),
                        &DhtWaveTelemetry::default(),
                        &settings,
                    );
                    // Compare one captured frame: the welcome screen has its own
                    // wall-clock dust, so separate draws are not symbol-identical.
                    let original = f.buffer_mut().clone();
                    let mask =
                        texture_mask(&original, original.area, state.theme.semantic.surface0);
                    super::super::apply_theme_effects_to_frame(
                        f,
                        &ThemeContext::new(Theme::show(), 0.07),
                    );
                    for (i, (before, after)) in original
                        .content
                        .iter()
                        .zip(&f.buffer_mut().content)
                        .enumerate()
                    {
                        if !mask[i] {
                            assert_eq!(before.symbol(), after.symbol());
                        }
                        assert_eq!(before.modifier, after.modifier);
                    }
                })
                .unwrap();
        }
    }
    /// Optional visual review artifact drawn by the real native screen renderer.
    #[test]
    #[ignore = "writes native frames to SUPERSEEDR_SHOW_GALLERY"]
    fn render_native_show_gallery() {
        use crate::{
            app::AppState,
            config::Settings,
            dht_service::{DhtStatus, DhtWaveTelemetry},
        };
        let path = std::env::var("SUPERSEEDR_SHOW_GALLERY").expect("set gallery output path");
        let settings = Settings::default();
        let mut state = AppState {
            theme: Theme::show(),
            ..AppState::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let mut frames = Vec::new();
        for (index, scene) in SCENES.iter().enumerate() {
            // One attack from each phrase shows the build, peak, break and return.
            for phase in [0.15, 3.35, 6.55, 9.75] {
                state.ui.effects_phase_time = index as f64 * SCENE_SECONDS + phase;
                terminal
                    .draw(|f| {
                        crate::tui::view::draw(
                            f,
                            &state,
                            &DhtStatus::default(),
                            &DhtWaveTelemetry::default(),
                            &settings,
                        )
                    })
                    .unwrap();
                let cells: Vec<_> = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| {
                        serde_json::json!([
                            cell.symbol(),
                            color_to_rgb(cell.fg),
                            color_to_rgb(cell.bg)
                        ])
                    })
                    .collect();
                frames.push(serde_json::json!({"name":format!("{:?}", scene.pattern), "phase":phase, "cells":cells}));
            }
        }
        std::fs::write(path, serde_json::to_vec(&frames).unwrap()).unwrap();
    }
}
