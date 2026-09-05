// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Short foreground cues sampled from Show's score. Each cohort has fixed seeds
//! and analytic trajectories, so redraws, frame drops and resizing need no state.

use super::{blend_colors, color_to_rgb, Buffer, Pattern, Rect, Score, SCENES, TAU};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Movement {
    Shards,
    Warp,
    Vortex,
    Glints,
    Wisps,
    Comets,
    Nova,
    Fountain,
    Confetti,
}

impl Movement {
    fn for_pattern(pattern: Pattern) -> Self {
        match pattern {
            Pattern::PrismChase
            | Pattern::MirrorShards
            | Pattern::DiamondLattice
            | Pattern::ZigzagLadder
            | Pattern::Crosshatch => Self::Shards,
            Pattern::PulseTunnel | Pattern::WarpGrid | Pattern::Hourglass => Self::Warp,
            Pattern::SpiralDrive | Pattern::Pinwheel | Pattern::OrbitInterference => Self::Vortex,
            Pattern::EchoChamber | Pattern::Honeycomb | Pattern::RipplePool => Self::Glints,
            Pattern::WaveInterference
            | Pattern::SineRibbons
            | Pattern::MoireWeave
            | Pattern::WovenRings => Self::Wisps,
            Pattern::RadarSweep
            | Pattern::SignalRain
            | Pattern::CircuitTraces
            | Pattern::SplitScan => Self::Comets,
            Pattern::StarAperture
            | Pattern::DiamondEcho
            | Pattern::Rosette
            | Pattern::ShutterFan => Self::Nova,
            Pattern::SteppedTerraces => Self::Fountain,
            Pattern::CheckerSwitch | Pattern::BinaryWeave | Pattern::PolarChecker => Self::Confetti,
        }
    }

    fn count(self) -> usize {
        match self {
            Self::Shards => 32,
            Self::Warp => 48,
            Self::Vortex => 24,
            Self::Glints => 10,
            Self::Wisps => 14,
            Self::Comets => 24,
            Self::Nova => 56,
            Self::Fountain => 32,
            Self::Confetti => 44,
        }
    }

    fn wild(self) -> bool {
        matches!(
            self,
            Self::Shards | Self::Warp | Self::Nova | Self::Confetti
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Mark {
    x: f64,
    y: f64,
    light: f64,
    glyph: &'static str,
    pair: usize,
    head: bool,
}

// Spatial variation is fixed for the lifetime of a cue; no frame-time noise.
fn unit(seed: usize) -> f64 {
    let mut n = (seed as u32).wrapping_add(0x9e37_79b9);
    n = (n ^ (n >> 16)).wrapping_mul(0x85eb_ca6b);
    n = (n ^ (n >> 13)).wrapping_mul(0xc2b2_ae35);
    f64::from(n ^ (n >> 16)) / f64::from(u32::MAX)
}

struct Flight {
    movement: Movement,
    pattern: Pattern,
    aspect: f64,
    cue: usize,
    index: usize,
    seed: usize,
}

impl Flight {
    fn position(&self, age: f64) -> (f64, f64) {
        let a = unit(self.seed);
        let b = unit(self.seed + 31);
        let side = if self.index.is_multiple_of(2) {
            -1.0
        } else {
            1.0
        };
        let angle = (self.index % 12) as f64 * TAU / 12.0 + a * 0.25 + self.cue as f64 * 0.35;
        let (x, y) = match self.movement {
            Movement::Shards => (
                side * self.aspect * (0.46 - age * (0.42 + a * 0.25)),
                (b - 0.5) * 0.45 + side * age * (a - 0.5) * 0.5 + age * age * 0.12,
            ),
            Movement::Warp => {
                let r = (0.025 + age * age * (0.8 + a * 0.6)) * self.aspect.max(1.0);
                (angle.cos() * r, angle.sin() * r)
            }
            Movement::Vortex => {
                let r = 0.09 + a * 0.32 + age * 0.10;
                let theta = angle + age * (2.8 + b);
                (theta.cos() * r * self.aspect, theta.sin() * r)
            }
            Movement::Glints => {
                let r = 0.12 + b * 0.28 + age * 0.04;
                (angle.cos() * r * self.aspect, angle.sin() * r)
            }
            Movement::Wisps => (
                (a - 0.5) * self.aspect + (age * 2.5 + b * TAU).sin() * 0.10,
                (b - 0.5) * 0.7 - age * 0.18,
            ),
            Movement::Comets => match self.pattern {
                Pattern::SignalRain => ((a - 0.5) * self.aspect, -0.46 + age * (0.7 + b * 0.4)),
                Pattern::CircuitTraces => {
                    let lane = ((a * 10.0).floor() / 10.0 - 0.5) * self.aspect;
                    (
                        lane + (age - 0.5).max(0.0) * side * 0.5,
                        -0.4 + age.min(0.5) * 1.2,
                    )
                }
                Pattern::RadarSweep => {
                    let theta = self.cue as f64 * TAU / 8.0 + age * 1.7;
                    let r = 0.18 + b * 0.28;
                    (theta.cos() * r * self.aspect, theta.sin() * r)
                }
                _ => (side * self.aspect * (0.48 - age), (b - 0.5) * 0.7),
            },
            Movement::Nova => {
                let r = age * (0.35 + a * 0.65);
                let center = if self.cue.is_multiple_of(4) {
                    -0.18
                } else {
                    0.18
                };
                (
                    center * self.aspect + angle.cos() * r,
                    angle.sin() * r * 0.65,
                )
            }
            Movement::Fountain => (
                side * self.aspect * (0.35 - age * (0.25 + a * 0.12)),
                0.43 - age * (1.3 + b * 0.25) + age * age * 0.95,
            ),
            Movement::Confetti => (
                (a - 0.5) * self.aspect + (age * 7.0 + b * TAU).sin() * 0.07,
                (b - 0.5) * 0.55 - age * (0.4 + a * 0.15) + age * age * 0.65,
            ),
        };
        (0.5 + x / self.aspect, 0.5 + y)
    }

    fn glyph(&self, age: f64) -> &'static str {
        match self.movement {
            Movement::Shards | Movement::Confetti => {
                ["╱", "◇", "╲", "▪"][(self.index + (age * 5.0) as usize) % 4]
            }
            Movement::Warp | Movement::Comets => "•",
            Movement::Vortex => "✧",
            Movement::Nova => "✦",
            Movement::Fountain => "◆",
            Movement::Glints | Movement::Wisps => "·",
        }
    }
}

fn marks(score: Score, area: Rect) -> Vec<Mark> {
    let position = score.step as f64 + score.phase;
    if area.is_empty() || score.phrase == 2 || position >= 7.75 {
        return Vec::new();
    }
    let pattern = SCENES[score.scene_index].pattern;
    let movement = Movement::for_pattern(pattern);
    let cues: &[usize] = match (score.phrase, movement.wild()) {
        (1, true) => &[0, 2, 4, 6],
        (3, true) => &[0, 3, 6],
        _ => &[0, 4],
    };
    let aspect = (f64::from(area.width) / (f64::from(area.height) * 2.0)).clamp(0.25, 4.0);
    let scale = (f64::from(area.width) * f64::from(area.height) / 4800.0)
        .sqrt()
        .clamp(0.25, 1.6);
    let count = (movement.count() as f64 * scale * [0.35, 1.0, 0.0, 1.15][score.phrase]) as usize;
    let pair = (score.step / 2 + score.phrase / 2) % 2;
    let mut result = Vec::with_capacity(count * 4);
    for &cue in cues {
        // Last cue finishes before the phrase cut. Earlier cues have a little
        // room for overlapping tails on the energetic scenes.
        let lifetime = if cue >= 6 { 1.65 } else { 2.15 };
        for index in 0..count {
            let spatial_id = if matches!(movement, Movement::Shards | Movement::Fountain) {
                index / 2
            } else {
                index
            };
            let seed = score.scene_index * 1024 + cue * 97 + spatial_id;
            let age = (position - cue as f64 - unit(seed + 7) * 0.12) / lifetime;
            if !(0.0..1.0).contains(&age) {
                continue;
            }
            let light = (age / 0.08).min(1.0) * (1.0 - age).powf(0.7);
            let flight = Flight {
                movement,
                pattern,
                aspect,
                cue,
                index,
                seed,
            };
            let trail = match movement {
                Movement::Glints | Movement::Wisps | Movement::Confetti => 1,
                Movement::Warp | Movement::Comets => 4,
                _ => 3,
            };
            for tail in (0..trail).rev() {
                let past = age - tail as f64 * 0.035;
                if past < 0.0 {
                    continue;
                }
                let (x, y) = flight.position(past);
                result.push(Mark {
                    x,
                    y,
                    light: light * 0.55_f64.powi(tail),
                    glyph: if tail == 0 { flight.glyph(age) } else { "·" },
                    pair: (pair + index % 2) % 2,
                    head: tail == 0,
                });
            }
        }
    }
    result
}

pub(super) fn apply(buf: &mut Buffer, area: Rect, clear_space: &[bool], score: Score) {
    let mut particles = marks(score, area);
    if particles.is_empty() {
        return;
    }
    // Heads take priority over tails when cues intersect or a small viewport
    // reaches its density budget. The original UI's mask remains authoritative.
    particles.sort_by(|a, b| {
        b.head
            .cmp(&a.head)
            .then_with(|| b.light.total_cmp(&a.light))
    });
    let mut occupied = vec![false; clear_space.len()];
    let budget = (clear_space.len() / 16).min(320);
    let mut drawn = 0;
    for particle in particles {
        if drawn >= budget {
            break;
        }
        if !(0.0..1.0).contains(&particle.x) || !(0.0..1.0).contains(&particle.y) {
            continue;
        }
        let x = (particle.x * f64::from(area.width)) as u16;
        let y = (particle.y * f64::from(area.height)) as u16;
        let index = usize::from(y) * usize::from(area.width) + usize::from(x);
        if !clear_space[index] || occupied[index] || particle.light < 0.06 {
            continue;
        }
        occupied[index] = true;
        drawn += 1;
        let cell = &mut buf[(area.x + x, area.y + y)];
        let palette = SCENES[score.scene_index].palette[particle.pair];
        let source = blend_colors(
            palette,
            (255, 255, 255),
            if particle.head { 0.45 } else { 0.05 },
        );
        cell.set_symbol(particle.glyph);
        cell.fg = blend_colors(color_to_rgb(cell.bg), color_to_rgb(source), particle.light);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        clear_background_markers, prepare_background, texture_mask, SCENE_SECONDS, STEP_SECONDS,
    };
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn cues_repeat_and_leave_the_break_and_phrase_cut_clear() {
        let area = Rect::new(0, 0, 120, 40);
        for scene in 0..30 {
            let start = scene as f64 * SCENE_SECONDS;
            let score = Score::at(start + 3.35);
            assert_eq!(marks(score, area), marks(score, area));
            assert!(!marks(score, area).is_empty(), "scene {scene}");
            for step in 16..24 {
                assert!(
                    marks(Score::at(start + (step as f64 + 0.5) * STEP_SECONDS), area).is_empty()
                );
            }
            for phrase in 0..4 {
                assert!(marks(
                    Score::at(start + (phrase as f64 * 8.0 + 7.9) * STEP_SECONDS),
                    area
                )
                .is_empty());
            }
        }
    }

    #[test]
    fn particles_move_and_wild_scenes_get_larger_cues() {
        let area = Rect::new(0, 0, 120, 40);
        let wild = Score::at(14.0 * SCENE_SECONDS + 3.35);
        let calm = Score::at(7.0 * SCENE_SECONDS + 3.35);
        let build = Score::at(14.0 * SCENE_SECONDS + 0.15);
        assert!(marks(wild, area).len() > marks(calm, area).len() * 3);
        assert!(marks(wild, area).len() > marks(build, area).len() * 2);
        let later = Score::at(14.0 * SCENE_SECONDS + 3.55);
        assert_ne!(marks(wild, area), marks(later, area));
    }

    #[test]
    fn foreground_is_bounded_and_never_overwrites_ui_content() {
        let theme = Theme::show();
        let area = Rect::new(5, 7, 80, 24);
        let mut original = Buffer::empty(area);
        prepare_background(&mut original, area);
        original.set_string(
            5,
            12,
            "界 e\u{301}   sample transfer",
            ratatui::style::Style::default(),
        );
        for x in 5..85 {
            original[(x, 18)].bg = theme.semantic.surface1;
        }
        let mask = texture_mask(&original, area, theme.semantic.surface0);
        clear_background_markers(&mut original, area);
        let mut saw_particle = false;
        for scene in 0..30 {
            for phase in [0.15, 3.35, 4.15, 9.75, 12.25] {
                let mut result = original.clone();
                apply(
                    &mut result,
                    area,
                    &mask,
                    Score::at(scene as f64 * SCENE_SECONDS + phase),
                );
                let mut changed = 0;
                for (i, (before, after)) in original.content.iter().zip(&result.content).enumerate()
                {
                    if before != after {
                        assert!(mask[i]);
                        assert_eq!(ratatui::text::Span::raw(after.symbol()).width(), 1);
                        changed += 1;
                    }
                }
                assert!(changed <= original.content.len() / 16);
                saw_particle |= changed > 0;
            }
        }
        assert!(saw_particle);
    }
}
